//! A transient systemd unit owns the restart and verification across cgroups.

use std::fs::{self, File};
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::service::{self, SystemctlRunner};

use super::{
    append_log, fail, is_commit, prune, read_status, registration, supervisor, write_status, State, Status, Step,
};

const PENDING: &str = "service-swap";
const REQUEST: &str = "request.json";
const ADDRESS: &str = "address";
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct HealthCheck {
    timeout: Duration,
    probe: fn(SocketAddr, &str, Option<&str>) -> std::io::Result<bool>,
}

#[derive(Deserialize, Serialize)]
struct Swap {
    current: PathBuf,
    candidate: PathBuf,
    previous: Option<PathBuf>,
    expect: Option<String>,
    old_address: SocketAddr,
    prune: bool,
    rollback: bool,
}

pub(super) fn launch(
    update: &super::Update,
    current: &Path,
    candidate: &Path,
    old_address: SocketAddr,
    rollback: bool,
) -> Result<()> {
    let root = &update.root;
    let candidate = candidate.canonicalize()?;
    let expect = candidate
        .strip_prefix(root.canonicalize()?)
        .ok()
        .and_then(|path| path.iter().next()?.to_str())
        .filter(|id| is_commit(id))
        .map(str::to_owned);
    ensure!(
        rollback || expect.is_some(),
        "missing staged server build ID"
    );
    let swap = Swap {
        current: current.to_owned(),
        candidate,
        previous: fs::canonicalize(root.join("previous")).ok(),
        expect,
        old_address,
        prune: update.config.prune,
        rollback,
    };
    launch_with(
        &service::ProcessRunner,
        &service::unit_path()?,
        root,
        &update.project,
        &swap,
    )
}

fn launch_with(
    runner: &impl SystemctlRunner,
    unit: &Path,
    root: &Path,
    project: &Path,
    swap: &Swap,
) -> Result<()> {
    let pending = root.join(PENDING);
    // The caller owns the builds lock, so any directory left behind is stale.
    match fs::remove_dir_all(&pending) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir(&pending)?;
    let mut cleanup = Pending(pending.clone());
    serde_json::to_writer(File::create(pending.join(REQUEST))?, swap)?;
    let unit_id = format!("--unit=fuzukue-swap-{:016x}", rand::random::<u64>());
    let helper = swap
        .current
        .to_str()
        .context("systemd requires a UTF-8 executable path")?;
    let request = command_argument(&pending)?;
    supervisor::record_versions(root, &swap.candidate, Some(&swap.current))?;
    let launch =
        service::track_current(runner, unit, &root.join("current"), project).and_then(|()| {
            service::checked_output(
                runner,
                "systemd-run",
                &[
                    "--user",
                    "--collect",
                    &unit_id,
                    "--property=Type=exec",
                    "--",
                    &helper,
                    "service",
                    "swap-check",
                    &request,
                ],
            )
        });
    if let Err(error) = launch {
        if let Err(restore) = restore_versions(root, swap) {
            bail!("{error:#}; restore build links: {restore:#}");
        }
        return Err(error);
    }
    // The transient unit now owns cleanup. The builds lock remains held until
    // the server has published its usual swap notice and returned from execute.
    cleanup.disarm();
    Ok(())
}

fn command_argument(path: &Path) -> Result<String> {
    // systemd expands dollars in transient ExecStart arguments, including on
    // versions that predate systemd-run --expand-environment=no.
    Ok(path
        .to_str()
        .context("systemd requires a UTF-8 path")?
        .replace('$', "$$"))
}

struct Pending(PathBuf);

