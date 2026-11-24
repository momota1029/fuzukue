//! The Linux launcher gives each recipe a new home, PID and network namespace.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use rustix::fs::{self as fd, Mode, OFlags, ResolveFlags};
use rustix::process::{kill_process_group, Pid, Signal};
use tokio::process::Command;

use super::{config, Broker, Cancellation, Pending, Record, State};

const TAIL_BYTES: u64 = 8 * 1024;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) async fn run(
    broker: &Broker,
    pending: &Pending,
    record: &mut Record,
    cancel: &Cancellation,
) -> Result<()> {
    ensure!(
        !broker
            .private
            .iter()
            .any(|path| config::inside(&pending.root, path) || config::inside(path, &pending.root)),
        "workspace overlaps server configuration or state"
    );
    let root = File::from(fd::open(&pending.root, DIRECTORY, Mode::empty())?);
    let (path, mut log) = open_log(&pending.root, &root, &record.request_id)?;
    record.log_path = Some(path);
    broker.state.lock().unwrap().update(record.clone())?;
    let result = launch(broker, pending, &root, &log, cancel).await;
    let log_tail = tail(&mut log);
    let (state, status, reason) = match result {
        Ok(Exit::Completed(status)) if status.code() == Some(77) => (
            State::Skipped,
            Some(status),
            Some("command reported a skipped run (exit 77)".into()),
        ),
        Ok(Exit::Completed(status)) => (State::Exited, Some(status), None),
        Ok(Exit::SetupFailed(status)) => (
            State::Refused,
            Some(status),
            Some("bubblewrap could not start the isolated command (see the log)".into()),
        ),
        Ok(Exit::Cancelled(status)) => (
            State::Cancelled,
            Some(status),
            Some("cancelled during execution".into()),
        ),
        Ok(Exit::Timeout(status)) => (
            State::TimedOut,
            Some(status),
            Some(format!("exceeded {} seconds", pending.recipe.timeout_secs)),
        ),
        Ok(Exit::BeforeStart) => (
            State::Cancelled,
            None,
            Some("cancelled before the command started".into()),
        ),
        Err(error) => (State::Refused, None, Some(format!("{error:#}"))),
    };
    record.exit_code = status.and_then(|status| status.code());
    record.signal = status.and_then(|status| status.signal());
    record.finish(state, reason);
    match log_tail {
        Ok(tail) => record.log_tail = tail,
        Err(error) => record.note(&format!("could not read the recipe log tail: {error}")),
    }
    Ok(())
}

fn open_log(root: &Path, base: &File, id: &str) -> Result<(PathBuf, File)> {
    // Open each component relative to its parent. A worker-controlled symlink cannot
    // redirect a broker write, even if it changes the output directory concurrently.
    let target = make_dir(base, "target")?;
    let logs = make_dir(&target, "recipe-logs")?;
    let name = format!("{id}.log");
    let file = fd::openat(
        &logs,
        &name,
        OFlags::RDWR
            | OFlags::APPEND
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    Ok((root.join("target/recipe-logs").join(name), File::from(file)))
}

fn make_dir(parent: &File, name: &str) -> Result<File> {
    match fd::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => {
            return Err(error).with_context(|| format!("create recipe log directory {name}"))
        }
    }
    let dir = fd::openat(parent, name, DIRECTORY, Mode::empty())
        .with_context(|| format!("open recipe log directory {name} (symlinks are forbidden)"))?;
    Ok(File::from(dir))
}

fn tail(file: &mut File) -> Result<String> {
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    // Lossy decoding can expand invalid bytes.
    let start = (text.len().saturating_sub(TAIL_BYTES as usize)..)
        .find(|&index| text.is_char_boundary(index))
        .unwrap();
    text.drain(..start);
    Ok(text)
}

enum Exit {
    Completed(ExitStatus),
    SetupFailed(ExitStatus),
    Cancelled(ExitStatus),
    Timeout(ExitStatus),
    BeforeStart,
}

