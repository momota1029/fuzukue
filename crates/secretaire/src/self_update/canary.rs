//! Build, run and retire one isolated candidate under the update lock.

#[cfg(target_os = "linux")]
mod isolation;

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{SelfUpdateCanaryState as Phase, SelfUpdateCanaryStatus};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, OwnedMutexGuard};
use tokio::time::Instant;

use super::{
    append_log, git, steps, supervisor, write_status, ProcessGroup, State, Step, Update, MAX_LOG,
};

const PORTS: std::ops::RangeInclusive<u16> = 7879..=7899;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct Control {
    commands: tokio::sync::Mutex<()>,
    job: Mutex<Option<Job>>,
    closing: AtomicBool,
}

#[derive(Clone)]
struct Job {
    stop: watch::Sender<bool>,
    promote: mpsc::Sender<oneshot::Sender<Result<()>>>,
}

impl Control {
    pub(super) fn ensure_open(&self) -> Result<()> {
        ensure!(
            !self.closing.load(Ordering::Acquire),
            "サーバーの終了処理が進んでいるよ"
        );
        Ok(())
    }

    fn job(&self) -> Option<Job> {
        self.job.lock().unwrap().clone()
    }
}

impl Update {
    pub(crate) async fn start_canary(self: &Arc<Self>, app: Arc<crate::App>) -> Result<()> {
        drop(self.admit()?);
        let mut build = Command::new("cargo");
        build.args(["build", "--locked", "--release", "-p", "secretaire"]);
        self.start_canary_with(app, build, Network::default()).await
    }

    async fn start_canary_with(
        self: &Arc<Self>,
        app: Arc<crate::App>,
        build: Command,
        network: Network,
    ) -> Result<()> {
        let _command = self.canary.commands.lock().await;
        self.canary.ensure_open()?;
        ensure!(
            !self.trial_pending() && !*self.swapping.borrow(),
            "サーバーの入れ替え中だよ"
        );
        ensure!(self.source.is_some(), "自己更新のソースを設定してねぇ");
        ensure!(
            self.config.canary_minutes > 0,
            "canary_minutes には正の分数を指定してねぇ"
        );
        #[cfg(not(target_os = "linux"))]
        bail!("試運転のファイル隔離には Linux が必要だよ");

        self.end_canary(false).await?;
        let run = self
            .run
            .clone()
            .try_lock_owned()
            .context("サーバーの更新が進んでいるよ")?;
        let lock = self.lock_builds()?;
        self.status.send_modify(|status| {
            status.active = true;
            status.cancelled = false;
            status.error = None;
            status.recovered = false;
            status.steps = steps(false);
            status.canary_status = Some(SelfUpdateCanaryStatus {
                state: Phase::Building,
                commit: None,
                port: None,
                connect_url: None,
                started_at: None,
                auto_discard_at: None,
                log: String::new(),
            });
        });
        let (stop_tx, stop) = watch::channel(false);
        let (promote_tx, promote) = mpsc::channel(1);
        *self.canary.job.lock().unwrap() = Some(Job {
            stop: stop_tx,
            promote: promote_tx,
        });
        let update = self.clone();
        tokio::spawn(update.run_canary(app, run, lock, stop, promote, build, network));
        Ok(())
    }

    pub(crate) async fn promote_canary(&self) -> Result<()> {
        let _command = self.canary.commands.lock().await;
        self.canary.ensure_open()?;
        ensure!(
            self.canary_phase() == Some(Phase::Running),
            "稼働中の試運転が見つからないよ"
        );
        let job = self.canary.job().context("試運転は終了したよ")?;
        let (result_tx, result) = oneshot::channel();
        job.promote
            .send(result_tx)
            .await
            .context("試運転は終了したよ")?;
        result.await.context("試運転は終了したよ")?
    }

    pub(crate) async fn discard_canary(&self) -> Result<()> {
        let _command = self.canary.commands.lock().await;
        self.end_canary(false).await
    }

    pub(crate) async fn shutdown_canary(&self) {
        self.canary.closing.store(true, Ordering::Release);
        let _command = self.canary.commands.lock().await;
        if let Err(error) = self.end_canary(true).await {
            tracing::warn!("stop canary: {error:#}");
        }
    }

    async fn end_canary(&self, shutdown: bool) -> Result<()> {
        if let Some(job) = self.canary.job() {
            ensure!(
                shutdown || self.canary_phase() != Some(Phase::Promoting),
                "試運転を昇格中だよ"
            );
            job.stop.send_replace(true);
            // The run lock is released after the process group exits and its state is removed.
            let _done = self.run.lock().await;
        }
        Ok(())
    }

