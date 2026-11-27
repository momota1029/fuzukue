//! GitHub device authorization and the authenticated issue-write path.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use fuzukue_proto::{
    GitHubAvatar, GitHubConnectState as State, GitHubReportAction as Action,
    GitHubReportError as Error, GitHubReportStatus as Status, GitHubReportUpdate as Update,
    ServerEvent, WorkspaceKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    time::{self, Instant},
};

use crate::{issue_threads::GitHubConfig, Workspace};

const DEVICE_CODE: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN: &str = "https://github.com/login/oauth/access_token";
const VERIFY: &str = "https://github.com/login/device";
const API: &str = "https://api.github.com";
const MAX_RESPONSE: u64 = 1024 * 1024;

type Result<T> = std::result::Result<T, Error>;
const RECONNECT: Error = Error::Rejected(401);

fn reconnect_message() -> &'static str {
    harness_text!(
        "Disconnect and reconnect GitHub.",
        "GitHub の連携を解除して、接続し直してください。",
    )
}

fn record_401(endpoint: &'static str) {
    tracing::warn!(at = %Utc::now(), endpoint, "GitHub returned HTTP 401");
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    #[default]
    PublicRepo,
    Repo,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::PublicRepo => "public_repo",
            Self::Repo => "repo",
        }
    }
}

// Requests and credentials deliberately have no Debug implementation.
struct Request {
    url: String,
    token: Option<String>,
    body: Body,
}

enum Body {
    Get,
    Form(String),
    Json(Value),
}

trait Http: Send + Sync {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Value>>;

    fn avatar(&self, _url: String) -> BoxFuture<'_, Option<(String, String)>> {
        Box::pin(async { None })
    }
}

struct GitHubHttp;

impl Http for GitHubHttp {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                use std::sync::LazyLock;
                static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
                    ureq::AgentBuilder::new()
                        .redirects(0)
                        .timeout(Duration::from_secs(15))
                        .user_agent(concat!("fuzukue/", env!("CARGO_PKG_VERSION")))
                        .build()
                });
                let is_api = request.token.is_some();
                let mut http = match request.body {
                    Body::Get => AGENT.get(&request.url),
                    _ => AGENT.post(&request.url),
                }
                .set(
                    "Accept",
                    if is_api {
                        "application/vnd.github+json"
                    } else {
                        "application/json"
                    },
                );
                if let Some(token) = request.token {
                    http = http.set("Authorization", &format!("Bearer {token}"));
                }
                let response = match request.body {
                    Body::Get => http.call(),
                    Body::Form(form) => http
                        .set("Content-Type", "application/x-www-form-urlencoded")
                        .send_string(&form),
                    Body::Json(body) => http
                        .set("Content-Type", "application/json")
                        .send_string(&body.to_string()),
                };
                // Discard transport diagnostics and API error bodies. They may contain secrets.
                let response = match response {
                    Ok(response) => response,
                    Err(ureq::Error::Status(code, response)) if !is_api && code == 400 => response,
                    Err(ureq::Error::Status(code, _)) => return Err(Error::Rejected(code)),
                    Err(_) => return Err(Error::Offline),
                };
                if response.status() >= 300 && response.status() != 400 {
                    return Err(Error::Rejected(response.status()));
                }
                let mut data = Vec::new();
                response
                    .into_reader()
                    .take(MAX_RESPONSE + 1)
                    .read_to_end(&mut data)
                    .map_err(|_| Error::Response)?;
                if data.len() as u64 > MAX_RESPONSE {
                    return Err(Error::Response);
                }
                serde_json::from_slice(&data).map_err(|_| Error::Response)
            })
            .await
            .map_err(|_| Error::Offline)?
        })
    }

    fn avatar(&self, url: String) -> BoxFuture<'_, Option<(String, String)>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || avatar::fetch(&url))
                .await
                .ok()
                .flatten()
        })
    }
}

