use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::prelude::{Engine, BASE64_URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, OnceCell, Semaphore};
use tokio::time::{timeout, Instant};

use crate::device_tokens::{Device, Store};

const TOKEN_FILE: &str = "remote-token";
const WHOIS_TIMEOUT: Duration = Duration::from_secs(5);
const WHOIS_TTL: Duration = Duration::from_secs(180);
const WHOIS_CACHE_SIZE: usize = 256;
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const BLOCK_DURATION: Duration = Duration::from_secs(15 * 60);
const MAX_FAILURES: usize = 10;
const MAX_TRACKED_IPS: usize = 4096;
const MAX_REQUEST_TARGET: usize = 16 * 1024;
const MAX_REQUEST_HEADERS: usize = 32 * 1024;
const MAX_REQUEST_BODY: usize = 64 * 1024;

#[derive(Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    remote: RemoteConfig,
    shutdown: ShutdownConfig,
    self_update: crate::self_update::Config,
    delegate: crate::delegate::admission::Overrides,
}

#[derive(Deserialize)]
#[serde(default)]
struct RemoteConfig {
    listen: Option<Vec<String>>,
    allowed_logins: Vec<String>,
    allow_shared_token: bool,
    trusted_proxy: HashSet<IpAddr>,
    local_listen: Option<SocketAddr>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            listen: None,
            allowed_logins: Vec::new(),
            allow_shared_token: true,
            trusted_proxy: HashSet::new(),
            local_listen: None,
        }
    }
}

pub(crate) const DEFAULT_DRAIN_SECONDS: u64 = 60;

#[derive(Deserialize)]
#[serde(default)]
struct ShutdownConfig {
    drain_seconds: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_seconds: DEFAULT_DRAIN_SECONDS,
        }
    }
}

pub struct Config {
    pub listen: Vec<String>,
    pub server_config: String,
    pub(crate) server_config_path: Option<PathBuf>,
    pub self_update: crate::self_update::Config,
    allowed_logins: HashSet<String>,
    allow_shared_token: bool,
    trusted_proxy: HashSet<IpAddr>,
    local_listen: Option<SocketAddr>,
    pub shutdown_drain_seconds: u64,
    pub(crate) delegate: crate::delegate::admission::Overrides,
}

impl Config {
    pub fn load() -> Result<Self> {
        let override_path = fuzukue_proto::compat::env_os("FUZUKUE_SERVER_CONFIG")
            .map(|path| crate::state::config_file(path.into()));
        let path = override_path
            .clone()
            .or_else(|| crate::state::config_dir().map(|dir| dir.join("server.toml")));
        let (config, server_config) = match &path {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let config = toml::from_str::<ConfigFile>(&content).map_err(|_| {
                        anyhow::anyhow!("invalid server configuration: {}", path.display())
                    })?;
                    (config, content)
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && override_path.is_none() => {
                    (ConfigFile::default(), String::new())
                }
                Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
            },
            None => (ConfigFile::default(), String::new()),
        };
        let listen_env = fuzukue_proto::compat::env("FUZUKUE_ADDR").ok();
        let listen = listen_addrs(listen_env.as_deref(), config.remote.listen.as_deref())?;
        let logins_env = fuzukue_proto::compat::env("FUZUKUE_ALLOWED_LOGINS").unwrap_or_default();
        let allowed_logins = config
            .remote
            .allowed_logins
            .iter()
            .map(String::as_str)
            .chain(logins_env.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        ensure!(
            config.shutdown.drain_seconds <= u32::MAX.into(),
            "shutdown.drain_seconds is too large"
        );
        Ok(Self {
            listen,
            shutdown_drain_seconds: config.shutdown.drain_seconds,
            self_update: config.self_update,
            server_config,
            server_config_path: path.map(std::path::absolute).transpose()?,
            allowed_logins,
            allow_shared_token: config.remote.allow_shared_token,
            trusted_proxy: config
                .remote
                .trusted_proxy
                .into_iter()
                .map(|ip| ip.to_canonical())
                .collect(),
            local_listen: config.remote.local_listen,
            delegate: config.delegate,
        })
    }

    pub fn access(&self, dir: &Path) -> Result<Access> {
        Access::with_whois(
            dir,
            self.allowed_logins.clone(),
            self.allow_shared_token,
            self.trusted_proxy.clone(),
            Box::new(|ip| Box::pin(tailscale_whois(ip))),
        )
    }

    pub async fn listeners(&self, canary: bool) -> Result<Vec<Listener>> {
        if canary {
            let [name] = self.listen.as_slice() else {
                bail!("試運転には待ち受けアドレスが一つ必要だよ");
            };
            let addr: SocketAddr = name.parse()?;
            ensure!(
                addr.ip() == std::net::Ipv4Addr::LOCALHOST,
                "試運転は 127.0.0.1 で待ち受けるよ"
            );
        }
        let mut listeners = Vec::new();
        for name in &self.listen {
            let addresses = tokio::net::lookup_host(name)
                .await
                .with_context(|| format!("resolve listen address {name}"))?
                .map(|addr| {
                    let mode = if canary {
                        ListenMode::Direct
                    } else {
                        listen_mode(addr, &self.trusted_proxy)?
                    };
                    Ok((addr, mode))
                })
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                !addresses.is_empty(),
                "listen address {name} did not resolve"
            );
            listeners.push(Listener {
                name: name.clone(),
                addresses,
            });
        }
        if let Some(addr) = self.local_listen.filter(|_| !canary) {
            ensure!(
                is_loopback(addr.ip()),
                "[remote].local_listen must be loopback"
            );
            ensure!(
                !self.trusted_proxy.is_empty(),
                "[remote].local_listen requires [remote].trusted_proxy"
            );
            listeners.push(Listener {
                name: addr.to_string(),
                addresses: vec![(addr, ListenMode::Direct)],
            });
        }
        Ok(listeners)
    }
}

