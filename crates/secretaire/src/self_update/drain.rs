//! Durable admission records and recovery at the backend submission boundary.
mod diagnostics;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Workspace;

const FILE: &str = "held-work.json";
const INTERRUPTED: &str =
    "The previous server stopped after claiming this work. Check the thread and worktree before retrying.";
const PREPARATION_INTERRUPTED: &str = "The previous server stopped before submitting this work.";
const STALLED_DELIVERY_MS: i64 = 5 * 60 * 1000;
pub(super) const ORPHAN_APPROVAL_AGE: Duration = Duration::from_secs(600);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Work {
    Delegate(crate::delegate::Assignment),
    Build {
        deck: String,
    },
    AutomaticReview {
        project: PathBuf,
        before: crate::auto_review::Snapshot,
        #[serde(default)]
        kind: Option<crate::reviewers::Kind>,
    },
    HumanReview {
        root: PathBuf,
        baseline: crate::human_edits::DeckBaseline,
    },
    Desk(crate::delegate::DeskTurn),
    Claude(crate::claude::PendingPrompt),
    Prompt {
        text: String,
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply_blocks: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply_interactions: bool,
    },
    Compact { instructions: String },
    WatchWake {
        watch: String,
        text: String,
    },
    GitHubWake {
        events: Vec<fuzukue_proto::GitHubActivity>,
    },
    Usage(Vec<fuzukue_proto::UsageWindow>),
    Checkpoint {
        result: String,
        directory: PathBuf,
    },
    LegacyPlan(fuzukue_proto::Plan),
    LegacyGitHub(GitHubHold),
    LegacyWatch {
        watch: String,
        paused: Option<bool>,
        digest: String,
        outbox: serde_json::Value,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct GitHubHold {
    pub policy: fuzukue_proto::GitHubPolicy,
    pub classes: Option<fuzukue_proto::GitHubPolicies>,
    pub pending: Vec<String>,
    pub responses: Vec<Vec<String>>,
}

impl Work {
    fn tracks_submission(&self) -> bool {
        use crate::delegate::DeskTurn;
        matches!(self, Self::Desk(DeskTurn::Report { .. } | DeskTurn::WorkshopReport { approval_id: Some(_), .. })
            | Self::Claude(_))
    }

    pub(crate) fn is_planner(&self) -> bool {
        match self {
            Self::Delegate(request) => request.is_planner(),
            Self::Desk(turn) => turn.is_plan_review(),
            _ => false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub session: String,
    pub work: Work,
    pub claimed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    planner_retried: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    // Only this phase proves that retrying a claimed record cannot duplicate submission.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    preparing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<DeliveryRetry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct DeliveryRetry {
    first_refused_at: i64,
    next_attempt_at: i64,
    attempts: u32,
    reason: String,
}

impl Record {
    fn pending(id: String, session: &str, work: Work) -> Self {
        Self {
            id, session: session.into(), work, claimed: false, planner_retried: false,
            created_at: Some(delivery_now()), review: None, preparing: false, retry: None,
        }
    }

    pub(crate) fn ready(&self, now: i64) -> bool {
        !self.claimed && self.review.is_none()
            && self.retry.as_ref().is_none_or(|retry| retry.next_attempt_at <= now)
    }

    fn requeue(&mut self, reason: String, now: i64) {
        let first_refused_at = self.retry.as_ref().map_or(now, |retry| retry.first_refused_at);
        let attempts = self.retry.as_ref().map_or(1, |retry| retry.attempts.saturating_add(1));
        let delay_secs = (1_i64 << (attempts - 1).min(5)).min(30);
        self.retry = Some(DeliveryRetry {
            first_refused_at, next_attempt_at: now + delay_secs * 1000, attempts, reason,
        });
        self.claimed = false;
        self.preparing = false;
        self.review = None;
    }

    pub(crate) fn stalled_report(&self, job_id: &str, now: i64) -> Option<serde_json::Value> {
        let retry = self.retry.as_ref()?;
        if self.claimed || self.review.is_some() || now - retry.first_refused_at < STALLED_DELIVERY_MS
            || !matches!(&self.work, Work::Desk(crate::delegate::DeskTurn::Report { job, .. }) if job == job_id)
        {
            return None;
        }
        Some(serde_json::json!({
            "id": self.id,
            "session": self.session,
            "pending_secs": (now - retry.first_refused_at) / 1000,
            "next_attempt_at": retry.next_attempt_at,
            "attempts": retry.attempts,
            "reason": retry.reason,
        }))
    }
}

enum Completion {
    Finish(Option<String>),
    DeliveryFailed { reason: String, at: i64 },
}

pub(crate) fn delivery_now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Default)]
struct State {
    records: Option<Vec<Record>>,
    errors: BTreeMap<String, String>,
    completions: BTreeMap<String, Completion>,
    pending_writes: Vec<Record>,
    usage_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Journal(Mutex<State>);

impl Journal {
    pub fn exists(dir: &Path) -> bool {
        dir.join(FILE).exists()
    }

    fn load(dir: &Path, state: &mut State) -> Result<()> {
        if state.records.is_none() {
            let path = dir.join(FILE);
            state.records = Some(match fs::read(&path) {
                Ok(bytes) => {
                    serde_json::from_slice(&bytes).with_context(|| path.display().to_string())?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error).with_context(|| path.display().to_string()),
            });
        }
        Ok(())
    }

    fn store(dir: &Path, records: &[Record]) -> Result<()> {
        fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut file, records)?;
        file.flush()?;
        crate::durable::persist(file, dir.join(FILE))?;
        Ok(())
    }

    fn edit<T>(
        &self,
        dir: &Path,
        session: &str,
        edit: impl FnOnce(&mut Vec<Record>) -> T,
    ) -> Result<T> {
        let mut state = self.0.lock().unwrap();
        let result = (|| {
            Self::load(dir, &mut state)?;
            let previous = state.records.as_ref().unwrap();
            let mut records = previous.clone();
            let result = edit(&mut records);
            Self::store(dir, &records)?;
            state.records = Some(records);
            Ok::<_, anyhow::Error>(result)
        })();
        match &result {
            Ok(_) => {
                state.errors.remove(session);
            }
            Err(error) => {
                state.errors.insert(session.into(), format!("{error:#}"));
            }
        }
        result
    }

    pub fn records(&self, dir: &Path) -> Result<Vec<Record>> {
        let mut state = self.0.lock().unwrap();
        Self::load(dir, &mut state)?;
        Ok(state.records.clone().unwrap())
    }

    pub(crate) fn queued_planner_count(&self, dir: &Path) -> usize {
        let mut state = self.0.lock().unwrap();
        if Self::load(dir, &mut state).is_err() {
            return 0;
        }
        state.records.as_ref().unwrap().iter()
            .filter(|record| !record.claimed && record.review.is_none() && record.work.is_planner())
            .count()
    }

    pub fn save_usage(&self, dir: &Path, windows: Vec<fuzukue_proto::UsageWindow>) {
        let mut state = self.0.lock().unwrap();
        let result = (|| {
            Self::load(dir, &mut state)?;
            let records = state.records.as_ref().unwrap();
            let last = records
                .iter()
                .find_map(|record| match &record.work {
                    Work::Usage(windows) => Some(windows.as_slice()),
                    _ => None,
                })
                .unwrap_or_default();
            if last == windows.as_slice() {
                return Ok(());
            }
            let mut records = records.clone();
            records.retain(|record| !matches!(record.work, Work::Usage(_)));
            if !windows.is_empty() {
                records.push(Record::pending("usage-windows".into(), "", Work::Usage(windows)));
            }
            Self::store(dir, &records)?;
            state.records = Some(records);
            Ok::<_, anyhow::Error>(())
        })();
        state.usage_error = result
            .err()
            .map(|error| format!("saving usage windows: {error:#}"));
    }

    pub fn enqueue(&self, dir: &Path, session: &str, id: String, work: Work) -> Result<String> {
        self.edit(dir, session, |records| {
            if let Work::Build { deck } = &work {
                if let Some(record) = records.iter().find(|record| {
                    !record.claimed && record.review.is_none()
                        && matches!(&record.work, Work::Build { deck: existing } if existing == deck)
                }) {
                    return record.id.clone();
                }
            }
            if let Work::Desk(crate::delegate::DeskTurn::Plan { resumed }) = &work {
                if let Some(record) = records.iter_mut().find(|record| {
                    record.session == session && !record.claimed && record.review.is_none()
                        && matches!(record.work, Work::Desk(crate::delegate::DeskTurn::Plan { .. }))
                }) {
                    if let Work::Desk(crate::delegate::DeskTurn::Plan { resumed: old }) = &mut record.work {
                        *old |= *resumed;
                    }
                    return record.id.clone();
                }
            }
            if records.iter().any(|record| record.id == id) {
                return id;
            }
            records.push(Record::pending(id.clone(), session, work));
            id
        })
    }

    pub fn defer(&self, dir: &Path, session: &str, id: String, work: Work) {
        if self
            .enqueue(dir, session, id.clone(), work.clone())
            .is_err()
        {
            self.0.lock().unwrap().pending_writes.push(Record::pending(id, session, work));
        }
    }

    pub fn force_claude_resume(&self, dir: &Path, session: &str) -> Result<()> {
        self.edit(dir, session, |records| {
            for record in records.iter_mut().filter(|record| {
                record.session == session && !record.claimed && record.review.is_none()
            }) {
                if let Work::Claude(prompt) = &mut record.work {
                    prompt.force = true;
                    prompt.not_before = None;
                }
            }
        })
    }

    pub fn claim(&self, dir: &Path, session: &str, id: &str) -> Result<bool> {
        self.claim_many(dir, session, &[id.into()])
    }

    pub(crate) fn delegation_queued(&self, dir: &Path, id: &str) -> Result<bool> {
        let mut state = self.0.lock().unwrap();
        Self::load(dir, &mut state)?;
        Ok(state.records.as_ref().unwrap().iter().any(|record| {
            record.id == id && !record.claimed && record.review.is_none()
                && matches!(record.work, Work::Delegate(_))
        }))
    }

    fn recover_interrupted(&self, dir: &Path, previous: &[Record]) -> Result<()> {
        let interrupted: std::collections::HashSet<_> = previous.iter()
            .filter(|record| record.claimed
                && (record.review.is_none() || record.review.as_deref() == Some(INTERRUPTED)))
            .map(|record| record.id.as_str())
            .collect();
        if interrupted.is_empty() {
            return Ok(());
        }
        let now = delivery_now();
        self.edit(dir, "", |records| {
            for record in records.iter_mut().filter(|record| record.claimed && interrupted.contains(record.id.as_str())) {
                if record.preparing {
                    record.requeue(PREPARATION_INTERRUPTED.into(), now);
                } else if !record.planner_retried && matches!(&record.work, Work::Delegate(request) if request.is_planner()) {
                    // Persist the retry marker before the scheduler can claim this assignment again.
                    record.planner_retried = true;
                    record.claimed = false;
                    record.review = None;
                } else if record.review.is_none() {
                    record.review = Some(INTERRUPTED.into());
                }
            }
        })
    }

    pub fn claim_many(&self, dir: &Path, session: &str, ids: &[String]) -> Result<bool> {
        if ids.is_empty() {
            return Ok(true);
        }
        let now = delivery_now();
        self.edit(dir, session, |records| {
            let eligible = |record: &Record| record.ready(now) && ids.contains(&record.id);
            if records.iter().filter(|record| eligible(record)).count() != ids.len() {
                return false;
            }
            for record in records.iter_mut().filter(|record| eligible(record)) {
                record.claimed = true;
                record.preparing = record.work.tracks_submission();
            }
            true
        })
    }

    pub(crate) fn begin_submission(&self, dir: &Path, session: &str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let claimed = self.edit(dir, session, |records| {
            let eligible = |record: &Record| {
                record.session == session && record.claimed && record.review.is_none() && ids.contains(&record.id)
            };
            if records.iter().filter(|record| eligible(record)).count() != ids.len() {
                return false;
            }
            for record in records.iter_mut().filter(|record| eligible(record)) {
                record.preparing = false;
            }
            true
        })?;
        anyhow::ensure!(claimed, "The held work is no longer claimed");
        Ok(())
    }

    pub fn finish(&self, dir: &Path, session: &str, id: &str, review: Option<String>) {
        self.resolve(dir, session, id, Completion::Finish(review));
    }

    /// Return true when the work can retry because it never reached submission.
    pub(crate) fn delivery_failed(&self, dir: &Path, session: &str, id: &str, reason: String) -> bool {
        self.resolve(dir, session, id, Completion::DeliveryFailed { reason, at: delivery_now() })
    }

    fn resolve(&self, dir: &Path, session: &str, id: &str, completion: Completion) -> bool {
        let mut requeued = false;
        if let Err(error) = self.edit(dir, session, |records| {
            requeued = complete(records, id, &completion);
        }) {
            self.0.lock().unwrap().completions.insert(id.into(), completion);
            tracing::warn!(%error, %session, %id, "could not save work completion");
        }
        requeued
    }

    pub fn retry(&self, dir: &Path) {
        let mut state = self.0.lock().unwrap();
        if state.errors.is_empty()
            && state.completions.is_empty()
            && state.pending_writes.is_empty()
        {
            return;
        }
        let result = (|| {
            Self::load(dir, &mut state)?;
            let mut records = state.records.clone().unwrap();
            records.extend(state.pending_writes.iter().cloned());
            for (id, completion) in &state.completions {
                complete(&mut records, id, completion);
            }
            Self::store(dir, &records)?;
            state.records = Some(records);
            Ok::<(), anyhow::Error>(())
        })();
        if result.is_ok() {
            state.completions.clear();
            state.pending_writes.clear();
            state.errors.clear();
        }
    }

    pub fn errors(&self) -> BTreeMap<String, String> {
        let state = self.0.lock().unwrap();
        let mut errors = state.errors.clone();
        if let Some(error) = &state.usage_error {
            errors.insert(String::new(), error.clone());
        }
        if let Some(records) = &state.records {
            for id in state.completions.keys() {
                let session = records
                    .iter()
                    .find(|record| &record.id == id)
                    .map_or_else(String::new, |record| record.session.clone());
                errors
                    .entry(session)
                    .or_insert_with(|| "Could not persist a work completion".into());
            }
        }
        for write in &state.pending_writes {
            errors
                .entry(write.session.clone())
                .or_insert_with(|| "Could not persist held work".into());
        }
        errors
    }
}

fn complete(records: &mut Vec<Record>, id: &str, completion: &Completion) -> bool {
    if matches!(completion, Completion::Finish(None)) {
        records.retain(|record| record.id != id);
    } else if let Some(record) = records.iter_mut().find(|record| record.id == id) {
        let reason = match completion {
            Completion::DeliveryFailed { reason, at } if record.preparing => {
                record.requeue(reason.clone(), *at);
                return true;
            }
            Completion::DeliveryFailed { reason, .. } | Completion::Finish(Some(reason)) => reason,
            Completion::Finish(None) => unreachable!(),
        };
        record.preparing = false;
        record.review = Some(reason.clone());
    }
    false
}

pub(crate) fn instruction(
    wait: i64,
    reason: fuzukue_proto::DrainReason,
    job: &str,
    cancelled: bool,
) -> String {
    let reason = match reason {
        fuzukue_proto::DrainReason::Update => "update",
        fuzukue_proto::DrainReason::Shutdown => "shutdown",
    };
    if cancelled {
        format!(
            "Server {reason} {wait} was cancelled. You may continue the current assignment {job}."
        )
    } else {
        format!(
            "Server {reason} {wait} is draining assignment {job}. Finish at a safe stopping point. Save results and important temporary files in the workspace. In your final report record changed paths, verification, worktree and commit if applicable, outstanding decisions and the next safe step. Finish without starting another job."
        )
    }
}

pub(crate) fn start(app: &Arc<Workspace>) {
    let weak = Arc::downgrade(app);
    // Capture only records from the previous process, before clients can submit work.
    let mut previous = app.drain.records(&app.state_dir).ok();
    tokio::spawn(async move {
        loop {
            let Some(app) = weak.upgrade() else { break };
            let orphan_age = app.registry.upgrade().and_then(|app| {
                app.self_update.get().map(|update| update.orphan_approval_age())
            }).unwrap_or(ORPHAN_APPROVAL_AGE);
            app.hub.expire_orphaned_approvals(orphan_age);
            if let Ok(_work) = app.admit_work() {
                if let Some(records) = &previous {
                    match app.drain.recover_interrupted(&app.state_dir, records) {
                        Ok(()) => previous = None,
                        Err(error) => tracing::warn!(%error, "could not recover interrupted work"),
                    }
                }
            }
            if app.admit_work().is_ok() {
                if let Ok(records) = app.drain.records(&app.state_dir) {
                    for record in records.into_iter().filter(|record| !record.claimed && record.review.is_none()) {
                        if let Work::Delegate(request) = record.work {
                            crate::delegate::restore_assignment(
                                &app,
                                record.session,
                                record.id,
                                request,
                            )
                            .await;
                        }
                    }
                }
                if let Ok(records) = app.drain.records(&app.state_dir) {
                    for record in records.iter().filter(|record| {
                        record.review.is_none()
                            && matches!(
                                record.work,
                                Work::LegacyPlan(_)
                                    | Work::LegacyGitHub(_)
                                    | Work::LegacyWatch { .. }
                                    | Work::WatchWake { .. }
                            )
                    }) {
                        let Ok(_work) = app.admit_work() else { break };
                        restore_legacy(&app, record);
                    }
                }
                crate::delegate::restore_desk(&app);
                crate::replay_prompts(&app).await;
                replay_background(&app).await;
            }
            app.drain.retry(&app.state_dir);
            let update = app.update().filter(|update| app.hold_planner_during_update && update.planner_work_held());
            drop(app);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = async { update.as_ref().unwrap().wait_for_planner_hold(false).await }, if update.is_some() => {}
            }
        }
    });
}

async fn replay_background(app: &Arc<Workspace>) {
    let Ok(records) = app.drain.records(&app.state_dir) else {
        return;
    };
    for record in records {
        if record.claimed || record.review.is_some() {
            continue;
        }
        if let Work::Build { deck } = record.work {
            app.restore_build(deck, record.id);
            continue;
        }
        if !matches!(
            record.work,
            Work::AutomaticReview { .. } | Work::HumanReview { .. }
        ) {
            continue;
        }
        let Ok(_work) = app.admit_work() else { return };
        if !matches!(
            app.drain.claim(&app.state_dir, &record.session, &record.id),
            Ok(true)
        ) {
            continue;
        }
        match record.work {
            Work::AutomaticReview { project, before, kind } => {
                app.schedule_reviews_for(&project, &record.session, before, kind)
                    .await;
            }
            Work::HumanReview { root, baseline } => {
                app.start_consistency(root, baseline, Duration::ZERO)
            }
            _ => unreachable!(),
        }
        app.drain
            .finish(&app.state_dir, &record.session, &record.id, None);
    }
}

pub(crate) const FORMAT_MARKER: &str = "secretaire-held-work-v1";

pub(crate) fn supports(path: &Path) -> Result<bool> {
    contains_marker(path, FORMAT_MARKER)
}

/// Whether an executable embeds a capability marker shorter than the buffer.
pub(crate) fn contains_marker(path: &Path, marker: &str) -> Result<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let marker = marker.as_bytes();
    let mut bytes = [0; 65536];
    let overlap = marker.len() - 1;
    let mut prefix = 0;
    loop {
        let read = file.read(&mut bytes[prefix..])?;
        if read == 0 {
            return Ok(false);
        }
        let size = prefix + read;
        if bytes[..size]
            .windows(marker.len())
            .any(|window| window == marker)
        {
            return Ok(true);
        }
        prefix = size.min(overlap);
        bytes.copy_within(size - prefix..size, 0);
    }
}

