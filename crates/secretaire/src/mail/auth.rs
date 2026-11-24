use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};
use rustix::termios::{tcgetattr, tcsetattr, LocalModes, OptionalActions, Termios};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use super::{Account, Auth};

#[derive(Deserialize, Serialize)]
#[serde(tag = "auth", rename_all = "lowercase")]
enum Credential {
    Password { user: String, app_password: String },
    Oauth2 { user: String, refresh_token: String },
}

type Credentials = BTreeMap<String, Credential>;

pub(super) enum Secret {
    Password(String),
    OAuth2(String),
}

#[derive(Default)]
pub(super) struct Tokens(HashMap<String, (String, Instant)>);

pub(super) struct CredentialFile(PathBuf);

impl CredentialFile {
    pub(super) fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn directory(&self, create: bool) -> Result<&Path> {
        let dir = self
            .0
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if create {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let meta = fs::symlink_metadata(dir).context("認証情報のディレクトリを開けない")?;
        ensure!(
            meta.is_dir() && meta.mode() & 0o777 == 0o700,
            "認証情報のディレクトリには 0700 の権限が必要"
        );
        Ok(dir)
    }

    fn lock(&self, create: bool) -> Result<File> {
        let dir = self.directory(create)?;
        let fd = open(
            dir.join(".mail-credentials.lock"),
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let file = File::from(fd);
        check_private(&file)?;
        flock(&file, FlockOperation::LockExclusive)?;
        Ok(file)
    }

    fn read(&self) -> Result<Credentials> {
        match open(
            &self.0,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let file = File::from(fd);
                check_private(&file)?;
                serde_json::from_reader(file.take(1024 * 1024))
                    .map_err(|_| anyhow::anyhow!("メール認証情報の形式が不正"))
            }
            Err(rustix::io::Errno::NOENT) => Ok(Credentials::new()),
            Err(_) => bail!("メール認証情報を開けない"),
        }
    }

    fn write(&self, credentials: &Credentials) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(self.directory(false)?)?;
        serde_json::to_writer(temp.as_file_mut(), credentials)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.0).map_err(|e| e.error)?;
        Ok(())
    }

    fn update(&self, update: impl FnOnce(&mut Credentials) -> Result<()>) -> Result<()> {
        let _lock = self.lock(true)?;
        let mut credentials = self.read()?;
        update(&mut credentials)?;
        self.write(&credentials)
    }

    pub(super) fn secret(&self, account: &Account, tokens: &mut Tokens) -> Result<Secret> {
        self.secret_with(account, tokens, &Ureq)
    }

    fn secret_with(
        &self,
        account: &Account,
        tokens: &mut Tokens,
        http: &impl TokenHttp,
    ) -> Result<Secret> {
        if let Some((token, expires)) = tokens.0.get(&account.name) {
            if *expires > Instant::now() + Duration::from_secs(15) {
                return Ok(Secret::OAuth2(token.clone()));
            }
        }
        let _lock = self.lock(false)?;
        let mut credentials = self.read()?;
        let credential = credentials
            .get_mut(&account.name)
            .context("このアカウントの mail auth が必要")?;
        match (&account.auth, credential) {
            (Auth::Password, Credential::Password { user, app_password })
                if *user == account.user =>
            {
                Ok(Secret::Password(app_password.clone()))
            }
            (
                Auth::Oauth2,
                Credential::Oauth2 {
                    user,
                    refresh_token,
                },
            ) if *user == account.user => {
                let response = exchange(
                    account,
                    http,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                    ],
                )?;
                if let Some(refresh) = response.refresh_token {
                    *refresh_token = refresh;
                    self.write(&credentials)?;
                }
                let expires =
                    Instant::now() + Duration::from_secs(response.expires_in.clamp(1, 86400));
                tokens.0.insert(
                    account.name.clone(),
                    (response.access_token.clone(), expires),
                );
                Ok(Secret::OAuth2(response.access_token))
            }
            _ => bail!("認証方式または利用者が設定と一致しない。mail auth が必要"),
        }
    }

    pub(super) fn authorize(&self, account: &Account) -> Result<()> {
        let credential = match account.auth {
            Auth::Password => {
                let mut password = password()?;
                if account.gmail() {
                    password.retain(|ch| ch != ' ');
                }
                ensure!(
                    !password.is_empty() && !password.chars().any(char::is_control),
                    "アプリパスワードが空か不正"
                );
                Credential::Password {
                    user: account.user.clone(),
                    app_password: password,
                }
            }
            Auth::Oauth2 => {
                let flow = Flow::new(account)?;
                println!("ブラウザーで開いて認可する URL\n{}", flow.url);
                eprint!("遷移先の URL: ");
                io::stderr().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                let code = flow.code(line.trim())?;
                let response = exchange(
                    account,
                    &Ureq,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", flow.redirect.as_str()),
                        ("code_verifier", &flow.verifier),
                    ],
                )?;
                Credential::Oauth2 {
                    user: account.user.clone(),
                    refresh_token: response
                        .refresh_token
                        .context("OAuth2 応答にリフレッシュトークンがない")?,
                }
            }
        };
        self.update(|credentials| {
            credentials.insert(account.name.clone(), credential);
            Ok(())
        })
    }

    pub(super) fn import(
        &self,
        path: &Path,
        accounts: &BTreeMap<String, Account>,
    ) -> Result<usize> {
        #[derive(Deserialize)]
        struct Legacy {
            user: String,
            app_password: Option<String>,
            refresh_token: Option<String>,
        }
        let legacy: BTreeMap<String, Legacy> = serde_json::from_reader(
            File::open(path)
                .context("移行元を開けない")?
                .take(1024 * 1024),
        )
        .map_err(|_| anyhow::anyhow!("移行元の認証ファイル形式が不正"))?;
        let mut imported = Vec::new();
        for (name, account) in accounts {
            let Some(item) = legacy.get(name).filter(|item| item.user == account.user) else {
                continue;
            };
            let credential = match account.auth {
                Auth::Password => item
                    .app_password
                    .as_ref()
                    .map(|value| Credential::Password {
                        user: item.user.clone(),
                        app_password: value.clone(),
                    }),
                Auth::Oauth2 => item.refresh_token.as_ref().map(|value| Credential::Oauth2 {
                    user: item.user.clone(),
                    refresh_token: value.clone(),
                }),
            };
            if let Some(credential) = credential {
                imported.push((name.clone(), credential));
            }
        }
        let count = imported.len();
        ensure!(count > 0, "移行できる設定済みアカウントがない");
        self.update(|credentials| {
            credentials.extend(imported);
            Ok(())
        })?;
        Ok(count)
    }
}

