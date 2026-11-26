//! Build a pinned server release and hand the restart to a detached supervisor.

mod build_cache;
mod canary;
pub(crate) mod drain;
mod notice;
mod prune;
mod registration;
mod retry;
pub(crate) mod service_swap;
mod supervisor;
pub(crate) mod wait_route;

use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{DrainProgress, DrainReason, DrainStatus};
use fuzukue_proto::{
    SelfUpdateStatus as Status, SelfUpdateStep, SelfUpdateStepKind as Step,
    SelfUpdateStepState as State, SelfUpdateWait, SelfUpdateWaitAction as WaitAction,
    SelfUpdateWaitReason,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{watch, OwnedRwLockReadGuard, RwLock, RwLockWriteGuard};

pub(crate) use supervisor::{health, legacy_handoff, managed, ready, relocate_build_links, run as supervise, HEALTH_ROUTE};
pub(crate) use notice::Recipient;
pub(crate) const BUILD_ID: &str = env!("FUZUKUE_SERVER_BUILD_ID");
const SOURCE_ENV: &str = "FUZUKUE_SELF_UPDATE_SOURCE";
const STATUS_FILE: &str = "status.json";
const MAX_LOG: usize = 8192;
const WINDOWS_PUBLISH: &str = "scripts/publish-windows.sh";
const WINDOWS_BUILD_STATE: &str = "FUZUKUE_WINDOWS_BUILD_STATE";
const PREBUILT: &str = "FUZUKUE_PREBUILT";
const WAIT_FORCE_GRACE: Duration = Duration::from_secs(5);
const WAIT_FORCE_LIMIT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    source: Option<PathBuf>,
    branch: String,
    tests: Vec<CommandLine>,
    shots: Vec<CommandLine>,
    prebuild: Option<Vec<CommandLine>>,
    publish: Vec<CommandLine>,
    prune: bool,
    canary_project: Option<PathBuf>,
    canary_minutes: u32,
    orphan_approval_seconds: u64,
    wait_stall_seconds: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source: None,
            branch: "main".into(),
            tests: vec![CommandLine::Shell("cargo test --locked -p secretaire -p fuzukue-proto -p fuzukue-view -p fuzukue-i18n -p fuzukue-cards".into())],
            shots: Vec::new(),
            prebuild: None,
            publish: Vec::new(),
            prune: true,
            canary_project: None,
            canary_minutes: 30,
            orphan_approval_seconds: drain::ORPHAN_APPROVAL_AGE.as_secs(),
            wait_stall_seconds: 300,
        }
    }
}

impl Config {
    fn prebuild(&self) -> &[CommandLine] {
        if let Some(commands) = &self.prebuild {
            return commands;
        }
        if matches!(self.publish.as_slice(), [command] if command.is_default_windows_publish()) {
            static DEFAULT: std::sync::LazyLock<[CommandLine; 1]> =
                std::sync::LazyLock::new(|| {
                    [CommandLine::Argv(vec![
                        WINDOWS_PUBLISH.into(),
                        "build".into(),
                    ])]
                });
            &*DEFAULT
        } else {
            &[]
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum CommandLine {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Copy)]
enum LogMode {
    Plain,
    Prebuild,
    Test,
}

impl LogMode {
    fn prefix(self) -> &'static str {
        match self {
            Self::Prebuild => "[prebuild] ",
            _ => "",
        }
    }
}

struct ProcessGroup(Option<rustix::process::Pid>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

impl CommandLine {
    fn is_default_windows_publish(&self) -> bool {
        match self {
            Self::Shell(script) => script == WINDOWS_PUBLISH,
            Self::Argv(args) => args == &[WINDOWS_PUBLISH],
        }
    }

    fn command(&self) -> Result<Command> {
        Ok(match self {
            Self::Shell(script) => {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", script]);
                cmd
            }
            Self::Argv(args) => {
                let (program, args) = args.split_first().context("empty self-update command")?;
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd
            }
        })
    }
}

pub(crate) struct Update {
    config: Config,
    source: Option<PathBuf>,
    project: PathBuf,
    root: PathBuf,
    exe: PathBuf,
    service_address: Option<std::net::SocketAddr>,
    status: watch::Sender<Status>,
    pub(crate) swapping: watch::Sender<bool>,
    gate: Arc<RwLock<()>>,
    wait_control: std::sync::Mutex<WaitControl>,
    wait_changed: tokio::sync::Notify,
    last_wait: std::sync::atomic::AtomicI64,
    drain_status: watch::Sender<Option<DrainStatus>>,
    pub(crate) shutdown_ready: watch::Sender<bool>,
    shutdown_signalled: std::sync::atomic::AtomicBool,
    trial_pending: std::sync::atomic::AtomicBool,
    run: Arc<tokio::sync::Mutex<()>>,
    result_notice: std::sync::Mutex<()>,
    canary: canary::Control,
    server_config: String,
}

#[derive(Clone, Copy, Default)]
enum WaitControl {
    #[default]
    Closed,
    Open(i64, DrainReason),
    Requested(i64, DrainReason, WaitAction),
    Forcing(i64, DrainReason),
    Cancelling(i64, DrainReason),
    Ready(i64, DrainReason),
}

impl WaitControl {
    fn owner(self) -> Option<(i64, DrainReason)> {
        match self {
            Self::Closed => None,
            Self::Open(id, reason) | Self::Requested(id, reason, _) | Self::Forcing(id, reason)
            | Self::Cancelling(id, reason) | Self::Ready(id, reason) => Some((id, reason)),
        }
    }
}

struct WaitGuard<'a> {
    control: &'a std::sync::Mutex<WaitControl>,
    id: i64,
    reason: DrainReason,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let mut control = self.control.lock().unwrap();
        if control.owner() == Some((self.id, self.reason))
            && !matches!(*control, WaitControl::Ready(_, DrainReason::Shutdown))
        {
            *control = WaitControl::Closed;
        }
    }
}

enum WaitOutcome<'a> {
    Ready(RwLockWriteGuard<'a, ()>),
    Cancelled,
    TimedOut(Vec<String>),
}

enum SwapOutcome {
    Cancelled,
    Launched,
}

impl Update {
    pub(crate) fn new(config: Config, home: &Path, project: PathBuf, addr: std::net::SocketAddr, server_config: String) -> Self {
        let exe = std::env::current_exe().unwrap_or_default();
        let source = config
            .source
            .clone()
            .or_else(|| fuzukue_proto::compat::env_os(SOURCE_ENV).map(PathBuf::from))
            .or_else(|| source_from_exe(&exe))
            .or_else(|| {
                crate::service::managed()
                    .then(|| std::env::var_os(crate::service::INSTALL_SOURCE_ENV).map(PathBuf::from))
                    .flatten()
            })
            .map(|path| {
                if let Ok(relative) = path.strip_prefix("~") {
                    home.join(relative)
                } else if path.is_relative() {
                    home.join(path)
                } else {
                    path
                }
            });
        let root = crate::state::root().join("server-builds");
        let root = std::path::absolute(&root).unwrap_or(root);
        let mut update = Self::with_paths(config, source, project, root, exe);
        update.server_config = server_config;
        update.service_address = crate::service::managed().then_some(addr);
        update
    }

    pub(crate) fn shutdown_only(project: &Path, state: &Path) -> Self {
        let update = Self::with_paths(Config::default(), None, project.into(), state.join("drain"), PathBuf::new());
        update.trial_pending.store(false, std::sync::atomic::Ordering::Release);
        update
    }

    fn with_paths(
        config: Config,
        source: Option<PathBuf>,
        project: PathBuf,
        root: PathBuf,
        exe: PathBuf,
    ) -> Self {
        let trial_pending = supervisor::managed() || service_swap::pending(&root);
        let update = Self {
            config,
            source,
            project,
            root,
            exe,
            service_address: None,
            status: watch::channel(Status::default()).0,
            swapping: watch::channel(false).0,
            gate: Arc::default(),
            wait_control: Default::default(),
            wait_changed: Default::default(),
            last_wait: Default::default(),
            drain_status: watch::channel(None).0,
            shutdown_ready: watch::channel(false).0,
            shutdown_signalled: false.into(),
            trial_pending: trial_pending.into(),
            run: Default::default(),
            result_notice: Default::default(),
            canary: Default::default(),
            server_config: String::new(),
        };
        update.restore_status();
        update
    }

    fn restore_status(&self) {
        let saved = read_status(&self.root);
        self.status.send_modify(|status| {
            *status = saved;
            if status.active && !service_swap::pending(&self.root) {
                if let Some(canary) = &mut status.canary_status {
                    canary.state = fuzukue_proto::SelfUpdateCanaryState::Failed;
                    append_log(&mut canary.log, "サーバーが再起動したため、試運転は終了したよ。\n");
                }
                fail(status, "previous server handoff did not finish".into());
            }
            if !status.active && status.canary_status.as_ref().is_some_and(|canary|
                canary.state == fuzukue_proto::SelfUpdateCanaryState::Promoting)
            {
                if let Some(error) = &status.error {
                    let canary = status.canary_status.as_mut().unwrap();
                    canary.state = fuzukue_proto::SelfUpdateCanaryState::Failed;
                    append_log(&mut canary.log, error);
                } else {
                    status.canary_status = None;
                }
            }
            if !holds_planner_work(status) {
                status.planner_work_held = None;
            }
            if status.steps.is_empty() && self.source.is_some() {
                status.steps = steps(false);
            }
            status.build_id = BUILD_ID.into();
            status.available = self.source.is_some();
            status.can_rollback = self.previous().is_some();
        });
    }