fn restore_legacy(app: &Workspace, record: &Record) -> bool {
    let result = match &record.work {
        Work::LegacyPlan(plan) => {
            crate::planner::restore_after_rollback(app, &record.session, plan)
        }
        Work::LegacyGitHub(hold) => app
            .hub
            .edit_threads(|store| store.restore_github_after_rollback(&record.session, hold)),
        Work::LegacyWatch {
            watch,
            paused,
            digest,
            outbox,
        } => crate::watch::restore_after_rollback(app, watch, *paused, digest, outbox),
        Work::WatchWake { .. } => {
            app.drain.finish(
                &app.state_dir,
                &record.session,
                &record.id,
                Some("Review the legacy watch excerpt and thread before retrying.".into()),
            );
            return true;
        }
        _ => return false,
    };
    match result {
        Ok(()) => app
            .drain
            .finish(&app.state_dir, &record.session, &record.id, None),
        Err(error) => {
            tracing::warn!(%error, "could not restore work stopped for an older build");
            app.drain.finish(
                &app.state_dir,
                &record.session,
                &record.id,
                Some(format!("{error:#}")),
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_restart_after_the_planner_retry_requires_review() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        let session = "thread-fixture";
        let job = "delegate:fixture";
        let assignment = serde_json::from_value(serde_json::json!({
            "prompt": "Read Git status", "caller": "planner", "routing": {},
        }))?;
        let old = Journal::default();
        old.enqueue(path, session, job.into(), Work::Delegate(assignment))?;
        assert!(old.claim(path, session, job)?);

        let first_restart = Journal::default();
        let held = first_restart.records(path)?;
        first_restart.recover_interrupted(path, &held)?;
        assert!(first_restart.claim(path, session, job)?);

        let second_restart = Journal::default();
        let held = second_restart.records(path)?;
        second_restart.recover_interrupted(path, &held)?;
        assert!(!second_restart.claim(path, session, job)?);
        assert_eq!(second_restart.records(path)?[0].review.as_deref(), Some(INTERRUPTED));
        Ok(())
    }

    #[test]
    fn recovery_requeues_a_preparing_record_once_and_reviews_submitting_work() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        let session = "thread-fixture";
        let old = Journal::default();
        let prompt = serde_json::from_value(serde_json::json!({
            "text": "Check the saved result", "images": [], "force": false, "kind": "Conversation",
        }))?;
        old.enqueue(path, session, "preparing".into(), Work::Claude(prompt))?;
        old.enqueue(path, session, "submitting".into(), Work::Desk(crate::delegate::DeskTurn::Report {
            job: "delegate:fixture".into(), text: "Verified result".into(), plan_failed: false,
        }))?;
        assert!(old.claim(path, session, "preparing")?);
        assert!(old.claim(path, session, "submitting")?);
        old.begin_submission(path, session, &["submitting".into()])?;

        let first_restart = Journal::default();
        let held = first_restart.records(path)?;
        first_restart.recover_interrupted(path, &held)?;
        first_restart.recover_interrupted(path, &held)?;
        let second_restart = Journal::default();
        let held = second_restart.records(path)?;
        second_restart.recover_interrupted(path, &held)?;

        let records = second_restart.records(path)?;
        let [preparing, submitting] = records.as_slice() else { panic!("missing records") };
        assert_eq!(preparing.id, "preparing");
        assert!(!preparing.claimed && preparing.review.is_none());
        let retry = preparing.retry.as_ref().unwrap();
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.reason, PREPARATION_INTERRUPTED);
        assert!(!preparing.ready(retry.next_attempt_at - 1));
        assert!(preparing.ready(retry.next_attempt_at));
        assert_eq!(submitting.id, "submitting");
        assert!(submitting.claimed && submitting.retry.is_none());
        assert_eq!(submitting.review.as_deref(), Some(INTERRUPTED));
        Ok(())
    }

    #[test]
    fn older_builds_load_a_journal_holding_approval_results() -> Result<()> {
        // The desk turns and records that builds before approval results decode.
        #[derive(Serialize, Deserialize)]
        enum OldDeskTurn {
            Question { id: String, job: String, text: String, replanned: bool },
            Report { job: String, text: String, plan_failed: bool },
            WorkshopReport { text: String },
            WatchEvent { watch: String, text: String },
            HandoffReport { worker: String, text: String },
            Plan { resumed: bool },
        }
        #[derive(Serialize, Deserialize)]
        enum OldWork {
            Desk(OldDeskTurn),
        }
        #[derive(Serialize, Deserialize)]
        struct OldRecord {
            id: String,
            session: String,
            work: OldWork,
            claimed: bool,
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path();
        Journal::default().enqueue(path, "thread-fixture", "approval-result-fixture".into(),
            Work::Desk(crate::delegate::DeskTurn::WorkshopReport {
                text: "Approval completed".into(), approval_id: Some("apr_fixture".into()),
            }))?;
        let current = Journal::default().records(path)?;
        assert!(current[0].work.tracks_submission());
        assert!(matches!(&current[0].work, Work::Desk(turn) if turn.is_approval_result()));

        let old: Vec<OldRecord> = serde_json::from_slice(&fs::read(path.join(FILE))?)?;
        assert!(matches!(&old[0].work, OldWork::Desk(OldDeskTurn::WorkshopReport { text }) if text == "Approval completed"));
        // What an older build saves again stays readable, as an ordinary report.
        fs::write(path.join(FILE), serde_json::to_vec(&old)?)?;
        let returned = Journal::default().records(path)?;
        assert!(matches!(&returned[0].work, Work::Desk(crate::delegate::DeskTurn::WorkshopReport {
            text, approval_id: None,
        }) if text == "Approval completed"));
        Ok(())
    }

    #[test]
    fn report_retry_preserves_the_first_refusal_and_caps_the_delay() {
        let mut record = Record::pending("test-report".into(), "thread-fixture",
            Work::Desk(crate::delegate::DeskTurn::Report {
                job: "delegate:fixture".into(), text: "Verified result".into(), plan_failed: false,
            }));
        let first_refusal = 1_000_000;
        let mut now = first_refusal;
        for delay in [1, 2, 4, 8, 16, 30, 30] {
            record.requeue("Partner switch wait".into(), now);
            let retry = record.retry.as_ref().unwrap();
            assert_eq!(retry.first_refused_at, first_refusal);
            assert_eq!(retry.next_attempt_at, now + delay * 1000);
            now = retry.next_attempt_at;
        }
        let stalled_at = first_refusal + STALLED_DELIVERY_MS;
        assert!(record.stalled_report("delegate:fixture", stalled_at - 1).is_none());
        assert!(record.stalled_report("other", stalled_at).is_none());
        let stalled = record.stalled_report("delegate:fixture", stalled_at).unwrap();
        assert_eq!(stalled["id"], "test-report");
        assert_eq!(stalled["pending_secs"], 300);
        assert_eq!(stalled["attempts"], 7);
    }
}
