use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::{fs::symlink, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{SelfUpdateStepKind, SelfUpdateStepState};
use rustix::process::{getppid, kill_process_group, setsid, Pid, Signal};

use super::{append_log, fail, prune, read_status, registration, write_status, SOURCE_ENV};

pub(super) const READY_ENV: &str = "FUZUKUE_SELF_UPDATE_READY";
const LEGACY_READY_ENV: &str = "LABO_SELF_UPDATE_READY";
const PRUNE_ENV: &str = "FUZUKUE_SELF_UPDATE_PRUNE";

pub(crate) fn legacy_handoff() -> bool {
    std::env::var_os(READY_ENV).is_none() && fuzukue_proto::compat::env_os(READY_ENV).is_some()
}
pub(crate) const HEALTH_ROUTE: &str = "/self-update/health";
const BUILD_HEADER: &str = "x-fuzukue-build-id";
#[derive(Clone, Copy)]
struct HealthCheck {
    timeout: Duration,
    probe: fn(SocketAddr, &str) -> std::io::Result<bool>,
}

pub(super) fn launch(
    root: &Path,
    source: Option<&Path>,
    current: &Path,
    candidate: &Path,
    lock: File,
    prune_enabled: bool,
) -> Result<()> {
    let mut cmd = Command::new(current);
    cmd.arg("self-swap")
        .arg(std::process::id().to_string())
        .arg(root)
        .arg(current)
        .arg(candidate)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::from(lock));
    cmd.env_remove(READY_ENV).env_remove(LEGACY_READY_ENV);
    cmd.env(PRUNE_ENV, if prune_enabled { "1" } else { "0" });
    if let Some(source) = source {
        cmd.env(SOURCE_ENV, source);
    }
    detach(&mut cmd);
    cmd.spawn().context("start self-swap supervisor")?;
    Ok(())
}

fn detach(cmd: &mut Command) {
    // Only the async-signal-safe setsid syscall runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| setsid().map(|_| ()).map_err(Into::into));
    }
}

fn spawn_with_etxtbsy_retry(mut cmd_factory: impl FnMut() -> Command) -> io::Result<Child> {
    retry_etxtbsy(|| cmd_factory().spawn())
}

fn retry_etxtbsy<T>(mut spawn: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    const MAX_RETRIES: usize = 8;
    let mut delay = Duration::from_millis(25);
    for retry in 1..=MAX_RETRIES {
        match spawn() {
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                // A concurrent fork can hold the executable's write fd until exec.
                eprintln!(
                    "secretaire self-swap: {error}; retrying server spawn in {} ms ({retry}/{MAX_RETRIES})",
                    delay.as_millis()
                );
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(400));
            }
            result => return result,
        }
    }
    spawn()
}

pub(crate) fn managed() -> bool {
    fuzukue_proto::compat::env_os(READY_ENV).is_some()
}

pub(crate) fn ready(addr: SocketAddr) -> Result<Option<PathBuf>> {
    if let Some(path) = fuzukue_proto::compat::env_os(READY_ENV).map(PathBuf::from) {
        let mut marker = tempfile::NamedTempFile::new_in(
            path.parent().context("invalid server health marker")?,
        )?;
        write!(marker, "{addr}")?;
        crate::durable::persist(marker, &path)?;
        return Ok(Some(path));
    }
    Ok(None)
}

pub(crate) async fn health(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    if !crate::remote::is_loopback(peer.ip()) {
        return (axum::http::StatusCode::NOT_FOUND, String::new()).into_response();
    }
    if !crate::canary() && !crate::service::managed() {
        return (axum::http::StatusCode::OK, std::process::id().to_string()).into_response();
    }
    (
        axum::http::StatusCode::OK,
        [(BUILD_HEADER, super::BUILD_ID)],
        std::process::id().to_string(),
    ).into_response()
}

