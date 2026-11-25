use super::{parse_args, result_spill};
use crate::{Workspace, cancellation::Cancellation};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fuzukue_proto::{ApprovalOwner, ApprovalRisk, FullApprovalMode};
use rustix::process::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::future::{Future, pending, ready};
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;

pub(crate) const NAME: &str = "exec";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
const INLINE_STREAM_BYTES: usize = 1024;
const FULL_REQUIRED: &str = "exec requires active workspace full approval. It is off or expired.";
pub(super) const TRUST_REQUIRED: &str = "exec requires a user-initiated turn or its trusted worker assignment. It is unavailable during automatic or external-input turns.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    cwd: Option<PathBuf>,
    timeout_ms: Option<u64>,
}

pub(super) fn definition() -> Value {
    json!({
        "name": NAME,
        "description": harness_text!(
            "Run a Bash command with closed stdin in a user-initiated turn or its trusted worker assignment while workspace full approval is active. Long stdout and stderr include a workspace-relative path and bounded head/tail. The command runs in the host environment with the server's OS permissions.",
            "完全承認モード中に、利用者が開始したターンまたはそのターンから依頼された作業役で Bash を実行する。標準入力は閉じる。長い標準出力と標準エラーには先頭・末尾と、ワークスペース相対の保存先を付ける。コマンドはサーバーと同じ OS ユーザーの権限で動く。"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": {"type": "string", "maxLength": MAX_COMMAND_BYTES},
                "cwd": {"type": "string", "description": harness_text!(
                    "Existing directory relative to the workspace (required at the front desk) or assigned checkout (defaults to its root for workers). Use . for the root.",
                    "受付ではワークスペースからの相対パスを指定する。作業役ではチェックアウトからの相対パスで、既定はそのルート。ルートは . で指定できる。"
                )},
                "timeout_ms": {"type": "integer", "default": DEFAULT_TIMEOUT_MS, "minimum": 1, "maximum": MAX_TIMEOUT_MS}
            },
            "required": ["command"],
            "additionalProperties": false
        }
    })
}

type Lease = (FullApprovalMode, watch::Receiver<()>);

pub(super) fn lease(app: &Workspace, session: &str) -> Result<Lease> {
    ensure!(!crate::canary(), "{FULL_REQUIRED}");
    let lease = app.hub.full_approval_lease()?.context(FULL_REQUIRED)?;
    ensure!(trusted_session(app, session), "{TRUST_REQUIRED}");
    Ok(lease)
}

pub(super) fn trusted_session(app: &Workspace, session: &str) -> bool {
    !app.issue_threads.intake_turn(session) && app.issue_threads.automatic_policy(session).is_none()
}

pub(crate) struct Turn {
    pub(crate) cancel: Arc<Cancellation>,
    pub(crate) id: String,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            cancel: Arc::default(),
            id: crate::mail::session_key(),
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct Worker {
    pub(crate) cwd: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) backend: fuzukue_proto::AgentBackend,
    pub(crate) cancel: Arc<Cancellation>,
    pub(crate) job_cancel: Arc<Cancellation>,
    pub(crate) attempt_cancel: Arc<Cancellation>,
    pub(crate) turn: String,
}

#[derive(Default)]
struct Processes {
    stopping: bool,
    jobs: HashMap<Pid, crate::job_process::Job>,
}

static PROCESSES: std::sync::LazyLock<Mutex<Processes>> = std::sync::LazyLock::new(Mutex::default);

pub(crate) fn shutdown() {
    let mut processes = PROCESSES.lock().unwrap();
    processes.stopping = true;
    for job in processes.jobs.values() {
        let _ = job.stop();
    }
}

struct Group(Pid);

impl Group {
    fn spawn(mut command: Command) -> Result<(tokio::process::Child, Self)> {
        let mut processes = PROCESSES.lock().unwrap();
        ensure!(!processes.stopping, "The server is stopping");
        let (child, job) = crate::job_process::Job::spawn(&mut command, "exec", None)
            .context("Could not start Bash")?;
        let pid = child
            .id()
            .and_then(|pid| Pid::from_raw(pid as i32))
            .context("Bash has no process ID")?;
        processes.jobs.insert(pid, job);
        Ok((child, Self(pid)))
    }

