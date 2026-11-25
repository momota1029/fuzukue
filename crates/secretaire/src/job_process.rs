//! Own long-running subprocesses and their descendants.

use std::io;
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
        let cgroup = linux::Cgroup::prepare(command, kind, memory_bytes);
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
    use std::sync::{Mutex, OnceLock};
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::process::Command;

    const MIB: u64 = 1024 * 1024;
    const DEFAULT_MIB: u64 = 8192;
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static EMPTYING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) struct Cgroup(PathBuf);

    fn delegated_root(membership: &str) -> Option<PathBuf> {
        let path = membership.lines().find_map(|line| line.strip_prefix("0::/"))?;
        let path = Path::new(path);
        if !path.components().all(|component| matches!(component, Component::Normal(_))) {
            return None;
        }
        let unit = path.parent()?;
        if path.file_name()? != "server"
            || !matches!(unit.file_name()?.to_str()?, "fuzukue.service" | "fuzukue-release.service")
        {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(unit))
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
        pub(super) fn prepare(command: &mut Command, kind: &str, limit: Option<u64>) -> Option<Self> {
            let root = root()?;
            EMPTYING.lock().unwrap().retain(|path| matches!(fs::remove_dir(path),
                Err(error) if error.kind() != io::ErrorKind::NotFound));
            let generation = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("job-{kind}-{}-{generation}", std::process::id()));
            let result = (|| -> io::Result<Self> {
                fs::create_dir(&dir)?;
                let cgroup = Self(dir.clone());
                let bytes = limit.unwrap_or_else(|| {
                    std::env::var("FUZUKUE_JOB_MEMORY_MIB").ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| *value > 0)
                        .and_then(|value| value.checked_mul(MIB))
                        .unwrap_or(DEFAULT_MIB * MIB)
                });
                fs::write(dir.join("memory.max"), bytes.to_string())?;
                fs::write(dir.join("memory.oom.group"), "1")?;
                // Fail over to a process group if the kernel cannot kill descendants atomically.
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
            match result {
                Ok(cgroup) => Some(cgroup),
                Err(error) => {
                    tracing::warn!(%error, path=?dir, "could not create job memory cgroup");
                    None
                }
            }
        }

        pub(super) fn kill(&self) -> io::Result<()> {
            fs::write(self.0.join("cgroup.kill"), "1")
        }
    }

    impl Drop for Cgroup {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir(&self.0) {
                if error.kind() != io::ErrorKind::NotFound {
                    EMPTYING.lock().unwrap().push(self.0.clone());
                }
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