    fn canary_phase(&self) -> Option<Phase> {
        self.status
            .borrow()
            .canary_status
            .as_ref()
            .map(|canary| canary.state)
    }

    fn canary_status(&self, f: impl FnOnce(&mut SelfUpdateCanaryStatus)) {
        self.status.send_modify(|status| {
            if let Some(canary) = &mut status.canary_status {
                f(canary);
            }
        });
    }

    fn canary_log(&self, text: &str) {
        self.canary_status(|canary| append_log(&mut canary.log, text));
    }

    pub(super) async fn check_canary_commit(&self, commit: &str) -> Result<()> {
        let source = self
            .source
            .as_deref()
            .context("自己更新のソースが見つからないよ")?;
        let head = git(source, &["rev-parse", "--verify", "HEAD"]).await?;
        let branch = git(source, &["symbolic-ref", "--short", "HEAD"]).await.ok();
        ensure!(
            head == commit && branch.as_deref() == Some(self.config.branch.as_str()),
            "試運転を作ったあとに更新元のブランチまたはコミットが変わったよ（候補 {commit}、現在 {head}）。試運転を作り直してねぇ"
        );
        Ok(())
    }

    async fn run_canary(
        self: Arc<Self>,
        app: Arc<crate::App>,
        run: OwnedMutexGuard<()>,
        lock: File,
        mut stop: watch::Receiver<bool>,
        promote: mpsc::Receiver<oneshot::Sender<Result<()>>>,
        build: Command,
        network: Network,
    ) {
        let result = self
            .canary_lifecycle(&app, &lock, &mut stop, promote, build, network)
            .await;
        if !matches!(result, Ok(End::Swapping)) {
            self.status.send_modify(|status| match result {
                Ok(End::Discarded(reason)) => {
                    status.wait = None;
                    let canary = status.canary_status.as_mut().unwrap();
                    canary.state = Phase::Discarded;
                    append_log(&mut canary.log, reason);
                    for step in &mut status.steps {
                        if matches!(step.state, State::Running | State::Pending) {
                            step.state = State::Skipped;
                        }
                    }
                    status.active = false;
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    let canary = status.canary_status.as_mut().unwrap();
                    canary.state = Phase::Failed;
                    append_log(&mut canary.log, &format!("{error}\n"));
                    super::fail(status, error);
                }
                Ok(End::Promote { .. } | End::Swapping) => unreachable!(),
            });
            self.save_canary_status();
            // A forked build or candidate may still hold a copy of this file
            // description while it exits. Release the lock before replacement.
            if let Err(error) = lock.unlock() {
                tracing::warn!("release the canary update lock: {error:#}");
            }
        }
        drop(lock);
        let mut job = self.canary.job.lock().unwrap();
        *job = None;
        drop(run);
    }