    fn kill(&self) {
        if let Some(job) = PROCESSES.lock().unwrap().jobs.get(&self.0) {
            let _ = job.stop();
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let job = PROCESSES.lock().unwrap().jobs.remove(&self.0);
        drop(job);
    }
}

fn directory(root: &Path, cwd: Option<&Path>) -> Result<File> {
    let cwd = cwd.unwrap_or_else(|| Path::new("."));
    ensure!(
        cwd.components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "exec cwd must be relative to the assigned root and contain no parent traversal"
    );
    let root = root.canonicalize()?;
    let cwd = root.join(cwd).canonicalize()?;
    ensure!(
        cwd.starts_with(&root),
        "exec cwd is outside the assigned root"
    );
    let dir = super::workspace_read::open_exec_directory(&cwd)?;
    ensure!(dir.metadata()?.is_dir(), "exec cwd must be a directory");
    Ok(dir)
}

fn command(args: &str, cwd: File, scratch: &Path) -> Command {
    let mut command = Command::new("/bin/bash");
    command
        .args(["--noprofile", "--norc", "-c", args])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Pass toolchain discovery and locale settings explicitly. Service tokens, shell
    // startup hooks, proxy settings, and credential sockets are absent.
    for name in [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(["TMPDIR", "TMP", "TEMP"].map(|name| (name, scratch)));
    // Hold the checked directory through spawn to avoid re-resolving an exchanged pathname.
    // fchdir is async-signal-safe.
    unsafe {
        command.pre_exec(move || rustix::process::fchdir(&cwd).map_err(Into::into));
    }
    command
}

struct Stream {
    bytes: Vec<u8>,
    total: u64,
}

impl Stream {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            total: 0,
        }
    }

    async fn drain(&mut self, mut pipe: impl AsyncRead + Unpin) -> std::io::Result<()> {
        let mut buffer = [0; 8192];
        loop {
            let count = pipe.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            self.total = self.total.saturating_add(count as u64);
            let keep = count.min(MAX_STREAM_BYTES - self.bytes.len());
            self.bytes.extend_from_slice(&buffer[..keep]);
            if keep < count {
                return Err(ErrorKind::FileTooLarge.into());
            }
        }
    }