async fn form(http: &dyn Http, url: &str, fields: &[(&str, &str)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    let result = http
        .send(Request {
            url: url.into(),
            token: None,
            body: Body::Form(body),
        })
        .await;
    if result == Err(RECONNECT) {
        record_401(if url == DEVICE_CODE {
            "POST /login/device/code"
        } else {
            "POST /login/oauth/access_token (device authorization)"
        });
        return Err(Error::Config);
    }
    result
}

async fn api(http: &dyn Http, path: &str, token: &str, body: Body) -> Result<Value> {
    http.send(Request {
        url: format!("{API}/{path}"),
        token: Some(token.into()),
        body,
    })
    .await
}

struct Device {
    device_code: String,
    user_code: String,
    deadline: Instant,
    expires_at: i64,
    interval: Duration,
}

impl Device {
    async fn start(http: &dyn Http, client_id: &str, scope: Scope) -> Result<Self> {
        let response = form(
            http,
            DEVICE_CODE,
            &[("client_id", client_id), ("scope", scope.name())],
        )
        .await?;
        let life = response["expires_in"]
            .as_u64()
            .filter(|seconds| (1..=3600).contains(seconds))
            .ok_or(Error::Response)?;
        let interval = response["interval"].as_u64().unwrap_or(5).clamp(1, 3600);
        if response["verification_uri"].as_str() != Some(VERIFY) {
            return Err(Error::Response);
        }
        let device_code = field(&response, "device_code")?;
        let user_code = field(&response, "user_code")?;
        if device_code == user_code
            || !user_code.split_once('-').is_some_and(|(a, b)| {
                [a, b].into_iter().all(|part| {
                    part.len() == 4
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                })
            })
        {
            return Err(Error::Response);
        }
        Ok(Self {
            device_code: device_code.into(),
            user_code: user_code.into(),
            deadline: Instant::now() + Duration::from_secs(life),
            expires_at: chrono::Utc::now().timestamp() + life as i64,
            interval: Duration::from_secs(interval),
        })
    }

    fn state(&self) -> State {
        State::Pending {
            code: self.user_code.clone(),
            url: VERIFY.into(),
            expires_at: self.expires_at,
        }
    }

    async fn authorize(
        &self,
        http: &dyn Http,
        client_id: &str,
        scope: Scope,
    ) -> Result<Credential> {
        time::timeout_at(self.deadline, async {
            let mut interval = self.interval;
            loop {
                time::sleep(interval).await;
                let response = form(
                    http,
                    ACCESS_TOKEN,
                    &[
                        ("client_id", client_id),
                        ("device_code", &self.device_code),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                )
                .await?;
                match response["error"].as_str() {
                    Some("authorization_pending") => {}
                    Some("slow_down") => {
                        let advertised = Duration::from_secs(
                            response["interval"].as_u64().unwrap_or(0).min(3600),
                        );
                        interval = (interval + Duration::from_secs(5)).max(advertised);
                    }
                    Some("access_denied") => return Err(Error::Denied),
                    Some("expired_token") => return Err(Error::Expired),
                    Some(_) => return Err(Error::Response),
                    None => {
                        let connected_at = Utc::now();
                        let token = field(&response, "access_token")?;
                        let refresh_token = response["refresh_token"]
                            .as_str()
                            .filter(|value| !value.is_empty());
                        if !token
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
                        {
                            return Err(Error::Response);
                        }
                        let user = api(http, "user", token, Body::Get).await;
                        if user == Err(RECONNECT) {
                            record_401("GET /user (device authorization)");
                        }
                        tracing::warn!(
                            %connected_at,
                            has_expires_in = response.get("expires_in").is_some(),
                            has_refresh_token = response.get("refresh_token").is_some(),
                            has_refresh_token_expires_in = response.get("refresh_token_expires_in").is_some(),
                            verification = verification_state(&user),
                            verification_error = ?user.as_ref().err(),
                            "GitHub OAuth device token received"
                        );
                        let user = user?;
                        let login = field(&user, "login")?;
                        if login == token
                            || refresh_token == Some(login)
                            || login == self.device_code
                            || !login
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        {
                            return Err(Error::Response);
                        }
                        return Ok(Credential {
                            client_id: client_id.into(),
                            scope,
                            token: token.into(),
                            login: login.into(),
                            connected_at: Some(connected_at),
                            avatar_url: user["avatar_url"]
                                .as_str()
                                .filter(|url| {
                                    !url.contains(token)
                                        && !url.contains(&self.device_code)
                                        && refresh_token.is_none_or(|refresh| !url.contains(refresh))
                                })
                                .and_then(avatar::safe_url),
                        });
                    }
                }
            }
        })
        .await
        .map_err(|_| Error::Expired)?
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 512)
        .ok_or(Error::Response)
}

#[derive(Deserialize, Serialize)]
struct Credential {
    client_id: String,
    scope: Scope,
    token: String,
    login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connected_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar_url: Option<String>,
}

struct TokenFile(PathBuf);

impl TokenFile {
    fn load(&self, config: &GitHubConfig) -> Result<Option<Credential>> {
        if config.oauth_client_id.trim().is_empty() || crate::canary() {
            return Ok(None);
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
            options.custom_flags(flags.bits() as i32);
        }
        let file = match options.open(&self.0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Storage),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let info = file.metadata().map_err(|_| Error::Storage)?;
            if !info.is_file() || info.permissions().mode() & 0o777 != 0o600 {
                return Err(Error::Storage);
            }
        }
        let stored: Credential =
            serde_json::from_reader(file.take(8192)).map_err(|_| Error::Storage)?;
        Ok((stored.client_id == config.oauth_client_id.trim()
            && stored.scope == config.oauth_scope)
            .then_some(stored))
    }

    fn save(&self, credential: &Credential, project: &Path) -> Result<()> {
        let store = || -> anyhow::Result<()> {
            let dir = self
                .0
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Missing state directory"))?;
            // Prevent accidentally placing a credential in the checkout with a custom state root.
            let project = project.canonicalize()?;
            fs::create_dir_all(dir)?;
            anyhow::ensure!(
                !dir.canonicalize()?.starts_with(project),
                "State directory is inside the workspace"
            );
            // NamedTempFile opens with 0600 on Unix before any content is written.
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            serde_json::to_writer(&mut file, credential)?;
            file.flush()?;
            crate::durable::persist(file, &self.0)?;
            Ok(())
        };
        store().map_err(|_| Error::Storage)
    }

    fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::Storage),
        }
    }
}