pub struct Listener {
    name: String,
    addresses: Vec<(SocketAddr, ListenMode)>,
}

impl Listener {
    pub async fn bind(&self) -> Result<(tokio::net::TcpListener, ListenMode)> {
        let mut last_error = None;
        for &(addr, mode) in &self.addresses {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => return Ok((listener, mode)),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.context("no listen addresses resolved")?)
            .with_context(|| format!("bind {}", self.name))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenMode {
    Direct,
    Proxy,
}

fn listen_mode(addr: SocketAddr, trusted_proxy: &HashSet<IpAddr>) -> Result<ListenMode> {
    if is_tailscale_ip(addr.ip()) {
        return Ok(ListenMode::Direct);
    }
    if !trusted_proxy.is_empty() {
        return Ok(ListenMode::Proxy);
    }
    ensure!(
        is_loopback(addr.ip()),
        "unsafe listen address {addr}: off-tailnet listeners require tokens and HTTPS via [remote].trusted_proxy"
    );
    Ok(ListenMode::Direct)
}

fn listen_addrs(env: Option<&str>, config: Option<&[String]>) -> Result<Vec<String>> {
    let addrs: Vec<_> = match (env, config) {
        (Some(env), _) => env.split(',').collect(),
        (None, Some(config)) => config.iter().map(String::as_str).collect(),
        (None, None) => vec!["127.0.0.1:7878"],
    };
    let mut seen = HashSet::new();
    let mut listen = Vec::new();
    for addr in addrs.into_iter().map(str::trim) {
        ensure!(
            !addr.is_empty(),
            "empty listen address in FUZUKUE_ADDR or [remote].listen"
        );
        if seen.insert(addr) {
            listen.push(addr.to_owned());
        }
    }
    ensure!(
        !listen.is_empty(),
        "[remote].listen needs at least one address"
    );
    Ok(listen)
}

type Whois = dyn Fn(IpAddr) -> BoxFuture<'static, Result<Option<String>>> + Send + Sync;

struct CachedLogin {
    expires: Instant,
    login: OnceCell<Option<String>>,
}

pub struct Access {
    allowed_logins: HashSet<String>,
    token_path: Option<PathBuf>,
    devices: Store,
    trusted_proxy: HashSet<IpAddr>,
    failures: Mutex<Failures>,
    token_slots: Arc<Semaphore>,
    whois: Box<Whois>,
    cache: Mutex<HashMap<IpAddr, Arc<CachedLogin>>>,
    lookup_slots: Semaphore,
}

impl Access {
    pub fn new(dir: &Path, allowed_logins: HashSet<String>) -> Result<Self> {
        Self::with_whois(
            dir,
            allowed_logins,
            true,
            HashSet::new(),
            Box::new(|ip| Box::pin(tailscale_whois(ip))),
        )
    }

    fn with_whois(
        dir: &Path,
        allowed_logins: HashSet<String>,
        allow_shared_token: bool,
        trusted_proxy: HashSet<IpAddr>,
        whois: Box<Whois>,
    ) -> Result<Self> {
        let token_path = if allow_shared_token {
            ensure_token(dir, false)?;
            Some(dir.join(TOKEN_FILE))
        } else {
            None
        };
        Ok(Self {
            allowed_logins,
            token_path,
            devices: Store::new(dir),
            trusted_proxy,
            failures: Mutex::default(),
            token_slots: Arc::new(Semaphore::new(32)),
            whois,
            cache: Mutex::default(),
            lookup_slots: Semaphore::new(8),
        })
    }

    async fn token_identity(&self, tokens: Vec<String>) -> Result<Option<Identity>> {
        if tokens.is_empty() {
            return Ok(None);
        }
        let slot = self.token_slots.clone().try_acquire_owned()?;
        let devices = self.devices.clone();
        let shared = self.token_path.clone();
        let check = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            for token in tokens {
                match devices.authenticate(&token, chrono::Utc::now().timestamp()) {
                    Ok(Some(device)) => return Some(Identity::Device(device)),
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "cannot check device token"),
                }
                if shared
                    .as_deref()
                    .is_some_and(|path| shared_token_matches(path, &token))
                {
                    return Some(Identity::SharedToken);
                }
            }
            None
        });
        Ok(timeout(Duration::from_secs(2), check).await??)
    }

    async fn allowed_tailnet_peer(&self, ip: IpAddr) -> Result<bool> {
        if self.allowed_logins.is_empty() || !is_tailscale_ip(ip) {
            return Ok(false);
        }
        let entry = {
            let mut cache = self.cache.lock().await;
            let now = Instant::now();
            // Keep in-flight lookups shared, and reclaim empty entries after failed lookups.
            cache.retain(|_, entry| {
                entry.expires > now && (Arc::strong_count(entry) > 1 || entry.login.initialized())
            });
            ensure!(
                cache.len() < WHOIS_CACHE_SIZE || cache.contains_key(&ip),
                "Tailscale WHOIS cache is full"
            );
            cache
                .entry(ip)
                .or_insert_with(|| {
                    Arc::new(CachedLogin {
                        expires: now + WHOIS_TTL,
                        login: OnceCell::new(),
                    })
                })
                .clone()
        };
        let login = timeout(
            WHOIS_TIMEOUT,
            entry.login.get_or_try_init(|| async {
                let _slot = self.lookup_slots.acquire().await?;
                (self.whois)(ip).await
            }),
        )
        .await??;
        Ok(login
            .as_ref()
            .is_some_and(|login| self.allowed_logins.contains(login)))
    }
}