    fn report(self, project: &Path, owner: &result_spill::Owner) -> Value {
        let text = String::from_utf8_lossy(&self.bytes);
        let replaced_invalid_utf8 = matches!(text, std::borrow::Cow::Owned(_));
        let binary = replaced_invalid_utf8 || self.bytes.contains(&0);
        let truncated =
            binary || self.total > INLINE_STREAM_BYTES as u64 || text.len() > INLINE_STREAM_BYTES;
        let mut report = json!({
            "bytes": self.total, "retained_bytes": self.bytes.len(), "truncated": truncated,
            "storage_truncated": self.total > self.bytes.len() as u64, "invalid_utf8_replaced": replaced_invalid_utf8
        });
        if truncated {
            let mut encoded = String::new();
            let bytes = if binary {
                // Wrap on complete base64 quanta so fs_read can paginate binary results.
                for chunk in self.bytes.chunks(57) {
                    STANDARD.encode_string(chunk, &mut encoded);
                    encoded.push('\n');
                }
                encoded.as_bytes()
            } else {
                text.as_bytes()
            };
            match result_spill::save(project, owner, "txt", bytes) {
                Ok(path) => {
                    report["path"] = json!(path);
                    report["encoding"] = json!(if binary { "base64" } else { "utf-8" });
                    report["hint"] = json!(if binary {
                        "Read this workspace-relative path with fs_read and decode its base64 text."
                    } else {
                        "Read this workspace-relative path with fs_read or fs_search. Use jq when it contains JSON or JSON Lines."
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "could not store exec output");
                    report["spill_error"] =
                        json!("Output storage was unavailable; only a preview is returned.");
                }
            }
            report["head"] = json!(result_spill::head(&text, INLINE_STREAM_BYTES / 2));
            report["tail"] = json!(
                &text
                    [text.ceil_char_boundary(text.len().saturating_sub(INLINE_STREAM_BYTES / 2))..]
            );
        } else {
            report["text"] = json!(text);
        }
        report
    }
}

struct Output {
    status: &'static str,
    exit: Option<ExitStatus>,
    stdout: Stream,
    stderr: Stream,
}

async fn execute(
    cmd: Command,
    timeout: Duration,
    cancelled: impl Future<Output = ()>,
    lease: Option<Lease>,
) -> Result<Output> {
    let approval_ended = crate::full_approval::wait_for_end(lease);
    tokio::pin!(cancelled, approval_ended);
    tokio::select! {
        biased;
        _ = &mut cancelled => return Err(crate::cancellation::Cancelled.into()),
        _ = &mut approval_ended => bail!("{FULL_REQUIRED}"),
        _ = ready(()) => {}
    }
    let (mut child, group) = Group::spawn(cmd)?;
    let stdout = child.stdout.take().context("Missing Bash stdout")?;
    let stderr = child.stderr.take().context("Missing Bash stderr")?;
    let mut out = Stream::new();
    let mut err = Stream::new();
    let (status, mut completed) = {
        let streams = async { tokio::try_join!(out.drain(stdout), err.drain(stderr)) };
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(streams, deadline);
        let mut completed = None;
        let mut drained = false;
        let status = {
            let wait = child.wait();
            tokio::pin!(wait);
            loop {
                if completed.is_some() && drained {
                    break "exited";
                }
                tokio::select! {
                    biased;
                    _ = &mut cancelled => break "cancelled",
                    _ = &mut approval_ended => break "approval_ended",
                    _ = &mut deadline => break "timed_out",
                    exit = &mut wait, if completed.is_none() => {
                        completed = Some(exit?);
                        // Reap background jobs even when the shell exits successfully.
                        group.kill();
                    }
                    result = &mut streams, if !drained => {
                        drained = true;
                        if let Err(error) = result {
                            if error.kind() == ErrorKind::FileTooLarge {
                                break "output_limit";
                            }
                            return Err(error.into());
                        }
                    }
                }
            }
        };
        if status != "exited" {
            group.kill();
            if !drained {
                // A descendant that started a separate session may still hold a pipe.
                let _ = tokio::time::timeout(Duration::from_millis(250), &mut streams).await;
            }
        }
        (status, completed)
    };
    if completed.is_none() {
        completed = Some(child.wait().await?);
    }
    Ok(Output {
        status,
        exit: completed,
        stdout: out,
        stderr: err,
    })
}

pub(super) struct Caller {
    pub(super) owner: ApprovalOwner,
    pub(super) turn: String,
    pub(super) cancel: Arc<Cancellation>,
    pub(super) worker: Option<Worker>,
}

impl Caller {
    async fn cancelled(&self) {
        let worker_cancelled = async {
            let Some(worker) = &self.worker else {
                return pending().await;
            };
            tokio::select! {
                _ = worker.job_cancel.cancelled() => {}
                _ = worker.attempt_cancel.cancelled() => {}
            }
        };
        tokio::select! {
            _ = self.cancel.cancelled() => {}
            _ = worker_cancelled => {}
        }
    }
}

pub(super) async fn call(
    app: &Workspace,
    session: &str,
    request: &Value,
    args: &Value,
    caller: Caller,
    lease: Lease,
) -> Result<Value> {
    let args: Args = parse_args(args)?;
    ensure!(
        !args.command.trim().is_empty()
            && args.command.len() <= MAX_COMMAND_BYTES
            && !args.command.contains('\0'),
        "exec command must contain 1–16384 bytes of Bash source and no NUL"
    );
    let timeout = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    ensure!(
        (1..=MAX_TIMEOUT_MS).contains(&timeout),
        "exec timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"
    );
    let _work = app.admit_work()?;
    caller.cancel.check()?;
    let project = app.project_dir.canonicalize()?;
    ensure!(
        caller.worker.is_some() || args.cwd.is_some(),
        "The front desk must specify exec cwd relative to the workspace. Use . for its root."
    );
    let root = caller
        .worker
        .as_ref()
        .map_or(project.as_path(), |worker| worker.cwd.as_path());
    let cwd = directory(root, args.cwd.as_deref())?;
    let scratch;
    let scratch_path = match &caller.worker {
        Some(worker) => worker.scratch.as_path(),
        None => {
            std::fs::create_dir_all(&app.state_dir)?;
            scratch = tempfile::Builder::new()
                .prefix("exec-")
                .tempdir_in(&app.state_dir)?;
            scratch.path()
        }
    };
    let scope = json!({"cwd": args.cwd.as_deref().unwrap_or_else(|| Path::new(".")), "timeout_ms": timeout});
    let identity = json!({
        "turn": caller.turn, "request": request,
        "fingerprint": crate::full_approval::fingerprint(&json!([args.command, &scope]))
    });
    let registration = crate::full_approval::registration(
        caller.owner.clone(),
        NAME,
        ApprovalRisk::High,
        identity,
        scope,
    );
    ensure!(!lease.1.has_changed().unwrap_or(true), "{FULL_REQUIRED}");
    if !crate::full_approval::authorize(&app.hub, registration)? {
        bail!("{FULL_REQUIRED}");
    }
    let start = Instant::now();
    let output = execute(
        command(&args.command, cwd, scratch_path),
        Duration::from_millis(timeout),
        caller.cancelled(),
        Some(lease),
    )
    .await?;
    let owner = result_spill::Owner::thread(session);
    let duration_ms = start.elapsed().as_millis() as u64;
    tokio::task::spawn_blocking(move || {
        let status = match (output.status, output.exit) {
            ("exited", Some(exit)) if exit.signal().is_some() => "signalled",
            (status, _) => status,
        };
        Ok(json!({
            "status": status,
            "exit_code": output.exit.and_then(|exit| exit.code()),
            "signal": output.exit.and_then(|exit| exit.signal()),
            "duration_ms": duration_ms,
            "stdout": output.stdout.report(&project, &owner),
            "stderr": output.stderr.report(&project, &owner),
        }))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_timeout_and_cancellation_kill_the_process_group() -> Result<()> {
        let temp = tempfile::tempdir()?;
        for (name, cancelled) in [("timeout", false), ("cancel", true), ("exited", false)] {
            let cancel = Cancellation::default();
            let code = if name == "exited" {
                format!("(sleep 1; printf leaked > {name}) & printf ready")
            } else {
                format!("(sleep 1; printf leaked > {name}) & printf ready; wait")
            };
            let cmd = command(&code, File::open(temp.path())?, temp.path());
            let delay = if cancelled {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(200)
            };
            let run = execute(cmd, delay, cancel.cancelled(), None);
            let stop = async {
                if cancelled {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    cancel.cancel();
                }
            };
            let (result, ()) = tokio::join!(run, stop);
            let output = result?;
            assert_eq!(
                output.status,
                match name {
                    "timeout" => "timed_out",
                    "cancel" => "cancelled",
                    _ => "exited",
                }
            );
            assert_eq!(output.stdout.bytes, b"ready");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn exec_spills_output_and_retains_the_exit_code() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project = temp.path().canonicalize()?;
        let code = "if read -r line; then exit 99; fi; printf 'start%02000dend' 1; printf diagnostic >&2; exit 17";
        let result = execute(
            command(code, File::open(&project)?, &project),
            Duration::from_secs(5),
            pending(),
            None,
        )
        .await?;
        assert_eq!(result.exit.and_then(|exit| exit.code()), Some(17));
        let owner = result_spill::Owner::thread("owner");
        let stdout = result.stdout.report(&project, &owner);
        assert_eq!(stdout["truncated"], true);
        assert_eq!(stdout["storage_truncated"], false);
        assert!(stdout["head"].as_str().unwrap().starts_with("start"));
        assert!(stdout["tail"].as_str().unwrap().ends_with("1end"));
        let saved = std::fs::read(project.join(stdout["path"].as_str().unwrap()))?;
        assert_eq!(saved.len(), 2008);
        assert_eq!(result.stderr.report(&project, &owner)["text"], "diagnostic");
        Ok(())
    }

    #[tokio::test]
    async fn exec_stops_at_the_capture_limit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let result = execute(
            command("yes", File::open(temp.path())?, temp.path()),
            Duration::from_secs(5),
            pending(),
            None,
        )
        .await?;
        assert_eq!(result.status, "output_limit");
        assert_eq!(result.stdout.bytes.len(), MAX_STREAM_BYTES);
        assert!(result.stdout.total > MAX_STREAM_BYTES as u64);
        Ok(())
    }

    #[test]
    fn exec_spill_preserves_binary_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project = temp.path().canonicalize()?;
        let owner = result_spill::Owner::thread("owner");
        let bytes = [b'a', 0, 0xff].repeat(20);
        let result = Stream {
            total: bytes.len() as u64,
            bytes: bytes.clone(),
        }
        .report(&project, &owner);
        assert_eq!(result["encoding"], "base64");
        let saved = std::fs::read_to_string(project.join(result["path"].as_str().unwrap()))?;
        assert_eq!(saved.lines().count(), 2);
        assert_eq!(STANDARD.decode(saved.lines().collect::<String>())?, bytes);
        Ok(())
    }
}
