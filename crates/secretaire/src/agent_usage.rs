//! Bounded, workspace-local invocation history. Only the brief title is retained.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Result;
use fuzukue_proto::{
    AgentCaller, AgentInvocation, AgentOutcome, AgentPeriodStats, AgentSelection, AgentStats,
    ServerEvent,
};

use crate::Workspace;

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const HISTORY_LIMIT: usize = 100;
const DAY_MS: i64 = 86_400_000;
const CURRENT: &str = "agent-usage.jsonl";
const PREVIOUS: &str = "agent-usage.previous.jsonl";
const LAST_USED: &str = "agent-last-used.json";

#[derive(Default)]
pub(crate) struct Log(Mutex<()>);

#[derive(Default)]
pub(crate) struct RunMetadata {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

pub(crate) struct Invocation {
    entry: AgentInvocation,
    timer: Instant,
}

impl Invocation {
    pub fn new(
        app: &Workspace,
        session: &str,
        caller: Option<AgentCaller>,
        agent: &str,
        job: &str,
        title: &str,
        selection: Option<AgentSelection>,
    ) -> Self {
        Self {
            entry: AgentInvocation {
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                workspace: app.id.clone(),
                session: session.into(),
                caller,
                agent: agent.into(),
                selection,
                duration_ms: 0,
                outcome: AgentOutcome::Failed,
                tokens: None,
                job: job.into(),
                routing: None,
                attempts: Vec::new(),
                title: title
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect(),
            },
            timer: Instant::now(),
        }
    }

    pub fn selected(&mut self, selection: AgentSelection) {
        self.entry.selection = Some(selection);
    }

    pub fn routed(&mut self, decision: fuzukue_proto::TierDecision, attempts: Vec<fuzukue_proto::TierAttempt>) {
        self.entry.routing = Some(decision);
        if let Some(last) = attempts.last() {
            self.entry.selection = Some(last.selection.clone());
        }
        self.entry.tokens = attempts.iter().filter_map(|attempt| attempt.tokens)
            .reduce(u64::saturating_add);
        self.entry.attempts = attempts;
    }