    pub(crate) fn follow_handoff(self: &Arc<Self>, ready: Option<PathBuf>, app: Arc<crate::App>) {
        let update = self.clone();
        tokio::spawn(async move {
            if let Some(ready) = ready {
                // On failure the supervisor terminates the trial before removing the marker.
                while service_swap::waiting(&ready) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                update.restore_status();
            }
            update
                .trial_pending
                .store(false, std::sync::atomic::Ordering::Release);
            if let Err(error) = update.finish_registration(&app.distribution).await {
                tracing::error!(%error, "complete client registration after server swap");
            }
            update.report_result_with_retry(&app).await;
        });
    }

    pub(crate) fn service_ready(&self, addr: std::net::SocketAddr) -> Result<Option<PathBuf>> {
        service_swap::ready(&self.root, addr)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    pub(crate) fn planner_work_held(&self) -> bool {
        holds_planner_work(&self.status.borrow())
    }

    pub(crate) async fn wait_for_planner_hold(&self, held: bool) {
        let _ = self.subscribe().wait_for(|status| holds_planner_work(status) == held).await;
    }

    pub(crate) fn start_planner_work<T>(&self, start: impl FnOnce() -> T) -> Option<T> {
        // Keep the status read lock through the journal claim. Update::start
        // either follows that claim or closes planner admission before it.
        let status = self.status.borrow();
        (!holds_planner_work(&status)).then(start)
    }

    fn report_planner_queue(&self, app: &crate::App) {
        let count = app.held_planner_count();
        self.status.send_if_modified(|status| {
            let held = (count > 0 && holds_planner_work(status)).then_some(count);
            if status.planner_work_held == held {
                return false;
            }
            status.planner_work_held = held;
            true
        });
    }

    async fn monitor_planner_queue(&self, app: &crate::App) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            self.report_planner_queue(app);
        }
    }

    pub(crate) async fn refresh(&self) -> Status {
        if let Ok(_run) = self.run.try_lock() {
            let commit = match &self.source {
                Some(source) => git(source, &["rev-parse", "--verify", "HEAD"]).await.ok(),
                None => None,
            };
            self.status.send_if_modified(|status| {
                if status.source_commit == commit {
                    false
                } else {
                    status.source_commit = commit;
                    true
                }
            });
        }
        let mut status = self.status.borrow().clone();
        stamp_wait(&mut status);
        status
    }

    pub(crate) fn trial_pending(&self) -> bool {
        self.trial_pending
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn orphan_approval_age(&self) -> Duration {
        Duration::from_secs(self.config.orphan_approval_seconds)
    }

    pub(crate) fn admit(&self) -> Result<OwnedRwLockReadGuard<()>> {
        self.admit_mutation(None).map(|(permit, _)| permit)
    }

    pub(crate) fn admit_mutation(&self, expected_epoch: Option<i64>) -> Result<(OwnedRwLockReadGuard<()>, i64)> {
        ensure!(!self.trial_pending(), "server startup check in progress");
        let control = self.wait_control.lock().unwrap();
        ensure!(matches!(*control, WaitControl::Closed), "server update is draining");
        let epoch = self.last_wait.load(std::sync::atomic::Ordering::Relaxed);
        ensure!(expected_epoch.is_none_or(|expected| expected == epoch), "a drain started after the request");
        let work = self
            .gate
            .clone()
            .try_read_owned()
            .context("server swap in progress")?;
        ensure!(!*self.swapping.borrow(), "server swap in progress");
        Ok((work, epoch))
    }

    pub(crate) fn hold_input<T>(&self, save: impl FnOnce() -> Result<T>) -> Result<T> {
        let control = self.wait_control.lock().unwrap();
        ensure!(!self.trial_pending() && !*self.swapping.borrow() && !matches!(*control, WaitControl::Ready(..)),
            "server swap in progress");
        save()
    }

    pub(crate) fn subscribe_drain(&self) -> watch::Receiver<Option<DrainStatus>> {
        self.drain_status.subscribe()
    }

    fn control(&self, id: i64, reason: DrainReason, action: WaitAction) -> Result<()> {
        let mut control = self.wait_control.lock().unwrap();
        ensure!(matches!(*control, WaitControl::Open(current, active) if id == current && active == reason),
            "this drain is no longer accepting controls");
        ensure!(reason != DrainReason::Shutdown || !self.shutdown_signalled.load(std::sync::atomic::Ordering::Acquire),
            "shutdown was requested by a signal");
        *control = WaitControl::Requested(id, reason, action);
        self.wait_changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn control_wait(&self, started_at: i64, action: WaitAction) -> Result<()> {
        self.control(started_at, DrainReason::Update, action)
    }

    pub(crate) fn cancel_shutdown(&self, started_at: i64) -> Result<()> {
        self.control(started_at, DrainReason::Shutdown, WaitAction::Cancel)
    }

    fn begin_wait(&self, reason: DrainReason, signal: bool) -> Result<Option<i64>> {
        use std::sync::atomic::Ordering;
        let mut control = self.wait_control.lock().unwrap();
        if reason == DrainReason::Shutdown && signal {
            self.shutdown_signalled.store(true, Ordering::Release);
            if let WaitControl::Requested(id, DrainReason::Shutdown, WaitAction::Cancel) = *control {
                *control = WaitControl::Open(id, reason);
                return Ok(None);
            }
            if (control.owner().is_some_and(|(_, reason)| reason == DrainReason::Shutdown)
                && !matches!(*control, WaitControl::Cancelling(..)))
                || *self.swapping.borrow()
            {
                return Ok(None);
            }
        } else {
            ensure!(matches!(*control, WaitControl::Closed) && !*self.swapping.borrow() && !self.trial_pending(),
                "the server is already draining or switching builds");
        }
        let now = chrono::Utc::now().timestamp_millis();
        let previous = self.last_wait.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
            |previous| Some(now.max(previous + 1))).unwrap();
        let id = now.max(previous + 1);
        *control = WaitControl::Open(id, reason);
        self.wait_changed.notify_waiters();
        self.drain_status.send_replace(Some(DrainStatus {
            started_at: id, reason, progress: DrainProgress::Draining, jobs_running: 0,
            blocking: SelfUpdateWaitReason::default(), can_cancel: !signal, message: None,
        }));
        Ok(Some(id))
    }

    pub(crate) fn request_shutdown(
        self: &Arc<Self>, app: Arc<crate::App>, limit: Duration, signal: bool,
    ) -> Result<()> {
        let Some(id) = self.begin_wait(DrainReason::Shutdown, signal)? else { return Ok(()) };
        let update = self.clone();
        let stop_by = tokio::time::Instant::now() + limit;
        tokio::spawn(async move {
            let wait = WaitGuard { control: &update.wait_control, id, reason: DrainReason::Shutdown };
            match update.wait_for_idle(&app, &wait, Some(stop_by)).await {
                Ok(WaitOutcome::Cancelled) => {}
                Ok(WaitOutcome::Ready(_)) => {
                    update.shutdown_ready.send_replace(true);
                }
                Ok(WaitOutcome::TimedOut(errors)) => {
                    app.mutations.interrupt().await;
                    app.retry_drain_saves();
                    update.set_drain_progress(id, DrainProgress::TimedOut, Some(errors.join("\n")));
                    update.shutdown_ready.send_replace(true);
                }
                Err(error) => {
                    tracing::error!(%error, "shutdown drain failed");
                    update.set_drain_progress(id, DrainProgress::Failed, Some(format!("{error:#}")));
                    if update.shutdown_signalled.load(std::sync::atomic::Ordering::Acquire) {
                        *update.wait_control.lock().unwrap() = WaitControl::Ready(id, DrainReason::Shutdown);
                        app.mutations.interrupt().await;
                        update.shutdown_ready.send_replace(true);
                    }
                }
            }
        });
        Ok(())
    }

    pub(crate) async fn force_shutdown(&self, app: &crate::App, started_at: Option<i64>) -> Result<()> {
        let id = match started_at {
            Some(id) => id,
            None => self.begin_wait(DrainReason::Shutdown, false)?.context("shutdown did not start")?,
        };
        {
            let mut control = self.wait_control.lock().unwrap();
            ensure!(matches!(*control, WaitControl::Open(current, DrainReason::Shutdown) if current == id),
                "this shutdown is no longer accepting controls");
            *control = WaitControl::Ready(id, DrainReason::Shutdown);
        }
        // Close admission before flushing records and committing to exit.
        app.mutations.interrupt().await;
        app.retry_drain_saves();
        match app.finish_transcript_flush().await {
            Ok(()) => self.set_drain_progress(id, DrainProgress::Ready, None),
            Err(error) => self.set_drain_progress(id, DrainProgress::Failed, Some(format!("{error:#}"))),
        }
        self.shutdown_ready.send_replace(true);
        Ok(())
    }

    fn set_drain_progress(&self, id: i64, progress: DrainProgress, message: Option<String>) {
        self.drain_status.send_if_modified(|status| {
            let Some(status) = status.as_mut().filter(|status| status.started_at == id) else { return false };
            status.progress = progress;
            if progress == DrainProgress::Ready {
                status.jobs_running = 0;
                status.blocking.gate_held = false;
                status.blocking.threads.retain(|thread| thread.queued || thread.paused);
            }
            status.can_cancel = false;
            status.message = message;
            true
        });
    }

    pub(crate) fn start(
        self: &Arc<Self>, app: Arc<crate::App>, rollback: bool, recipient: Option<Recipient>,
    ) -> Result<()> {
        self.canary.ensure_open()?;
        drop(self.admit()?);
        ensure!(
            !self.trial_pending() && !*self.swapping.borrow(),
            "server swap in progress"
        );
        let previous = if rollback {
            Some(self.previous().context("no previous server build")?)
        } else {
            ensure!(
                self.source.is_some(),
                "self-update source is not configured"
            );
            None
        };
        // An owned guard spans the asynchronous pipeline.
        let run = self
            .run
            .clone()
            .try_lock_owned()
            .context("self-update already running")?;
        let lock = self.lock_builds()?;
        let supervisor_lock = lock.try_clone()?;
        self.report_result(&app)?;
        let mut status = self.status.borrow().clone();
        status.active = true;
        status.error = None;
        status.recovered = false;
        status.cancelled = false;
        status.steps = steps(rollback);
        status.steps[0].state = State::Running;
        status.canary_status = None;
        status.planner_work_held = None;
        self.begin_result(status, recipient, rollback)?;
        let update = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                result = update.execute(&app, previous, supervisor_lock) => result,
                _ = update.monitor_planner_queue(&app) => unreachable!(),
            };
            if let Err(error) = result {
                update.fail(format!("{error:#}"));
                let drain = update.drain_status.borrow().clone();
                if let Some(status) = drain {
                    if status.reason == DrainReason::Update {
                        update.set_drain_progress(status.started_at, DrainProgress::Failed, Some(format!("{error:#}")));
                    }
                }
            }
            let launched = *update.swapping.borrow();
            if !launched {
                if update.status.borrow().active {
                    update.fail("server swap ended before launch".into());
                }
                if let Err(error) = update.save_status() {
                    tracing::warn!("save self-update status: {error:#}");
                }
            }
            drop(lock);
            drop(run);
            if !launched {
                update.report_result_with_retry(&app).await;
            }
        });
        Ok(())
    }

    fn lock_builds(&self) -> Result<File> {
        match fs::DirBuilder::new().mode(0o700).create(&self.root) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(
                    self.root
                        .parent()
                        .context("invalid server builds directory")?,
                )?;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.root)?;
            }
            Err(err) => return Err(err.into()),
        }
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("lock"))?;
        lock.try_lock().context("another server is updating")?;
        Ok(lock)
    }

    async fn execute(&self, app: &crate::App, previous: Option<PathBuf>, lock: File) -> Result<()> {
        let rollback = previous.is_some();
        let (candidate, registration) = match previous {
            Some(path) => (path, None),
            None => {
                let source = self.source.as_deref().context("no self-update source")?;
                let mut cargo = Command::new("cargo");
                cargo.args(["build", "--locked", "--release", "-p", "secretaire"]);
                self.pipeline(source, cargo).await?
            }
        };
        self.swap(app, &candidate, lock, rollback, None, registration).await?;
        Ok(())
    }

    async fn swap(&self, app: &crate::App, candidate: &Path, lock: File, rollback: bool, pinned: Option<&str>, registration: Option<tempfile::TempDir>) -> Result<SwapOutcome> {
        let current = self.snapshot_current()?;
        let id = self.begin_wait(DrainReason::Update, false)?.context("wait did not start")?;
        let wait = WaitGuard { control: &self.wait_control, id, reason: DrainReason::Update };
        let idle = self.wait_for_idle(app, &wait, None).await;
        if let Err(error) = &idle {
            self.set_drain_progress(wait.id, DrainProgress::Failed, Some(format!("{error:#}")));
            app.notify_drain_cancelled(wait.id, wait.reason).await;
        }
        let WaitOutcome::Ready(_idle) = idle? else {
            return Ok(SwapOutcome::Cancelled);
        };
        if !drain::supports(candidate)? {
            app.protect_legacy_rollback()?;
        }
        self.set_drain_progress(wait.id, DrainProgress::Ready, None);
        self.step(Step::Wait, State::Passed);
        self.step(Step::Swap, State::Running);
        if let Some(commit) = pinned {
            self.check_canary_commit(commit).await?;
        }
        let control = self.wait_control.lock().unwrap();
        if control.owner() != Some((wait.id, wait.reason)) {
            return Ok(SwapOutcome::Cancelled);
        }
        self.save_status()?;
        let registration = registration
            .map(|checkout| self.queue_registration(checkout, candidate))
            .transpose()?;
        let launch = if let Some(addr) = self.service_address {
            service_swap::launch(self, &current, candidate, addr, rollback)
        } else {
            supervisor::launch(
                &self.root, self.source.as_deref(), &current, candidate, lock, self.config.prune,
            )
        };
        if let Err(error) = launch {
            if let Some(path) = registration {
                if let Err(cleanup) = fs::remove_dir_all(path) {
                    tracing::warn!(%cleanup, "remove client registration after failed launch");
                }
            }
            return Err(error);
        }
        self.swapping.send_replace(true);
        Ok(SwapOutcome::Launched)
    }

    async fn wait_for_idle<'a>(
        &'a self, app: &crate::App, wait: &WaitGuard<'_>, stop_by: Option<tokio::time::Instant>,
    ) -> Result<WaitOutcome<'a>> {
        let started_at = wait.id;
        let drain = wait.reason;
        if drain == DrainReason::Update {
            self.report_planner_queue(app);
            self.step(Step::Wait, State::Running);
        }
        use tokio::time::Instant;
        let stall_limit = Duration::from_secs(u64::from(self.config.wait_stall_seconds.max(1)));
        let mut last_progress = Instant::now();
        let mut stall_by = (drain == DrainReason::Update).then(|| last_progress + stall_limit);
        let mut last_blockers: Vec<fuzukue_proto::SelfUpdateBlocker> = Vec::new();
        let mut last_jobs = 0;
        let mut last_errors = 0;
        let mut force_started = None;
        let mut automatic_force = false;
        let mut force_error: Option<String> = None;
        loop {
            let changed = self.wait_changed.notified();
            tokio::pin!(changed);
            let control = *self.wait_control.lock().unwrap();
            if control.owner() != Some((started_at, drain)) || matches!(control, WaitControl::Ready(..)) {
                return Ok(WaitOutcome::Cancelled);
            }
            if matches!(control, WaitControl::Open(..)) {
                let prepare = app.prepare_drain(started_at, drain);
                let deadline = stop_by.or(stall_by).expect("every drain has a deadline");
                tokio::select! {
                    _ = tokio::time::timeout_at(deadline, prepare) => {}
                    _ = &mut changed => {}
                }
            } else {
                app.retry_drain_saves();
            }
            let (gate, mut reason) = self.swap_gate(app);
            let ready = gate.is_some() && reason.threads.iter().all(|thread| {
                !thread.running && !thread.worker && !thread.checking
            }) && !reason.checking_sessions && !reason.gate_held;
            let mut errors = app.drain_errors(&mut reason);
            let awaiting_reply = reason.blockers.iter().any(|blocker| matches!(blocker.kind,
                fuzukue_proto::SelfUpdateBlockerKind::Approval | fuzukue_proto::SelfUpdateBlockerKind::Question));
            if ready && errors.is_empty() && !awaiting_reply {
                // The drain holds the admission gate while saving the last
                // transcripts. A failed checkpoint keeps a swap from proceeding.
                let deadline = stop_by.or(stall_by).unwrap_or_else(|| Instant::now() + WAIT_FORCE_LIMIT);
                match tokio::time::timeout_at(deadline, app.flush_dirty_transcripts()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(format!("{error:#}")),
                    Err(_) => errors.push("Timed out saving dirty transcripts".into()),
                }
            }
            let jobs = app.drain_job_count(&reason);
            let now = Instant::now();
            if stall_by.is_some() {
                let blocker_finished = jobs < last_jobs || errors.len() < last_errors
                    || last_blockers.iter().any(|old| !reason.blockers.iter().any(|current| {
                        (current.kind, &current.workspace, &current.id, current.started_at)
                            == (old.kind, &old.workspace, &old.id, old.started_at)
                    }));
                if blocker_finished {
                    last_progress = now;
                } else if let Some(progress) = app.last_partner_progress() {
                    last_progress = last_progress.max(progress);
                }
                stall_by = Some(last_progress + stall_limit);
                last_jobs = jobs;
                last_errors = errors.len();
                last_blockers.clone_from(&reason.blockers);
            }
            let ready = ready && errors.is_empty() && !awaiting_reply;
            let expired = stop_by.is_some_and(|stop_by| tokio::time::Instant::now() >= stop_by);
            let cancelled = {
                let mut control = self.wait_control.lock().unwrap();
                if control.owner() != Some((started_at, drain)) || matches!(*control, WaitControl::Ready(..)) {
                    return Ok(WaitOutcome::Cancelled);
                }
                match *control {
                    WaitControl::Requested(_, _, WaitAction::Cancel)
                        if drain == DrainReason::Update || !self.shutdown_signalled.load(std::sync::atomic::Ordering::Acquire) => {
                        *control = WaitControl::Cancelling(started_at, drain);
                        true
                    }
                    WaitControl::Requested(_, _, WaitAction::Force) => {
                        *control = WaitControl::Forcing(started_at, drain);
                        force_started = Some(now);
                        stall_by = None;
                        force_error = None;
                        false
                    }
                    _ if ready && app.drain_errors(&mut reason).is_empty() => {
                        *control = WaitControl::Ready(started_at, drain);
                        if drain == DrainReason::Shutdown {
                            self.set_drain_progress(started_at, DrainProgress::Ready, None);
                        }
                        return Ok(WaitOutcome::Ready(gate.unwrap()));
                    }
                    WaitControl::Open(..) if stall_by.is_some_and(|deadline| now >= deadline) => {
                        let blocker_kinds: Vec<_> = reason.blockers.iter().map(|blocker| blocker.kind).collect();
                        tracing::warn!(started_at, jobs, ?blocker_kinds, gate_held = reason.gate_held,
                            save_errors = errors.len(), elapsed_secs = elapsed_wait(started_at),
                            stalled_secs = now.saturating_duration_since(last_progress).as_secs(),
                            "Update drain stalled; requesting force");
                        *control = WaitControl::Requested(started_at, drain, WaitAction::Force);
                        automatic_force = true;
                        continue;
                    }
                    _ if expired => {
                        *control = WaitControl::Ready(started_at, drain);
                        return Ok(WaitOutcome::TimedOut(errors));
                    }
                    _ => false,
                }
            };
            drop(gate);
            if cancelled {
                app.notify_drain_cancelled(started_at, drain).await;
                self.set_drain_progress(started_at, DrainProgress::Cancelled, None);
                if drain == DrainReason::Update {
                    self.status.send_modify(|status| {
                        status.wait = None;
                        status.active = false;
                        status.cancelled = true;
                        status.planner_work_held = None;
                        for step in &mut status.steps {
                            if matches!(step.state, State::Running | State::Pending) {
                                step.state = State::Skipped;
                            }
                        }
                    });
                    self.save_status()?;
                }
                return Ok(WaitOutcome::Cancelled);
            }

            let forcing = force_started.is_some();
            let mut messages: Vec<_> = errors.iter().map(String::as_str).collect();
            if automatic_force {
                messages.push("Drain made no progress before the timeout. Interrupting work for the server swap.");
            }
            messages.extend(force_error.as_deref());
            let log = messages.join("\n");
            let status = DrainStatus {
                started_at, reason: drain, jobs_running: jobs,
                progress: if forcing { DrainProgress::Forcing } else { DrainProgress::Draining },
                blocking: reason.clone(),
                can_cancel: !forcing && (drain == DrainReason::Update || !self.shutdown_signalled.load(std::sync::atomic::Ordering::Acquire)),
                message: (!log.is_empty()).then(|| log.clone()),
            };
            self.drain_status.send_if_modified(|current| {
                if current.as_ref().is_some_and(|current| current.started_at == started_at && current != &status
                    && matches!(current.progress, DrainProgress::Draining | DrainProgress::Forcing))
                {
                    *current = Some(status);
                    true
                } else { false }
            });
            if drain == DrainReason::Update && self.status.send_if_modified(|status| {
                let mut log_changed = false;
                if let Some(step) = status.steps.iter_mut().find(|step| step.kind == Step::Wait) {
                    log_changed = step.log != log;
                    step.log.clone_from(&log);
                }
                if status.wait.as_ref().is_some_and(|wait| wait.reason == reason && wait.forcing == forcing) {
                    return log_changed;
                }
                status.wait = Some(SelfUpdateWait { started_at, elapsed_secs: elapsed_wait(started_at), reason, forcing });
                true
            }) {
                self.save_status()?;
            }
            if let Some(start) = force_started {
                let reap_at = start + WAIT_FORCE_GRACE;
                let force_by = start + WAIT_FORCE_LIMIT;
                if now >= force_by {
                    let mut message = "Timed out draining work for the server swap.".to_owned();
                    if !log.is_empty() {
                        append_log(&mut message, &format!("\n{log}"));
                    }
                    if automatic_force {
                        bail!("{message}");
                    }
                    let mut control = self.wait_control.lock().unwrap();
                    if control.owner() == Some((started_at, drain)) {
                        *control = WaitControl::Open(started_at, drain);
                    }
                    force_error = Some(message);
                    force_started = None;
                    last_progress = now;
                    stall_by = Some(last_progress + stall_limit);
                } else if errors.is_empty() {
                    let reap = now >= reap_at;
                    let deadline = if reap { force_by } else { reap_at };
                    if let Ok(Err(error)) = tokio::time::timeout_at(deadline, app.interrupt_for_swap(started_at, reap)).await {
                        force_error = Some(format!("{error:#}"));
                    }
                }
            }
            let tick = Instant::now() + Duration::from_millis(250);
            tokio::select! {
                _ = tokio::time::sleep_until(stop_by.or(stall_by).map_or(tick, |deadline| tick.min(deadline))) => {}
                _ = &mut changed => {}
            }
        }
    }

    fn swap_gate(&self, app: &crate::App) -> (Option<RwLockWriteGuard<'_, ()>>, SelfUpdateWaitReason) {
        let gate = self.gate.try_write().ok();
        let mut reason = app.swap_blockers();
        reason.gate_held = gate.is_none();
        let blocked = reason.checking_sessions || reason.threads.iter().any(|thread| thread.running || thread.worker || thread.checking);
        (if blocked { None } else { gate }, reason)
    }

    async fn pipeline(&self, source: &Path, build: Command) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
        self.with_build_cache(self.prepare_pipeline(source, build)).await
    }

    async fn prepare_pipeline(&self, source: &Path, build: Command) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
        let (checkout, commit) = self.prepare_checkout(source).await?;
        let build_state = checkout.path().join(".windows-build");
        let prebuild = self.config.prebuild();
        // A failed server step drops the prebuild future and its process group.
        let (prebuilt, staged) = tokio::try_join!(
            biased;
            async { Ok(self.prebuild(checkout.path(), &build_state, prebuild).await) },
            async {
                let staged = self.build_and_test(checkout.path(), &commit, build, &self.root).await?;
                if !prebuild.is_empty() {
                    self.step(Step::Publish, State::Running);
                }
                Ok::<_, anyhow::Error>(staged)
            },
        )?;
        prebuilt?;
        Ok((staged, self.prepared_registration(checkout)))
    }

    async fn prepare_checkout(&self, source: &Path) -> Result<(tempfile::TempDir, String)> {
        self.step(Step::Check, State::Running);
        let source = fs::canonicalize(source).context("open self-update source")?;
        ensure!(
            is_workspace(&source),
            "self-update source is not the fuzukue workspace"
        );
        let top = git(&source, &["rev-parse", "--show-toplevel"]).await?;
        ensure!(
            Path::new(&top) == source,
            "self-update source must be the repository root"
        );
        ensure!(
            git(&source, &["symbolic-ref", "--short", "HEAD"]).await? == self.config.branch,
            "expected source branch {}",
            self.config.branch
        );
        let commit = git(&source, &["rev-parse", "--verify", "HEAD"]).await?;
        ensure!(
            is_commit(&commit) && commit != BUILD_ID,
            "source commit matches running build"
        );
        self.status.send_modify(|status| {
            status.source_commit = Some(commit.clone());
            if let Some(canary) = &mut status.canary_status {
                canary.commit = Some(commit.clone());
            }
        });
        ensure!(
            git(
                &source,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--",
                    ".",
                    ":(exclude)threads",
                    ":(exclude)inbox",
                ],
            )
            .await?
            .is_empty(),
            "self-update source has local changes"
        );

        // A private checkout pins every step, even if the user's branch moves during the build.
        let checkout = tempfile::Builder::new()
            .prefix("source-")
            .tempdir_in(&self.root)?;
        self.clone_checkout(&source, checkout.path(), &commit).await?;
        self.step(Step::Check, State::Passed);
        Ok((checkout, commit))
    }

    async fn clone_checkout(&self, source: &Path, checkout: &Path, commit: &str) -> Result<()> {
        let mut clone = Command::new("git");
        clone
            .args(["clone", "--quiet", "--shared", "--no-checkout", "--"])
            .arg(source)
            .arg(checkout);
        self.command(Step::Check, source, &mut clone, None).await?;
        let mut checkout_cmd = Command::new("git");
        checkout_cmd.args(["checkout", "--quiet", "--detach", commit]);
        self.command(Step::Check, checkout, &mut checkout_cmd, None).await
    }

    fn prepared_registration(&self, checkout: tempfile::TempDir) -> Option<tempfile::TempDir> {
        self.step(
            Step::Publish,
            if self.config.publish.is_empty() {
                if self.config.prebuild().is_empty() {
                    State::Skipped
                } else {
                    State::Passed
                }
            } else {
                State::Pending
            },
        );
        (!self.config.publish.is_empty()).then_some(checkout)
    }

    async fn prebuild(
        &self,
        cwd: &Path,
        build_state: &Path,
        commands: &[CommandLine],
    ) -> Result<()> {
        for command in commands {
            let mut command = command.command()?;
            command
                .env(WINDOWS_BUILD_STATE, build_state)
                .env_remove(PREBUILT)
                .env_remove("FUZUKUE_SERVER_BINARY")
                .env_remove("LABO_SERVER_BINARY");
            let (status, _, _) = self
                .run_command(Step::Publish, cwd, &mut command, None, LogMode::Prebuild)
                .await?;
            ensure!(
                status.success(),
                "Publish prebuild command failed: {status}"
            );
        }
        Ok(())
    }

    async fn build_and_test(
        &self,
        checkout: &Path,
        commit: &str,
        mut build: Command,
        staging: &Path,
    ) -> Result<PathBuf> {
        self.step(Step::Build, State::Running);
        // Remove the previous cargo output so success must produce a binary this run.
        let release = self.root.join("target/release");
        fs::create_dir_all(&release)?;
        ensure!(
            release.canonicalize()? == self.root.canonicalize()?.join("target/release"),
            "server build target must be inside the staging directory"
        );
        let output = release.join("secretaire");
        match fs::remove_file(&output) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        build.env("FUZUKUE_SERVER_BUILD_ID", commit);
        self.command(Step::Build, checkout, &mut build, None)
            .await?;
        let staged = stage_binary(staging, commit, &output)?;
        self.step(Step::Build, State::Passed);
        for (step, commands) in [
            (Step::Test, &self.config.tests),
            (Step::Shot, &self.config.shots),
        ] {
            if commands.is_empty() {
                self.step(step, State::Skipped);
                continue;
            }
            self.step(step, State::Running);
            for command in commands {
                if step == Step::Test {
                    self.test_command(checkout, &staged, command).await?;
                } else {
                    self.command(step, checkout, &mut command.command()?, Some(&staged))
                        .await?;
                }
            }
            self.step(step, State::Passed);
        }
        Ok(staged)
    }

    async fn test_command(&self, cwd: &Path, staged: &Path, command: &CommandLine) -> Result<()> {
        let (status, stdout, stderr) = self
            .run_command(
                Step::Test,
                cwd,
                &mut command.command()?,
                Some(staged),
                LogMode::Test,
            )
            .await?;
        if status.success() {
            return Ok(());
        }
        let Some(retry) = retry::CargoTest::after_failure(command, status, &stdout, &stderr) else {
            bail!("Test command failed: {status}");
        };
        let failed = stdout.failures();
        let mut still_failing = Vec::new();
        for &name in &failed {
            let (status, stdout, _) = self
                .run_command(
                    Step::Test,
                    cwd,
                    &mut retry.command(name),
                    Some(staged),
                    LogMode::Test,
                )
                .await?;
            if !status.success() || stdout.passed == 0 {
                still_failing.push(name);
            }
        }
        self.log(Step::Test, &format!("retried: {}\n", failed.join(", ")));
        ensure!(
            still_failing.is_empty(),
            "Test retry failed: {}",
            still_failing.join(", ")
        );
        Ok(())
    }

    async fn command(
        &self,
        step: Step,
        cwd: &Path,
        cmd: &mut Command,
        staged: Option<&Path>,
    ) -> Result<()> {
        let (status, _, _) = self
            .run_command(step, cwd, cmd, staged, LogMode::Plain)
            .await?;
        ensure!(status.success(), "{step:?} command failed: {status}");
        Ok(())
    }

    async fn run_command(
        &self,
        step: Step,
        cwd: &Path,
        cmd: &mut Command,
        staged: Option<&Path>,
        mode: LogMode,
    ) -> Result<(ExitStatus, retry::Report, retry::Report)> {
        self.log(step, &format!("{}$ {:?}\n", mode.prefix(), cmd.as_std()));
        cmd.current_dir(cwd)
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_PROFILE_DEV_DEBUG", "0")
            .env("CARGO_PROFILE_TEST_DEBUG", "0")
            .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
            .env("FUZUKUE_PROJECT_DIR", &self.project)
            .env("LABO_PROJECT_DIR", &self.project)
            .env_remove(supervisor::READY_ENV)
            .env_remove("LABO_SELF_UPDATE_READY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if step == Step::Test {
            cmd.env_remove("FUZUKUE_SERVICE");
        }
        if let Some(staged) = staged {
            cmd.env("FUZUKUE_SERVER_BINARY", staged)
                .env("LABO_SERVER_BINARY", staged);
        }
        let separate_group = step == Step::Publish
            || self.status.borrow().canary_status.is_some();
        let scoped = matches!(step, Step::Build | Step::Test | Step::Shot | Step::Publish);
        if scoped {
            let (mut child, _job) = crate::job_process::Job::spawn(cmd, "update-build", None)
                .with_context(|| format!("launch {step:?} command"))?;
            let stdout = child.stdout.take().context("missing stdout")?;
            let stderr = child.stderr.take().context("missing stderr")?;
            return Ok(tokio::try_join!(
                child.wait(),
                self.read_log(step, stdout, mode),
                self.read_log(step, stderr, mode)
            )?);
        }
        if separate_group {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().with_context(|| format!("launch {step:?} command"))?;
        let mut group = ProcessGroup(
            child.id().filter(|_| separate_group)
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
        );
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let result = tokio::try_join!(
            child.wait(),
            self.read_log(step, stdout, mode),
            self.read_log(step, stderr, mode)
        )?;
        if result.0.success() {
            group.0 = None;
        }
        Ok(result)
    }

    async fn with_build_cache<T>(
        &self,
        build: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        // The update/canary caller holds lock_builds across this operation.
        let root = self.root.clone();
        self.log(Step::Build, "Checking the shared self-update Cargo target\n");
        tokio::task::spawn_blocking(move || build_cache::maintain(&root, true)).await??;
        let result = build.await;
        let root = self.root.clone();
        match tokio::task::spawn_blocking(move || build_cache::maintain(&root, false)).await {
            Ok(Ok(true)) => self.log(Step::Build, "Cleared the self-update Cargo target at its storage limit\n"),
            Ok(Ok(false)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not maintain the self-update Cargo target"),
            Err(error) => tracing::warn!(%error, "self-update Cargo target maintenance failed"),
        }
        result
    }

    async fn read_log(
        &self,
        step: Step,
        mut stream: impl AsyncRead + Unpin,
        mode: LogMode,
    ) -> std::io::Result<retry::Report> {
        let mut report = retry::Report::default();
        if !matches!(mode, LogMode::Plain) {
            let mut stream = BufReader::new(stream);
            let mut buf = Vec::new();
            while stream.read_until(b'\n', &mut buf).await? != 0 {
                let line = String::from_utf8_lossy(&buf);
                if matches!(mode, LogMode::Test) {
                    report.observe(&line);
                    self.log(step, &line);
                } else {
                    self.log(step, &format!("{}{line}", mode.prefix()));
                    if !line.ends_with('\n') {
                        self.log(step, "\n");
                    }
                }
                buf.clear();
            }
            return Ok(report);
        }
        let mut buf = [0; 4096];
        loop {
            let count = stream.read(&mut buf).await?;
            if count == 0 {
                return Ok(report);
            }
            self.log(step, &String::from_utf8_lossy(&buf[..count]));
        }
    }

    fn log(&self, kind: Step, text: &str) {
        self.status.send_modify(|status| {
            if let Some(canary) = &mut status.canary_status {
                append_log(&mut canary.log, text);
            }
            if let Some(step) = status.steps.iter_mut().find(|step| step.kind == kind) {
                append_log(&mut step.log, text);
            }
        });
    }

    fn step(&self, kind: Step, state: State) {
        self.status.send_modify(|status| {
            if kind == Step::Wait && state != State::Running {
                status.wait = None;
            }
            if matches!(kind, Step::Test | Step::Shot) && state == State::Running {
                if let Some(canary) = &mut status.canary_status {
                    canary.state = fuzukue_proto::SelfUpdateCanaryState::Testing;
                }
            }
            if let Some(step) = status.steps.iter_mut().find(|step| step.kind == kind) {
                step.state = state;
            }
        });
        self.save_result_progress();
    }

    fn fail(&self, error: String) {
        self.status.send_modify(|status| fail(status, error));
    }

    fn previous(&self) -> Option<PathBuf> {
        let current = fs::canonicalize(self.root.join("current")).ok()?;
        let previous = fs::canonicalize(self.root.join("previous")).ok()?;
        let root = fs::canonicalize(&self.root).ok()?;
        (current == self.exe
            && previous != current
            && previous.starts_with(&root)
            && previous.is_file())
        .then_some(previous)
    }

    #[cfg(not(target_os = "linux"))]
    fn snapshot_current(&self) -> Result<PathBuf> {
        bail!("実行中のサーバーの保存には Linux の procfs が必要だよ");
    }

    #[cfg(target_os = "linux")]
    fn snapshot_current(&self) -> Result<PathBuf> {
        let running = Path::new("/proc/self/exe");
        if fs::canonicalize(&self.root).is_ok_and(|root| self.exe.starts_with(root)) {
            if let (Ok(current), Ok(path)) = (fs::metadata(running), fs::metadata(&self.exe)) {
                if (current.dev(), current.ino()) == (path.dev(), path.ino()) {
                    return Ok(self.exe.clone());
                }
            }
        }
        let id = if is_commit(BUILD_ID) {
            BUILD_ID
        } else {
            "original"
        };
        stage_binary(&self.root, id, running)
    }
}

fn append_log(log: &mut String, text: &str) {
    log.push_str(text);
    if log.len() > MAX_LOG {
        let start = log.ceil_char_boundary(log.len() - MAX_LOG);
        log.drain(..start);
    }
}

fn steps(rollback: bool) -> Vec<SelfUpdateStep> {
    let steps = [
        Step::Check,
        Step::Build,
        Step::Test,
        Step::Shot,
        Step::Wait,
        Step::Swap,
        Step::Publish,
    ];
    steps
        .into_iter()
        .filter(|step| !rollback || matches!(step, Step::Wait | Step::Swap))
        .map(|kind| SelfUpdateStep {
            kind,
            state: State::Pending,
            log: String::new(),
        })
        .collect()
}

fn holds_planner_work(status: &Status) -> bool {
    status.active && status.canary_status.is_none()
        && !status.steps.iter().any(|step| step.kind == Step::Swap && step.state == State::Passed)
}

pub(crate) fn fail(status: &mut Status, error: String) {
    status.active = false;
    status.wait = None;
    status.planner_work_held = None;
    status.error = Some(error);
    if let Some(step) = status
        .steps
        .iter_mut()
        .find(|step| step.state == State::Running)
    {
        step.state = State::Failed;
    }
}

fn elapsed_wait(started_at: i64) -> u64 {
    chrono::Utc::now().timestamp_millis().saturating_sub(started_at).max(0) as u64 / 1000
}

pub(crate) fn stamp_wait(status: &mut Status) {
    if let Some(wait) = &mut status.wait {
        wait.elapsed_secs = elapsed_wait(wait.started_at);
    }
}

fn is_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_workspace(repo: &Path) -> bool {
    fs::read_to_string(repo.join("Cargo.toml"))
        .ok()
        .and_then(|manifest| manifest.parse::<toml::Value>().ok())
        .and_then(|manifest| {
            manifest
                .get("workspace")?
                .get("members")?
                .as_array()
                .cloned()
        })
        .is_some_and(|members| {
            ["crates/secretaire", "crates/fuzukue-proto"]
                .iter()
                .all(|path| members.iter().any(|member| member.as_str() == Some(path)))
        })
        && repo.join("crates/secretaire/Cargo.toml").is_file()
}

pub(crate) fn source_from_exe(exe: &Path) -> Option<PathBuf> {
    let repo = exe.parent()?.parent()?.parent()?;
    (exe == repo.join("target/release/secretaire") && is_workspace(repo)).then(|| repo.to_owned())
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await?;
    if !result.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn stage_binary(root: &Path, commit: &str, source: &Path) -> Result<PathBuf> {
    let parent = root.join(commit);
    fs::create_dir_all(&parent)?;
    let attempt = tempfile::Builder::new()
        .prefix("build-")
        .tempdir_in(&parent)?;
    let binary = attempt.path().join("secretaire");
    ensure!(fs::copy(source, &binary)? > 0, "empty staged server binary");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o500))?;
    File::open(&binary)?.sync_all()?;
    Ok(attempt.keep().join("secretaire"))
}

