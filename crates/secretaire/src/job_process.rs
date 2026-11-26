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

use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, Weak};
use rustix::process::{getpgrp, kill_process_group, waitid, Pid, Signal, WaitId, WaitIdOptions};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

// Track all process groups started for one worker attempt, including server-side exec.
pub(crate) struct Attempt(Mutex<Option<Vec<Weak<Job>>>>);

impl Default for Attempt {
    fn default() -> Self {
        Self(Mutex::new(Some(Vec::new())))
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum CommandKind {
    Native,
    ServerExec,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ResourceUsage {
    pub(crate) cpu: u64,
    pub(crate) io_bytes: u64,
}

// Include the start time so PID reuse and process churn cannot corrupt deltas.
pub(crate) type UsageSample = HashMap<(i32, u64), ResourceUsage>;

pub(crate) fn usage_advanced(current: &UsageSample, previous: &UsageSample) -> bool {
    current.iter().any(|(identity, usage)| {
        let previous = previous.get(identity).copied().unwrap_or_default();
        usage.cpu > previous.cpu || usage.io_bytes > previous.io_bytes
    })
}

impl Attempt {
    pub(crate) fn stop(&self) {
        if let Some(jobs) = self.0.lock().unwrap().take() {
            for job in jobs.iter().filter_map(Weak::upgrade) {
                if let Err(error) = job.stop() {
                    tracing::warn!(%error, "could not stop a worker process group");
                }
            }
        }
    }

    pub(crate) fn usage(&self, kind: CommandKind) -> UsageSample {
        let mut sample = UsageSample::new();
        #[cfg(target_os = "linux")]
        {
            let jobs = self.0.lock().unwrap();
            for job in jobs.iter().flatten().filter_map(Weak::upgrade) {
                if job.command_kind == Some(kind) {
                    job.sample(&mut sample, kind);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (kind, &mut sample);
        sample
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ProcessGroup {
    leader: Option<Pid>,
    stopped: bool,
}

pub(crate) struct Job {
    group: Mutex<ProcessGroup>,
    #[cfg(target_os = "linux")]
    command_kind: Option<CommandKind>,
    #[cfg(target_os = "linux")]
    cgroup: Option<linux::Cgroup>,
}

impl Job {
    pub(crate) fn spawn(
        command: &mut Command,
        kind: &str,
        memory_bytes: Option<u64>,
    ) -> io::Result<(JobChild, Arc<Self>)> {
        #[cfg(target_os = "linux")]
        let (child, cgroup) = linux::spawn(command, kind, memory_bytes)?;
        #[cfg(not(target_os = "linux"))]
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        #[cfg(not(target_os = "linux"))]
        let _ = (kind, memory_bytes);
        let leader = child.id().and_then(|id| Pid::from_raw(id as i32))
            .ok_or_else(|| io::Error::other("missing child process group"))?;
        let job = Arc::new(Self {
            group: Mutex::new(ProcessGroup { leader: Some(leader), stopped: false }),
            #[cfg(target_os = "linux")]
            command_kind: match kind {
                "exec" => Some(CommandKind::ServerExec),
                "claude" | "codex" => Some(CommandKind::Native),
                _ => None,
            },
            #[cfg(target_os = "linux")]
            cgroup,
        });
        Ok((JobChild::new(child, job.clone(), leader), job))
    }

    pub(crate) fn spawn_tracked(
        command: &mut Command,
        kind: &str,
        attempt: Option<&Attempt>,
    ) -> io::Result<(JobChild, Arc<Self>)> {
        // Hold the registry lock through spawn so stop cannot miss a new child.
        let mut registry = attempt.map(|attempt| attempt.0.lock().unwrap());
        let jobs = registry.as_mut().map(|jobs| {
            jobs.as_mut().ok_or_else(|| io::Error::other("worker attempt has ended"))
        }).transpose()?;
        let (child, job) = Self::spawn(command, kind, None)?;
        if let Some(jobs) = jobs {
            jobs.retain(|job| job.strong_count() != 0);
            jobs.push(Arc::downgrade(&job));
        }
        Ok((child, job))
    }

    #[cfg(target_os = "linux")]
    fn sample(&self, sample: &mut UsageSample, kind: CommandKind) {
        // Keep the leader's PID reserved while resolving its process tree.
        let group = self.group.lock().unwrap();
        let Some(leader) = group.leader.filter(|_| !group.stopped) else { return };
        linux::sample_command(sample, leader.as_raw_nonzero().get(), kind, self.cgroup.as_ref());
    }

    pub(crate) fn stop(&self) -> io::Result<()> {
        self.stop_group(&mut self.group.lock().unwrap())
    }

    fn stop_group(&self, group: &mut ProcessGroup) -> io::Result<()> {
        if group.stopped {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = &self.cgroup {
            if let Err(error) = cgroup.kill() {
                tracing::warn!(%error, "could not stop job cgroup");
            } else {
                group.stopped = true;
                return Ok(());
            }
        }

        signal_group(group, Signal::KILL)?;
        group.stopped = true;
        Ok(())
    }

    pub(crate) fn terminate(&self) -> io::Result<()> {
        let mut group = self.group.lock().unwrap();
        if group.stopped {
            return Ok(());
        }
        signal_group(&mut group, Signal::TERM)
    }
}

fn signal_group(group: &mut ProcessGroup, signal: Signal) -> io::Result<()> {
    let Some(leader) = group.leader else { return Ok(()) };
    if leader == getpgrp() {
        return Err(io::Error::other("refusing to signal the server process group"));
    }
    // JobChild disarms the PGID under this lock before reaping.
    // An exited, unreaped leader still reserves the numeric ID.
    match child_exited(leader) {
        Ok(_) => match kill_process_group(leader, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        },
        Err(rustix::io::Errno::CHILD) => group.leader = None,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn child_exited(pid: Pid) -> rustix::io::Result<bool> {
    loop {
        match waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error),
        }
    }
}

// Own every route to reaping this child, including Tokio's orphan reaper on drop.
pub(crate) struct JobChild {
    child: Child,
    job: Arc<Job>,
    #[cfg(target_os = "linux")]
    exit_ready: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

impl JobChild {
    fn new(mut child: Child, job: Arc<Job>, leader: Pid) -> Self {
        #[cfg(target_os = "linux")]
        let exit_ready = rustix::process::pidfd_open(leader, rustix::process::PidfdFlags::empty())
            .ok().and_then(|fd| {
                // SAFETY: AsyncFd owns this pidfd exclusively until deregistration.
                unsafe { tokio::io::unix::AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }.ok()
            });
        #[cfg(not(target_os = "linux"))]
        let _ = leader;
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            job,
            #[cfg(target_os = "linux")]
            exit_ready,
        }
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        let leader = self.job.group.lock().unwrap().leader;
        let cleanup = if let Some(leader) = leader {
            #[cfg(target_os = "linux")]
            if let Some(ready) = &self.exit_ready {
                let _ = ready.readable().await?;
            }
            while !child_exited(leader)? {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            self.disarm()
        } else {
            Ok(())
        };
        let status = self.child.wait().await?;
        cleanup?;
        Ok(status)
    }

    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.job.stop()?;
        self.wait().await.map(|_| ())
    }

    fn disarm(&self) -> io::Result<()> {
        let mut group = self.job.group.lock().unwrap();
        let result = self.job.stop_group(&mut group);
        group.leader = None;
        result
    }
}

impl Drop for JobChild {
    fn drop(&mut self) {
        if let Err(error) = self.disarm() {
            tracing::warn!(%error, "could not clean up the job before reaping its leader");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[tokio::test]
async fn stopping_attempt_kills_its_shell_child_and_disallows_new_processes() -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 300 & echo $!; wait"]).stdout(Stdio::piped());
    let attempt = Attempt::default();
    let (mut child, _job) = Job::spawn_tracked(&mut command, "test", Some(&attempt))?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)).await??;
    let pid: u32 = line.trim().parse()?;
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    attempt.stop();
    assert!(Job::spawn_tracked(&mut Command::new("/bin/true"), "test", Some(&attempt)).is_err());
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

#[cfg(all(test, target_os = "linux"))]
#[tokio::test]
async fn leader_exit_stops_descendants_before_reaping() -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 300 & echo $!"]).stdout(Stdio::piped());
    let (mut child, job) = Job::spawn(&mut command, "test", None)?;
    let mut stdout = child.stdout.take().unwrap();
    let mut output = String::new();
    let (status, _) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(child.wait(), stdout.read_to_string(&mut output))
    }).await??;

    assert!(status.success());
    assert!(output.trim().parse::<u32>().is_ok());
    assert!(job.group.lock().unwrap().leader.is_none());
    job.stop()?;
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{CommandKind, ResourceUsage, UsageSample};
    use std::collections::HashMap;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Write};
    use std::os::fd::AsRawFd;
    use std::path::{Component, Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tokio::process::{Child, Command};
    use tokio::sync::Notify;

    const MIB: u64 = 1024 * 1024;
    const DEFAULT_HIGH_MIB: u64 = 16 * 1024;
    const DEFAULT_MAX_MIB: u64 = 24 * 1024;
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
        kill_file: File,
        cleaning: AtomicBool,
    }

    struct Process {
        pid: i32,
        parent: i32,
        group: i32,
        started: u64,
        cpu: u64,
        shell: bool,
    }

    impl Process {
        fn read(pid: i32) -> Option<Self> {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_, comm) = stat.split_once('(')?;
            let (comm, stat) = comm.rsplit_once(')')?;
            // The suffix starts at field 3. CPU includes waited-for children,
            // which can start and exit between samples.
            let fields: Vec<_> = stat.split_whitespace().collect();
            let cpu = fields.get(11..15)?.iter().try_fold(0_u64, |total, ticks| {
                Some(total.saturating_add(ticks.parse::<u64>().ok()?))
            })?;
            Some(Self {
                pid,
                parent: fields.get(1)?.parse().ok()?,
                group: fields.get(2)?.parse().ok()?,
                started: fields.get(19)?.parse().ok()?,
                cpu,
                shell: matches!(comm, "sh" | "bash" | "dash" | "zsh" | "fish" | "ksh" | "ash"),
            })
        }

        fn usage(&self) -> ResourceUsage {
            let io = fs::read_to_string(format!("/proc/{}/io", self.pid)).unwrap_or_default();
            let io_bytes = io.lines().filter_map(|line| {
                line.strip_prefix("read_bytes: ")
                    .or_else(|| line.strip_prefix("write_bytes: "))?
                    .parse::<u64>().ok()
            }).fold(0_u64, u64::saturating_add);
            ResourceUsage { cpu: self.cpu, io_bytes }
        }
    }

    pub(super) fn sample_command(
        sample: &mut UsageSample,
        leader: i32,
        kind: CommandKind,
        cgroup: Option<&Cgroup>,
    ) {
        let Some(root) = Process::read(leader) else { return };
        let include_root = kind == CommandKind::ServerExec;
        if include_root {
            if let Some(cgroup) = cgroup {
                if let Some(usage) = cgroup.usage() {
                    sample.insert((leader, root.started), usage);
                }
                return;
            }
            sample.insert((leader, root.started), root.usage());
        }

        let Ok(entries) = fs::read_dir("/proc") else { return };
        let mut children: HashMap<i32, Vec<Process>> = HashMap::new();
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse().ok()) else { continue };
            let Some(process) = Process::read(pid) else { continue };
            children.entry(process.parent).or_default().push(process);
        }

        // Native backends share their cgroup with runtime and MCP helpers.
        // Attribute activity to shell subtrees and descendants with their own group.
        let mut pending = vec![(leader, include_root)];
        while let Some((parent, include_parent)) = pending.pop() {
            for process in children.remove(&parent).into_iter().flatten() {
                let include = include_parent || process.shell || process.group != root.group;
                if include {
                    sample.insert((process.pid, process.started), process.usage());
                }
                pending.push((process.pid, include));
            }
        }
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

    fn host_total_ram() -> Option<u64> {
        fs::read_to_string("/proc/meminfo").ok().and_then(|contents| {
            contents.lines().find_map(|line| {
                let value = line.strip_prefix("MemTotal:")?.split_whitespace().next()?;
                value.parse::<u64>().ok()?.checked_mul(1024)
            })
        })
    }

    fn job_memory_limits(limit: Option<u64>, env_limit_mib: Option<&str>, total_ram: Option<u64>) -> (u64, u64) {
        if let Some(bytes) = limit {
            return (bytes / 2, bytes);
        }
        if let Some(bytes) = env_limit_mib
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .and_then(|value| value.checked_mul(MIB))
        {
            return (bytes / 2, bytes);
        }
        default_job_memory_limits_for_ram(total_ram)
    }

    fn default_job_memory_limits_for_ram(total_ram: Option<u64>) -> (u64, u64) {
        let Some(total_ram) = total_ram else {
            return (DEFAULT_HIGH_MIB * MIB, DEFAULT_MAX_MIB * MIB);
        };
        let high = (total_ram / 2 / MIB).max(1) * MIB;
        let max = (total_ram.saturating_mul(3) / 4 / MIB).max(high / MIB) * MIB;
        (high.min(DEFAULT_HIGH_MIB * MIB), max.min(DEFAULT_MAX_MIB * MIB))
    }

    impl Cgroup {
        pub(super) fn prepare(command: &mut Command, kind: &str, limit: Option<u64>) -> io::Result<Option<Self>> {
            let Some(root) = root() else { return Ok(None) };
            let generation = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("job-{kind}-{}-{generation}", std::process::id()));
            let result = (|| -> io::Result<Self> {
                fs::create_dir(&dir)?;
                // Pin the newly created cgroup so cleanup cannot signal a replacement.
                let kill_file = match OpenOptions::new().write(true).open(dir.join("cgroup.kill")) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = fs::remove_dir(&dir);
                        return Err(error);
                    }
                };
                let cgroup = Self { dir: dir.clone(), kill_file, cleaning: AtomicBool::new(false) };
                let (high, max) = job_memory_limits(
                    limit,
                    std::env::var("FUZUKUE_JOB_MEMORY_MIB").ok().as_deref(),
                    host_total_ram(),
                );
                fs::write(dir.join("memory.high"), high.to_string())?;
                fs::write(dir.join("memory.max"), max.to_string())?;
                fs::write(dir.join("memory.oom.group"), "1")?;
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

        fn usage(&self) -> Option<ResourceUsage> {
            let cpu = fs::read_to_string(self.dir.join("cpu.stat")).ok()?;
            let cpu = cpu.lines().find_map(|line| line.strip_prefix("usage_usec "))?.parse().ok()?;
            // The I/O controller may not be available in the delegated subtree.
            let io = fs::read_to_string(self.dir.join("io.stat")).unwrap_or_default();
            let io_bytes = io.split_whitespace().filter_map(|field| {
                field.strip_prefix("rbytes=").or_else(|| field.strip_prefix("wbytes="))?.parse::<u64>().ok()
            }).fold(0_u64, u64::saturating_add);
            Some(ResourceUsage { cpu, io_bytes })
        }

        pub(super) fn kill(&self) -> io::Result<()> {
            let membership = fs::read_to_string("/proc/self/cgroup")?;
            let path = membership_path(&membership)
                .ok_or_else(|| io::Error::other("cannot determine the server cgroup"))?;
            if Path::new("/sys/fs/cgroup").join(path).starts_with(&self.dir) {
                return Err(io::Error::other("refusing to kill the server cgroup"));
            }
            (&self.kill_file).write_all(b"1")?;
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
    fn default_job_memory_limits_scale_to_host_ram() {
        assert_eq!(job_memory_limits(None, None, None), (16 * 1024 * MIB, 24 * 1024 * MIB));
        assert_eq!(job_memory_limits(None, None, Some(8 * 1024 * MIB)), (4 * 1024 * MIB, 6 * 1024 * MIB));
        assert_eq!(job_memory_limits(None, None, Some(64 * 1024 * MIB)), (16 * 1024 * MIB, 24 * 1024 * MIB));
        assert_eq!(job_memory_limits(Some(8 * 1024 * MIB), Some("20480"), Some(64 * 1024 * MIB)), (4 * 1024 * MIB, 8 * 1024 * MIB));
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