fn device_update(state: &State) -> Option<Update> {
    match state {
        State::Pending {
            code,
            url,
            expires_at,
        } => Some(Update::Device {
            user_code: code.clone(),
            verification_uri: url.clone(),
            expires_at: *expires_at,
        }),
        _ => None,
    }
}

#[derive(Default)]
struct Operation {
    attempt: Option<Attempt>,
    cards: BTreeMap<String, ServerEvent>,
    avatar_request: Option<String>,
    avatar: Option<GitHubAvatar>,
}

struct Attempt {
    id: String,
    client_id: String,
    scope: Scope,
    state: watch::Sender<State>,
    cancel: Option<oneshot::Sender<()>>,
    sessions: BTreeSet<String>,
}

impl Attempt {
    fn matches(&self, config: &GitHubConfig) -> bool {
        self.client_id == config.oauth_client_id.trim() && self.scope == config.oauth_scope
    }

    fn card(&self, session: &str) -> ServerEvent {
        ServerEvent::GitHubConnectCard {
            session: session.into(),
            attempt: self.id.clone(),
            state: self.state.borrow().clone(),
        }
    }
}

impl Operation {
    fn watch_in_chat(&mut self, app: &Workspace, session: &str) {
        let attempt = self.attempt.as_mut().expect("attempt is open");
        if attempt.sessions.insert(session.into()) {
            self.cards.insert(session.into(), attempt.card(session));
            app.hub.publish_snapshot(attempt.card(session));
        }
    }

    fn finish(&mut self, app: &Workspace, state: State) {
        let Some(attempt) = &mut self.attempt else {
            return;
        };
        attempt.cancel.take();
        attempt.state.send_replace(state.clone());
        for session in &attempt.sessions {
            let event = attempt.card(session);
            self.cards.insert(session.clone(), event.clone());
            app.hub.publish_snapshot(event);
        }
    }
}

pub(crate) struct Auth {
    http: Box<dyn Http>,
    token: TokenFile,
    operation: Mutex<Operation>,
    reads: read::Cache,
}

