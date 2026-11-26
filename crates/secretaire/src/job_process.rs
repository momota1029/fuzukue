//! Own long-running subprocesses and their descendants.

use std::io;
#[cfg(target_os = "linux")]
pub(crate) use linux::{check_service_isolation, request_cache_reclaim, start_cache_reclaim};

#[cfg(not(target_os = "linux"))]
pub(crate) fn check_service_isolation() {}

#[cfg(not(target_os = "linux"))]
pub(crate) fn request_cache_reclaim() {}

#[cfg(not(target_os = "linux"))]
pub(crate) fn start_cache_reclaim() {}

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
        let (child, cgroup) = linux::spawn(command, kind, memory_bytes)?;
        #[cfg(not(target_os = "linux"))]
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        #[cfg(not(target_os = "linux"))]
        let _ = (kind, memory_bytes);
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tokio::process::{Child, Command};
    use tokio::sync::Notify;

    const MIB: u64 = 1024 * 1024;
    const DEFAULT_MIB: u64 = 8192;
    const DEFAULT_CACHE_MIB: u64 = 1536;
    const RECLAIM_CHUNK: u64 = 512 * MIB;
    const LEGACY_CHUNK: u64 = 128 * MIB;
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static ADMISSION: Mutex<()> = Mutex::new(());
    static CACHE_CHANGED: Notify = Notify::const_new();

    pub(super) fn spawn(
        command: &mut Command,
        kind: &str,
        memory_bytes: Option<u64>,
    ) -> io::Result<(Child, Option<Cgroup>)> {
        // The unit can be reclaimed between jobs. Serialize that check and write
        // with creating the job cgroup and moving the new process into it.
        let _admission = ADMISSION.lock().unwrap();
        let cgroup = Cgroup::prepare(command, kind, memory_bytes)?;
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        Ok((child, cgroup))
    }

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
            let cleanup = move || {
                cleanup(&dir);
                request_cache_reclaim();
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(cleanup);
            } else {
                cleanup();
            }
        }
    }

    impl Drop for Cgroup {
        fn drop(&mut self) {
            self.start_cleanup();
        }
    }

    fn populated(dir: &Path) -> io::Result<bool> {
        let events = fs::read_to_string(dir.join("cgroup.events"))?;
        match events.lines().find_map(|line| line.strip_prefix("populated ")) {
            Some("0") => Ok(false),
            Some("1") => Ok(true),
            _ => Err(io::Error::other("missing cgroup population")),
        }
    }

    fn cleanup(dir: &Path) {
        // SIGKILL is asynchronous. Wait for descendants, then reclaim this
        // cgroup while its cache is still directly addressable.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match populated(dir) {
                Ok(false) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    tracing::warn!(path=?dir, "job cgroup did not become empty");
                    return;
                }
            }
        }
        let mut reclaim = Reclaim::default();
        // The unit sweep can handle residual charges after this bounded pass,
        // including dirty pages that the kernel could not reclaim yet.
        for _ in 0..8 {
            if !reclaim_step(dir, 0, &mut reclaim) {
                break;
            }
        }
        if let Err(error) = fs::remove_dir(dir) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path=?dir, "could not remove job cgroup");
            }
        }
    }

    pub(crate) fn request_cache_reclaim() {
        CACHE_CHANGED.notify_one();
    }

    pub(crate) fn start_cache_reclaim() {
        let Some(root) = root() else { return };
        let budget = std::env::var("FUZUKUE_SERVICE_CACHE_MIB").ok()
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|value| value.checked_mul(MIB))
            .unwrap_or(DEFAULT_CACHE_MIB * MIB);
        if budget == 0 {
            return;
        }
        if let Err(error) = OpenOptions::new().write(true).open(root.join("memory.reclaim")) {
            tracing::debug!(%error, path=?root, "unit cache reclaim unavailable");
            return;
        }
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut reclaim = Reclaim::default();
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = CACHE_CHANGED.notified() => {}
                }
                match tokio::task::spawn_blocking(move || {
                    for _ in 0..32 {
                        let Ok(_admission) = ADMISSION.try_lock() else { break };
                        if !unit_idle(root).unwrap_or(false) || !reclaim_step(root, budget, &mut reclaim) {
                            break;
                        }
                    }
                    reclaim
                }).await {
                    Ok(next) => reclaim = next,
                    Err(_) => break,
                }
            }
        });
    }

    fn unit_idle(root: &Path) -> io::Result<bool> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_name() == "server" || !entry.file_type()?.is_dir() {
                continue;
            }
            match populated(&entry.path()) {
                Ok(false) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                result => return result.map(|_| false),
            }
        }
        Ok(true)
    }

    #[derive(Default)]
    struct MemoryStat {
        file: u64,
        shmem: u64,
        slab: u64,
        slab_reclaimable: u64,
        inactive_file: u64,
    }

    impl MemoryStat {
        fn read(dir: &Path) -> io::Result<Self> {
            Ok(Self::parse(&fs::read_to_string(dir.join("memory.stat"))?))
        }

        fn parse(stat: &str) -> Self {
            let mut result = Self::default();
            for line in stat.lines() {
                let Some((key, value)) = line.split_once(' ') else { continue };
                let field = match key {
                    "file" => &mut result.file,
                    "shmem" => &mut result.shmem,
                    "slab" => &mut result.slab,
                    "slab_reclaimable" => &mut result.slab_reclaimable,
                    "inactive_file" => &mut result.inactive_file,
                    _ => continue,
                };
                *field = value.parse().unwrap_or(0);
            }
            result
        }

        fn file_cache(&self) -> u64 {
            self.file.saturating_sub(self.shmem)
        }

        fn cache(&self) -> u64 {
            self.file_cache().saturating_add(self.slab)
        }

        fn excess(&self, budget: u64) -> u64 {
            self.cache().saturating_sub(budget)
                .min(self.file_cache().saturating_add(self.slab_reclaimable))
        }

        fn legacy_request(&self, bytes: u64) -> u64 {
            // Old kernels choose what to reclaim. Only make small requests when
            // there is plenty of inactive file cache, and check for swap after each.
            bytes.min(LEGACY_CHUNK).min(self.inactive_file / 4)
        }
    }

    #[derive(Default)]
    enum Reclaim {
        #[default]
        FilePreferred,
        Legacy,
        Unavailable,
    }

    impl Reclaim {
        fn write(&mut self, dir: &Path, stat: &MemoryStat, bytes: u64) -> io::Result<()> {
            let control = dir.join("memory.reclaim");
            match self {
                Self::Unavailable => return Ok(()),
                Self::FilePreferred => {
                    let result = fs::write(&control, format!("{bytes} swappiness=0"));
                    if !result.as_ref().is_err_and(|error| error.raw_os_error() == Some(libc::EINVAL)) {
                        return result;
                    }
                    *self = Self::Legacy;
                }
                Self::Legacy => {}
            }
            let request = stat.legacy_request(bytes);
            if request < MIB {
                return Ok(());
            }
            let swap = || fs::read_to_string(dir.join("memory.swap.current")).ok()
                .and_then(|value| value.trim().parse::<u64>().ok());
            let Some(before) = swap() else { return Ok(()) };
            let result = fs::write(control, request.to_string());
            if swap().is_none_or(|after| after > before) {
                *self = Self::Unavailable;
                tracing::debug!(path=?dir, "stopped legacy cache reclaim after swap usage changed");
            }
            result
        }
    }

    fn reclaim_step(dir: &Path, budget: u64, reclaim: &mut Reclaim) -> bool {
        let Ok(before) = MemoryStat::read(dir) else { return false };
        let bytes = before.excess(budget).min(RECLAIM_CHUNK);
        if bytes < MIB || matches!(reclaim, Reclaim::Unavailable) {
            return false;
        }
        if let Err(error) = reclaim.write(dir, &before, bytes) {
            // EAGAIN reports partial reclaim, so read the counters before deciding
            // whether to retry. Unsupported or inaccessible controls are best effort.
            if error.kind() != io::ErrorKind::WouldBlock {
                *reclaim = Reclaim::Unavailable;
                tracing::debug!(%error, path=?dir, "cache reclaim unavailable");
                return false;
            }
        }
        MemoryStat::read(dir).is_ok_and(|after| after.cache() < before.cache())
    }

    #[cfg(test)]
    #[test]
    fn unit_reclaim_skips_populated_jobs_but_can_run_with_just_the_server() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = dir.path().join("server");
        let job = dir.path().join("job-test");
        fs::create_dir(&server)?;
        fs::create_dir(&job)?;
        fs::write(server.join("cgroup.events"), "populated 1\nfrozen 0\n")?;
        fs::write(job.join("cgroup.events"), "populated 1\nfrozen 0\n")?;
        assert!(!unit_idle(dir.path())?);
        fs::write(job.join("cgroup.events"), "populated 0\nfrozen 0\n")?;
        assert!(unit_idle(dir.path())?);
        Ok(())
    }

    #[cfg(test)]
    #[test]
    fn reclaim_counts_file_and_slab_and_requests_zero_swappiness() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let counters = format!(
            "anon {}\nfile {}\nshmem {}\nslab {}\nslab_reclaimable {}\ninactive_file {}\n",
            900 * MIB, 400 * MIB, 100 * MIB, 80 * MIB, 60 * MIB, 200 * MIB,
        );
        let stat = MemoryStat::parse(&counters);
        assert_eq!(stat.excess(360 * MIB), 20 * MIB);
        assert_eq!(stat.excess(0), 360 * MIB);
        assert_eq!(stat.legacy_request(100 * MIB), 50 * MIB);
        assert_eq!(stat.excess(400 * MIB), 0);
        fs::write(dir.path().join("memory.stat"), counters)?;
        fs::write(dir.path().join("memory.reclaim"), "")?;
        assert!(!reclaim_step(dir.path(), 360 * MIB, &mut Reclaim::default()));
        assert_eq!(fs::read_to_string(dir.path().join("memory.reclaim"))?,
            format!("{} swappiness=0", 20 * MIB));
        Ok(())
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