    pub fn finish<T>(mut self, app: &Workspace, result: &Result<T>, metadata: RunMetadata) {
        self.entry.duration_ms = self.timer.elapsed().as_millis() as u64;
        self.entry.outcome = crate::tiers::outcome(result);
        if let Some(selection) = &mut self.entry.selection {
            selection.model = metadata.model.or_else(|| selection.model.take());
            selection.effort = metadata.effort.or_else(|| selection.effort.take());
        }
        if self.entry.attempts.is_empty() {
            self.entry.tokens = metadata.tokens;
        }
        match app.agent_usage.append(&app.state_dir, &self.entry) {
            Ok(()) => app.hub.publish_snapshot(ServerEvent::AgentUsageChanged),
            Err(error) => tracing::error!(
                job = self.entry.job,
                "作業役の履歴を保存できない: {error:#}"
            ),
        }
    }
}

fn entries(path: &Path) -> Result<Vec<AgentInvocation>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).split(b'\n') {
        // An interrupted append can leave an incomplete line.
        if let Ok(entry) = serde_json::from_slice(&line?) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn last_used(dir: &Path) -> Result<BTreeMap<String, i64>> {
    match fs::read(dir.join(LAST_USED)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

fn update_last_used(last: &mut BTreeMap<String, i64>, entry: &AgentInvocation) {
    let time = last
        .entry(entry.agent.clone())
        .or_insert(entry.started_at_ms);
    *time = (*time).max(entry.started_at_ms);
}

impl Log {
    fn append(&self, dir: &Path, entry: &AgentInvocation) -> Result<()> {
        let mut bytes = serde_json::to_vec(entry)?;
        bytes.push(b'\n');
        let _lock = self.0.lock().unwrap();
        fs::create_dir_all(dir)?;
        let current = dir.join(CURRENT);
        if fs::metadata(&current).is_ok_and(|m| m.len() + bytes.len() as u64 > MAX_BYTES) {
            let previous = dir.join(PREVIOUS);
            let mut last = last_used(dir)?;
            for old in entries(&previous)? {
                update_last_used(&mut last, &old);
            }
            let mut temp = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(&mut temp, &last)?;
            temp.persist(dir.join(LAST_USED))?;
            fs::rename(&current, previous)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&current)?;
        // Delimit a torn last line before appending the next record.
        if file.metadata()?.len() > 0 {
            use std::io::{Read, Seek, SeekFrom};
            let mut tail = File::open(&current)?;
            tail.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            tail.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(&bytes)?;
        Ok(())
    }

    pub fn history(
        &self,
        dir: &Path,
        agent: Option<&str>,
        session: Option<&str>,
    ) -> Result<Vec<AgentInvocation>> {
        let _lock = self.0.lock().unwrap();
        let mut history = entries(&dir.join(CURRENT))?;
        history.extend(entries(&dir.join(PREVIOUS))?);
        history.retain(|entry| {
            agent.is_none_or(|name| name == entry.agent)
                && session.is_none_or(|id| id == entry.session)
        });
        history.sort_by_key(|entry| std::cmp::Reverse(entry.started_at_ms));
        history.truncate(HISTORY_LIMIT);
        Ok(history)
    }

    pub fn stats(
        &self,
        dir: &Path,
        names: impl IntoIterator<Item = String>,
    ) -> Result<BTreeMap<String, AgentStats>> {
        let _lock = self.0.lock().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let mut last = last_used(dir)?;
        let mut history = entries(&dir.join(CURRENT))?;
        history.extend(entries(&dir.join(PREVIOUS))?);
        let mut by_agent = BTreeMap::<_, Vec<_>>::new();
        for entry in &history {
            update_last_used(&mut last, entry);
            if entry.started_at_ms >= now - 30 * DAY_MS {
                by_agent
                    .entry(entry.agent.as_str())
                    .or_default()
                    .push(entry);
            }
        }
        let mut names: std::collections::BTreeSet<_> = names.into_iter().collect();
        names.extend(by_agent.keys().map(|name| (*name).to_owned()));
        Ok(names.into_iter()
            .map(|name| {
                let recent = by_agent.get(name.as_str()).map_or(&[][..], Vec::as_slice);
                let seven = recent
                    .iter()
                    .copied()
                    .filter(|entry| entry.started_at_ms >= now - 7 * DAY_MS);
                let stats = AgentStats {
                    days_7: period(seven),
                    days_30: period(recent.iter().copied()),
                    last_used_at_ms: last.get(&name).copied(),
                };
                (name, stats)
            })
            .collect())
    }
}

fn period<'a>(entries: impl Iterator<Item = &'a AgentInvocation>) -> AgentPeriodStats {
    let mut durations = Vec::new();
    let mut successes = 0;
    let mut tokens = 0u64;
    let mut tiers = BTreeMap::<_, fuzukue_proto::TierStats>::new();
    for entry in entries {
        durations.push(entry.duration_ms);
        successes += usize::from(entry.outcome == AgentOutcome::Ok);
        tokens = tokens.saturating_add(entry.tokens.unwrap_or_default());
        for attempt in &entry.attempts {
            let stats = tiers.entry(attempt.tier).or_default();
            stats.attempts += 1;
            stats.passes += usize::from(attempt.outcome == AgentOutcome::Ok);
            stats.stalls += usize::from(attempt.outcome == AgentOutcome::Stalled);
            stats.total_tokens = stats.total_tokens.saturating_add(attempt.tokens.unwrap_or_default());
            if let Some(cost) = attempt.cost_usd {
                *stats.cost_usd.get_or_insert(0.0) += cost;
            }
            if let Some(cost) = attempt.weighted_tokens {
                *stats.weighted_tokens.get_or_insert(0.0) += cost;
            }
        }
    }
    let uses = durations.len();
    durations.sort_unstable();
    AgentPeriodStats {
        uses,
        success_rate: (uses > 0).then(|| successes as f64 / uses as f64),
        median_duration_ms: (uses > 0)
            .then(|| durations[(uses - 1) / 2].midpoint(durations[uses / 2])),
        total_tokens: tokens,
        tiers,
    }
}

pub(crate) fn never_used(stats: &BTreeMap<String, AgentStats>) -> Vec<String> {
    stats
        .iter()
        .filter(|(_, stat)| stat.last_used_at_ms.is_none())
        .map(|(name, _)| name.clone())
        .collect()
}

impl Workspace {
    pub(crate) fn record_partner_usage(
        &self, session: &str, backend: fuzukue_proto::AgentBackend,
        kind: crate::front_desk::TurnKind, outcome: AgentOutcome, duration_ms: Option<u64>,
        details: Option<&fuzukue_proto::RunDetails>,
    ) {
        let (agent, title) = if matches!(kind, crate::front_desk::TurnKind::WorkerDigest) {
            ("worker_digest", harness_text!("Worker digest", "作業役のダイジェスト"))
        } else {
            ("front_desk", harness_text!("Front desk turn", "応対役のターン"))
        };
        let now = chrono::Utc::now().timestamp_millis();
        let duration_ms = duration_ms.unwrap_or_default();
        let entry = AgentInvocation {
            started_at_ms: now.saturating_sub(i64::try_from(duration_ms).unwrap_or(i64::MAX)),
            workspace: self.id.clone(),
            session: session.into(),
            caller: None,
            agent: agent.into(),
            selection: Some(details.map_or(AgentSelection { backend, model: None, effort: None }, |details| details.selection.clone())),
            duration_ms,
            outcome,
            tokens: details.and_then(|details| details.tokens),
            job: crate::mail::session_key(),
            title: title.into(),
            routing: None,
            attempts: Vec::new(),
        };
        match self.agent_usage.append(&self.state_dir, &entry) {
            Ok(()) => self.hub.publish_snapshot(ServerEvent::AgentUsageChanged),
            Err(error) => tracing::warn!("Could not record {agent} usage: {error:#}"),
        }
    }

    pub(crate) async fn agent_stats(self: &Arc<Self>) -> Result<BTreeMap<String, AgentStats>> {
        let names = self
            .agent_catalog()
            .await?
            .into_keys()
            .chain(["worker", "summary", "consult", "thread_title", "front_desk", "worker_digest"].map(str::to_owned));
        let app = self.clone();
        tokio::task::spawn_blocking(move || app.agent_usage.stats(&app.state_dir, names)).await?
    }
}