    async fn canary_lifecycle(
        &self,
        app: &crate::App,
        lock: &File,
        stop: &mut watch::Receiver<bool>,
        mut promote: mpsc::Receiver<oneshot::Sender<Result<()>>>,
        build: Command,
        network: Network,
    ) -> Result<End> {
        let dir = tempfile::Builder::new()
            .prefix("canary-")
            .tempdir_in(&self.root)?;
        let mut runtime = None;
        let result: Result<End> = async {
            let built = tokio::select! {
                biased;
                _ = wait_for_stop(stop) => return Ok(End::Discarded("試運転を終了したよ。\n")),
                result = self.build_canary(dir.path(), build) => result?,
            };
            let (checkout, commit, binary, project) = built;
            let lifetime = Duration::from_secs(u64::from(self.config.canary_minutes) * 60);
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, (network.port)()?));
            let candidate = Runtime::start(&binary, &project, dir.path(), &self.server_config, addr)?;
            let started = chrono::Utc::now().timestamp_millis();
            let expires = Instant::now() + lifetime;
            self.canary_status(|status| {
                status.port = Some(addr.port());
                status.connect_url = Some(format!("ws://{addr}/ws"));
                status.started_at = Some(started);
                status.auto_discard_at = Some(started + lifetime.as_millis() as i64);
            });
            // Keep ownership outside the cancellable futures so discard can reap it.
            let pid_file = fs::write(dir.path().join("pid"), candidate.pid.to_string());
            runtime = Some(candidate);
            let candidate = runtime.as_mut().unwrap();
            pid_file.context("試運転の PID を記録できない")?;
            tokio::select! {
                biased;
                _ = wait_for_stop(stop) => return Ok(End::Discarded("試運転を終了したよ。\n")),
                result = candidate.wait_healthy(&commit, HEALTH_TIMEOUT, network.probe.clone()) => result?,
            }
            self.read_canary_log(candidate);
            self.canary_log(&format!("試運転につなぐ URL は ws://{addr}/ws だよ。\n"));
            self.canary_status(|status| status.state = Phase::Running);
            self.save_canary_status();
            if let Some(end) = self.await_promotion(candidate, stop, &mut promote, expires, &commit).await? {
                return Ok(end);
            }
            self.stop_candidate(&mut runtime).await?;
            self.step(Step::Publish, State::Running);
            let build_state = checkout.path().join(".windows-build");
            tokio::select! {
                biased;
                _ = wait_for_stop(stop) => Ok(End::Discarded("試運転を終了したよ。\n")),
                result = async {
                    self.prebuild(checkout.path(), &build_state, self.config.prebuild()).await?;
                    self.check_canary_commit(&commit).await?;
                    let binary = promote_binary(&self.root, &commit, &binary)?;
                    Ok::<_, anyhow::Error>((binary, self.prepared_registration(checkout)))
                } => {
                    let (binary, registration) = result?;
                    Ok(End::Promote { binary, commit, registration })
                },
            }
        }.await;

        let stopped = self.stop_candidate(&mut runtime).await;
        let removed = dir.close();
        if removed.is_ok() {
            self.canary_log("試運転の一時ディレクトリを 1 件削除したよ。\n");
        }
        stopped?;
        removed.context("試運転の一時ディレクトリを削除できない")?;

        match result? {
            end @ End::Discarded(_) => Ok(end),
            End::Promote { binary, commit, registration } => {
                tokio::select! {
                    biased;
                    _ = wait_for_stop(stop) => Ok(End::Discarded("サーバーの終了に合わせて試運転を終了したよ。\n")),
                    result = self.swap(app, &binary, lock.try_clone()?, false, Some(&commit), registration) => match result? {
                        super::SwapOutcome::Cancelled => Ok(End::Discarded("入れ替えを中止したよ。\n")),
                        super::SwapOutcome::Launched => Ok(End::Swapping),
                    }
                }
            }
            End::Swapping => unreachable!(),
        }
    }

    async fn stop_candidate(&self, runtime: &mut Option<Runtime>) -> Result<()> {
        if let Some(mut candidate) = runtime.take() {
            let result = candidate.stop().await;
            self.read_canary_log(&mut candidate);
            result.context("試運転のプロセスを停止できない")?;
            self.canary_log("試運転のサーバーを 1 件停止したよ。\n");
        }
        Ok(())
    }

    async fn build_canary(
        &self,
        dir: &Path,
        build: Command,
    ) -> Result<(tempfile::TempDir, String, PathBuf, PathBuf)> {
        let source = self
            .source
            .as_deref()
            .context("自己更新のソースが見つからないよ")?;
        let (checkout, commit) = self.prepare_checkout(source).await?;
        self.log(
            Step::Publish,
            "試運転では Windows クライアントの事前ビルドと配布を省くよ。\n",
        );
        self.step(Step::Publish, State::Skipped);
        let binary = self
            .build_and_test(checkout.path(), &commit, build, dir)
            .await?;
        let project = match &self.config.canary_project {
            Some(project) => {
                let home =
                    PathBuf::from(std::env::var_os("HOME").context("HOME が設定されていない")?);
                let project = project
                    .strip_prefix("~")
                    .map(|path| home.join(path))
                    .unwrap_or_else(|_| home.join(project))
                    .canonicalize()
                    .context("canary_project を開けない")?;
                for real in [&self.project, &source.canonicalize()?, &self.root] {
                    ensure!(
                        !real.starts_with(&project) && !project.starts_with(real),
                        "canary_project には本番と更新元から独立したディレクトリを指定してねぇ"
                    );
                }
                ensure!(
                    project.is_dir(),
                    "canary_project にディレクトリを指定してねぇ"
                );
                project
            }
            None => {
                let project = dir.join("project");
                self.clone_checkout(checkout.path(), &project, &commit)
                    .await?;
                // Detach from the private build checkout as a remote; the shared
                // objects remain readable until the canary and the checkout close.
                git(&project, &["remote", "remove", "origin"]).await?;
                project
            }
        };
        Ok((checkout, commit, binary, project))
    }

    async fn await_promotion(
        &self,
        candidate: &mut Runtime,
        stop: &mut watch::Receiver<bool>,
        promote: &mut mpsc::Receiver<oneshot::Sender<Result<()>>>,
        expires: Instant,
        commit: &str,
    ) -> Result<Option<End>> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                biased;
                _ = wait_for_stop(stop) => return Ok(Some(End::Discarded("試運転を終了したよ。\n"))),
                _ = tokio::time::sleep_until(expires) => return Ok(Some(End::Discarded("制限時間に達したので、試運転を自動で破棄したよ。\n"))),
                result = candidate.child.wait() => bail!("試運転のサーバーが終了したよ ({})", result?),
                _ = tick.tick() => self.read_canary_log(candidate),
                Some(reply) = promote.recv() => {
                    if let Err(error) = self.check_canary_commit(commit).await {
                        self.status.send_modify(|status| {
                            let message = format!("{error:#}");
                            append_log(&mut status.canary_status.as_mut().unwrap().log, &format!("{message}\n"));
                            status.error = Some(message);
                        });
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    if Instant::now() >= expires {
                        let _ = reply.send(Err(anyhow::anyhow!("試運転の制限時間に達したよ")));
                        continue;
                    }
                    self.status.send_modify(|status| {
                        let canary = status.canary_status.as_mut().unwrap();
                        canary.state = Phase::Promoting;
                        append_log(&mut canary.log, "試運転で使った候補を本番へ昇格するよ。\n");
                        status.error = None;
                    });
                    let _ = reply.send(Ok(()));
                    return Ok(None);
                }
            }
        }
    }

    fn read_canary_log(&self, candidate: &mut Runtime) {
        match candidate.log_tail() {
            Ok(log) if !log.is_empty() => self.canary_log(&log),
            Ok(_) => {}
            Err(error) => tracing::warn!("read canary log: {error:#}"),
        }
    }

    fn save_canary_status(&self) {
        if let Err(error) = write_status(&self.root, &self.status.borrow()) {
            tracing::warn!("save canary status: {error:#}");
        }
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

enum End {
    Discarded(&'static str),
    Promote { binary: PathBuf, commit: String, registration: Option<tempfile::TempDir> },
    Swapping,
}

type Probe = Arc<dyn Fn(SocketAddr, &str, Option<&str>) -> std::io::Result<bool> + Send + Sync>;

struct Network {
    port: fn() -> Result<u16>,
    probe: Probe,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            port: || choose_port(PORTS),
            probe: Arc::new(supervisor::probe_build),
        }
    }
}