impl Auth {
    pub(crate) fn new(state: &Path) -> Self {
        Self {
            http: Box::new(GitHubHttp),
            token: TokenFile(state.join("github-oauth.json")),
            operation: Mutex::default(),
            reads: read::Cache::default(),
        }
    }

    fn status(&self, app: &Workspace, config: &GitHubConfig) -> Result<Update> {
        Ok(Update::Status {
            status: Status {
                enabled: configured(config),
                login: self.token.load(config)?.map(|stored| stored.login),
                report_repo: report_target(app, config),
                workspace_kind: workspace_kind(&app.project_dir),
            },
        })
    }

    fn account_state(&self, config: &GitHubConfig, operation: &Operation) -> Result<State> {
        if !configured(config) {
            return Ok(State::NotConfigured);
        }
        if let Some(credential) = self.token.load(config)? {
            return Ok(State::Connected {
                login: credential.login,
            });
        }
        Ok(operation
            .attempt
            .as_ref()
            .filter(|attempt| attempt.matches(config))
            .map_or(State::Disconnected, |attempt| {
                attempt.state.borrow().clone()
            }))
    }

    pub(crate) async fn snapshots(app: &Arc<Workspace>) -> Vec<ServerEvent> {
        let auth = &app.github_auth;
        let mut operation = auth.operation.lock().await;
        let state = config(app)
            .and_then(|config| auth.account_state(&config, &operation))
            .unwrap_or_else(|error| State::Failed { error });
        let avatar = if let State::Connected { login } = &state {
            if operation.avatar_request.is_none() {
                if let Ok(Some(credential)) =
                    config(app).and_then(|config| auth.token.load(&config))
                {
                    Self::load_avatar(app, &mut operation, login.clone(), credential.avatar_url);
                }
            }
            operation
                .avatar
                .as_ref()
                .filter(|avatar| &avatar.login == login)
                .cloned()
        } else {
            None
        };
        std::iter::once(ServerEvent::GitHubAccountChanged { state })
            .chain(avatar.map(|avatar| ServerEvent::GitHubAvatar { avatar }))
            .chain(operation.cards.values().cloned())
            .collect()
    }

    fn load_avatar(
        app: &Arc<Workspace>,
        operation: &mut Operation,
        login: String,
        url: Option<String>,
    ) {
        let Some(url) = url.as_deref().and_then(avatar::safe_url) else {
            return;
        };
        let id = crate::mail::session_key();
        operation.avatar_request = Some(id.clone());
        let app = app.clone();
        tokio::spawn(async move {
            let Some((etag, png)) = app.github_auth.http.avatar(url).await else {
                return;
            };
            let mut operation = app.github_auth.operation.lock().await;
            if operation.avatar_request.as_ref() == Some(&id) {
                let avatar = GitHubAvatar { login, etag, png };
                operation.avatar = Some(avatar.clone());
                app.hub
                    .publish_snapshot(ServerEvent::GitHubAvatar { avatar });
            }
        });
    }

