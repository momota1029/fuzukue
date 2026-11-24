use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context as _, Result};
use fuzukue_proto::{ClientManifest, CLIENT_PLATFORMS};
use sha2::{Digest, Sha256};

use crate::conn::HttpTarget;

pub const BUILD_ID: &str = env!("FUZUKUE_CLIENT_BUILD_ID");
const HEALTH_ENV: &str = "FUZUKUE_UPDATE_PENDING_COMMIT";
const LEGACY_HEALTH_ENV: &str = "LABO_UPDATE_PENDING_COMMIT";
const WELCOME_WINDOW: Duration = Duration::from_secs(100);
const RESTART_GRACE: Duration = Duration::from_secs(60);
pub const HELPER_ARG: &str = "--apply-client-update";

pub fn platform() -> Option<&'static str> {
    CLIENT_PLATFORMS
        .iter()
        .copied()
        .find(|value| value.split_once('-') == Some((std::env::consts::OS, std::env::consts::ARCH)))
}

pub fn available(own: &str, rejected: Option<&str>, manifest: &ClientManifest) -> bool {
    Some(manifest.platform.as_str()) == platform()
        && !manifest.commit.is_empty()
        && manifest.commit != own
        && rejected != Some(&manifest.commit)
}

pub fn rejected(manifest: &ClientManifest) -> Option<String> {
    rejected_for(&std::env::current_exe().ok()?, manifest)
}

fn rejected_for(exe: &Path, manifest: &ClientManifest) -> Option<String> {
    let marker = sidecar(exe, ".rejected");
    if manifest.commit == BUILD_ID {
        let _ = remove_if_exists(&marker);
        return None;
    }
    fs::read_to_string(marker).ok()
}

pub fn manifest(http: &HttpTarget, platform: &str) -> Result<Option<ClientManifest>> {
    ensure!(CLIENT_PLATFORMS.contains(&platform));
    match http.distribution("GET", &format!("{platform}/manifest"), None) {
        Ok(response) => Ok(Some(serde_json::from_reader(
            response.into_reader().take(8192),
        )?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn download(http: &HttpTarget, manifest: &ClientManifest, output: &mut fs::File) -> Result<()> {
    let sha = &manifest.sha256;
    ensure!(CLIENT_PLATFORMS.contains(&manifest.platform.as_str()));
    ensure!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    let response =
        http.distribution("GET", &format!("{}?sha256={sha}", manifest.platform), None)?;
    verify(response.into_reader(), &mut *output, manifest)?;
    output.sync_all()?;
    Ok(())
}

fn verify(input: impl Read, mut output: impl Write, manifest: &ClientManifest) -> Result<()> {
    let mut input = input.take(manifest.size.saturating_add(1));
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut size = 0;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    ensure!(
        size == manifest.size && format!("{:x}", hash.finalize()) == manifest.sha256,
        tr!("distribution-hash-mismatch")
    );
    Ok(())
}

fn sidecar(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub fn prepare(http: &HttpTarget, manifest: &ClientManifest) -> Result<tempfile::TempPath> {
    let exe = std::env::current_exe()?;
    let mut staged =
        tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?;
    download(http, manifest, staged.as_file_mut())?;
    staged
        .as_file()
        .set_permissions(fs::metadata(exe)?.permissions())?;
    Ok(staged.into_temp_path())
}

pub fn launch_helper(staged: tempfile::TempPath, commit: &str) -> Result<UpdateHandoff> {
    let exe = std::env::current_exe()?;
    let marker = sidecar(&exe, ".pending");
    let mut pending = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)?;
    let result = (|| {
        pending.write_all(commit.as_bytes())?;
        remove_if_exists(&sidecar(&exe, ".update-ready"))?;
        remove_if_exists(&sidecar(&exe, ".trial"))?;
        remove_if_exists(&sidecar(&exe, ".trial-restart"))?;
        let helper = sidecar(&exe, ".update-helper.exe");
        fs::copy(&exe, &helper)?;
        let mut child = Command::new(helper)
            .arg(HELPER_ARG)
            .arg(&exe)
            .arg(&staged)
            .arg(commit)
            .args(std::env::args_os().skip(1))
            .spawn()?;
        if let Err(err) = staged.keep() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err.into());
        }
        Ok(UpdateHandoff(exe))
    })();
    if result.is_err() {
        let _ = remove_if_exists(&marker);
    }
    result
}

pub struct UpdateHandoff(PathBuf);

impl UpdateHandoff {
    pub fn release(self) -> Result<()> {
        fs::write(sidecar(&self.0, ".update-ready"), [])?;
        Ok(())
    }
}

#[cfg(not(test))]
pub fn legacy_config_trial() -> bool {
    std::env::var_os(HEALTH_ENV).is_none()
        && fuzukue_proto::compat::env(HEALTH_ENV).as_deref() == Ok(BUILD_ID)
        && std::env::current_exe().is_ok_and(|exe| sidecar(&exe, ".trial").exists())
}

pub fn connected() {
    if fuzukue_proto::compat::env(HEALTH_ENV).as_deref() == Ok(BUILD_ID) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = confirm(&sidecar(&exe, ".trial"), BUILD_ID);
        }
    }
    // Any connected run of a legacy-named client (not only the first one after an
    // update) installs the renamed copy; a trial still in progress is skipped inside.
    offer_renamed_executable();
}

