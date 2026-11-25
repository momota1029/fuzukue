//! Explicit worker execution policy and the host capability needed to enforce it.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Approval {
    Ask,
    Auto,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub sandbox: Option<Sandbox>,
    pub approval: Option<Approval>,
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Policy {
    pub sandbox: Sandbox,
    pub approval: Approval,
    pub full: bool,
}

impl Settings {
    pub(crate) fn effective(self, full: bool) -> Option<Policy> {
        if !full && self.sandbox.is_none() && self.approval.is_none() {
            return None;
        }
        Some(Policy {
            sandbox: self.sandbox.unwrap_or(if full { Sandbox::FullAccess } else { Sandbox::WorkspaceWrite }),
            approval: if full { Approval::Never } else { self.approval.unwrap_or(Approval::Ask) },
            full,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Capability {
    checked_at: i64,
    bwrap: Option<PathBuf>,
    user_namespace: Option<bool>,
    user_namespace_error: Option<String>,
    workspace_write: bool,
    unshare_net: Option<bool>,
    error: Option<String>,
    network_error: Option<String>,
}

static CAPABILITY: tokio::sync::OnceCell<Capability> = tokio::sync::OnceCell::const_new();

pub(crate) async fn capability() -> &'static Capability {
    CAPABILITY.get_or_init(|| async {
        #[cfg(target_os = "linux")]
        let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
            .into_iter().map(PathBuf::from).find(|path| path.is_file());
        #[cfg(not(target_os = "linux"))]
        let bwrap = None;
        probe(bwrap).await
    }).await
}

async fn output(mut command: Command) -> Result<()> {
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(3), command.output()).await??;
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(output.status.success(), "{}: {}", output.status, stderr.trim());
    Ok(())
}

async fn probe(bwrap: Option<PathBuf>) -> Capability {
    let mut report = Capability {
        checked_at: chrono::Utc::now().timestamp_millis(),
        bwrap,
        user_namespace: None,
        user_namespace_error: None,
        workspace_write: false,
        unshare_net: None,
        error: None,
        network_error: None,
    };
    #[cfg(target_os = "linux")]
    if Path::new("/usr/bin/unshare").is_file() {
        let mut unshare = Command::new("/usr/bin/unshare");
        unshare.args(["--user", "--map-root-user", "--", "/bin/true"]);
        match output(unshare).await {
            Ok(()) => report.user_namespace = Some(true),
            Err(error) => {
                report.user_namespace = Some(false);
                report.user_namespace_error = Some(error.to_string());
            }
        }
    }
    let Some(path) = report.bwrap.as_deref() else {
        report.error = Some("bubblewrap executable is absent".into());
        return report;
    };
    let mut minimal = sandbox_base(path);
    for root in ["/usr", "/lib", "/lib64"] {
        minimal.args(["--ro-bind-try", root, root]);
    }
    minimal.args(["--", "/usr/bin/true"]);
    if let Err(error) = output(minimal).await {
        report.error = Some(format!("bubblewrap minimal root probe: {error:#}"));
        return report;
    }
    match probe_mount(path, false).await {
        Ok(()) => report.workspace_write = true,
        Err(error) => {
            report.error = Some(format!("{error:#}"));
            return report;
        }
    }
    match probe_mount(path, true).await {
        Ok(()) => report.unshare_net = Some(true),
        Err(error) => {
            report.unshare_net = Some(false);
            report.network_error = Some(format!("{error:#}"));
        }
    }
    report
}

async fn probe_mount(bwrap: &Path, isolate_net: bool) -> Result<()> {
    let root = tempfile::Builder::new().prefix("fuzukue-bwrap-").tempdir()?;
    let root_path = root.path().canonicalize()?;
    let allowed = root_path.join("allowed");
    let blocked = root_path.join("blocked");
    std::fs::create_dir(&allowed)?;
    std::fs::create_dir(&blocked)?;
    let mut command = sandbox_command(bwrap, &root_path, &[&allowed], isolate_net);
    command.args(["/bin/sh", "-ec",
        "printf ok > \"$1/marker\"; if (printf bad > \"$2/marker\") 2>/dev/null; then exit 3; fi",
        "bwrap-probe",
    ]).arg(&allowed).arg(&blocked);
    output(command).await.context("bubblewrap mount/exec probe")?;
    ensure!(std::fs::read(allowed.join("marker")).is_ok_and(|bytes| bytes == b"ok"),
        "bubblewrap did not write to the allowed directory");
    ensure!(!blocked.join("marker").exists(), "bubblewrap wrote to the blocked directory");
    Ok(())
}

fn sandbox_base(bwrap: &Path) -> Command {
    let mut command = Command::new(bwrap);
    command.args([
        "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--disable-userns", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
    ]);
    command
}

fn sandbox_command(bwrap: &Path, cwd: &Path, writable: &[&Path], isolate_net: bool) -> Command {
    let mut command = sandbox_base(bwrap);
    command.args(["--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev"]);
    if isolate_net {
        command.arg("--unshare-net");
    }
    for root in writable {
        command.arg("--bind").arg(root).arg(root);
    }
    command.arg("--chdir").arg(cwd).arg("--");
    command
}

impl Policy {
    pub(crate) async fn command(self, program: &Path, scope: &crate::worker_scope::Scope) -> Result<Command> {
        if self.sandbox == Sandbox::FullAccess {
            return Ok(Command::new(program));
        }
        let host = capability().await;
        if !host.workspace_write {
            bail!("requested worker sandbox is unavailable: {}", host.error.as_deref().unwrap_or("probe failed"));
        }
        let mut writable = vec![scope.scratch.as_path()];
        if self.sandbox == Sandbox::WorkspaceWrite {
            writable.push(&scope.cwd);
        }
        // The backend needs its API connection and loopback MCP in this namespace.
        let mut command = sandbox_command(host.bwrap.as_deref().context("missing bubblewrap path")?, &scope.cwd, &writable, false);
        command.arg(program);
        Ok(command)
    }

    /// Give a sandboxed backend a writable runtime directory and copy its file-based login.
    pub(crate) fn backend_home(self, cmd: &mut Command, scratch: &Path, variable: &str, default: &str, files: &[&str]) -> Result<()> {
        if self.sandbox == Sandbox::FullAccess {
            return Ok(());
        }
        let source = std::env::var_os(variable).map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(default)));
        let dest = scratch.join(default);
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let mut mkdir = std::fs::DirBuilder::new();
        match mkdir.mode(0o700).create(&dest) {
            Ok(()) => {
                if let Some(source) = source {
                    for file in files {
                        let mut from = match std::fs::File::open(source.join(file)) {
                            Ok(from) => from,
                            Err(error) if error.kind() == ErrorKind::NotFound => continue,
                            Err(error) => return Err(error.into()),
                        };
                        let mut to = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dest.join(file))?;
                        std::io::copy(&mut from, &mut to)?;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => ensure!(
                std::fs::symlink_metadata(&dest)?.is_dir(), "worker backend home must be a directory"
            ),
            Err(error) => return Err(error.into()),
        }
        cmd.env(variable, dest);
        Ok(())
    }
}

pub(crate) type StoredSettings = OnceLock<std::sync::Mutex<Settings>>;

pub(crate) fn settings(workspace: &crate::Workspace) -> Result<&std::sync::Mutex<Settings>> {
    if let Some(settings) = workspace.worker_execution.get() {
        return Ok(settings);
    }
    let settings = match std::fs::read(workspace.state_dir.join("worker-execution.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == ErrorKind::NotFound => Settings::default(),
        Err(error) => return Err(error.into()),
    };
    Ok(workspace.worker_execution.get_or_init(|| std::sync::Mutex::new(settings)))
}

pub(crate) fn set(workspace: &crate::Workspace, change: Settings) -> Result<()> {
    use std::io::Write;
    let mut current = settings(workspace)?.lock().unwrap();
    std::fs::create_dir_all(&workspace.state_dir)?;
    let mut file = tempfile::NamedTempFile::new_in(&workspace.state_dir)?;
    file.write_all(&serde_json::to_vec(&change)?)?;
    file.as_file().sync_all()?;
    file.persist(workspace.state_dir.join("worker-execution.json"))?;
    *current = change;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn probe_checks_mount_write_boundary_and_optional_network_namespace() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let fake = dir.path().join("fake-bwrap");
        let install = |body: &str| -> Result<()> {
            std::fs::write(&fake, format!("#!/bin/sh\n{body}\n"))?;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))?;
            Ok(())
        };
        assert!(!probe(None).await.workspace_write);

        install("printf '%s\\n' 'bwrap: Failed to mount tmpfs: No such file or directory' >&2; exit 1")?;
        let denied = probe(Some(fake.clone())).await;
        assert!(!denied.workspace_write);
        assert!(denied.error.unwrap().contains("Failed to mount tmpfs: No such file or directory"));
        assert_eq!(denied.unshare_net, None);

        install("exit 0")?;
        let no_write = probe(Some(fake.clone())).await;
        assert!(!no_write.workspace_write);
        assert!(no_write.error.unwrap().contains("did not write to the allowed directory"));

        install(r#"
            while [ "$#" -gt 0 ]; do
                case "$1" in
                    --unshare-net) printf '%s\n' 'network namespace denied' >&2; exit 1 ;;
                    bwrap-probe) printf ok > "$2/marker"; exit 0 ;;
                esac
                shift
            done
        "#)?;
        let working = probe(Some(fake)).await;
        assert!(working.workspace_write);
        assert_eq!(working.unshare_net, Some(false));
        assert!(working.network_error.unwrap().contains("network namespace denied"));
        assert!(working.checked_at > 0);
        Ok(())
    }

    #[test]
    fn sandbox_and_approval_are_independent() {
        let defaults = Settings::default();
        assert!(defaults.effective(false).is_none());
        let full = defaults.effective(true).unwrap();
        assert_eq!(full.sandbox, Sandbox::FullAccess);
        assert_eq!(full.approval, Approval::Never);

        let settings = Settings { sandbox: Some(Sandbox::ReadOnly), approval: Some(Approval::Auto) };
        let configured = settings.effective(false).unwrap();
        assert_eq!(configured.sandbox, Sandbox::ReadOnly);
        assert_eq!(configured.approval, Approval::Auto);
        let full = settings.effective(true).unwrap();
        assert_eq!(full.sandbox, Sandbox::ReadOnly);
        assert_eq!(full.approval, Approval::Never);
    }
}