    async fn start(
        app: &Arc<Workspace>,
        config: &GitHubConfig,
        session: Option<&str>,
    ) -> Result<watch::Receiver<State>> {
        if !configured(config) {
            return Err(Error::Disabled);
        }
        let auth = &app.github_auth;
        let mut operation = auth.operation.lock().await;
        if auth.token.load(config)?.is_some() {
            return Err(Error::AlreadyConnected);
        }
        if let Some(attempt) = &operation.attempt {
            if attempt.matches(config) && attempt.cancel.is_some() {
                let state = attempt.state.subscribe();
                if let Some(session) = session {
                    operation.watch_in_chat(app, session);
                }
                return Ok(state);
            }
            if attempt.cancel.is_some() {
                operation.finish(
                    app,
                    State::Failed {
                        error: Error::Cancelled,
                    },
                );
            }
            operation.attempt = None;
        }
        operation.avatar_request = None;
        operation.avatar = None;
        let client_id = config.oauth_client_id.trim().to_owned();
        let scope = config.oauth_scope;
        let device = match Device::start(&*auth.http, &client_id, scope).await {
            Ok(device) => device,
            Err(error) => {
                app.hub.publish_snapshot(ServerEvent::GitHubAccountChanged {
                    state: failed(error),
                });
                return Err(error);
            }
        };
        let state = device.state();
        let (updates, receiver) = watch::channel(state.clone());
        let (cancel, cancelled) = oneshot::channel();
        let id = crate::mail::session_key();
        operation.attempt = Some(Attempt {
            id: id.clone(),
            client_id: client_id.clone(),
            scope,
            state: updates,
            cancel: Some(cancel),
            sessions: BTreeSet::new(),
        });
        if let Some(session) = session {
            operation.watch_in_chat(app, session);
        }
        app.hub
            .publish_snapshot(ServerEvent::GitHubAccountChanged { state });
        let app = app.clone();
        tokio::spawn(async move {
            let auth = &app.github_auth;
            let result = tokio::select! {
                _ = cancelled => return,
                result = device.authorize(&*auth.http, &client_id, scope) => result,
            };
            let mut operation = auth.operation.lock().await;
            let Some(attempt) = operation
                .attempt
                .as_ref()
                .filter(|attempt| attempt.id == id)
            else {
                return;
            };
            let result = result.and_then(|credential| {
                if !attempt.matches(&self::config(&app)?) {
                    return Err(Error::Cancelled);
                }
                auth.token.save(&credential, &app.project_dir)?;
                tracing::warn!(
                    workspace = %app.id,
                    connected_at = ?credential.connected_at,
                    "GitHub OAuth connection saved"
                );
                Ok(credential)
            });
            let state = result.map_or_else(failed, |credential| {
                Self::load_avatar(
                    &app,
                    &mut operation,
                    credential.login.clone(),
                    credential.avatar_url,
                );
                State::Connected {
                    login: credential.login,
                }
            });
            operation.finish(&app, state.clone());
            app.hub
                .publish_snapshot(ServerEvent::GitHubAccountChanged { state });
        });
        Ok(receiver)
    }

    async fn connect(
        app: &Arc<Workspace>,
        config: &GitHubConfig,
        request: u64,
        reply: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<Update> {
        let mut updates = Self::start(app, config, None).await?;
        loop {
            let state = updates.borrow_and_update().clone();
            if let Some(device) = device_update(&state) {
                let _ = reply.send(event(request, Ok(device)));
            } else {
                return match state {
                    State::Connected { .. } => app.github_auth.status(app, &self::config(app)?),
                    State::Denied => Err(Error::Denied),
                    State::Expired => Err(Error::Expired),
                    State::Failed { error } => Err(error),
                    _ => Err(Error::Cancelled),
                };
            }
            tokio::select! {
                _ = reply.closed() => return Err(Error::Cancelled),
                result = updates.changed() => result.map_err(|_| Error::Cancelled)?,
            }
        }
    }

    async fn disconnect(&self, app: &Workspace, expected: Option<&State>) -> Result<Update> {
        if crate::canary() {
            return Err(Error::Disabled);
        }
        let mut operation = self.operation.lock().await;
        let config = config(app)?;
        if let Some(expected) = expected {
            if &self.account_state(&config, &operation)? != expected {
                return Err(Error::AccountChanged);
            }
        }
        self.token.remove()?;
        operation.avatar_request = None;
        operation.avatar = None;
        if operation
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.cancel.is_some())
        {
            operation.finish(
                app,
                State::Failed {
                    error: Error::Cancelled,
                },
            );
        }
        operation.attempt = None;
        let state = self.account_state(&config, &operation)?;
        app.hub
            .publish_snapshot(ServerEvent::GitHubAccountChanged { state });
        self.status(app, &config)
    }

    pub(crate) async fn create_if_connected(
        &self,
        config: &GitHubConfig,
        repo: &str,
        title: &str,
        body: &str,
        label: Option<&str>,
    ) -> Result<Option<String>> {
        let _operation = self.operation.lock().await;
        let Some(credential) = self.token.load(config)? else {
            return Ok(None);
        };
        create_issue(&*self.http, &credential, repo, title, body, label)
            .await
            .map(Some)
    }
}

fn report_target(app: &Workspace, config: &GitHubConfig) -> Option<String> {
    config.report_target(crate::workshop::origin_repo(&app.project_dir).as_deref())
}