fn check_private(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.mode() & 0o777 == 0o600,
        "メール認証情報は権限 0600 の通常ファイルで指定する"
    );
    Ok(())
}

fn password() -> Result<String> {
    struct Restore<'a>(&'a File, Termios);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = tcsetattr(self.0, OptionalActions::Now, &self.1);
        }
    }

    let mut tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("アプリパスワード入力には端末が必要")?;
    let original = tcgetattr(&tty)?;
    let mut hidden = original.clone();
    hidden.local_modes.remove(LocalModes::ECHO);
    write!(tty, "アプリパスワード: ")?;
    tty.flush()?;
    tcsetattr(&tty, OptionalActions::Now, &hidden)?;
    let restore = Restore(&tty, original);
    let mut line = String::new();
    let result = BufReader::new(&tty).read_line(&mut line);
    drop(restore);
    writeln!(tty)?;
    result?;
    Ok(line.trim_end_matches(['\r', '\n']).into())
}

struct Flow {
    url: Url,
    redirect: Url,
    verifier: String,
    state: String,
}

impl Flow {
    fn new(account: &Account) -> Result<Self> {
        let mut url = account.endpoint("authorize")?;
        let redirect = account.redirect()?;
        let verifier = nonce();
        let state = nonce();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("client_id", account.client_id()?),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", account.scope()?),
            ("state", &state),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ]);
        url.query_pairs_mut().extend_pairs(account.auth_params.iter());
        Ok(Self {
            url,
            redirect,
            verifier,
            state,
        })
    }

    fn code(&self, input: &str) -> Result<String> {
        let mut redirected =
            Url::parse(input).map_err(|_| anyhow::anyhow!("リダイレクト URL が不正"))?;
        let params = redirected.query_pairs().into_owned().collect::<Vec<_>>();
        redirected.set_query(None);
        ensure!(
            redirected == self.redirect,
            "登録されたリダイレクト URI と一致しない"
        );
        let mut codes = params.iter().filter(|(key, _)| key == "code");
        let code = codes
            .next()
            .filter(|(_, value)| !value.is_empty())
            .context("認可コードがない。認可結果をブラウザーで確認する")?;
        let mut states = params.iter().filter(|(key, _)| key == "state");
        ensure!(
            states.next().is_some_and(|(_, value)| *value == self.state)
                && states.next().is_none()
                && codes.next().is_none()
                && !params.iter().any(|(key, _)| key == "error"),
            "認可の state または応答が不正"
        );
        Ok(code.1.clone())
    }
}

pub(super) fn nonce() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

trait TokenHttp {
    fn post(&self, url: &Url, form: &[(&str, &str)]) -> Result<TokenResponse>;
}

struct Ureq;

impl TokenHttp for Ureq {
    fn post(&self, url: &Url, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(30))
            .build();
        let response = agent
            .post(url.as_str())
            .send_form(form)
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => {
                    anyhow::anyhow!("OAuth2 トークン要求に失敗 (HTTP {code})")
                }
                ureq::Error::Transport(_) => anyhow::anyhow!("OAuth2 トークン要求に接続できない"),
            })?;
        ensure!(
            response.status() == 200,
            "OAuth2 トークン応答のステータスが不正"
        );
        serde_json::from_reader(response.into_reader().take(1024 * 1024))
            .map_err(|_| anyhow::anyhow!("OAuth2 トークン応答の形式が不正"))
    }
}

fn exchange(
    account: &Account,
    http: &impl TokenHttp,
    grant: &[(&str, &str)],
) -> Result<TokenResponse> {
    let mut form = vec![
        ("client_id", account.client_id()?),
        ("scope", account.scope()?),
    ];
    if let Some(secret) = account.client_secret.as_deref().filter(|s| !s.is_empty()) {
        form.push(("client_secret", secret));
    }
    form.extend_from_slice(grant);
    let mut response = http.post(&account.endpoint("token")?, &form)?;
    ensure!(
        !response.access_token.is_empty() && !response.access_token.chars().any(char::is_control),
        "OAuth2 アクセストークンが不正"
    );
    response.refresh_token = response.refresh_token.filter(|token| !token.is_empty());
    Ok(response)
}

#[cfg(test)]
mod tests;