fn promote_binary(root: &Path, commit: &str, binary: &Path) -> Result<PathBuf> {
    let dir = root.join(commit);
    fs::create_dir_all(&dir)?;
    let attempt = tempfile::Builder::new().prefix("build-").tempdir_in(dir)?;
    let target = attempt.path().join("secretaire");
    fs::rename(binary, &target)?;
    Ok(attempt.keep().join("secretaire"))
}

struct Runtime {
    child: Child,
    group: ProcessGroup,
    pid: u32,
    addr: SocketAddr,
    log: File,
}

impl Runtime {
    fn start(
        binary: &Path,
        project: &Path,
        dir: &Path,
        config: &str,
        addr: SocketAddr,
    ) -> Result<Self> {
        let mut cmd = runtime_command(binary, project, dir, config, addr)?;
        let path = dir.join("server.log");
        let output = File::options().append(true).create_new(true).open(&path)?;
        let log = File::open(path)?;
        cmd.stdout(output.try_clone()?).stderr(output);
        let child = cmd.spawn().context("試運転のサーバーを起動できない")?;
        let pid = child.id().context("試運転の PID を取得できない")?;
        let group = ProcessGroup(rustix::process::Pid::from_raw(pid as i32));
        Ok(Self {
            child,
            group,
            pid,
            addr,
            log,
        })
    }

    async fn wait_healthy(&mut self, commit: &str, timeout: Duration, check: Probe) -> Result<()> {
        let probe = async {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let (addr, pid, commit) = (self.addr, self.pid.to_string(), commit.to_owned());
                let check = check.clone();
                if tokio::task::spawn_blocking(move || check(addr, &pid, Some(&commit)))
                    .await?
                    .unwrap_or(false)
                {
                    return Ok(());
                }
            }
        };
        tokio::select! {
            result = self.child.wait() => bail!("ヘルスチェックの前に試運転のサーバーが終了したよ ({})", result?),
            result = tokio::time::timeout(timeout, probe) => result.context("試運転のヘルスチェックが時間内に通らなかったよ")?,
        }
    }

    async fn stop(&mut self) -> std::io::Result<()> {
        if let Some(pid) = self.group.0 {
            #[cfg(target_os = "linux")]
            isolation::stop_session(pid)?;
            self.group.0 = None;
        }
        self.child.wait().await?;
        Ok(())
    }

    fn log_tail(&mut self) -> std::io::Result<String> {
        let length = self.log.metadata()?.len();
        let position = self.log.stream_position()?;
        if length.saturating_sub(position) > MAX_LOG as u64 {
            self.log.seek(SeekFrom::Start(length - MAX_LOG as u64))?;
        }
        let mut bytes = Vec::new();
        (&mut self.log)
            .take(MAX_LOG as u64)
            .read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn choose_port(ports: std::ops::RangeInclusive<u16>) -> Result<u16> {
    find_port(ports, |port| {
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map(drop)
    })
}

fn find_port(
    ports: std::ops::RangeInclusive<u16>,
    mut try_bind: impl FnMut(u16) -> std::io::Result<()>,
) -> Result<u16> {
    for port in ports {
        match try_bind(port) {
            Ok(()) => return Ok(port),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error).context("試運転のポートを調べられない"),
        }
    }
    bail!("試運転用の空きポートが見つからないよ")
}