pub(crate) fn run(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let usage = "usage: secretaire self-swap <parent-pid> <builds-dir> <current> <candidate> [server args...]";
    let parent = args
        .next()
        .and_then(|pid| pid.to_str()?.parse().ok())
        .and_then(Pid::from_raw)
        .context(usage)?;
    let root = PathBuf::from(args.next().context(usage)?);
    let current = PathBuf::from(args.next().context(usage)?);
    let candidate = PathBuf::from(args.next().context(usage)?);
    // stdin owns the updater's file lock. The new server gets /dev/null.
    // Reparenting proves this particular parent exited, regardless of PID reuse.
    ensure!(parent != rustix::process::getpid(), "{usage}");
    while getppid() == Some(parent) {
        std::thread::sleep(Duration::from_millis(50));
    }
    swap(
        &root,
        &current,
        &candidate,
        &args.collect::<Vec<_>>(),
        HealthCheck {
            timeout: Duration::from_secs(30),
            probe,
        },
        std::env::var_os(PRUNE_ENV).as_deref() != Some(std::ffi::OsStr::new("0")),
    )
    .map(|_| ())
}

fn swap(
    root: &Path,
    current: &Path,
    candidate: &Path,
    argv: &[OsString],
    check: HealthCheck,
    prune_enabled: bool,
) -> Result<Child> {
    let mut status = read_status(root);
    let mut accepted = status.clone();
    accepted.active = false;
    match boot(root, candidate, argv, check, || {
        record_versions(root, candidate, Some(current))?;
        let mut step = accepted
            .steps
            .iter_mut()
            .find(|step| step.kind == SelfUpdateStepKind::Swap);
        if prune_enabled {
            prune::run(root, candidate, current, |text| {
                if let Some(step) = &mut step {
                    append_log(&mut step.log, text);
                } else {
                    let _ = std::io::stderr().write_all(text.as_bytes());
                }
            });
        }
        if let Some(step) = step {
            step.state = SelfUpdateStepState::Passed;
        }
        registration::accept(candidate, &mut accepted);
        save_outcome(root, &accepted);
        Ok(())
    }) {
        Ok(child) => Ok(child),
        Err(error) => {
            registration::discard(candidate, &mut status);
            eprintln!(
                "secretaire self-swap: {error:#}; restarting {}",
                current.display()
            );
            fail(&mut status, format!("{error:#}"));
            boot(root, current, argv, check, || {
                if fs::canonicalize(root.join("current")).ok().as_deref() != Some(current) {
                    if let Err(error) = record_versions(root, current, None) {
                        eprintln!("secretaire self-swap: record restored build: {error:#}");
                    }
                }
                status.recovered = true;
                save_outcome(root, &status);
                Ok(())
            })
            .map_err(|error| {
                let error = error.context("failed to restore previous server");
                status.error = Some(format!("{}; {error:#}", status.error.unwrap_or_default()));
                save_outcome(root, &status);
                error
            })
        }
    }
}

fn save_outcome(root: &Path, status: &fuzukue_proto::SelfUpdateStatus) {
    if let Err(error) = write_status(root, status) {
        eprintln!("secretaire self-swap: save outcome: {error:#}");
    }
}

fn boot(
    root: &Path,
    binary: &Path,
    argv: &[OsString],
    check: HealthCheck,
    on_healthy: impl FnOnce() -> Result<()>,
) -> Result<Child> {
    let trial = tempfile::Builder::new()
        .prefix("health-")
        .tempdir_in(root)?;
    let ready = trial.path().join("address");
    let legacy_state = root.parent().is_some_and(|path| path.ends_with(fuzukue_proto::compat::LEGACY_APP_DIR));
    let legacy_binary = binary.ends_with("labo-server");
    let ready_env = if legacy_binary || legacy_state { LEGACY_READY_ENV } else { READY_ENV };
    let mut legacy_env = BTreeMap::new();
    if legacy_binary {
        for (key, value) in std::env::vars_os() {
            if key == PRUNE_ENV {
                continue;
            }
            if let Some(old) = key.to_str().and_then(fuzukue_proto::compat::legacy_env_name) {
                if old != LEGACY_READY_ENV {
                    legacy_env.insert(old.into(), value);
                }
            }
        }
        if let Some(source) = fuzukue_proto::compat::env_os(SOURCE_ENV) {
            legacy_env.insert("LABO_SELF_UPDATE_SOURCE".into(), source);
        }
        if !legacy_state {
            configure_legacy_roots(&mut legacy_env, root)?;
            for name in ["FUZUKUE_SERVER_CONFIG", "FUZUKUE_MAIL_CONFIG"] {
                if let Some(path) = fuzukue_proto::compat::env_os(name) {
                    let legacy = fuzukue_proto::compat::legacy_env_name(name).unwrap();
                    legacy_env.insert(legacy.into(), crate::state::config_file(path.into()).into());
                }
            }
        }
    }
    let mut child = spawn_with_etxtbsy_retry(|| {
        let mut cmd = Command::new(binary);
        cmd.args(argv)
            .env_remove(READY_ENV)
            .env_remove(LEGACY_READY_ENV)
            .env_remove(PRUNE_ENV)
            .env(ready_env, &ready)
            .envs(&legacy_env)
            .stdin(Stdio::null());
        detach(&mut cmd);
        cmd
    })
    .with_context(|| format!("failed to start {}", binary.display()))?;
    let result = wait_healthy(&mut child, &ready, check).and_then(|()| on_healthy());
    if let Err(error) = result {
        // Only signal the session we just started, while its leader's PID is
        // still owned by this Child. Reap it before attempting to rebind.
        if child.try_wait()?.is_none() {
            if let Some(pid) = Pid::from_raw(child.id() as i32) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            child.kill().ok();
            child.wait()?;
        }
        return Err(error).with_context(|| format!("check {}", binary.display()));
    }
    Ok(child)
}