fn active_trial() -> Option<PathBuf> {
    if fuzukue_proto::compat::env(HEALTH_ENV).as_deref() != Ok(BUILD_ID) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    (fs::read_to_string(sidecar(&exe, ".trial")).as_deref().ok() == Some(BUILD_ID)).then_some(exe)
}

pub fn trial_pending() -> bool {
    active_trial().is_some()
}

pub fn server_restarting() {
    if let Some(exe) = active_trial() {
        let restart = sidecar(&exe, ".trial-restart");
        if fs::read_to_string(&restart).as_deref().ok() != Some(BUILD_ID) {
            let _ = fs::write(restart, BUILD_ID);
        }
    }
}

pub fn offer_renamed_executable() {
    #[cfg(windows)]
    if let Err(error) = create_renamed_copy() {
        // GUI processes have no console: leave the reason next to the executable.
        if let Ok(exe) = std::env::current_exe() {
            let _ = fs::write(
                sidecar(&exe, ".rename-error.txt"),
                format!("Could not install fuzukue.exe beside {}: {error:#}\n", exe.display()),
            );
        }
    }
}

#[cfg(windows)]
fn create_renamed_copy() -> Result<()> {
    let exe = std::env::current_exe()?;
    let legacy_name = exe.file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("labo-client.exe"));
    if !legacy_name || sidecar(&exe, ".trial").exists() {
        return Ok(());
    }
    let destination = exe.with_file_name("fuzukue.exe");
    match destination.symlink_metadata() {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut staged = tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?;
    io::copy(&mut fs::File::open(&exe)?, staged.as_file_mut())?;
    match staged.persist_noclobber(&destination) {
        Ok(_) => eprintln!("Installed {}; existing shortcuts still work", destination.display()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn helper(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let exe = PathBuf::from(args.next().context("missing executable")?);
    let staged = PathBuf::from(args.next().context("missing staged build")?);
    let commit = args
        .next()
        .context("missing build ID")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid build ID"))?;
    let args: Vec<_> = args.collect();
    let ready = sidecar(&exe, ".update-ready");
    let pending = sidecar(&exe, ".pending");
    let backup = sidecar(&exe, ".old");
    let trial = sidecar(&exe, ".trial");
    let restart = sidecar(&exe, ".trial-restart");
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.is_file() {
            ensure!(Instant::now() < deadline, "update handoff timed out");
            std::thread::sleep(Duration::from_millis(100));
        }
        remove_if_exists(&ready)?;
        let result = replace_with(&exe, &staged, &backup, || {
            fs::write(&trial, &commit)?;
            wait_for_health(start(&exe, &args, Some(&commit))?, &trial, &restart, &commit)
        });
        if let Err(err) = result {
            fs::write(sidecar(&exe, ".rejected"), &commit)?;
            start(&exe, &args, None)?;
            return Err(err);
        }
        remove_if_exists(&sidecar(&exe, ".rejected"))?;
        Ok(())
    })();
    let _ = remove_if_exists(&staged);
    let _ = remove_if_exists(&trial);
    let _ = remove_if_exists(&restart);
    let _ = remove_if_exists(&pending);
    result
}

fn confirm(marker: &Path, build_id: &str) -> io::Result<()> {
    if fs::read_to_string(marker).as_deref().ok() == Some(build_id) {
        remove_if_exists(marker)?;
    }
    Ok(())
}

fn replace_with(
    exe: &Path,
    staged: &Path,
    backup: &Path,
    check_start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    install(exe, staged, backup)?;
    if let Err(err) = check_start() {
        fs::rename(backup, exe).context("restoring previous executable")?;
        return Err(err);
    }
    Ok(())
}

fn install(exe: &Path, staged: &Path, backup: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        remove_if_exists(backup)?;
        fs::rename(exe, backup)?;
    }
    #[cfg(not(windows))]
    {
        let previous =
            tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?;
        fs::copy(exe, previous.path())?;
        previous.persist(backup)?;
    }
    if let Err(err) = fs::rename(staged, exe) {
        #[cfg(windows)]
        fs::rename(backup, exe).context("restoring executable after failed replacement")?;
        return Err(err.into());
    }
    Ok(())
}

