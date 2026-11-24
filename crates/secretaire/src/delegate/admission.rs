//! The server-wide queue for delegated work.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{GoalPace, GoalParallelLimits, LocalizedMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::touches::WriteSet;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Overrides {
    max_parallel: Option<usize>,
    max_parallel_hard: Option<usize>,
    goal_normal_running: Option<usize>,
    goal_economy_running: Option<usize>,
    max_load_per_cpu: Option<f64>,
    min_available_memory_mib: Option<u64>,
    pub(crate) hold_planner_during_update: Option<bool>,
}

impl Overrides {
    pub(crate) fn load_workspace(project: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            delegate: Overrides,
        }
        let path = fuzukue_proto::compat::workspace_config(project);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<File>(&text).map(|file| file.delegate)
                .with_context(|| format!("read delegate configuration from {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Config {
    max_parallel: usize,
    max_parallel_hard: usize,
    goal_normal_running: usize,
    goal_economy_running: usize,
    max_load_per_cpu: f64,
    min_available_memory_mib: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            max_parallel_hard: 12,
            goal_normal_running: GoalParallelLimits::default().normal,
            goal_economy_running: GoalParallelLimits::default().economy,
            max_load_per_cpu: 1.5,
            min_available_memory_mib: 4096,
        }
    }
}

impl Config {
    fn apply(&mut self, overrides: Overrides) {
        self.max_parallel = overrides.max_parallel.unwrap_or(self.max_parallel);
        self.max_parallel_hard = overrides
            .max_parallel_hard
            .unwrap_or(self.max_parallel_hard);
        self.goal_normal_running = overrides.goal_normal_running.unwrap_or(self.goal_normal_running);
        self.goal_economy_running = overrides.goal_economy_running.unwrap_or(self.goal_economy_running);
        self.max_load_per_cpu = overrides.max_load_per_cpu.unwrap_or(self.max_load_per_cpu);
        self.min_available_memory_mib = overrides
            .min_available_memory_mib
            .unwrap_or(self.min_available_memory_mib);
    }

    pub(crate) fn load(project: &Path, server: Overrides) -> Result<Self> {
        let mut config = Self::default();
        config.apply(server);
        config.apply(Overrides::load_workspace(project)?);
        ensure!(
            config.max_parallel > 0 && config.max_parallel <= config.max_parallel_hard,
            "[delegate] requires 1 <= max_parallel <= max_parallel_hard"
        );
        ensure!(
            config.goal_normal_running > 0 && config.goal_economy_running > 0,
            "[delegate] goal running limits must be positive"
        );
        ensure!(
            config.max_load_per_cpu.is_finite() && config.max_load_per_cpu >= 0.0,
            "delegate.max_load_per_cpu must be finite and nonnegative"
        );
        ensure!(
            config.min_available_memory_mib <= u64::MAX / 1024,
            "delegate.min_available_memory_mib is too large"
        );
        Ok(config)
    }
}

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Headroom {
    load: Option<(f64, usize)>,
    available_kib: Option<u64>,
}

impl Headroom {
    #[cfg(test)]
    fn read() -> Self {
        // Test runners share host resources. Load thresholds use explicit samples in tests.
        Self::default()
    }