pub(crate) fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => matches!(ip.octets(), [100, 64..=127, _, _]),
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

async fn tailscale_whois(ip: IpAddr) -> Result<Option<String>> {
    let output = tokio::process::Command::new("tailscale")
        .args(["whois", "--json", &ip.to_string()])
        .kill_on_drop(true)
        .stderr(std::process::Stdio::null())
        .output()
        .await?;
    ensure!(output.status.success(), "Tailscale WHOIS failed");
    whois_login(&output.stdout)
}

fn whois_login(json: &[u8]) -> Result<Option<String>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct WhoisResponse {
        user_profile: Option<Profile>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Profile {
        login_name: String,
    }
    let response: WhoisResponse = serde_json::from_slice(json)?;
    Ok(response.user_profile.map(|profile| profile.login_name))
}

pub(crate) fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

fn ws_query_token(req: &Request) -> Option<std::borrow::Cow<'_, str>> {
    if req.uri().path() != "/ws"
        || *req.method() != Method::GET
        || req
            .headers()
            .get(header::SEC_WEBSOCKET_VERSION)
            .is_none_or(|v| v != "13")
        || !req.headers().contains_key(header::SEC_WEBSOCKET_KEY)
        || !req
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        || !req
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
    {
        return None;
    }
    let mut tokens = url::form_urlencoded::parse(req.uri().query()?.as_bytes())
        .filter_map(|(key, token)| (key == "token").then_some(token));
    let token = tokens.next()?;
    tokens.next().is_none().then_some(token)
}

#[derive(Default)]
struct Failures(HashMap<IpAddr, FailedAuth>);

#[derive(Default)]
struct FailedAuth {
    recent: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl FailedAuth {
    fn prune(&mut self, now: Instant) {
        if self.blocked_until.is_some_and(|end| end <= now) {
            self.blocked_until = None;
        }
        while self
            .recent
            .front()
            .is_some_and(|time| *time + FAILURE_WINDOW <= now)
        {
            self.recent.pop_front();
        }
    }
}

impl Failures {
    fn limited(&mut self, ip: IpAddr, now: Instant) -> bool {
        if let Some(entry) = self.0.get_mut(&ip) {
            entry.prune(now);
            return entry.blocked_until.is_some();
        }
        if self.0.len() >= MAX_TRACKED_IPS {
            self.0.retain(|_, entry| {
                entry.prune(now);
                entry.blocked_until.is_some() || !entry.recent.is_empty()
            });
        }
        self.0.len() >= MAX_TRACKED_IPS
    }