fn canary_config(config: &str, addr: SocketAddr) -> Result<String> {
    use toml::{Table, Value};
    let mut config = config.parse::<Table>()?;
    config.remove("self_update");
    let remote = config
        .entry("remote")
        .or_insert_with(|| Value::Table(Table::new()));
    remote
        .as_table_mut()
        .context("[remote] の設定を開けない")?
        .insert(
            "listen".into(),
            Value::Array(vec![Value::String(addr.to_string())]),
        );
    Ok(toml::to_string(&config)?)
}

fn runtime_command(
    binary: &Path,
    project: &Path,
    dir: &Path,
    config: &str,
    addr: SocketAddr,
) -> Result<Command> {
    let home = dir.join("home");
    let state = dir.join("state");
    let config_home = dir.join("config");
    let tmp = dir.join("tmp");
    for path in [&home, &state, &config_home, &tmp] {
        fs::create_dir(path)?;
    }
    let config_file = config_home.join("server.toml");
    fs::write(&config_file, canary_config(config, addr)?)?;
    let mut cmd = Command::new(binary);
    cmd.arg(project)
        .current_dir(project)
        .env_clear()
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for name in [
        "PATH",
        "LANG",
        "LC_ALL",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "FUZUKUE_ALLOWED_LOGINS",
        "LABO_ALLOWED_LOGINS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    for (name, path) in [
        ("HOME", &home),
        ("XDG_CONFIG_HOME", &config_home),
        ("XDG_STATE_HOME", &state),
        ("XDG_CACHE_HOME", &home.join(".cache")),
        ("XDG_DATA_HOME", &home.join(".local/share")),
        ("XDG_RUNTIME_DIR", &tmp),
        ("TMPDIR", &tmp),
        ("TMP", &tmp),
        ("TEMP", &tmp),
        ("FUZUKUE_SERVER_CONFIG", &config_file),
    ] {
        cmd.env(name, path);
    }
    cmd.env("FUZUKUE_CANARY", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("FUZUKUE_AUTO_REVIEW", "0");
    #[cfg(not(test))]
    copy_chat_credentials(&home, &mut cmd)?;
    #[cfg(target_os = "linux")]
    isolation::configure(&mut cmd, dir, project)?;
    Ok(cmd)
}

#[cfg(not(test))]
fn copy_chat_credentials(home: &Path, cmd: &mut Command) -> Result<()> {
    // Give interactive chats independent credential files and local histories.
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| real_home.as_ref().map(|home| home.join(".codex")));
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| real_home.as_ref().map(|home| home.join(".claude")));
    let load = |path: PathBuf| -> Result<Option<serde_json::Value>> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("試運転用のチャット認証を用意できない"),
        }
    };
    if let Some(source) = codex {
        let target = home.join(".codex");
        fs::create_dir(&target)?;
        if let Some(mut auth) = load(source.join("auth.json"))? {
            // Leave production refresh credentials with the production session.
            if let Some(tokens) = auth
                .get_mut("tokens")
                .and_then(serde_json::Value::as_object_mut)
            {
                tokens.insert(
                    "refresh_token".into(),
                    serde_json::Value::String(String::new()),
                );
            }
            use std::os::unix::fs::OpenOptionsExt;
            let file = File::options()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(target.join("auth.json"))?;
            serde_json::to_writer(file, &auth)?;
        }
        if let Some(models) = load(source.join("models_cache.json"))? {
            serde_json::to_writer(File::create(target.join("models_cache.json"))?, &models)?;
        }
    }
    if let Some(source) = claude.filter(|_| {
        [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none())
    }) {
        if let Some(auth) = load(source.join(".credentials.json"))? {
            if let Some(token) = auth["claudeAiOauth"]["accessToken"].as_str() {
                cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token);
            }
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