async fn launch(
    broker: &Broker,
    pending: &Pending,
    root: &File,
    log: &File,
    cancel: &Cancellation,
) -> Result<Exit> {
    if cancel.check().is_err() {
        return Ok(Exit::BeforeStart);
    }
    ensure!(
        broker.paths.bwrap.is_some() || broker.config.recipe_runner.allow_unisolated,
        "no bubblewrap isolation tool found; [recipe_runner].allow_unisolated is false"
    );

    let cwd = File::from(
        fd::openat2(
            root,
            &pending.recipe.cwd,
            DIRECTORY,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .context("recipe cwd must be a real directory inside the calling workspace")?,
    );
    let private = tempfile::Builder::new()
        .prefix("recipe-runtime-")
        .tempdir_in(&broker.runtime)?;
    for dir in ["home", "tmp", "cargo", "gradle", "rustup"] {
        fs::create_dir(private.path().join(dir))?;
    }
    let (mut command, sandbox_status);
    let mut passed_fds = Vec::new();
    if let Some(bwrap) = &broker.paths.bwrap {
        let status = tempfile::tempfile_in(&broker.runtime)?;
        command = sandbox(
            bwrap,
            root,
            &status,
            pending,
            broker.paths.host_home.as_deref(),
            private.path(),
        );
        passed_fds.extend([root.as_raw_fd(), status.as_raw_fd()]);
        sandbox_status = Some(status);
    } else {
        command = Command::new(&pending.argv[0]);
        command.args(&pending.argv[1..]).env_clear();
        let cargo_bin = broker
            .paths
            .host_home
            .as_ref()
            .map(|home| home.join(".cargo/bin"));
        let rustup = broker
            .paths
            .host_home
            .as_ref()
            .map(|home| home.join(".rustup"));
        command.envs(environment(
            private.path(),
            cargo_bin.as_deref(),
            rustup.as_deref(),
            pending,
        ));
        command.current_dir(format!("/proc/self/fd/{}", cwd.as_raw_fd()));
        sandbox_status = None;
    }

    let memory_bytes =
        libc::rlim_t::try_from(u64::from(pending.recipe.max_memory_mib) * 1024 * 1024)
            .context("recipe memory limit exceeds the platform limit")?;
    limits(&mut command, memory_bytes, passed_fds);
    let mut child = command
        .process_group(0)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .spawn()
        .context("could not launch recipe command")?;
    let group = ProcessGroup(
        child
            .id()
            .and_then(|pid| Pid::from_raw(pid as i32))
            .context("missing recipe PID")?,
    );
    let timeout = Duration::from_secs(u64::from(pending.recipe.timeout_secs));
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(timeout, child.wait()) => Some(result),
    };
    // Stop lingering descendants even after the launcher exits. In the sandbox, killing
    // the bubblewrap monitor also tears down its PID namespace.
    drop(group);
    match result {
        None => Ok(Exit::Cancelled(child.wait().await?)),
        Some(Err(_)) => Ok(Exit::Timeout(child.wait().await?)),
        Some(Ok(status)) => {
            let status = status?;
            if let Some(mut report) = sandbox_status {
                report.rewind()?;
                let mut lines = BufReader::new(report).lines();
                let started = lines.any(|line| {
                    line.ok()
                        .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
                        .is_some_and(|record| record.get("exit-code").is_some())
                });
                if !started {
                    return Ok(Exit::SetupFailed(status));
                }
            }
            Ok(Exit::Completed(status))
        }
    }
}

struct ProcessGroup(Pid);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = kill_process_group(self.0, Signal::KILL);
    }
}

fn limits(command: &mut Command, memory_bytes: libc::rlim_t, passed_fds: Vec<RawFd>) {
    let parent = std::process::id() as libc::pid_t;
    // Only async-signal-safe syscalls are used between fork and exec.
    unsafe {
        command.pre_exec(move || {
            cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0))?;
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            cvt(libc::getrlimit(libc::RLIMIT_AS, &mut limit))?;
            let limit = memory_bytes.min(limit.rlim_max);
            cvt(libc::setrlimit(
                libc::RLIMIT_AS,
                &libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                },
            ))?;
            for fd in &passed_fds {
                cvt(libc::fcntl(*fd, libc::F_SETFD, 0))?;
            }
            Ok(())
        });
    }
}