fn start(exe: &Path, args: &[OsString], pending: Option<&str>) -> Result<Child> {
    let mut command = Command::new(exe);
    command.args(args).env_remove(HEALTH_ENV).env_remove(LEGACY_HEALTH_ENV);
    if let Some(commit) = pending {
        command.env(HEALTH_ENV, commit);
    }
    Ok(command.spawn()?)
}

fn wait_for_health(mut child: Child, marker: &Path, restart: &Path, commit: &str) -> Result<()> {
    let start = Instant::now();
    while marker.exists() {
        if child.try_wait()?.is_some() {
            anyhow::bail!("updated client exited before connecting");
        }
        if welcome_timed_out(start.elapsed(), restart, commit) {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("updated client did not connect");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn welcome_timed_out(elapsed: Duration, restart: &Path, commit: &str) -> bool {
    elapsed >= WELCOME_WINDOW
        && (elapsed >= WELCOME_WINDOW + RESTART_GRACE
            || fs::read_to_string(restart).as_deref().ok() != Some(commit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ClientManifest {
        ClientManifest {
            platform: platform().unwrap().into(),
            commit: "new".into(),
            built_at: "2026-11-20T00:00:00Z".into(),
            sha256: format!("{:x}", Sha256::digest(b"good")),
            size: 4,
        }
    }

    #[test]
    fn compares_build_ids_and_remembers_rejected_build() {
        let manifest = fixture();
        assert!(available("old", None, &manifest));
        assert!(!available("new", None, &manifest));
        assert!(!available("old", Some("new"), &manifest));
    }

    #[test]
    fn running_the_manifest_build_clears_rejection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let exe = dir.path().join("client");
        let marker = sidecar(&exe, ".rejected");
        let mut manifest = fixture();
        fs::write(&marker, &manifest.commit)?;
        let rejected = rejected_for(&exe, &manifest);
        assert_eq!(rejected.as_deref(), Some(manifest.commit.as_str()));
        assert!(!available(BUILD_ID, rejected.as_deref(), &manifest));
        assert!(marker.exists());

        manifest.commit = BUILD_ID.into();
        fs::write(&marker, BUILD_ID)?;
        assert_eq!(rejected_for(&exe, &manifest), None);
        assert!(!available(BUILD_ID, None, &manifest));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn startup_marker_requires_matching_build_and_failure_restores_previous_binary() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let exe = dir.path().join("client");
        let stage = sidecar(&exe, ".stage");
        let backup = sidecar(&exe, ".old");
        let trial = sidecar(&exe, ".trial");
        fs::write(&exe, "old")?;
        fs::write(&stage, "new")?;
        fs::write(&trial, "new")?;
        assert!(replace_with(&exe, &stage, &backup, || {
            confirm(&trial, "wrong")?;
            assert!(trial.exists());
            anyhow::bail!("failed to connect")
        })
        .is_err());
        assert_eq!(fs::read_to_string(&exe)?, "old");

        fs::write(&stage, "new")?;
        replace_with(&exe, &stage, &backup, || {
            confirm(&trial, "new")?;
            assert!(!trial.exists());
            Ok(())
        })?;
        assert_eq!(fs::read_to_string(&exe)?, "new");
        assert_eq!(fs::read_to_string(&backup)?, "old");
        Ok(())
    }

    #[test]
    fn rejects_a_corrupt_download_before_replacement() {
        let manifest = fixture();
        assert!(verify(b"evil".as_slice(), io::sink(), &manifest).is_err());
        assert!(verify(b"good+".as_slice(), io::sink(), &manifest).is_err());
        assert!(verify(b"good".as_slice(), io::sink(), &manifest).is_ok());
    }

    #[test]
    fn welcome_timeout_extends_only_for_a_matching_restart_notice() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let restart = dir.path().join(".trial-restart");
        assert!(welcome_timed_out(WELCOME_WINDOW, &restart, "new"));
        fs::write(&restart, "old")?;
        assert!(welcome_timed_out(WELCOME_WINDOW, &restart, "new"));
        fs::write(&restart, "new")?;
        assert!(!welcome_timed_out(WELCOME_WINDOW, &restart, "new"));
        assert!(welcome_timed_out(WELCOME_WINDOW + RESTART_GRACE, &restart, "new"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exited_client_fails_the_trial_even_during_restart_grace() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let trial = dir.path().join(".trial");
        let restart = dir.path().join(".trial-restart");
        fs::write(&trial, "new")?;
        fs::write(&restart, "new")?;
        let mut child = Command::new("sh").args(["-c", "exit 1"]).spawn()?;
        child.wait()?;
        let error = wait_for_health(child, &trial, &restart, "new").unwrap_err();
        assert!(error.to_string().contains("exited before connecting"));
        Ok(())
    }
}