fn read_status(root: &Path) -> Status {
    notice::SavedStatus::read(root).unwrap_or_default().status
}

fn write_status(root: &Path, status: &Status) -> Result<()> {
    notice::SavedStatus::save(root, status)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let root = path.parent().context("missing state directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(temp.as_file_mut(), value)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_gate(dir: &Path) -> Arc<Update> {
    Arc::new(Update::with_paths(Config::default(), None, dir.into(), dir.join("server-builds"), PathBuf::new()))
}

#[cfg(test)]
mod drain_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn source(dir: &Path) -> Result<PathBuf> {
        let source = dir.join("source");
        fs::create_dir_all(source.join("crates/secretaire"))?;
        fs::write(source.join("crates/secretaire/Cargo.toml"), "")?;
        fs::write(
            source.join("Cargo.toml"),
            "[workspace]\nmembers = ['crates/secretaire', 'crates/fuzukue-proto']\n",
        )?;
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &[
                "-c",
                "user.email=test@example.org",
                "-c",
                "user.name=Test",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            let status = std::process::Command::new("git")
                .current_dir(&source)
                .args(args)
                .status()?;
            ensure!(status.success(), "git {args:?} failed");
        }
        Ok(source)
    }

    fn fake_build() -> Command {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"mkdir -p "$CARGO_TARGET_DIR/release" &&
            printf '#!/bin/sh\necho staged\n' > "$CARGO_TARGET_DIR/release/secretaire""#,
        ]);
        command
    }

    pub(super) fn fixture(dir: &Path, source: &Path, config: Config) -> Result<Update> {
        let root = dir.join("builds");
        fs::create_dir(&root)?;
        Ok(Update::with_paths(
            config,
            Some(source.to_owned()),
            dir.to_owned(),
            root,
            PathBuf::new(),
        ))
    }

    fn begin(update: &Update) {
        update.status.send_modify(|status| status.steps = steps(false));
    }

    #[tokio::test]
    async fn failing_test_stops_before_shots_and_publishing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let config = toml::from_str(
            r#"
          tests = ["echo test-failed >&2; exit 7"]
          shots = ['touch "$FUZUKUE_PROJECT_DIR/shot"']
          publish = ['touch "$FUZUKUE_PROJECT_DIR/published"']
        "#,
        )?;
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let error = update.pipeline(&source, fake_build()).await.unwrap_err();
        update.fail(error.to_string());
        let steps = &update.status.borrow().steps;
        assert_eq!(steps[2].state, State::Failed);
        assert!(steps[2].log.contains("test-failed"));
        assert_eq!(steps[3].state, State::Pending);
        assert_eq!(steps[6].state, State::Pending);
        assert!(!dir.path().join("shot").exists());
        assert!(!dir.path().join("published").exists());
        Ok(())
    }

    #[test]
    fn cleanup_defaults_to_enabled_and_can_be_disabled() -> Result<()> {
        assert!(toml::from_str::<Config>("")?.prune);
        assert!(!toml::from_str::<Config>("prune = false")?.prune);
        Ok(())
    }

    #[test]
    fn default_windows_publish_uses_prebuild_unless_overridden() -> Result<()> {
        for publish in [
            r#"publish = ["scripts/publish-windows.sh"]"#,
            r#"publish = [["scripts/publish-windows.sh"]]"#,
        ] {
            let config: Config = toml::from_str(publish)?;
            let [CommandLine::Argv(args)] = config.prebuild() else {
                bail!("expected the Windows build command");
            };
            assert_eq!(args, &[WINDOWS_PUBLISH, "build"]);
            let config: Config = toml::from_str(&format!("{publish}\nprebuild = []"))?;
            assert!(config.prebuild().is_empty());
        }
        let config: Config =
            toml::from_str(r#"publish = ["scripts/publish-windows.sh /my/project"]"#)?;
        assert!(config.prebuild().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn prebuild_overlaps_server_steps_and_registration_requires_swap_success() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let config = toml::from_str(
            r#"
            prebuild = [[ "sh", "-c", '''
                set -e
                touch "$FUZUKUE_PROJECT_DIR/started"
                echo prebuild-stdout
                echo prebuild-stderr >&2
                while [ ! -f "$FUZUKUE_PROJECT_DIR/publish-ready" ]; do sleep 0.02; done
                sleep 0.05
                test -z "${FUZUKUE_SERVER_BINARY+x}"
                touch "$FUZUKUE_WINDOWS_BUILD_STATE"
            ''' ]]
            tests = ['''
                set -e
                while [ ! -f "$FUZUKUE_PROJECT_DIR/started" ]; do sleep 0.02; done
                test ! -f "$FUZUKUE_PROJECT_DIR/registered"
            ''']
            shots = ['touch "$FUZUKUE_PROJECT_DIR/shot"']
            publish = ['''
                set -e
                test "$FUZUKUE_PREBUILT" = 1
                test -f "$FUZUKUE_WINDOWS_BUILD_STATE"
                test -f "$FUZUKUE_PROJECT_DIR/shot"
                test -x "$FUZUKUE_SERVER_BINARY"
                printf 'registered\n' >> "$FUZUKUE_PROJECT_DIR/registered"
            ''']
        "#,
        )?;
        let mut update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let ((candidate, registration), ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(update.pipeline(&source, fake_build()), async {
                update
                    .subscribe()
                    .wait_for(|status| status.steps[6].state == State::Running)
                    .await?;
                fs::write(dir.path().join("publish-ready"), "")?;
                Ok(())
            })
        })
        .await??;
        update.queue_registration(registration.unwrap(), &candidate)?;
        drop(update.lock_builds()?);
        update.exe = candidate.clone();
        supervisor::record_versions(&update.root, &candidate, None)?;
        let distribution = crate::distribution::Distribution::new(dir.path());
        let mut notices = distribution.subscribe();
        let registered = dir.path().join("registered");
        // Even when the current link points to a trial, registration waits for acceptance.
        update.finish_registration(&distribution).await?;
        assert!(!registered.exists());
        assert!(notices.try_recv().is_err());
        update.status.send_modify(|status| registration::accept(&candidate, status));
        update.finish_registration(&distribution).await?;
        update.finish_registration(&distribution).await?;
        assert_eq!(fs::read_to_string(registered)?, "registered\n");
        assert_eq!(notices.try_recv()?, None);
        assert!(notices.try_recv().is_err());
        let status = update.status.borrow();
        assert_eq!(status.steps[6].state, State::Passed);
        assert!(status.steps[6].log.contains("[prebuild] prebuild-stdout\n"));
        assert!(status.steps[6].log.contains("[prebuild] prebuild-stderr\n"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failing_test_kills_prebuild_group_without_registering() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let config = toml::from_str(
            r#"
            prebuild = ['''
                sleep 60 &
                printf '%s\n' "$!" > "$FUZUKUE_PROJECT_DIR/descendant"
                touch "$FUZUKUE_PROJECT_DIR/started"
                wait
            ''']
            tests = ['''
                while [ ! -f "$FUZUKUE_PROJECT_DIR/started" ]; do sleep 0.02; done
                exit 7
            ''']
            publish = ['touch "$FUZUKUE_PROJECT_DIR/registered"']
        "#,
        )?;
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            update.pipeline(&source, fake_build()),
        )
        .await?
        .unwrap_err();
        update.fail(error.to_string());
        assert!(!dir.path().join("registered").exists());
        {
            let status = update.status.borrow();
            assert_eq!(status.steps[2].state, State::Failed);
            assert_eq!(status.steps[6].state, State::Pending);
        }
        let pid = fs::read_to_string(dir.path().join("descendant"))?;
        let stat = format!("/proc/{}/stat", pid.trim());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fs::read_to_string(&stat).ok().and_then(|stat| {
                stat.rsplit_once(") ")
                    .map(|(_, rest)| rest.starts_with("Z "))
            }) == Some(false)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_registration_retries_from_the_saved_command_on_startup() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let config = toml::from_str(r#"
            tests = []
            publish = [
                'printf "first\n" >> "$FUZUKUE_PROJECT_DIR/registered"',
                'test -f "$FUZUKUE_PROJECT_DIR/retry" && printf "second\n" >> "$FUZUKUE_PROJECT_DIR/registered"',
            ]
        "#)?;
        let mut update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let (candidate, registration) = update.pipeline(&source, fake_build()).await?;
        update.queue_registration(registration.unwrap(), &candidate)?;
        drop(update.lock_builds()?);
        supervisor::record_versions(&update.root, &candidate, None)?;
        update.exe = candidate.clone();
        update.status.send_modify(|status| registration::accept(&candidate, status));
        let distribution = crate::distribution::Distribution::new(dir.path());
        let mut notices = distribution.subscribe();
        let error = update.finish_registration(&distribution).await.unwrap_err();
        assert!(error.to_string().contains("Server swap succeeded, but client registration failed"));
        let saved = read_status(&update.root);
        assert_eq!(saved.steps[5].state, State::Passed);
        assert_eq!(saved.steps[6].state, State::Failed);
        assert!(!saved.active && !saved.recovered);
        assert!(notices.try_recv().is_err());

        fs::write(dir.path().join("retry"), "")?;
        let restarted = Update::with_paths(
            Config::default(), None, update.project, update.root, candidate,
        );
        restarted.finish_registration(&distribution).await?;
        assert_eq!(fs::read_to_string(dir.path().join("registered"))?, "first\nsecond\n");
        assert_eq!(notices.try_recv()?, None);
        assert!(restarted.status.borrow().error.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn failed_prebuild_fails_publish_after_server_steps() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let config = toml::from_str(
            r#"
            prebuild = ["echo windows-build-failed >&2; exit 9"]
            tests = ["true"]
            shots = ["true"]
            publish = ['touch "$FUZUKUE_PROJECT_DIR/registered"']
        "#,
        )?;
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let error = update.pipeline(&source, fake_build()).await.unwrap_err();
        update.fail(error.to_string());
        let status = update.status.borrow();
        assert_eq!(status.steps[2].state, State::Passed);
        assert_eq!(status.steps[3].state, State::Passed);
        assert_eq!(status.steps[6].state, State::Failed);
        assert!(
            status.steps[6]
                .log
                .contains("[prebuild] windows-build-failed")
        );
        assert!(!dir.path().join("registered").exists());
        Ok(())
    }

    async fn retry_fixture(pass_on_retry: bool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let cargo = dir.path().join("cargo");
        fs::write(
            &cargo,
            r#"#!/bin/sh
                set -eu
                test "${FUZUKUE_SERVICE+x}" != x
                calls="$FUZUKUE_PROJECT_DIR/test-calls"
                printf '%s\n' "$*" >> "$calls"
                if [ "$*" = 'test -p fixture -- --exact graph::clock' ] &&
                   [ -f "$FUZUKUE_PROJECT_DIR/pass-on-retry" ]; then
                    printf 'test graph::clock ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n'
                    exit 0
                fi
                printf 'test graph::clock ... FAILED\n\nfailures:\n    graph::clock\n\n'
                printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored\n'
                printf 'error: test failed, to rerun pass `--lib`\n' >&2
                exit 101
            "#,
        )?;
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755))?;
        if pass_on_retry {
            fs::write(dir.path().join("pass-on-retry"), "")?;
        }
        let config = Config {
            tests: vec![CommandLine::Argv(vec![
                cargo.to_str().context("non-UTF-8 fixture path")?.into(),
                "test".into(),
                "-p".into(),
                "fixture".into(),
            ])],
            publish: vec![CommandLine::Shell(
                r#"touch "$FUZUKUE_PROJECT_DIR/registered""#.into(),
            )],
            ..Config::default()
        };
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let result = update.pipeline(&source, fake_build()).await;
        if pass_on_retry {
            result?;
        } else {
            let error = result.unwrap_err().to_string();
            assert_eq!(error, "Test retry failed: graph::clock");
            update.fail(error);
        }
        let status = update.status.borrow();
        assert_eq!(
            status.steps[2].state,
            if pass_on_retry {
                State::Passed
            } else {
                State::Failed
            }
        );
        assert!(status.steps[2].log.contains("retried: graph::clock"));
        assert_eq!(
            fs::read_to_string(dir.path().join("test-calls"))?,
            "test -p fixture\ntest -p fixture -- --exact graph::clock\n",
        );
        assert!(!dir.path().join("registered").exists());
        Ok(())
    }

    #[tokio::test]
    async fn failing_test_is_retried_once_then_passes() -> Result<()> {
        retry_fixture(true).await
    }

    #[tokio::test]
    async fn failing_test_is_retried_once_then_stops_the_update() -> Result<()> {
        retry_fixture(false).await
    }

    #[tokio::test]
    async fn source_check_ignores_workspace_data_and_rejects_other_changes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        fs::create_dir(source.join("threads"))?;
        fs::write(source.join("threads/x.md"), "conversation")?;
        fs::create_dir(source.join("inbox"))?;
        fs::write(source.join("inbox/y.png"), b"attachment")?;
        let config = toml::from_str("tests = []")?;
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        update.pipeline(&source, fake_build()).await?;

        let untracked = source.join("crates/secretaire/new.rs");
        fs::write(&untracked, "// new source file")?;
        let error = update.pipeline(&source, fake_build()).await.unwrap_err();
        assert_eq!(error.to_string(), "self-update source has local changes");
        fs::remove_file(untracked)?;

        fs::write(source.join("crates/secretaire/Cargo.toml"), "# local edit")?;
        let error = update.pipeline(&source, fake_build()).await.unwrap_err();
        assert_eq!(error.to_string(), "self-update source has local changes");
        Ok(())
    }

    #[tokio::test]
    async fn build_stages_an_immutable_copy_under_the_source_commit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = source(dir.path())?;
        let live = source.join("target/release/secretaire");
        fs::create_dir_all(live.parent().unwrap())?;
        fs::write(&live, "running")?;
        // This fixture represents an ignored normal Cargo target directory.
        fs::write(source.join(".git/info/exclude"), "/target/\n")?;
        let config = toml::from_str(
            r#"tests = ['test -x "$FUZUKUE_SERVER_BINARY" && "$FUZUKUE_SERVER_BINARY"']"#,
        )?;
        let update = fixture(dir.path(), &source, config)?;
        begin(&update);
        let (target, _) = update.pipeline(&source, fake_build()).await?;
        let commit = git(&source, &["rev-parse", "HEAD"]).await?;
        assert!(target.starts_with(update.root.join(commit)));
        assert_eq!(target.file_name().unwrap(), "secretaire");
        assert_eq!(fs::read_to_string(live)?, "running");
        assert!(fs::read_to_string(target)?.contains("echo staged"));
        Ok(())
    }

    #[tokio::test]
    async fn swap_cannot_admit_new_work_or_start_with_an_active_job_or_turn() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        fs::create_dir(&project)?;
        fs::write(
            project.join("fuzukue.toml"),
            r#"
            [tests.fixture]
            command = ["sh", "-c", "while [ ! -f finish ]; do sleep 0.05; done"]
            timeout = 5
        "#,
        )?;
        let app = crate::App::new(
            dir.path().to_owned(),
            project.clone(),
            dir.path().join("state"),
            "127.0.0.1:0".into(),
            None,
        )?;
        let workspace = app.select(None)?;
        let update = Arc::new(fixture(dir.path(), dir.path(), Config::default())?);
        assert!(app.self_update.set(update.clone()).is_ok());
        let mut jobs = workspace.run_tests(None, None).await?;
        assert_eq!(jobs.len(), 1);
        let (_, reason) = update.swap_gate(&app);
        assert!(reason.gate_held);
        assert!(reason.threads.is_empty());
        assert!(matches!(&reason.blockers[..], [blocker]
            if blocker.kind == fuzukue_proto::SelfUpdateBlockerKind::Test
                && blocker.title.as_deref() == Some("fixture") && blocker.started_at > 0));
        fs::write(project.join("finish"), "")?;
        assert_eq!(jobs.remove(0).await?.state, fuzukue_proto::TestState::Passed);
        assert!(app.swap_blockers().blockers.is_empty());
        let session = workspace.hub.edit_threads(|store| {
            store.create_titled(None, Some("Active thread".into()))
        })?;
        workspace
            .hub
            .publish(fuzukue_proto::ServerEvent::TurnActivity {
                session: session.clone(),
                running: true,
            });
        let (_, reason) = update.swap_gate(&app);
        assert!(!reason.gate_held);
        assert_eq!(reason.threads.len(), 1);
        assert_eq!(reason.threads[0].id, session);
        assert_eq!(reason.threads[0].title.as_deref(), Some("Active thread"));
        assert!(reason.threads[0].running);
        assert!(matches!(&reason.blockers[..], [blocker]
            if blocker.kind == fuzukue_proto::SelfUpdateBlockerKind::Turn
                && blocker.title.as_deref() == Some("Active thread")));
        workspace
            .hub
            .publish(fuzukue_proto::ServerEvent::TurnActivity {
                session,
                running: false,
            });
        let (idle, reason) = update.swap_gate(&app);
        assert_eq!(reason, SelfUpdateWaitReason::default());
        let idle = idle.context("server did not become idle")?;
        assert!(update.admit().is_err());
        drop(idle);
        Ok(())
    }

    pub(super) fn waiting_fixture(dir: &Path) -> Result<(Arc<crate::App>, Update)> {
        let project = dir.join("project");
        fs::create_dir(&project)?;
        let app = crate::App::new(
            dir.to_owned(), project, dir.join("state"), "127.0.0.1:0".into(), None,
        )?;
        let update = fixture(dir, dir, Config::default())?;
        update.status.send_modify(|status| {
            status.active = true;
            status.steps = steps(false);
        });
        Ok((app, update))
    }

    #[tokio::test]
    async fn wait_persists_changed_reasons_and_can_be_cancelled() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, update) = waiting_fixture(dir.path())?;
        let _gate = update.admit()?;
        assert!(update.control_wait(0, WaitAction::Cancel).is_err());
        let mut statuses = update.subscribe();
        let id = update.begin_wait(DrainReason::Update, false)?.unwrap();
        let wait = WaitGuard { control: &update.wait_control, id, reason: DrainReason::Update };
        let action = async {
            let started_at = statuses.wait_for(|status| status.wait.is_some()).await?
                .wait.as_ref().unwrap().started_at;
            let saved = fs::read(update.root.join(STATUS_FILE))?;
            assert!(read_status(&update.root).wait.unwrap().reason.gate_held);
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(!statuses.has_changed()?);
            assert_eq!(fs::read(update.root.join(STATUS_FILE))?, saved);
            assert!(update.control_wait(started_at - 1, WaitAction::Cancel).is_err());
            update.control_wait(started_at, WaitAction::Cancel)?;
            assert!(update.control_wait(started_at, WaitAction::Force).is_err());
            Ok(())
        };
        let (idle, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(update.wait_for_idle(&app, &wait, None), action)
        }).await??;
        assert!(matches!(idle, WaitOutcome::Cancelled));
        let saved = read_status(&update.root);
        assert!(saved.cancelled);
        assert!(!saved.active);
        assert!(saved.wait.is_none());
        assert!(saved.error.is_none());
        assert!(saved.steps.iter().all(|step| step.state == State::Skipped));
        assert!(!*update.swapping.borrow());
        Ok(())
    }

    #[tokio::test]
    async fn force_interrupts_the_turn_and_preserves_its_queue_before_swap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, update) = waiting_fixture(dir.path())?;
        let workspace = app.select(None)?;
        let update = Arc::new(update);
        assert!(app.self_update.set(update.clone()).is_ok());
        fs::write(workspace.project_dir.join("fuzukue.toml"), r#"
            [tests.fixture]
            command = ["sleep", "60"]
            timeout = 60
        "#)?;
        let job = workspace.run_tests(None, None).await?.remove(0);
        let mut command = Command::new("sh");
        command.arg("-c").arg(r#"
IFS= read -r prompt || exit 1
IFS= read -r request || exit 2
case "$request" in *'"subtype":"interrupt"'*) touch interrupted ;; *) exit 3 ;; esac
printf '%s\n' '{"type":"result","is_error":false}'
IFS= read -r prompt && touch queued-ran
"#);
        let session = "thread-active";
        let claude = crate::ClaudeSession::spawn_command(
            command, session.into(), workspace.project_dir.clone(), &workspace, Box::new(|_| {}),
        )?;
        workspace.sessions.lock().await.insert(session.into(), claude.clone());
        claude.prompt("running").await?;
        claude.prompt("queued").await?;

        let mut statuses = update.subscribe();
        let id = update.begin_wait(DrainReason::Update, false)?.unwrap();
        let wait = WaitGuard { control: &update.wait_control, id, reason: DrainReason::Update };
        let action = async {
            let started_at = {
                let status = statuses.wait_for(|status| status.wait.is_some()).await?;
                let wait = status.wait.as_ref().unwrap();
                assert!(wait.reason.gate_held);
                assert!(wait.reason.threads[0].running);
                assert!(wait.reason.threads[0].queued);
                wait.started_at
            };
            update.control_wait(started_at, WaitAction::Force)?;
            assert!(update.admit().is_err());
            Ok(())
        };
        let (idle, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(update.wait_for_idle(&app, &wait, None), action)
        }).await??;
        let WaitOutcome::Ready(_idle) = idle else { anyhow::bail!("forced swap did not reach the swap gate") };
        assert!(workspace.project_dir.join("interrupted").exists());
        assert!(!workspace.project_dir.join("queued-ran").exists());
        assert_eq!(claude.swap_activity(), Some((false, true, false)));
        let held = workspace.drain.records(&workspace.state_dir)?;
        assert!(held.iter().any(|record| !record.claimed && matches!(record.work, drain::Work::Claude(_))));
        assert!(held.iter().any(|record| record.claimed && record.review.is_some()));
        assert!(update.admit().is_err());
        assert_eq!(job.await?.state, fuzukue_proto::TestState::Cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn restart_restores_the_thread_list_and_session_record() -> Result<()> {
        use fuzukue_proto::{Actor, DelegateReport, ServerEvent};

        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        fs::create_dir(&project)?;
        fs::write(
            project.join("fuzukue.toml"),
            r#"
            [tests.fixture]
            command = ["sh", "-c", "echo saved-test-output"]
        "#,
        )?;
        let start = || {
            crate::App::new(
                dir.path().to_owned(),
                project.clone(),
                dir.path().join("state"),
                "127.0.0.1:0".into(),
                None,
            )
        };
        let job = "delegate:claimed-fixture";
        let (id, cursor) = {
            let app = start()?;
            let workspace = app.select(None)?;
            let id = workspace
                .hub
                .edit_threads(|store| store.create_titled(None, Some("Before restart".into())))?;
            workspace.record_session(&id, |record| record.last_partner_codex = true);
            let mut jobs = workspace.run_tests(None, None).await?;
            assert_eq!(jobs.remove(0).await?.state, fuzukue_proto::TestState::Passed);
            workspace.hub.publish(ServerEvent::TurnActivity { session: id.clone(), running: true });
            let assignment = serde_json::from_value(serde_json::json!({
                "prompt": "Interrupted fixture", "caller": "front_desk", "routing": {},
            }))?;
            workspace.drain.enqueue(&workspace.state_dir, &id, job.into(), drain::Work::Delegate(assignment))?;
            assert!(workspace.drain.claim(&workspace.state_dir, &id, job)?);
            workspace.hub.publish(ServerEvent::ToolCall {
                session: id.clone(), item: job.into(), actor: Actor::System, name: "delegate".into(),
                input: serde_json::json!({"model": "fixture", "started_at_ms": chrono::Utc::now().timestamp_millis()}),
            });
            let (initial, _) = workspace.hub.subscribe_client(0, true);
            assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::ChatActivity { turns, workers }
                if turns.contains(&id) && workers[&id].contains(job))));
            let cursor = initial.iter().map(|event| event.seq).max().unwrap();
            assert!(cursor > 0);
            (id, cursor)
        };
        let app = start()?;
        let workspace = app.select(None)?;
        let (events, _) = workspace.hub.subscribe_client(0, true);
        assert!(events.iter().any(|event| matches!(&event.event,
            fuzukue_proto::ServerEvent::Threads { threads }
            if threads.iter().any(|thread| thread.id == id && thread.title == "Before restart")
        )));
        assert!(workspace.session_records.lock().unwrap()[&id].last_partner_codex);
        assert!(workspace
            .test_snapshots()
            .iter()
            .any(|event| matches!(event,
                fuzukue_proto::ServerEvent::TestRun { run } if run.output.contains("saved-test-output")
            )));
        let (resumed, _) = workspace.hub.subscribe_client(cursor, true);
        assert!(resumed.iter().any(|env| matches!(&env.event,
            ServerEvent::ToolResult { session, item, ok: false, output }
            if session == &id && item == job && serde_json::from_str::<DelegateReport>(output).unwrap().interrupted
                && env.seq > cursor)));
        assert!(resumed.iter().any(|env| matches!(&env.event, ServerEvent::ChatActivity { turns, workers }
            if turns.is_empty() && workers.is_empty())));
        assert!(events.iter().any(|env| matches!(&env.event,
            ServerEvent::ToolResult { session, item, output, .. }
            if session == &id && item == job && serde_json::from_str::<DelegateReport>(output).unwrap().interrupted)));
        Ok(())
    }

    #[tokio::test]
    async fn resolved_standalone_git_approval_leaves_idle_thread_inactive_at_swap() -> Result<()> {
        use fuzukue_proto::{Actor, ServerEvent};

        let dir = tempfile::tempdir()?;
        let (app, update) = waiting_fixture(dir.path())?;
        let workspace = app.select(None)?;
        let id = workspace.hub.edit_threads(|store| store.create(None))?;
        let (_, mut live) = workspace.hub.subscribe(u64::MAX);
        let (owner, session) = (workspace.clone(), id.clone());
        let pending = tokio::spawn(async move {
            crate::delegate::request_approval(&owner, &session, fuzukue_proto::GIT_APPROVAL,
                Actor::System, String::from("Fixture Git operation")).await
        });
        let ServerEvent::ApprovalRequested { request_id, .. } = tokio::time::timeout(Duration::from_secs(5), live.recv()).await??.event
            else { anyhow::bail!("expected standalone Git approval") };
        crate::resolve_approval(&workspace, request_id, "accept".into(), false).await?;
        assert_eq!(pending.await??, "accept");
        assert!(!workspace.hub.thread_active(&id));
        assert!(update.swap_gate(&app).0.is_some());
        let (full, _) = workspace.hub.subscribe_client(0, true);
        assert!(!full.iter().any(|env| matches!(&env.event, ServerEvent::TurnActivity { session, running: true } if session == &id)));
        assert!(full.iter().any(|env| matches!(&env.event, ServerEvent::ChatActivity { turns, .. } if turns.is_empty())));
        Ok(())
    }
}
