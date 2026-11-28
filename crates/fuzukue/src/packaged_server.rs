//! Lifecycle of the per-user LaunchAgent installed by the macOS package.

use std::{path::PathBuf, process::Command};

const LABEL: &str = "com.fuzukue.desktop.server";

pub(super) struct PackagedServer {
    plist: PathBuf,
}

impl PackagedServer {
    pub(super) fn installed() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let home = dirs::home_dir()?;
        let app = home.join("Applications/fuzukue.app/Contents");
        let installer = home.join("Library/Application Support/fuzukue/desktop-install");
        let plist = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
        let expected = app.join("MacOS/fuzukue").canonicalize().ok()?;
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        (exe == expected
            && app.join("Resources/install.sh").is_file()
            && installer.join("release").is_file()
            && plist.is_file())
        .then_some(Self { plist })
    }

    // Match the loaded job's path, as the installer does. A missing, unloaded
    // or replaced job must not be started by the app.
    fn loaded_job(&self) -> Option<(String, Option<u32>)> {
        let owner = Command::new("/usr/bin/plutil")
            .args(["-extract", "FuzukueInstaller", "raw", "-o", "-"])
            .arg(&self.plist)
            .output()
            .ok()?;
        if !owner.status.success() || std::str::from_utf8(&owner.stdout).ok()?.trim() != "one-line" {
            return None;
        }
        let uid = Command::new("/usr/bin/id").arg("-u").output().ok()?;
        if !uid.status.success() {
            return None;
        }
        let uid = std::str::from_utf8(&uid.stdout).ok()?.trim().parse::<u32>().ok()?;
        let service = format!("gui/{uid}/{LABEL}");
        let job = Command::new("/bin/launchctl").args(["print", &service]).output().ok()?;
        if !job.status.success() {
            return None;
        }
        let pid = loaded_pid(std::str::from_utf8(&job.stdout).ok()?, self.plist.to_str()?)?;
        Some((service, pid))
    }

    pub(super) fn owns_peer(&self, pid: u32) -> bool {
        pid > 0 && self.loaded_job().is_some_and(|(_, running)| running == Some(pid))
    }

    pub(super) fn start_if_stopped(&self) -> bool {
        let Some((service, None)) = self.loaded_job() else {
            return false;
        };
        // Without -k, an agent that started since print is left alone.
        Command::new("/bin/launchctl")
            .args(["kickstart", &service])
            .output()
            .is_ok_and(|result| result.status.success())
    }
}

fn loaded_pid(status: &str, plist: &str) -> Option<Option<u32>> {
    let field = |key| {
        status.lines().filter_map(|line| line.trim().split_once(" = "))
            .find_map(|(name, value)| (name == key).then_some(value))
    };
    if field("path") != Some(plist) {
        return None;
    }
    match field("pid") {
        Some(pid) => pid.parse().ok().filter(|&pid| pid > 0).map(Some),
        None => Some(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_server_requires_the_installer_job_path_and_a_valid_pid() {
        let plist = "/Users/a b/Library/LaunchAgents/com.fuzukue.desktop.server.plist";
        assert_eq!(loaded_pid(&format!("\tpath = {plist}\n\tpid = 42\n"), plist), Some(Some(42)));
        assert_eq!(loaded_pid(&format!("\tpath = {plist}\n\tstate = not running\n"), plist), Some(None));
        assert_eq!(loaded_pid(&format!("\tpath = {plist}\n\tpid = ?\n"), plist), None);
        assert_eq!(loaded_pid("\tpath = /other.plist\n\tpid = 42\n", plist), None);
        assert_eq!(loaded_pid("\tpid = 42\n", plist), None);
    }
}