    fn failed(&mut self, ip: IpAddr, now: Instant) {
        let entry = self.0.entry(ip).or_default();
        entry.recent.push_back(now);
        if entry.recent.len() >= MAX_FAILURES {
            entry.recent.clear();
            entry.blocked_until = Some(now + BLOCK_DURATION);
        }
    }
}

enum Identity {
    Local,
    Device(Device),
    SharedToken,
    Tailnet,
}

pub(crate) const AUDIT_DIRECTIVE: &str = "secretaire::remote::audit=info";

fn audit(
    peer: Option<SocketAddr>,
    client: Option<IpAddr>,
    identity: Option<&Identity>,
    result: &str,
) {
    let (device, method) = match identity {
        Some(Identity::Device(device)) => (Some(device.name.as_str()), "device"),
        Some(Identity::SharedToken) => (None, "shared_token"),
        Some(Identity::Tailnet) => (None, "tailnet"),
        Some(Identity::Local) => (None, "local"),
        None => (None, "none"),
    };
    tracing::info!(target: "secretaire::remote::audit", ?device, ?peer, ?client, method, result, "remote access");
}

fn deny(
    peer: Option<SocketAddr>,
    client: Option<IpAddr>,
    status: StatusCode,
    reason: &str,
) -> Response {
    audit(peer, client, None, reason);
    status.into_response()
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn forwarded_client(headers: &HeaderMap) -> Option<IpAddr> {
    // The trusted proxy overwrites the list or appends its verified peer.
    // Earlier values may have come from the client.
    let ip: IpAddr = single_header(headers, "x-forwarded-for")?
        .rsplit(',')
        .next()?
        .trim()
        .parse()
        .ok()?;
    Some(ip.to_canonical())
}

#[derive(Clone, Copy)]
pub(crate) struct ClientIp(pub IpAddr);

#[derive(Clone)]
pub(crate) struct DeviceSession {
    store: Store,
    id: String,
}

impl DeviceSession {
    pub(crate) fn id(&self) -> &str { &self.id }

    async fn revoked(&self) {
        let mut ticks = tokio::time::interval(DEVICE_CHECK_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            let store = self.store.clone();
            let id = self.id.clone();
            let check = tokio::task::spawn_blocking(move || {
                store.is_active(&id, chrono::Utc::now().timestamp())
            });
            match timeout(DEVICE_CHECK_INTERVAL, check).await {
                Ok(Ok(Ok(true))) => {}
                _ => return,
            }
        }
    }

    pub(crate) async fn run(self, serve: impl Future<Output = ()>) {
        // Dropping the serving future also drops the socket, including when it
        // is idle or a write is blocked.
        tokio::select! {
            biased;
            _ = self.revoked() => {}
            _ = serve => {}
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReviewEditor(pub(crate) String);

/// Explicit client credential. The local transport and worker MCP keys do not create this.
#[derive(Clone)]
pub(crate) struct SettingsPrincipal(String);

impl SettingsPrincipal {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn paired_device(&self) -> bool {
        self.0.starts_with("device:")
    }

    #[cfg(test)]
    pub(crate) fn paired_device_fixture() -> Self {
        Self("device:fixture".into())
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self("fixture".into())
    }
}

impl Identity {
    fn settings_principal(&self) -> Option<SettingsPrincipal> {
        match self {
            Self::Device(device) => Some(SettingsPrincipal(format!("device:{}", device.id))),
            Self::SharedToken => Some(SettingsPrincipal("shared_token_user".into())),
            _ => None,
        }
    }

    fn review_editor(&self) -> Option<ReviewEditor> {
        match self {
            Self::Device(device) => Some(ReviewEditor(format!("editor:device:{}", device.id))),
            Self::SharedToken => Some(ReviewEditor("editor:shared-token".into())),
            _ => None,
        }
    }
}

pub async fn enforce(State(access): State<Arc<Access>>, mut req: Request, next: Next) -> Response {
    let Some(&ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        return deny(None, None, StatusCode::FORBIDDEN, "missing_peer");
    };
    let mode = req.extensions().get::<ListenMode>().copied();
    // In-process protocol tests can exercise token auth without a transport.
    // Real listeners always enforce the transport policy.
    let check_transport = mode.is_some() || !cfg!(test);
    let proxy = mode == Some(ListenMode::Proxy);
    let peer_ip = peer.ip().to_canonical();
    let local = !proxy && peer_ip.is_loopback();
    if req.uri().path() == "/mcp" && !local {
        return deny(
            Some(peer),
            None,
            StatusCode::FORBIDDEN,
            "mcp_requires_loopback",
        );
    }
    if req.uri().path() == crate::self_update::wait_route::PATH && !local {
        return deny(Some(peer), None, StatusCode::NOT_FOUND, "update_wait_requires_loopback");
    }
    if req.uri().path() == crate::self_update::HEALTH_ROUTE && proxy {
        return deny(
            Some(peer),
            None,
            StatusCode::NOT_FOUND,
            "health_requires_loopback",
        );
    }
    let editor_route = req.uri().path() == crate::reviewers::section::blind::editor::ROUTE;
    let settings_route = matches!(req.uri().path(), "/ws" | crate::full_approval::SETTINGS_PATH | crate::full_approval::EXECUTION_PATH);
    if local {
        if editor_route || settings_route {
            let tokens: Vec<_> = bearer(req.headers()).map(str::to_owned).into_iter().collect();
            if !tokens.is_empty() {
                match access.token_identity(tokens).await {
                    Ok(Some(identity)) => {
                        if editor_route {
                            if let Some(editor) = identity.review_editor() {
                                req.extensions_mut().insert(editor);
                            }
                        }
                        if settings_route {
                            if let Some(principal) = identity.settings_principal() {
                                req.extensions_mut().insert(principal);
                            }
                            if let Identity::Device(device) = identity {
                                req.extensions_mut().insert(DeviceSession {
                                    store: access.devices.clone(),
                                    id: device.id,
                                });
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return deny(Some(peer), Some(peer_ip), StatusCode::SERVICE_UNAVAILABLE, "authentication_busy"),
                }
            }
        }
        audit(Some(peer), Some(peer_ip), Some(&Identity::Local), "allowed");
        req.extensions_mut().insert(ClientIp(peer_ip));
        return next.run(req).await;
    }

    let header_size: usize = req
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
        .sum();
    if header_size > MAX_REQUEST_HEADERS {
        return deny(
            Some(peer),
            None,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "headers_too_large",
        );
    }
    if req
        .uri()
        .path_and_query()
        .is_some_and(|target| target.as_str().len() > MAX_REQUEST_TARGET)
    {
        return deny(
            Some(peer),
            None,
            StatusCode::URI_TOO_LONG,
            "target_too_large",
        );
    }
    if single_header(req.headers(), "content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|len| len > MAX_REQUEST_BODY as u64)
    {
        return deny(
            Some(peer),
            None,
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
        );
    }
    DefaultBodyLimit::max(MAX_REQUEST_BODY).apply(&mut req);

    let client = if proxy {
        if !access.trusted_proxy.contains(&peer_ip) {
            return deny(Some(peer), None, StatusCode::FORBIDDEN, "untrusted_proxy");
        }
        let Some(client) = forwarded_client(req.headers()) else {
            return deny(
                Some(peer),
                None,
                StatusCode::FORBIDDEN,
                "missing_forwarded_client",
            );
        };
        if !single_header(req.headers(), "x-forwarded-proto")
            .is_some_and(|s| s.eq_ignore_ascii_case("https"))
        {
            return deny(
                Some(peer),
                Some(client),
                StatusCode::FORBIDDEN,
                "https_required",
            );
        }
        client
    } else {
        if check_transport && !is_tailscale_ip(peer_ip) {
            return deny(
                Some(peer),
                Some(peer_ip),
                StatusCode::FORBIDDEN,
                "https_required",
            );
        }
        peer_ip
    };

    if access.failures.lock().await.limited(client, Instant::now()) {
        return deny(
            Some(peer),
            Some(client),
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
        );
    }
    let tokens = bearer(req.headers())
        .into_iter()
        .map(std::borrow::Cow::Borrowed)
        .chain(ws_query_token(&req))
        .filter(|token| !token.is_empty())
        .map(std::borrow::Cow::into_owned)
        .collect();
    let identity = match access.token_identity(tokens).await {
        Ok(Some(identity)) => Some(identity),
        token => {
            let tailnet = if proxy {
                Ok(false)
            } else {
                access.allowed_tailnet_peer(client).await
            };
            match (token, tailnet) {
                (_, Ok(true)) => Some(Identity::Tailnet),
                (_, Err(_)) => {
                    return deny(
                        Some(peer),
                        Some(client),
                        StatusCode::SERVICE_UNAVAILABLE,
                        "whois_unavailable",
                    );
                }
                (Ok(_), Ok(false)) => None,
                (Err(_), Ok(false)) => {
                    return deny(
                        Some(peer),
                        Some(client),
                        StatusCode::SERVICE_UNAVAILABLE,
                        "authentication_busy",
                    );
                }
            }
        }
    };
    {
        let mut failures = access.failures.lock().await;
        let now = Instant::now();
        if failures.limited(client, now) {
            return deny(
                Some(peer),
                Some(client),
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
            );
        }
        if identity.is_none() {
            failures.failed(client, now);
        }
    }
    let Some(identity) = identity else {
        return deny(
            Some(peer),
            Some(client),
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
        );
    };
    if let Identity::Device(device) = &identity {
        req.extensions_mut().insert(DeviceSession {
            store: access.devices.clone(),
            id: device.id.clone(),
        });
    }
    if editor_route {
        if let Some(editor) = identity.review_editor() {
            req.extensions_mut().insert(editor);
        }
    }
    if settings_route {
        if let Some(principal) = identity.settings_principal() {
            req.extensions_mut().insert(principal);
        }
    }
    req.extensions_mut().insert(ClientIp(client));
    audit(Some(peer), Some(client), Some(&identity), "allowed");
    let mut response = next.run(req).await;
    if proxy {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response
}

fn shared_token_matches(path: &Path, supplied: &str) -> bool {
    let token = match read_token(path) {
        Ok(token) => token,
        Err(e) => {
            tracing::warn!(error_kind = ?e.kind(), "cannot read remote token");
            return false;
        }
    };
    let supplied = Sha256::digest(supplied.as_bytes());
    let token = Sha256::digest(token.as_bytes());
    bool::from(supplied.ct_eq(&token))
}

fn read_token(path: &Path) -> io::Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote token must be a regular 0600 file",
        ));
    }
    let mut token = String::new();
    file.take(64).read_to_string(&mut token)?;
    let token = token.trim_end_matches('\n');
    if BASE64_URL_SAFE_NO_PAD
        .decode(token)
        .map_or(true, |bytes| bytes.len() != 32)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid remote token file",
        ));
    }
    Ok(token.into())
}

pub fn ensure_token(dir: &Path, rotate: bool) -> Result<String> {
    let path = dir.join(TOKEN_FILE);
    if !rotate {
        match read_token(&path) {
            Ok(token) => return Ok(token),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    std::fs::create_dir_all(dir)?;
    let token = BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    writeln!(file, "{token}")?;
    file.as_file().sync_all()?;
    if rotate {
        file.persist(&path)?;
    } else if let Err(e) = file.persist_noclobber(&path) {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            return Ok(read_token(&path)?);
        }
        return Err(e.into());
    }
    Ok(token)
}

pub fn token_cli(args: impl Iterator<Item = String>) -> Result<()> {
    let mut rotate = false;
    let mut project = None;
    for arg in args {
        match arg.as_str() {
            "--rotate" => rotate = true,
            _ if arg.starts_with('-') || project.is_some() => {
                bail!("usage: secretaire token [--rotate] [project]")
            }
            _ => project = Some(PathBuf::from(arg)),
        }
    }
    let project = project.unwrap_or_else(|| ".".into()).canonicalize()?;
    println!(
        "{}",
        ensure_token(&crate::state::dir_for(&project), rotate)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    // These tests use synthetic addresses in the Tailscale ranges, including their boundaries.
    use super::*;
    use axum::body::Body;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[test]
    fn listen_accepts_both_config_and_comma_list() {
        let config = toml::from_str::<ConfigFile>(
            r#"[remote]
listen = ["127.0.0.1:7878", "100.64.0.21:7878"]
allowed_logins = ["owner@example.com"]"#,
        )
        .unwrap();
        let listen = config.remote.listen.as_deref();
        assert_eq!(config.remote.allowed_logins, ["owner@example.com"]);
        assert_eq!(listen_addrs(None, None).unwrap(), ["127.0.0.1:7878"]);
        assert_eq!(
            listen_addrs(None, listen).unwrap(),
            ["127.0.0.1:7878", "100.64.0.21:7878"]
        );
        assert_eq!(
            listen_addrs(Some(" 127.0.0.1:9000, [::1]:9000, 127.0.0.1:9000 "), listen).unwrap(),
            ["127.0.0.1:9000", "[::1]:9000"]
        );
        assert!(listen_addrs(Some("127.0.0.1:7878,"), None).is_err());
        assert!(listen_addrs(None, Some(&[])).is_err());
    }

    #[tokio::test]
    async fn listener_plan_rejects_unsafe_addresses_before_binding() {
        let mut config = Config {
            listen: vec!["127.0.0.1:7878".into(), "0.0.0.0:7878".into()],
            server_config: String::new(),
            server_config_path: None,
            self_update: Default::default(),
            allowed_logins: HashSet::new(),
            allow_shared_token: true,
            trusted_proxy: HashSet::new(),
            local_listen: None,
            shutdown_drain_seconds: DEFAULT_DRAIN_SECONDS,
            delegate: Default::default(),
        };
        for addr in [
            "0.0.0.0:7878",
            "[::]:7878",
            "192.168.1.2:7878",
            "[::ffff:192.168.1.2]:7878",
        ] {
            config.listen[1] = addr.into();
            let error = config.listeners(false).await.err().unwrap();
            assert!(error.to_string().contains("require tokens and HTTPS"));
        }
        config.listen[1] = "100.64.0.21:7878".into();
        let plan = config.listeners(false).await.unwrap();
        assert!(plan
            .iter()
            .all(|listener| listener.addresses[0].1 == ListenMode::Direct));

        config.trusted_proxy.insert("127.0.0.1".parse().unwrap());
        config.listen.push("192.168.1.2:7878".into());
        config.local_listen = Some("127.0.0.1:7879".parse().unwrap());
        let plan = config.listeners(false).await.unwrap();
        let modes: Vec<_> = plan
            .iter()
            .map(|listener| listener.addresses[0].1)
            .collect();
        assert_eq!(
            modes,
            [
                ListenMode::Proxy,
                ListenMode::Direct,
                ListenMode::Proxy,
                ListenMode::Direct
            ]
        );

        config.listen = vec!["127.0.0.1:7880".into()];
        let trial = config.listeners(true).await.unwrap();
        assert_eq!(trial.len(), 1);
        assert_eq!(trial[0].addresses[0].1, ListenMode::Direct);
    }

    #[test]
    fn whois_uses_the_user_profile() {
        assert_eq!(
            whois_login(br#"{"Node": {}, "UserProfile": {"LoginName": "owner@example.com"}}"#)
                .unwrap()
                .as_deref(),
            Some("owner@example.com")
        );
        assert_eq!(
            whois_login(br#"{"Node": {"LoginName": "owner@example.com"}}"#).unwrap(),
            None
        );
        assert_eq!(whois_login(br#"{"UserProfile": null}"#).unwrap(), None);
        assert!(whois_login(b"incomplete response").is_err());
        assert!(whois_login(br#"{"UserProfile": {"LoginName": 123}}"#).is_err());
        for ip in ["100.64.0.0", "100.127.255.255", "fd7a:115c:a1e0:ffff::"] {
            assert!(is_tailscale_ip(ip.parse().unwrap()));
        }
        for ip in ["100.63.255.255", "100.128.0.0", "fd7a:115c:a1e1::"] {
            assert!(!is_tailscale_ip(ip.parse().unwrap()));
        }
    }

    #[test]
    fn token_file_is_private_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        let original = ensure_token(dir.path(), false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(read_token(&path).unwrap() == original);
        assert!(ensure_token(dir.path(), false).unwrap() == original);
        let rotated = ensure_token(dir.path(), true).unwrap();
        assert!(rotated != original);
        assert!(read_token(&path).unwrap() == rotated);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tailnet_auth_retries_transient_whois_failures_without_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lookups = calls.clone();
        let access = Access::with_whois(
            dir.path(),
            HashSet::from(["owner@example.com".into()]),
            false,
            HashSet::new(),
            Box::new(move |_| {
                let attempt = lookups.fetch_add(1, Ordering::Relaxed);
                Box::pin(async move {
                    match attempt {
                        0 => std::future::pending().await,
                        1..=MAX_FAILURES => Err(io::Error::from(io::ErrorKind::NotFound).into()),
                        _ => {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            Ok(Some("owner@example.com".into()))
                        }
                    }
                })
            }),
        )
        .unwrap();
        let router = axum::Router::new()
            .route("/", axum::routing::get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(access),
                enforce,
            ))
            .layer(axum::Extension(ListenMode::Direct));
        let request = || {
            let router = router.clone();
            async move {
                let req = Request::builder()
                    .uri("/")
                    .extension(ConnectInfo(
                        "100.64.0.21:1234".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap();
                router.oneshot(req).await.unwrap().status()
            }
        };

        // A timeout followed by repeated command failures must leave the IP eligible to retry.
        for _ in 0..=MAX_FAILURES {
            assert_eq!(request().await, StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(request().await, StatusCode::OK);
        assert_eq!(request().await, StatusCode::OK);
        assert_eq!(calls.load(Ordering::Relaxed), MAX_FAILURES + 2);
    }

    #[tokio::test]
    async fn authenticates_files_and_ws_and_keeps_mcp_local() {
        let dir = tempfile::tempdir().unwrap();
        let token = ensure_token(dir.path(), false).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let lookups = calls.clone();
        let access = Access::with_whois(
            dir.path(),
            HashSet::from(["owner@example.com".into()]),
            true,
            HashSet::new(),
            Box::new(move |ip| {
                lookups.fetch_add(1, Ordering::Relaxed);
                Box::pin(async move {
                    let login = if ip == "100.64.0.21".parse::<IpAddr>().unwrap() || ip.is_ipv6()
                    {
                        Some("owner@example.com")
                    } else if ip == "100.64.0.22".parse::<IpAddr>().unwrap() {
                        Some("Owner@example.com")
                    } else {
                        None
                    };
                    Ok(login.map(str::to_owned))
                })
            }),
        )
        .unwrap();
        std::fs::write(dir.path().join("note.md"), "hello").unwrap();
        let app = Arc::new(crate::Workspace::new(
            dir.path().into(),
            "127.0.0.1:7878".into(),
            dir.path().into(),
            Arc::new(crate::Hub::new()),
        ));
        let access = Arc::new(access);
        let router = crate::router(crate::App::fixture(app), access.clone())
            .layer(axum::Extension(ListenMode::Direct));
        let request =
            |peer: &str, method: Method, path: &str, token: Option<&str>, upgrade: bool| {
                let mut req = Request::builder()
                    .method(method)
                    .uri(path)
                    .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
                if let Some(token) = token {
                    req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
                if upgrade {
                    req = req
                        .header(header::CONNECTION, "keep-alive, Upgrade")
                        .header(header::UPGRADE, "websocket")
                        .header(header::SEC_WEBSOCKET_VERSION, "13")
                        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==");
                }
                let req = req
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap();
                let router = router.clone();
                async move { router.oneshot(req).await.unwrap().status() }
            };
        let path = "/file/note.md";
        let local = "[::ffff:127.0.0.1]:1234";
        let owner = "100.64.0.21:1234";
        let other = "100.64.0.22:1234";
        let unlisted = "100.64.0.23:1234";
        for peer in [local, owner, owner, "[fd7a:115c:a1e0:ffff::2]:1234"] {
            assert_eq!(
                request(peer, Method::GET, path, None, false).await,
                StatusCode::OK
            );
        }
        for peer in [other, other, unlisted, unlisted] {
            assert_eq!(
                request(peer, Method::GET, path, None, false).await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert!(!access
            .allowed_tailnet_peer("192.168.1.10".parse().unwrap())
            .await
            .unwrap());
        assert_eq!(
            request("192.168.1.10:1234", Method::GET, path, Some(&token), false).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        for supplied in ["", "wrong"] {
            assert_eq!(
                request(owner, Method::GET, path, Some(supplied), false).await,
                StatusCode::OK
            );
            assert_eq!(
                request(
                    owner,
                    Method::GET,
                    &format!("/ws?token={supplied}"),
                    None,
                    true
                )
                .await,
                StatusCode::UPGRADE_REQUIRED
            );
        }
        assert_eq!(
            request(unlisted, Method::GET, path, Some(&token), false).await,
            StatusCode::OK
        );
        assert_eq!(
            request(
                unlisted,
                Method::GET,
                "/source/note.md",
                Some(&token),
                false
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            request(unlisted, Method::GET, path, Some("wrong"), false).await,
            StatusCode::UNAUTHORIZED
        );
        let query = format!("/ws?token={token}");
        assert_eq!(
            request(unlisted, Method::GET, &query, None, false).await,
            StatusCode::UNAUTHORIZED
        );
        // A synthetic request has no hyper transport to upgrade, so Axum reaches the
        // upgrade extractor and rejects with 426 after authentication succeeds.
        assert_eq!(
            request(unlisted, Method::GET, &query, None, true).await,
            StatusCode::UPGRADE_REQUIRED
        );
        assert_eq!(
            request(
                unlisted,
                Method::GET,
                &format!("{path}?token={token}"),
                None,
                true
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        let mcp = "/mcp?session=main";
        assert_eq!(
            request(local, Method::POST, mcp, None, false).await,
            StatusCode::ACCEPTED
        );
        for peer in [owner, unlisted] {
            assert_eq!(
                request(peer, Method::POST, mcp, Some(&token), false).await,
                StatusCode::FORBIDDEN
            );
        }
        let rotated = ensure_token(dir.path(), true).unwrap();
        assert_eq!(
            request(unlisted, Method::GET, path, Some(&token), false).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(unlisted, Method::GET, path, Some(&rotated), false).await,
            StatusCode::OK
        );
        access.token_slots.close();
        assert_eq!(
            request(owner, Method::GET, path, Some("wrong"), false).await,
            StatusCode::OK
        );
        assert_eq!(
            request(unlisted, Method::GET, path, Some("wrong"), false).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    fn proxy_fixture(dir: &Path) -> axum::Router {
        let settings: ConfigFile = toml::from_str(
            r#"[remote]
allow_shared_token = false
trusted_proxy = ["127.0.0.1"]"#,
        )
        .unwrap();
        let access = Access::with_whois(
            dir,
            HashSet::from(["owner@example.com".into()]),
            settings.remote.allow_shared_token,
            settings.remote.trusted_proxy,
            Box::new(|_| panic!("a forwarded IP must not be used for Tailscale authentication")),
        )
        .unwrap();
        std::fs::write(dir.join("note.md"), "hello").unwrap();
        let workspace = Arc::new(crate::Workspace::new(
            dir.into(),
            "127.0.0.1:7879".into(),
            dir.into(),
            Arc::new(crate::Hub::new()),
        ));
        crate::router(crate::App::fixture(workspace), Arc::new(access))
            .layer(axum::Extension(ListenMode::Proxy))
    }

    fn proxy_request(path: &str) -> axum::http::request::Builder {
        Request::builder().uri(path).extension(ConnectInfo(
            "[::ffff:127.0.0.1]:1234".parse::<SocketAddr>().unwrap(),
        ))
    }

    fn ws_upgrade(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(header::CONNECTION, "keep-alive, Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
    }

    #[tokio::test]
    async fn proxy_requires_trusted_peer_https_and_device_when_shared_token_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let router = proxy_fixture(dir.path());
        assert!(!dir.path().join(TOKEN_FILE).exists());
        let shared = ensure_token(dir.path(), false).unwrap();
        let store = Store::new(dir.path());
        let token = store
            .add("phone", None, chrono::Utc::now().timestamp())
            .unwrap();
        let path = "/file/note.md";
        let secure = |path| {
            proxy_request(path)
                .header("x-forwarded-for", "127.0.0.1, 203.0.113.1")
                .header("x-forwarded-proto", "https")
        };
        let bearer = format!("Bearer {token}");
        let query = format!("/ws?token={token}");
        let untrusted = secure(path).extension(ConnectInfo(
            "192.168.1.2:1234".parse::<SocketAddr>().unwrap(),
        ));
        let tailnet = |path| {
            proxy_request(path)
                .header("x-forwarded-for", "100.64.0.21")
                .header("x-forwarded-proto", "https")
        };
        for (request, expected) in [
            (
                proxy_request(path).header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (
                untrusted.header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (
                proxy_request(path)
                    .header("x-forwarded-for", "203.0.113.1")
                    .header("x-forwarded-proto", "http")
                    .header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (
                secure(path)
                    .header("x-forwarded-proto", "http")
                    .header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (
                secure(path)
                    .header("x-forwarded-for", "203.0.113.2")
                    .header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (tailnet(path), StatusCode::UNAUTHORIZED),
            (
                secure(path).header(header::AUTHORIZATION, format!("Bearer {shared}")),
                StatusCode::UNAUTHORIZED,
            ),
            (
                secure(path).header(header::AUTHORIZATION, &bearer),
                StatusCode::OK,
            ),
            (
                secure("/source/note.md").header(header::AUTHORIZATION, &bearer),
                StatusCode::OK,
            ),
            (secure(&query), StatusCode::UNAUTHORIZED),
            (ws_upgrade(secure(&query)), StatusCode::UPGRADE_REQUIRED),
            (
                ws_upgrade(secure("/ws").header(header::AUTHORIZATION, &bearer)),
                StatusCode::UPGRADE_REQUIRED,
            ),
            (
                proxy_request("/mcp?session=main").method(Method::POST),
                StatusCode::FORBIDDEN,
            ),
            (
                secure("/mcp?session=main")
                    .method(Method::POST)
                    .header(header::AUTHORIZATION, &bearer),
                StatusCode::FORBIDDEN,
            ),
            (
                secure(crate::self_update::HEALTH_ROUTE).header(header::AUTHORIZATION, &bearer),
                StatusCode::NOT_FOUND,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
        store.revoke("phone").unwrap();
        let request = secure(path)
            .header(header::AUTHORIZATION, bearer)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn proxy_limits_requests_and_counts_failures_by_forwarded_ip() {
        let dir = tempfile::tempdir().unwrap();
        let router = proxy_fixture(dir.path());
        let token = Store::new(dir.path())
            .add("phone", None, chrono::Utc::now().timestamp())
            .unwrap();
        let secure = |path: &str, ip: &str| {
            proxy_request(path)
                .header("x-forwarded-for", ip)
                .header("x-forwarded-proto", "https")
        };
        let path = "/file/note.md";
        let post = secure("/client/notify", "203.0.113.1")
            .method(Method::POST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let oversized_body = Body::from(vec![b' '; MAX_REQUEST_BODY + 1]);
        for (request, expected) in [
            (
                secure(
                    &format!("/file/{}", "x".repeat(MAX_REQUEST_TARGET)),
                    "203.0.113.1",
                )
                .body(Body::empty())
                .unwrap(),
                StatusCode::URI_TOO_LONG,
            ),
            (
                secure(path, "203.0.113.1")
                    .header("x-large", "x".repeat(MAX_REQUEST_HEADERS))
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            ),
            (
                post.body(oversized_body).unwrap(),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ] {
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
        for i in 0..=MAX_FAILURES {
            let request = secure(path, &format!("198.51.100.{i}, 203.0.113.1"))
                .body(Body::empty())
                .unwrap();
            let expected = if i < MAX_FAILURES {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
        for (ip, expected) in [
            ("203.0.113.1", StatusCode::TOO_MANY_REQUESTS),
            ("203.0.113.2", StatusCode::OK),
        ] {
            let request = secure(path, ip)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
    }

    #[test]
    fn failure_window_and_block_expiry_use_the_clock() {
        let start = Instant::now();
        let ip = "203.0.113.1".parse().unwrap();
        let mut failures = Failures::default();
        failures.failed(ip, start);
        let now = start + FAILURE_WINDOW;
        for _ in 0..MAX_FAILURES {
            assert!(!failures.limited(ip, now));
            failures.failed(ip, now);
        }
        assert!(failures.limited(ip, now + BLOCK_DURATION - Duration::from_millis(1)));
        assert!(!failures.limited(ip, now + BLOCK_DURATION));
        failures.failed(ip, now + BLOCK_DURATION);
        assert!(!failures.limited(ip, now + BLOCK_DURATION));
    }

    #[tokio::test]
    async fn revocation_closes_a_live_websocket_within_five_seconds() -> Result<()> {
        use fuzukue_proto::{Envelope, ServerEvent, PROTOCOL_VERSION};
        use tokio::io::{duplex, DuplexStream};
        use tokio_tungstenite::{
            client_async,
            tungstenite::{client::IntoClientRequest, Message},
        };

        struct OneConnection(Option<DuplexStream>);

        impl axum::serve::Listener for OneConnection {
            type Io = DuplexStream;
            type Addr = ();

            async fn accept(&mut self) -> (Self::Io, Self::Addr) {
                match self.0.take() {
                    Some(stream) => (stream, ()),
                    None => std::future::pending().await,
                }
            }

            fn local_addr(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path());
        let token = store.add("phone", None, chrono::Utc::now().timestamp())?;
        let peer: SocketAddr = "127.0.0.1:1234".parse()?;
        let router = proxy_fixture(dir.path()).layer(axum::Extension(ConnectInfo(peer)));
        let (client, stream) = duplex(128 * 1024);
        let server =
            tokio::spawn(async move { axum::serve(OneConnection(Some(stream)), router).await });
        let mut request = "ws://localhost/ws".into_client_request()?;
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
        request
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.1".parse()?);
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse()?);
        let deadline = Duration::from_secs(5);
        let (mut socket, _) = timeout(deadline, client_async(request, client)).await??;
        let hello = serde_json::json!({"type": "hello", "protocol": PROTOCOL_VERSION});
        socket.send(Message::Text(hello.to_string().into())).await?;
        let first = timeout(deadline, socket.next())
            .await?
            .context("WebSocket closed before welcome")??;
        let event: Envelope = serde_json::from_str(first.to_text()?)?;
        assert!(matches!(event.event, ServerEvent::Welcome { .. }));

        store.revoke("phone")?;
        timeout(deadline, async {
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await
        .context("revoked WebSocket stayed open")?;
        server.abort();
        Ok(())
    }
}