    #[cfg(not(test))]
    fn read() -> Self {
        let load = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse().ok())
            .zip(std::thread::available_parallelism().ok().map(usize::from));
        let available_kib = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("MemAvailable:"))?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            });
        Self {
            load,
            available_kib,
        }
    }

    fn blocked(&self, config: Config) -> Option<LoadWait> {
        if let Some((load, cpus)) = self.load {
            let limit = config.max_load_per_cpu * cpus as f64;
            if config.max_load_per_cpu > 0.0 && load >= limit {
                return Some(LoadWait::Cpu { load, limit });
            }
        }
        let minimum = config.min_available_memory_mib * 1024;
        if let Some(available) = self.available_kib {
            if minimum > 0 && available <= minimum {
                return Some(LoadWait::Memory { available, minimum });
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum LoadWait {
    Cpu { load: f64, limit: f64 },
    Memory { available: u64, minimum: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WaitReason {
    SoftCap {
        running: usize,
        limit: usize,
        unknown_write_set: bool,
    },
    HardCap {
        running: usize,
        limit: usize,
    },
    Overlap {
        job: String,
        title: String,
        path: Option<String>,
    },
    Load {
        load: f64,
        limit: f64,
    },
    Memory {
        available_kib: u64,
        minimum_kib: u64,
    },
    Draining,
    UsagePause {
        backend: Option<fuzukue_proto::AgentBackend>,
        resume_at: Option<i64>,
    },
    BackendBusy {
        backend: fuzukue_proto::AgentBackend,
    },
    PlannerPace {
        pace: GoalPace,
        running: usize,
        limit: usize,
        delay_ms: Option<u64>,
    },
    Earlier {
        job: String,
        title: String,
    },
    SlotsHeld,
    QueueStorage,
}

impl WaitReason {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::SoftCap {
                running,
                limit,
                unknown_write_set,
            } => {
                let suffix = if *unknown_write_set {
                    harness_text!("; write set unknown", "・変更予定のパスが不明")
                } else {
                    ""
                };
                harness_format!(
                    "soft cap reached ({running}/{limit} running{suffix})",
                    "同時実行の通常上限（{running}/{limit} 件実行中{suffix}）"
                )
            }
            Self::HardCap { running, limit } => harness_format!(
                "hard cap reached ({running}/{limit} running)",
                "同時実行の最大上限（{running}/{limit} 件実行中）"
            ),
            Self::Overlap {
                title,
                path: Some(path),
                ..
            } => harness_format!(
                "waiting for {title} (overlapping {path})",
                "{title} の終了待ち（変更予定が重複: {path}）"
            ),
            Self::Overlap {
                title, path: None, ..
            } => harness_format!(
                "waiting for {title} (its write set is unknown)",
                "{title} の終了待ち（その作業の変更予定が不明）"
            ),
            Self::Load { load, limit } => harness_format!(
                "CPU load {load:.2}/{limit:.2}",
                "CPU 負荷 {load:.2}/{limit:.2}"
            ),
            Self::Memory {
                available_kib,
                minimum_kib,
            } => harness_format!(
                "available memory {} MiB (minimum {} MiB)",
                "空きメモリ {} MiB（下限 {} MiB）",
                available_kib / 1024,
                minimum_kib / 1024
            ),
            Self::Draining => harness_text!(
                "server update is draining",
                "サーバー更新のため新規開始を保留中"
            )
            .into(),
            Self::UsagePause { backend, resume_at } => {
                let label = backend
                    .map(|backend| backend.label())
                    .unwrap_or(harness_text!("goal", "目標"));
                let message =
                    harness_format!("{label} usage pause", "{label} の使用量による一時停止");
                match resume_at.and_then(|time| chrono::DateTime::from_timestamp(time, 0)) {
                    Some(time) => {
                        let reset = time.format("%Y-%m-%d %H:%M UTC");
                        harness_format!("{message} (resumes {reset})", "{message}（再開 {reset}）")
                    }
                    None => message,
                }
            }
            Self::BackendBusy { backend } => {
                let label = backend.label();
                harness_format!("{label} backend is initializing", "{label} の起動待ち")
            }
            Self::PlannerPace {
                running,
                limit,
                delay_ms: Some(_), ..
            } => format!("goal pacing between starts ({running}/{limit} running)"),
            Self::PlannerPace {
                running,
                limit,
                delay_ms: None, ..
            } => format!("goal pacing limit reached ({running}/{limit} running)"),
            Self::Earlier { title, .. } => {
                harness_format!("waiting for {title} to start first", "{title} の開始待ち")
            }
            Self::SlotsHeld => harness_text!(
                "waiting for worker slots to be released",
                "作業枠の解放待ち"
            )
            .into(),
            Self::QueueStorage => harness_text!(
                "waiting to claim the saved assignment",
                "保存済みの依頼を開始できるまで待機"
            )
            .into(),
        }
    }

    pub(crate) fn localized(&self) -> Option<LocalizedMessage> {
        let Self::PlannerPace { pace, running, limit, delay_ms } = self else {
            return None;
        };
        Some(LocalizedMessage::new(
            if delay_ms.is_some() { "server-parallel-start-delay" } else { "server-parallel-limit" },
            [
                ("setting", match pace {
                    GoalPace::Full => "full",
                    GoalPace::Normal => "normal",
                    GoalPace::Economy => "economy",
                }.into()),
                ("running", running.to_string()),
                ("limit", limit.to_string()),
            ],
        ))
    }

    pub(crate) fn same_decision(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Load { .. }, Self::Load { .. }) | (Self::Memory { .. }, Self::Memory { .. }) => {
                true
            }
            (
                Self::PlannerPace {
                    pace,
                    running,
                    limit,
                    delay_ms,
                },
                Self::PlannerPace {
                    pace: previous_pace,
                    running: previous_running,
                    limit: previous_limit,
                    delay_ms: previous_delay,
                },
            ) => {
                pace == previous_pace
                    && running == previous_running
                    && limit == previous_limit
                    && delay_ms.is_some() == previous_delay.is_some()
            }
            _ => self == previous,
        }
    }

    pub(crate) fn blocking_job(&self) -> Option<&str> {
        match self {
            Self::Overlap { job, .. } | Self::Earlier { job, .. } => Some(job),
            _ => None,
        }
    }
}

impl From<LoadWait> for WaitReason {
    fn from(reason: LoadWait) -> Self {
        match reason {
            LoadWait::Cpu { load, limit } => Self::Load { load, limit },
            LoadWait::Memory { available, minimum } => Self::Memory {
                available_kib: available,
                minimum_kib: minimum,
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Ready,
    Running,
}

struct Entry {
    id: String,
    label: String,
    writes: Arc<WriteSet>,
    phase: Phase,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Entry>,
    held: usize,
    sample: Option<(Instant, Headroom)>,
}

impl Queue {
    fn can_run(&self, job: &Entry, config: Config) -> Result<(), WaitReason> {
        let running = || {
            self.jobs
                .iter()
                .filter(|entry| entry.phase == Phase::Running)
        };
        let count = running().count();
        if self.held > 0 {
            return Err(WaitReason::SlotsHeld);
        }
        if count < config.max_parallel {
            return Ok(());
        }
        if count >= config.max_parallel_hard {
            return Err(WaitReason::HardCap {
                running: count,
                limit: config.max_parallel_hard,
            });
        }
        if !job.writes.known() {
            return Err(WaitReason::SoftCap {
                running: count,
                limit: config.max_parallel,
                unknown_write_set: true,
            });
        }
        for active in running() {
            if let Some(path) = job.writes.overlap(&active.writes) {
                return Err(WaitReason::Overlap {
                    job: active.id.clone(),
                    title: active.label.clone(),
                    path: path.map(str::to_owned),
                });
            }
        }
        Ok(())
    }

    fn first_ready(&self, config: Config) -> Option<&Entry> {
        self.jobs
            .iter()
            .find(|entry| entry.phase == Phase::Ready && self.can_run(entry, config).is_ok())
    }

    fn load_wait(&mut self, config: Config) -> Option<LoadWait> {
        if self
            .sample
            .as_ref()
            .is_none_or(|(read_at, _)| read_at.elapsed() >= SAMPLE_INTERVAL)
        {
            self.sample = Some((Instant::now(), Headroom::read()));
        }
        self.sample
            .as_ref()
            .and_then(|(_, sample)| sample.blocked(config))
    }
}

pub(crate) struct Scheduler {
    config: Config,
    queue: Mutex<Queue>,
    changed: watch::Sender<()>,
}

impl Scheduler {
    pub(crate) fn new(config: Config) -> Self {
        let (changed, _) = watch::channel(());
        Self {
            config,
            queue: Mutex::default(),
            changed,
        }
    }

    pub(crate) fn goal_parallel_limits(&self) -> GoalParallelLimits {
        GoalParallelLimits {
            normal: self.config.goal_normal_running,
            economy: self.config.goal_economy_running,
        }
    }

    pub(super) fn rename(&self, job: &str, label: &str) {
        if let Some(entry) = self.queue.lock().unwrap().jobs.iter_mut().find(|entry| entry.id == job) {
            label.clone_into(&mut entry.label);
            self.changed.send_replace(());
        }
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        id: String,
        label: String,
        writes: Arc<WriteSet>,
    ) -> Request {
        self.queue.lock().unwrap().jobs.push_back(Entry {
            id: id.clone(),
            label,
            writes,
            phase: Phase::Queued,
        });
        Request {
            scheduler: self.clone(),
            id,
        }
    }

    #[cfg(test)]
    pub(crate) async fn hold_all_slots(self: &Arc<Self>) -> Hold {
        let mut changed = self.changed.subscribe();
        self.queue.lock().unwrap().held += 1;
        self.changed.send_replace(());
        let hold = Hold(self.clone());
        while self
            .queue
            .lock()
            .unwrap()
            .jobs
            .iter()
            .any(|job| job.phase == Phase::Running)
        {
            let _ = changed.changed().await;
        }
        hold
    }
}

#[cfg(test)]
pub(crate) struct Hold(Arc<Scheduler>);

#[cfg(test)]
impl Drop for Hold {
    fn drop(&mut self) {
        self.0.queue.lock().unwrap().held -= 1;
        self.0.changed.send_replace(());
    }
}

pub(crate) struct Request {
    scheduler: Arc<Scheduler>,
    id: String,
}

impl Request {
    pub(crate) async fn acquire(&self, mut waiting: impl FnMut(WaitReason)) -> Slot<'_> {
        let mut changed = self.scheduler.changed.subscribe();
        let mut ready = Ready(self, true);
        self.set_phase(Phase::Ready);
        let mut last_reason = None;
        loop {
            let reason = match self.try_acquire() {
                Ok(slot) => {
                    ready.1 = false;
                    return slot;
                }
                Err(reason) => reason,
            };
            let sample = matches!(reason, WaitReason::Load { .. } | WaitReason::Memory { .. });
            if last_reason.as_ref() != Some(&reason) {
                waiting(reason.clone());
                last_reason = Some(reason);
            }
            tokio::select! {
                _ = changed.changed() => {}
                _ = tokio::time::sleep(SAMPLE_INTERVAL), if sample => {}
            }
        }
    }

    fn try_acquire(&self) -> Result<Slot<'_>, WaitReason> {
        let mut queue = self.scheduler.queue.lock().unwrap();
        let config = self.scheduler.config;
        let index = queue
            .jobs
            .iter()
            .position(|entry| entry.id == self.id)
            .unwrap();
        queue.can_run(&queue.jobs[index], config)?;
        if let Some(load) = queue.load_wait(config) {
            return Err(load.into());
        }
        let first = queue.first_ready(config).expect("this request is ready");
        if first.id != self.id {
            return Err(WaitReason::Earlier {
                job: first.id.clone(),
                title: first.label.clone(),
            });
        }
        queue.jobs[index].phase = Phase::Running;
        self.scheduler.changed.send_replace(());
        Ok(Slot(self))
    }

    fn set_phase(&self, phase: Phase) {
        let mut queue = self.scheduler.queue.lock().unwrap();
        queue
            .jobs
            .iter_mut()
            .find(|entry| entry.id == self.id)
            .unwrap()
            .phase = phase;
        self.scheduler.changed.send_replace(());
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        self.scheduler
            .queue
            .lock()
            .unwrap()
            .jobs
            .retain(|entry| entry.id != self.id);
        self.scheduler.changed.send_replace(());
    }
}

// An admission attempt can be cancelled while a planner ticket waits.
struct Ready<'a>(&'a Request, bool);

impl Drop for Ready<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.set_phase(Phase::Queued);
        }
    }
}

pub(crate) struct Slot<'a>(&'a Request);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.set_phase(Phase::Queued);
    }
}

#[cfg(test)]
mod tests;