fn cvt(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sandbox(
    bwrap: &Path,
    root: &File,
    status: &File,
    pending: &Pending,
    host_home: Option<&Path>,
    private: &Path,
) -> Command {
    let mut command = Command::new(bwrap);
    command
        .env_clear()
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--dir",
            "/etc",
            "--dir",
            "/run",
            "--tmpfs",
            "/dev/shm",
            "--dir",
            "/recipe",
            "--dir",
            "/workspace",
            "--json-status-fd",
        ])
        .arg(status.as_raw_fd().to_string())
        .args(["--bind-fd", &root.as_raw_fd().to_string(), "/workspace"])
        .arg("--bind")
        .arg(private)
        .arg("/recipe/runtime")
        .arg("--bind")
        .arg(private.join("tmp"))
        .arg("/tmp");

    for path in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/etc/alternatives",
        "/etc/fonts",
        "/etc/ssl/certs",
    ] {
        command.args(["--ro-bind-try", path, path]);
    }
    // Expose only the toolchain and dependency caches, never Cargo/Gradle credentials
    // or the whole host home. Writable state is created fresh for each invocation.
    if let Some(home) = host_home {
        for (source, destination) in [
            (".cargo/bin", "/recipe/cargo-bin"),
            (".cargo/registry", "/recipe/runtime/cargo/registry"),
            (".cargo/git", "/recipe/runtime/cargo/git"),
            (".rustup/toolchains", "/recipe/runtime/rustup/toolchains"),
            (
                ".rustup/settings.toml",
                "/recipe/runtime/rustup/settings.toml",
            ),
            (".gradle/caches/modules-2", "/recipe/gradle-deps/modules-2"),
            (".gradle/wrapper/dists", "/recipe/gradle-dists"),
        ] {
            let path = home.join(source);
            // Do not mount a cache path redirected to another part of the host.
            if path.canonicalize().is_ok_and(|resolved| resolved == path) {
                command.arg("--ro-bind").arg(path).arg(destination);
            }
        }
    }
    for (name, value) in environment(
        Path::new("/recipe/runtime"),
        Some(Path::new("/recipe/cargo-bin")),
        None,
        pending,
    ) {
        command.arg("--setenv").arg(name).arg(value);
    }
    command
        .arg("--chdir")
        .arg(Path::new("/workspace").join(&pending.recipe.cwd))
        .arg("--")
        .args(&pending.argv);
    command
}

fn environment(
    private: &Path,
    cargo_bin: Option<&Path>,
    rustup: Option<&Path>,
    pending: &Pending,
) -> BTreeMap<String, PathBuf> {
    let path = cargo_bin.map_or_else(
        || "/usr/bin:/bin".to_owned(),
        |bin| format!("{}:/usr/bin:/bin", bin.display()),
    );
    let mut env = BTreeMap::from([
        ("HOME".into(), private.join("home")),
        ("TMPDIR".into(), private.join("tmp")),
        ("TMP".into(), private.join("tmp")),
        ("TEMP".into(), private.join("tmp")),
        ("CARGO_HOME".into(), private.join("cargo")),
        (
            "RUSTUP_HOME".into(),
            rustup.map_or_else(|| private.join("rustup"), Path::to_path_buf),
        ),
        ("GRADLE_USER_HOME".into(), private.join("gradle")),
        (
            "GRADLE_RO_DEP_CACHE".into(),
            PathBuf::from("/recipe/gradle-deps"),
        ),
        ("PATH".into(), path.into()),
        ("LANG".into(), "C.UTF-8".into()),
    ]);
    env.extend(
        pending
            .recipe
            .env
            .iter()
            .map(|(name, value)| (name.clone(), PathBuf::from(value))),
    );
    env
}
