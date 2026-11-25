//! Own long-running subprocesses and their descendants.

use std::io;
#[cfg(target_os = "linux")]
pub(crate) use linux::check_service_isolation;

#[cfg(not(target_os = "linux"))]
pub(crate) fn check_service_isolation() {}

use std::sync::atomic::{AtomicBool, Ordering};
use rustix::process::{kill_process_group, Pid, Signal};
use tokio::process::{Child, Command};

pub(crate) struct Job {
    group: Pid,
    stopped: AtomicBool,
    #[cfg(target_os = "linux")]
    cgroup: Option<linux::Cgroup>,
}

impl Job {
    pub(crate) fn spawn(command: &mut Command, kind: &str, memory_bytes: Option<u64>) -> io::Result<(Child, Self)> {
        #[cfg(target_os = "linux")]
        let cgroup = linux::Cgroup::prepare(command, kind, memory_bytes)?;
        #[cfg(not(target_os = "linux"))]
        let _ = (kind, memory_bytes);
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        let group = child.id().and_then(|id| Pid::from_raw(id as i32))
            .ok_or_else(|| io::Error::other("missing child process group"))?;
        Ok((child, Self {
            group,
            stopped: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            cgroup,
        }))
    }

    pub(crate) fn stop(&self) -> io::Result<()> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = &self.cgroup {
            if let Err(error) = cgroup.kill() {
                tracing::warn!(%error, "could not stop job cgroup");
            } else {
                return Ok(());
            }
        }
        match kill_process_group(self.group, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => {
                self.stopped.store(false, Ordering::Release);
                Err(error.into())
            }
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(all(test, target_os = "linux"))]
#[tokio::test]
async fn dropping_job_stops_its_shell_child() -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 300 & echo $!; wait"]).stdout(Stdio::piped());
    let (mut child, job) = Job::spawn(&mut command, "test", None)?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)).await??;
    let pid: u32 = line.trim().parse()?;
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    drop(job);
    tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
            if state.as_deref().is_none_or(|stat| stat.rsplit_once(") ").is_some_and(|(_, tail)|
                tail.starts_with("Z ") || tail.starts_with("X ")))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await?;
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::{Component, Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tokio::process::Command;

    const MIB: u64 = 1024 * 1024;
    const DEFAULT_MIB: u64 = 8192;
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub(super) struct Cgroup {
        dir: PathBuf,
        cleaning: AtomicBool,
    }

    fn membership_path(membership: &str) -> Option<&Path> {
        let path = membership.lines().find_map(|line| line.strip_prefix("0::/"))?;
        let path = Path::new(path);
        path.components().all(|component| matches!(component, Component::Normal(_))).then_some(path)
    }

    fn delegated_root(membership: &str) -> Option<PathBuf> {
        let path = membership_path(membership)?;
        if path.file_name()? != "server" {
            return None;
        }
        let unit = path.parent()?;
        if service_unit(unit).is_none() {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(unit))
    }

    fn service_unit(path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        matches!(name, "fuzukue.service" | "fuzukue-release.service").then_some(name)
    }

    pub(crate) fn check_service_isolation() {
        let Ok(membership) = fs::read_to_string("/proc/self/cgroup") else { return };
        let Some(path) = membership_path(&membership) else { return };
        let unit = if path.file_name().is_some_and(|name| name == "server") {
            path.parent().and_then(service_unit)
        } else {
            service_unit(path)
        };
        if let Some(unit) = unit.filter(|_| root().is_none()) {
            tracing::warn!("Job memory isolation is unavailable. To enable it and restart the user service, run: secretaire service enable-job-isolation {unit}");
        }
    }

    fn root() -> Option<&'static Path> {
        ROOT.get_or_init(|| {
            let root = delegated_root(&fs::read_to_string("/proc/self/cgroup").ok()?)?;
            if let Err(error) = fs::write(root.join("cgroup.subtree_control"), "+memory") {
                tracing::warn!(%error, path=?root, "job memory cgroups are unavailable");
                return None;
            }
            Some(root)
        }).as_deref()
    }

    impl Cgroup {
        pub(super) fn prepare(command: &mut Command, kind: &str, limit: Option<u64>) -> io::Result<Option<Self>> {
            let Some(root) = root() else { return Ok(None) };
            let generation = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("job-{kind}-{}-{generation}", std::process::id()));
            let result = (|| -> io::Result<Self> {
                fs::create_dir(&dir)?;
                let cgroup = Self { dir: dir.clone(), cleaning: AtomicBool::new(false) };
                let bytes = limit.unwrap_or_else(|| {
                    std::env::var("FUZUKUE_JOB_MEMORY_MIB").ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| *value > 0)
                        .and_then(|value| value.checked_mul(MIB))
                        .unwrap_or(DEFAULT_MIB * MIB)
                });
                fs::write(dir.join("memory.high"), (bytes / 2).to_string())?;
                fs::write(dir.join("memory.max"), bytes.to_string())?;
                fs::write(dir.join("memory.oom.group"), "1")?;
                // Require atomic descendant cleanup, including daemons that called setsid.
                OpenOptions::new().write(true).open(dir.join("cgroup.kill"))?;
                let procs = OpenOptions::new().write(true).open(dir.join("cgroup.procs"))?;
                // Writing zero puts the newly forked process in this group before exec.
                // Keep the fd CLOEXEC and only call async-signal-safe code here.
                unsafe {
                    command.pre_exec(move || {
                        if libc::write(procs.as_raw_fd(), b"0".as_ptr().cast(), 1) == 1 {
                            Ok(())
                        } else {
                            Err(io::Error::last_os_error())
                        }
                    });
                }
                Ok(cgroup)
            })();
            result.map(Some).map_err(|error| {
                tracing::warn!(%error, path=?dir, "could not create job memory cgroup");
                error
            })
        }

        pub(super) fn kill(&self) -> io::Result<()> {
            fs::write(self.dir.join("cgroup.kill"), "1")?;
            self.start_cleanup();
            Ok(())
        }

        fn start_cleanup(&self) {
            if self.cleaning.swap(true, Ordering::AcqRel) {
                return;
            }
            let dir = self.dir.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || cleanup(&dir));
            } else {
                cleanup(&dir);
            }
        }
    }

    impl Drop for Cgroup {
        fn drop(&mut self) {
            self.start_cleanup();
        }
    }

    fn cleanup(dir: &Path) {
        // cgroup.kill sends SIGKILL but task exit and writeback may still be in flight.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs::read_to_string(dir.join("cgroup.events")) {
                Ok(events) if events.lines().any(|line| line == "populated 0") => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => break,
            }
        }
        // Reclaim while the charge still belongs to this job. Old kernels lack this file.
        if let Ok(stat) = fs::read_to_string(dir.join("memory.stat")) {
            let file_bytes = stat.lines().find_map(|line| line.strip_prefix("file "))
                .and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
            if file_bytes > 0 {
                if let Err(error) = fs::write(dir.join("memory.reclaim"), file_bytes.to_string()) {
                    tracing::debug!(%error, path=?dir, "job cache reclaim was unavailable or partial");
                }
            }
        }
        if let Err(error) = fs::remove_dir(dir) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path=?dir, "could not remove job cgroup");
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn only_recognizes_our_delegated_service_leaf() {
        assert_eq!(delegated_root("0::/user.slice/user@1000.service/app.slice/fuzukue.service/server\n"),
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/user@1000.service/app.slice/fuzukue.service")));
        assert!(delegated_root("0::/user.slice/some-other.service/server").is_none());
        assert!(delegated_root("0::/user.slice/fuzukue.service").is_none());
        assert!(delegated_root("0::/../fuzukue.service/server").is_none());
    }
}