fn wait_healthy(child: &mut Child, marker: &Path, check: HealthCheck) -> Result<()> {
    let deadline = Instant::now() + check.timeout;
    let pid = child.id().to_string();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("server exited before becoming healthy: {status}");
        }
        if let Ok(addr) = fs::read_to_string(marker) {
            if let Ok(addr) = addr.parse::<SocketAddr>() {
                if addr.ip().is_loopback() && (check.probe)(addr, &pid).unwrap_or(false) {
                    ensure!(
                        child.try_wait()?.is_none(),
                        "server exited during health check"
                    );
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("server health check timed out")
}

fn probe(addr: SocketAddr, pid: &str) -> std::io::Result<bool> {
    probe_build(addr, pid, None)
}

pub(super) fn probe_build(addr: SocketAddr, pid: &str, build: Option<&str>) -> std::io::Result<bool> {
    let timeout = Duration::from_millis(300);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {HEALTH_ROUTE} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.take(4096).read_to_string(&mut response)?;
    Ok(health_response_matches(&response, pid, build))
}

pub(super) fn health_response_matches(response: &str, pid: &str, build: Option<&str>) -> bool {
    response
        .split_once("\r\n\r\n")
        .is_some_and(|(headers, body)| {
            headers
                .lines()
                .next()
                .is_some_and(|line| line.split_whitespace().nth(1) == Some("200"))
                && body == pid
                && build.is_none_or(|expected| {
                    headers.lines().filter_map(|line| line.split_once(':')).any(|(name, value)| {
                        name.eq_ignore_ascii_case(BUILD_HEADER) && value.trim() == expected
                    })
                })
        })
}

fn configure_legacy_roots(env: &mut BTreeMap<OsString, OsString>, builds: &Path) -> Result<()> {
    // Use private XDG roots so rollback can use migrated data even after a cross-device copy.
    let state = builds.parent().context("missing server state directory")?.canonicalize()?;
    let views = builds.join("legacy-runtime");
    let config = crate::state::config_dir();
    for (variable, dir, target) in [
        ("XDG_STATE_HOME", views.join("state"), Some(state)),
        ("XDG_CONFIG_HOME", views.join("config"), config),
    ] {
        let Some(target) = target else { continue };
        fs::create_dir_all(&dir)?;
        let alias = dir.join(fuzukue_proto::compat::LEGACY_APP_DIR);
        match symlink(&target, &alias) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists
                && fs::read_link(&alias).ok().as_deref() == Some(&target) => {}
            Err(error) => return Err(error.into()),
        }
        env.insert(variable.into(), dir.into());
    }
    Ok(())
}

pub(crate) fn relocate_build_links(old_state: &Path, state: &Path) -> Result<()> {
    let old = old_state.join("server-builds");
    let root = state.join("server-builds");
    let Ok(target) = fs::read_link(root.join("versions")) else { return Ok(()) };
    let generation = target.strip_prefix(&old).unwrap_or(&target);
    // The old supervisor wrote absolute links. Only repair links it owned within server-builds.
    if generation.is_absolute() || generation.components().count() != 1 || !generation.to_string_lossy().starts_with("versions-") {
        return Ok(());
    }
    let dir = root.join(generation);
    if !dir.symlink_metadata().is_ok_and(|entry| entry.is_dir()) {
        return Ok(());
    }
    for name in ["current", "previous"] {
        let link = dir.join(name);
        if let Ok(target) = fs::read_link(&link) {
            if let Ok(relative) = target.strip_prefix(&old) {
                replace_link(&link, &Path::new("..").join(relative))?;
            }
        }
    }
    if target != generation {
        replace_link(&root.join("versions"), generation)?;
    }
    Ok(())
}

fn replace_link(path: &Path, target: &Path) -> Result<()> {
    let staging = tempfile::tempdir_in(path.parent().context("missing symlink directory")?)?;
    let link = staging.path().join("link");
    symlink(target, &link)?;
    fs::rename(link, path)?;
    Ok(())
}

pub(super) fn record_versions(root: &Path, current: &Path, previous: Option<&Path>) -> Result<()> {
    let generation = tempfile::Builder::new()
        .prefix("versions-")
        .tempdir_in(root)?;
    let relative = |binary: &Path| {
        binary.strip_prefix(root).map_or_else(|_| binary.to_path_buf(), |path| Path::new("..").join(path))
    };
    symlink(relative(current), generation.path().join("current"))?;
    if let Some(previous) = previous {
        symlink(relative(previous), generation.path().join("previous"))?;
    }
    let link = generation.path().join("versions-link");
    symlink(generation.path().file_name().context("missing version directory")?, &link)?;
    for name in ["current", "previous"] {
        let pointer = root.join(name);
        if !pointer.is_symlink() {
            symlink(Path::new("versions").join(name), pointer)?;
        }
    }
    // Both pointers change in one rename.
    let old = fs::read_link(root.join("versions")).ok().map(|path| root.join(path));
    fs::rename(link, root.join("versions"))?;
    let _ = generation.keep();
    if let Some(old) = old.filter(|path| {
        path.parent() == Some(root)
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("versions-"))
    }) {
        let _ = fs::remove_dir_all(old);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(root: &Path, name: &str, body: &str) -> Result<PathBuf> {
        let path = root.join(name);
        let mut file = File::create(&path)?;
        writeln!(file, "#!/bin/sh\n{body}")?;
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        file.sync_all()?;
        drop(file);
        Ok(path)
    }

    const HEALTHY: &str = r#"printf restarted > "$1"
printf '127.0.0.1:12000' > "$FUZUKUE_SELF_UPDATE_READY"
exec sleep 30"#;

    fn fake_health_check() -> HealthCheck {
        HealthCheck {
            timeout: Duration::from_secs(1),
            probe: |addr, _| Ok(addr.port() == 12000),
        }
    }

    #[test]
    fn spawn_retries_etxtbsy_until_success() {
        let mut attempts = 0;
        let result = retry_etxtbsy(|| {
            attempts += 1;
            if attempts <= 3 {
                return Err(io::Error::from(rustix::io::Errno::TXTBSY));
            }
            Ok("spawned")
        });
        assert_eq!(result.unwrap(), "spawned");
        assert_eq!(attempts, 4);
    }

    #[test]
    fn spawn_returns_etxtbsy_after_eight_retries() {
        let mut attempts = 0;
        let error = retry_etxtbsy::<()>(|| {
            attempts += 1;
            Err(io::Error::from(rustix::io::Errno::TXTBSY))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
        assert_eq!(attempts, 9);
    }

    #[test]
    fn spawn_does_not_retry_other_errors() {
        let mut attempts = 0;
        let error = retry_etxtbsy::<()>(|| {
            attempts += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn supervisor_rolls_back_after_candidate_spawn_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let current = script(root, "current-server", HEALTHY)?;
        let candidate = root.join("missing-server");
        let marker = root.join("called");
        let mut child = swap(
            root,
            &current,
            &candidate,
            &[marker.clone().into_os_string()],
            fake_health_check(),
            false,
        )?;
        child.kill()?;
        child.wait()?;
        assert_eq!(fs::read_to_string(marker)?, "restarted");
        let status = read_status(root);
        assert!(status.recovered);
        assert!(status.error.unwrap().contains("failed to start"));
        Ok(())
    }

    #[test]
    fn supervisor_rolls_back_after_candidate_health_timeout() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let current = script(root, "current-server", HEALTHY)?;
        let candidate = script(
            root,
            "candidate-server",
            r#"printf '127.0.0.1:12001' > "$FUZUKUE_SELF_UPDATE_READY"; exec sleep 30"#,
        )?;
        let marker = root.join("called");
        let stale = root.join("c".repeat(40));
        fs::create_dir(&stale)?;
        let mut child = swap(
            root,
            &current,
            &candidate,
            &[marker.clone().into_os_string()],
            fake_health_check(),
            true,
        )?;
        // Reap the only surviving fake server, even if an assertion fails.
        child.kill()?;
        child.wait()?;
        assert_eq!(fs::read_to_string(marker)?, "restarted");
        assert_eq!(fs::canonicalize(root.join("current"))?, current);
        let status = read_status(root);
        assert!(status.recovered);
        assert!(status.error.unwrap().contains("health check timed out"));
        assert!(stale.is_dir());
        Ok(())
    }

    #[test]
    fn legacy_state_supervisor_hands_off_to_renamed_server() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("labo-gui/server-builds");
        fs::create_dir_all(&root)?;
        let old = script(&root, "labo-server", "exit 10")?;
        let new = script(&root, "secretaire", r#"
test -z "${FUZUKUE_SELF_UPDATE_READY:-}" || exit 11
printf '127.0.0.1:12000' > "$LABO_SELF_UPDATE_READY"
exec sleep 30"#)?;
        let mut child = swap(&root, &old, &new, &[], fake_health_check(), true)?;
        child.kill()?;
        child.wait()?;
        assert_eq!(root.join("current").canonicalize()?, new);
        assert_eq!(root.join("previous").canonicalize()?, old);
        Ok(())
    }

    #[test]
    fn migrated_absolute_links_keep_renamed_current_and_legacy_previous() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("labo-gui");
        let builds = old.join("server-builds");
        let generation = builds.join("versions-legacy");
        fs::create_dir_all(&generation)?;
        let current = script(&builds, "secretaire", "exit 0")?;
        let previous = script(&builds, "labo-server", "exit 0")?;
        symlink(current, generation.join("current"))?;
        symlink(previous, generation.join("previous"))?;
        symlink(generation, builds.join("versions"))?;
        symlink("versions/current", builds.join("current"))?;
        symlink("versions/previous", builds.join("previous"))?;
        let new = fuzukue_proto::compat::user_dir(dir.path());
        relocate_build_links(&old, &new)?;
        let builds = new.join("server-builds");
        assert_eq!(builds.join("current").canonicalize()?, builds.join("secretaire"));
        assert_eq!(builds.join("previous").canonicalize()?, builds.join("labo-server"));
        Ok(())
    }

    #[test]
    fn healthy_candidate_updates_both_version_paths() -> Result<()> {
        for prune_enabled in [true, false] {
            assert_healthy_swap(prune_enabled)?;
        }
        Ok(())
    }

    fn assert_healthy_swap(prune_enabled: bool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let current_dir = root.join("a".repeat(40));
        let candidate_dir = root.join("b".repeat(40));
        let stale = root.join("c".repeat(40));
        for dir in [&current_dir, &candidate_dir, &stale] {
            fs::create_dir(dir)?;
        }
        let current = script(&current_dir, "secretaire", "exit 10")?;
        let candidate = script(&candidate_dir, "secretaire", HEALTHY)?;
        symlink(root.join("missing-target"), root.join("target"))?;
        write_status(
            root,
            &fuzukue_proto::SelfUpdateStatus {
                active: true,
                steps: super::super::steps(true),
                ..Default::default()
            },
        )?;
        let mut child = swap(
            root,
            &current,
            &candidate,
            &[root.join("called").into_os_string()],
            fake_health_check(),
            prune_enabled,
        )?;
        child.kill()?;
        child.wait()?;
        assert_eq!(fs::canonicalize(root.join("current"))?, candidate);
        assert_eq!(fs::canonicalize(root.join("previous"))?, current);
        assert_eq!(stale.exists(), !prune_enabled);
        let status = read_status(root);
        assert!(!status.recovered);
        let step = status
            .steps
            .iter()
            .find(|step| step.kind == SelfUpdateStepKind::Swap)
            .unwrap();
        assert_eq!(step.state, SelfUpdateStepState::Passed);
        assert_eq!(step.log.contains("cleanup removed 1 entries"), prune_enabled);
        assert_eq!(step.log.contains("warning: self-update cleanup"), prune_enabled);
        Ok(())
    }
}