impl Pending {
    fn disarm(&mut self) {
        self.0.clear();
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

pub(super) fn pending(root: &Path) -> bool {
    service::managed() && root.join(PENDING).is_dir() && builds_locked(root)
}

fn builds_locked(root: &Path) -> bool {
    File::open(root.join("lock"))
        .is_ok_and(|lock| matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

pub(super) fn waiting(ready: &Path) -> bool {
    // The lock is released if the transient unit is terminated unexpectedly.
    let root = if ready.ends_with(PENDING) {
        ready.parent()
    } else {
        ready.parent().and_then(Path::parent)
    };
    ready.exists() && root.is_some_and(builds_locked)
}

pub(super) fn ready(root: &Path, addr: SocketAddr) -> Result<Option<PathBuf>> {
    if !pending(root) {
        return Ok(None);
    }
    let pending = root.join(PENDING);
    let mut marker = tempfile::NamedTempFile::new_in(&pending)?;
    use std::io::Write;
    writeln!(marker, "{} {addr}", std::process::id())?;
    crate::durable::persist(marker, pending.join(ADDRESS))?;
    Ok(Some(pending))
}

pub(crate) fn check(pending: &Path) -> Result<()> {
    check_with(
        &service::ProcessRunner,
        pending,
        HealthCheck {
            timeout: TIMEOUT,
            probe: supervisor::probe_build,
        },
    )
}

fn check_with(runner: &impl SystemctlRunner, pending: &Path, check: HealthCheck) -> Result<()> {
    ensure!(
        pending.file_name().is_some_and(|name| name == PENDING),
        "invalid service swap directory"
    );
    let root = pending
        .parent()
        .context("missing server builds directory")?;
    let lock = File::options()
        .read(true)
        .write(true)
        .open(root.join("lock"))?;
    lock.lock()?;
    let swap: Swap = serde_json::from_reader(File::open(pending.join(REQUEST))?)?;
    let _cleanup = Pending(pending.to_owned());
    let mut status = read_status(root);
    let trial = restart(runner, pending).and_then(|()| {
        wait_healthy(
            runner,
            pending,
            &swap.candidate,
            swap.expect.as_deref(),
            swap.rollback,
            swap.old_address,
            check,
        )
    });
    match trial {
        Ok(()) => {
            accept(root, &swap, &mut status);
            registration::accept(&swap.candidate, &mut status);
            write_status(root, &status)
        }
        Err(error) => {
            registration::discard(&swap.candidate, &mut status);
            fail(&mut status, format!("{error:#}"));
            let restored = restore_versions(root, &swap)
                .and_then(|()| restart(runner, pending))
                .and_then(|()| {
                    wait_healthy(
                        runner,
                        pending,
                        &swap.current,
                        Some(&status.build_id),
                        true,
                        swap.old_address,
                        check,
                    )
                });
            if let Err(error) = restored {
                let error = error.context("failed to restore previous server");
                status.error = Some(format!(
                    "{}; {error:#}",
                    status.error.as_deref().unwrap_or_default()
                ));
                write_status(root, &status)?;
                return Err(error);
            }
            status.recovered = true;
            write_status(root, &status)
        }
    }
}

fn accept(root: &Path, swap: &Swap, status: &mut Status) {
    status.active = false;
    let mut step = status.steps.iter_mut().find(|step| step.kind == Step::Swap);
    if swap.prune {
        prune::run(root, &swap.candidate, &swap.current, |text| {
            if let Some(step) = &mut step {
                append_log(&mut step.log, text);
            } else {
                eprint!("{text}");
            }
        });
    }
    if let Some(step) = step {
        step.state = State::Passed;
    }
}

fn restore_versions(root: &Path, swap: &Swap) -> Result<()> {
    supervisor::record_versions(root, &swap.current, swap.previous.as_deref())
}

fn restart(runner: &impl SystemctlRunner, pending: &Path) -> Result<()> {
    match fs::remove_file(pending.join(ADDRESS)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    service::systemctl(runner, &["restart", service::UNIT])?;
    Ok(())
}

fn wait_healthy(
    runner: &impl SystemctlRunner,
    pending: &Path,
    binary: &Path,
    expect: Option<&str>,
    allow_older: bool,
    old_address: SocketAddr,
    check: HealthCheck,
) -> Result<()> {
    let deadline = Instant::now() + check.timeout;
    loop {
        if let Ok(marker) = fs::read_to_string(pending.join(ADDRESS)) {
            if let Some((pid, addr)) = marker.trim().split_once(' ') {
                if let Ok(addr) = addr.parse::<SocketAddr>() {
                    if addr.ip().is_loopback() && (check.probe)(addr, pid, expect).unwrap_or(false)
                    {
                        return Ok(());
                    }
                }
            }
        } else if allow_older && older_binary_healthy(runner, binary, old_address, check) {
            // A build from before service support has a PID-only health route.
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "server health check timed out (expected {})",
            expect.unwrap_or("previous build")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(target_os = "linux"))]
fn older_binary_healthy(_: &impl SystemctlRunner, _: &Path, _: SocketAddr, _: HealthCheck) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn older_binary_healthy(
    runner: &impl SystemctlRunner,
    binary: &Path,
    addr: SocketAddr,
    check: HealthCheck,
) -> bool {
    let Ok(pid) = service::systemctl(
        runner,
        &["show", service::UNIT, "--property=MainPID", "--value"],
    ) else {
        return false;
    };
    if !pid.parse::<u32>().is_ok_and(|pid| pid > 0) {
        return false;
    }
    let same_file = fs::metadata(format!("/proc/{pid}/exe")).and_then(|running| {
        let expected = fs::metadata(binary)?;
        Ok((running.dev(), running.ino()) == (expected.dev(), expected.ino()))
    });
    same_file.unwrap_or(false) && (check.probe)(addr, &pid, None).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Output};

    const OLD_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn detached_ready_marker_requires_a_live_handoff_lock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let trial = tempfile::tempdir_in(root.path())?;
        let ready = trial.path().join("address");
        File::create(&ready)?;
        let lock = File::create(root.path().join("lock"))?;
        lock.lock()?;
        assert!(waiting(&ready));
        drop(lock);
        assert!(!waiting(&ready));
        Ok(())
    }

    struct Scripts {
        root: PathBuf,
        swap: Swap,
        reject_launch: bool,
    }

    impl SystemctlRunner for Scripts {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            Command::new(program)
                .args(args)
                .env_clear()
                .env("PATH", self.root.join("bin"))
                .env("ROOT", &self.root)
                .env("CANDIDATE", &self.swap.candidate)
                .env("ADDR", self.swap.old_address.to_string())
                .env("REJECT_LAUNCH", if self.reject_launch { "1" } else { "0" })
                .output()
        }
    }

    impl Scripts {
        fn create(root: &Path, addr: SocketAddr) -> Result<Self> {
            let current = root.join(OLD_ID).join("secretaire");
            let candidate = root.join(NEW_ID).join("secretaire");
            let previous = root.join("previous-build/secretaire");
            for path in [&current, &candidate, &previous] {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, "")?;
            }
            supervisor::record_versions(root, &current, Some(&previous))?;
            File::create(root.join("lock"))?;
            let swap = Swap {
                current,
                candidate,
                previous: Some(previous),
                expect: Some(NEW_ID.into()),
                old_address: addr,
                prune: false,
                rollback: false,
            };
            let scripts = root.join("bin");
            fs::create_dir(&scripts)?;
            for (program, body) in [
                (
                    "systemd-run",
                    r#"#!/bin/sh
set -eu
test "$1" = --user
test "$2" = --collect
case "$3" in --unit=fuzukue-swap-*) ;; *) exit 1;; esac
test "$4" = --property=Type=exec
test "$5" = --
test "$7" = service
test "$8" = swap-check
test "$9" = "$ROOT/service-swap"
test "$(/usr/bin/readlink -f "$ROOT/current")" = "$CANDIDATE"
printf '%s\n' "$@" > "$ROOT/transient"
test "$REJECT_LAUNCH" = 0
"#,
                ),
                (
                    "systemctl",
                    r#"#!/bin/sh
set -eu
test "$1" = --user
case "$2" in
  daemon-reload) printf 'reload\n' >> "$ROOT/systemctl.log";;
  restart)
    test "$3" = fuzukue.service
    current=$(/usr/bin/readlink -f "$ROOT/current")
    printf 'restart %s\n' "$current" >> "$ROOT/systemctl.log"
    if test "$current" = "$CANDIDATE"; then pid=42; else pid=41; fi
    printf '%s %s\n' "$pid" "$ADDR" > "$ROOT/service-swap/address"
    ;;
  *) exit 1;;
esac
"#,
                ),
            ] {
                let path = scripts.join(program);
                fs::write(&path, body)?;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            let mut status = Status {
                build_id: OLD_ID.into(),
                active: true,
                steps: super::super::steps(false),
                ..Status::default()
            };
            status
                .steps
                .iter_mut()
                .find(|step| step.kind == Step::Swap)
                .unwrap()
                .state = State::Running;
            write_status(root, &status)?;
            Ok(Self {
                root: root.to_owned(),
                swap,
                reject_launch: false,
            })
        }

        fn launch(&self, fallback_exec: bool) -> Result<PathBuf> {
            let unit = self.root.join(service::UNIT);
            let project = Path::new("/日本語 100%/project");
            let executable = if fallback_exec {
                self.swap.current.clone()
            } else {
                self.root.join("current")
            };
            fs::write(
                &unit,
                format!(
                    "[Service]\nExecStart=:\"{}\" \"/日本語 100%%/project\"\n",
                    executable.display().to_string().replace('%', "%%")
                ),
            )?;
            let lock = File::options()
                .read(true)
                .write(true)
                .open(self.root.join("lock"))?;
            lock.try_lock()?;
            let pending = self.root.join(PENDING);
            launch_with(self, &unit, &self.root, project, &self.swap)?;
            let args = fs::read_to_string(self.root.join("transient"))?;
            assert!(args.contains("\nservice\nswap-check\n"));
            assert!(args.lines().any(|arg| Path::new(arg) == self.swap.current));
            assert_eq!(
                fs::canonicalize(self.root.join("previous"))?,
                self.swap.current
            );
            assert_eq!(
                fs::canonicalize(self.root.join("current"))?,
                self.swap.candidate
            );
            assert!(waiting(&pending));
            lock.unlock()?;
            Ok(pending)
        }

        fn systemctl_log(&self) -> Result<String> {
            Ok(fs::read_to_string(self.root.join("systemctl.log"))?)
        }

        fn queue_registration(&self) -> Result<super::super::Update> {
            let config = toml::from_str(r#"publish = ['printf "registered\n" >> "$FUZUKUE_PROJECT_DIR/registered"']"#)?;
            let update = super::super::Update::with_paths(
                config, None, self.root.clone(), self.root.clone(), self.swap.candidate.clone(),
            );
            update.queue_registration(tempfile::tempdir_in(&self.root)?, &self.swap.candidate)?;
            Ok(update)
        }
    }

    fn mock_response(pid: &str, expect: Option<&str>, id: &str) -> bool {
        let response = format!("HTTP/1.0 200 OK\r\nX-Fuzukue-Build-Id: {id}\r\n\r\n{pid}");
        supervisor::health_response_matches(&response, pid, expect)
    }

    #[tokio::test]
    async fn service_swap_accepts_expected_build_with_one_restart() -> Result<()> {
        let tmp = tempfile::Builder::new()
            .prefix("service % 日本語 ")
            .tempdir()?;
        let root = tmp.path().canonicalize()?;
        let scripts = Scripts::create(&root, "127.0.0.1:7878".parse()?)?;
        let update = scripts.queue_registration()?;
        let distribution = crate::distribution::Distribution::new(&root);
        let pending = scripts.launch(true)?;
        assert!(!waiting(&pending), "no running helper owns the lock yet");
        update.finish_registration(&distribution).await?;
        assert!(!root.join("registered").exists());
        check_with(
            &scripts,
            &pending,
            HealthCheck {
                timeout: Duration::ZERO,
                probe: |addr, pid, expect| {
                    assert_eq!(addr, "127.0.0.1:7878".parse::<SocketAddr>().unwrap());
                    assert_eq!(pid, "42");
                    Ok(mock_response("42", expect, NEW_ID))
                },
            },
        )?;
        assert!(!pending.exists());
        assert_eq!(
            scripts.systemctl_log()?,
            format!("reload\nrestart {}\n", scripts.swap.candidate.display())
        );
        let status = read_status(&root);
        assert_eq!(status.steps.iter().find(|s| s.kind == Step::Swap).unwrap().state, State::Passed);
        assert!(!status.active && !status.recovered && status.error.is_none());
        update.restore_status();
        update.finish_registration(&distribution).await?;
        update.finish_registration(&distribution).await?;
        assert_eq!(fs::read_to_string(root.join("registered"))?, "registered\n");
        assert_eq!(
            fs::canonicalize(root.join("current"))?,
            scripts.swap.candidate
        );
        assert_eq!(
            fs::canonicalize(root.join("previous"))?,
            scripts.swap.current
        );
        Ok(())
    }

    #[tokio::test]
    async fn service_swap_rejects_wrong_build_and_restores_links_with_two_restarts() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().canonicalize()?;
        let scripts = Scripts::create(&root, "127.0.0.1:7878".parse()?)?;
        let update = scripts.queue_registration()?;
        let pending = scripts.launch(false)?;
        check_with(
            &scripts,
            &pending,
            HealthCheck {
                timeout: Duration::ZERO,
                probe: |_, pid, expect| Ok(mock_response(pid, expect, OLD_ID)),
            },
        )?;
        assert!(!pending.exists());
        assert_eq!(
            scripts.systemctl_log()?,
            format!(
                "restart {}\nrestart {}\n",
                scripts.swap.candidate.display(),
                scripts.swap.current.display()
            )
        );
        let status = read_status(&root);
        assert_eq!(status.steps.iter().find(|s| s.kind == Step::Swap).unwrap().state, State::Failed);
        assert_eq!(status.steps.iter().find(|s| s.kind == Step::Publish).unwrap().state, State::Skipped);
        assert!(!status.active && status.recovered);
        assert!(status.error.unwrap().contains(NEW_ID));
        assert_eq!(
            fs::canonicalize(root.join("current"))?,
            scripts.swap.current
        );
        assert_eq!(
            fs::canonicalize(root.join("previous"))?,
            scripts.swap.previous.unwrap()
        );
        update.finish_registration(&crate::distribution::Distribution::new(&root)).await?;
        assert!(!root.join("registered").exists());
        Ok(())
    }

    #[test]
    fn rejected_transient_unit_restores_links_without_restart() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().canonicalize()?;
        let mut scripts = Scripts::create(&root, "127.0.0.1:7878".parse()?)?;
        scripts.reject_launch = true;
        let error = scripts.launch(false).unwrap_err();
        assert!(error.to_string().contains("systemd-run"));
        assert!(!root.join(PENDING).exists());
        assert!(!root.join("systemctl.log").exists());
        assert_eq!(
            fs::canonicalize(root.join("current"))?,
            scripts.swap.current
        );
        assert_eq!(
            fs::canonicalize(root.join("previous"))?,
            scripts.swap.previous.unwrap()
        );
        Ok(())
    }
}