fn workspace_kind(project: &Path) -> WorkspaceKind {
    if project.join("research").is_dir() {
        WorkspaceKind::Research
    } else if project.join("papers").is_dir() || project.join("deck.toml").is_file() {
        WorkspaceKind::Paper
    } else if project.join(".git").exists() || project.join("README.md").is_file() {
        WorkspaceKind::Software
    } else {
        WorkspaceKind::Empty
    }
}

async fn create_issue(
    http: &dyn Http,
    credential: &Credential,
    repo: &str,
    title: &str,
    body: &str,
    label: Option<&str>,
) -> Result<String> {
    if !crate::workshop::valid_repo(repo) {
        return Err(Error::Repository);
    }
    let mut data = json!({"title": title, "body": body});
    if let Some(label) = label {
        data["labels"] = json!([label]);
    }
    let response = api(
        http,
        &format!("repos/{repo}/issues"),
        &credential.token,
        Body::Json(data),
    )
    .await;
    if response == Err(RECONNECT) {
        record_401("POST /repos/{owner}/{repo}/issues");
        if !verify_rejected_token(http, credential).await {
            return Err(Error::Response);
        }
    }
    let number = response?["number"]
        .as_u64()
        .filter(|number| *number > 0)
        .ok_or(Error::Response)?;
    // Build the URL from the approved repository and issue number, never from an arbitrary response URL.
    Ok(format!("https://github.com/{repo}/issues/{number}"))
}

fn verification_state(result: &Result<Value>) -> &'static str {
    match result {
        Ok(_) => "accepted",
        Err(RECONNECT) => "rejected",
        Err(_) => "inconclusive",
    }
}

async fn verify_rejected_token(http: &dyn Http, credential: &Credential) -> bool {
    let result = api(http, "user", &credential.token, Body::Get).await;
    if result == Err(RECONNECT) {
        record_401("GET /user (token verification)");
    }
    tracing::warn!(
        at = %Utc::now(),
        connected_at = ?credential.connected_at,
        verification = verification_state(&result),
        verification_error = ?result.as_ref().err(),
        "GitHub OAuth token verification via GET /user"
    );
    result == Err(RECONNECT)
}

pub(crate) fn event(request: u64, result: Result<Update>) -> ServerEvent {
    ServerEvent::GitHubReportResult { request, result }
}

fn config(app: &Workspace) -> Result<GitHubConfig> {
    GitHubConfig::from_workspace(&app.project_dir).map_err(|_| Error::Config)
}

fn configured(config: &GitHubConfig) -> bool {
    !crate::canary() && !config.oauth_client_id.trim().is_empty()
}

fn failed(error: Error) -> State {
    match error {
        Error::Denied => State::Denied,
        Error::Expired => State::Expired,
        error => State::Failed { error },
    }
}

pub(crate) async fn handle(
    app: &Arc<Workspace>,
    request: u64,
    action: Action,
    reply: &mpsc::UnboundedSender<ServerEvent>,
) -> Result<Update> {
    let auth = &app.github_auth;
    let config = config(app)?;
    match action {
        Action::Status => auth.status(app, &config),
        Action::Connect => Auth::connect(app, &config, request, reply).await,
        Action::Disconnect => auth.disconnect(app, None).await,
        Action::Submit {
            repo,
            login,
            title,
            body,
        } => {
            if crate::canary() || config.oauth_client_id.trim().is_empty() {
                return Err(Error::Disabled);
            }
            let target = report_target(app, &config)
                .filter(|target| target.eq_ignore_ascii_case(&repo))
                .ok_or(Error::Repository)?;
            if title.trim().is_empty() || body.trim().is_empty() {
                return Err(Error::EmptyReport);
            }
            let _work = app.admit_work().map_err(|_| Error::Cancelled)?;
            let _operation = auth.operation.lock().await;
            let credential = auth.token.load(&config)?.ok_or(Error::NotConnected)?;
            if credential.login != login {
                return Err(Error::AccountChanged);
            }
            let url = create_issue(&*auth.http, &credential, &target, &title, &body, None).await?;
            app.issues_refresh.notify_one();
            Ok(Update::Submitted { url })
        }
    }
}

mod avatar;
pub(crate) mod mcp;
pub(crate) mod read;

#[cfg(test)]
mod tests;
