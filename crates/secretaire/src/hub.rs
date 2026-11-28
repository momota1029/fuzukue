//! イベントに seq を振り、直近のログを保持しつつ全クライアントへ配る。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use fuzukue_proto::{Actor, CachedSession, DelegateReport, Envelope, Question, QuestionAnswer, QuestionOutcome,
    SelfUpdateBlocker, SelfUpdateBlockerKind as BlockerKind, ServerEvent};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::questions::{self, QuestionReply};

mod notifications;
pub(crate) mod folder_grants;
mod approval_queue;
pub(crate) mod approval_store;

use crate::state::{BACKLOG, BACKLOG_BYTES};
use crate::threads::payloads::{self, Refs, Source};
use crate::threads::timing as transcript_timing;
use crate::tool_guard::{Guard, Limits, Refusal};
const HANDOFF_LIMIT: usize = 3;
const HANDOFF_WINDOW: Duration = Duration::from_secs(60 * 60);
const PAYLOAD_CONVERSION_LOG: &str = "secretaire::hub::payload_conversion";
pub(crate) const PAYLOAD_CONVERSION_DIRECTIVE: &str = "secretaire::hub::payload_conversion=info";

#[derive(Default)]
pub(crate) struct TranscriptResume {
    pub epoch: Option<String>,
    pub cached_sessions: Vec<CachedSession>,
}

pub struct Hub {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Envelope>,
    worker_events: broadcast::Sender<WorkerEvent>,
    epoch: String,
    tool_limits: Limits,
    approval_queue: std::sync::OnceLock<approval_store::Queue>,
    last_payload_skip: Mutex<Option<Instant>>,
}

struct Inner {
    next_seq: u64,
    log: ReplayLog,
    writer: Option<crate::state::TimelineWriter>,
    threads: Option<crate::threads::Store>,
    partner_started: HashMap<String, PartnerTurn>,
    worker_tool_guards: HashMap<String, (String, Arc<Guard>)>,
    next_partner_generation: u64,
    handoffs_ready: HashSet<String>,
    recent_handoffs: HashMap<String, VecDeque<tokio::time::Instant>>,
    last_partner_progress: Option<tokio::time::Instant>,
    folder_grants: Option<crate::folder_grants::Store>,
    running: HashSet<String>,
    github_auth_requests: HashMap<String, crate::github_auth::mcp::Tool>,
    compactions: HashMap<String, (Arc<crate::cancellation::Cancellation>, i64)>,
    workers: BTreeMap<(String, String), WorkerActivity>,
    payload_holds: HashMap<String, usize>,
    background: BTreeMap<(BlockerKind, String), i64>,
    chat_pauses: HashMap<String, i64>,
    notifications: notifications::State,
    activity: Option<tokio::sync::watch::Sender<()>>,
    approvals: HashMap<String, PendingApproval>,
    resolved_approvals: HashSet<String>,
    questions: HashMap<String, PendingQuestion>,
    question_clients: usize,
    question_clients_offline_since: Instant,
}

impl Inner {
    fn payload_busy(&self, session: &str) -> Option<&'static str> {
        if self.running.contains(session) {
            Some("active partner turn")
        } else if self.compactions.contains_key(session) {
            Some("active compaction")
        } else if self.payload_holds.contains_key(session) {
            Some("live worker or Claude turn")
        } else {
            None
        }
    }
}

pub(crate) struct PayloadHold {
    hub: Arc<Hub>,
    session: String,
}

impl Drop for PayloadHold {
    fn drop(&mut self) {
        let mut inner = self.hub.inner.lock().unwrap();
        let count = inner.payload_holds.get_mut(&self.session).expect("registered payload hold");
        *count -= 1;
        if *count == 0 {
            inner.payload_holds.remove(&self.session);
        }
        if let Some(threads) = &mut inner.threads {
            threads.mark_payload_activity(&self.session);
        }
    }
}

#[derive(Default)]
struct ReplayLog {
    items: VecDeque<(Envelope, u64)>,
    bytes: u64,
}

impl ReplayLog {
    fn push(&mut self, envelope: Envelope, size: u64) {
        self.items.push_back((envelope, size));
        self.bytes += size;
        while self.items.len() > BACKLOG || self.items.len() > 1 && self.bytes > BACKLOG_BYTES {
            self.bytes -= self.items.pop_front().unwrap().1;
        }
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &Envelope> + Clone {
        self.items.iter().map(|(envelope, _)| envelope)
    }

    fn retain_mut(&mut self, mut keep: impl FnMut(&mut Envelope) -> bool) {
        self.items.retain_mut(|(envelope, size)| {
            let keep = keep(envelope);
            if !keep {
                self.bytes -= *size;
            }
            keep
        });
    }
}

fn append_timeline(writer: &mut Option<crate::state::TimelineWriter>, env: &Envelope, refs: &Refs) -> u64 {
    writer.as_mut().map_or_else(
        || serde_json::to_vec(env).expect("serializable timeline envelope").len() as u64 + 1,
        |writer| writer.append_with_payloads(env, refs),
    )
}

struct PendingQuestion {
    envelope: Envelope,
    observed: Observed,
    backend: Option<(Value, oneshot::Sender<QuestionReply>)>,
}

impl PendingQuestion {
    fn live(&self) -> bool {
        self.backend.as_ref().is_some_and(|(_, reply)| !reply.is_closed())
    }
}

pub(crate) struct QuestionClient<'a>(&'a Hub);

impl Drop for QuestionClient<'_> {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().unwrap();
        inner.question_clients -= 1;
        if inner.question_clients == 0 {
            inner.question_clients_offline_since = Instant::now();
        }
    }
}

struct PendingApproval {
    envelope: Envelope,
    observed: Observed,
    waiter: ApprovalWaiter,
    full: Option<approval_queue::LegacyFullApproval>,
}

struct Observed {
    instant: Instant,
    timestamp: i64,
}

struct PartnerTurn {
    generation: u64,
    update_proof: Option<String>,
    observed: Observed,
    request: Option<crate::front_desk::Request>,
    request_revision: u64,
    handoff: Option<crate::front_desk::Escalation>,
    escalated: bool,
    external_approval_input: bool,
    tool_guard: Arc<Guard>,
}

pub(crate) struct WorkerToolGuard<'a> {
    hub: &'a Hub,
    job: &'a str,
    pub(crate) guard: Arc<Guard>,
}

impl Drop for WorkerToolGuard<'_> {
    fn drop(&mut self) {
        self.hub.inner.lock().unwrap().worker_tool_guards.remove(self.job);
    }
}

impl Observed {
    fn now() -> Self {
        Self { instant: Instant::now(), timestamp: chrono::Utc::now().timestamp_millis() }
    }
}

#[derive(Clone)]
pub(crate) struct WorkerEvent {
    pub(crate) event: ServerEvent,
    pub(crate) artifact_paths: Option<Arc<[String]>>,
    pub(crate) failure_tail: Option<Arc<str>>,
}

struct WorkerActivity {
    started_at: Option<i64>,
    title: Option<String>,
}

enum ApprovalWaiter {
    Backend,
    Folder { automatic: bool },
    Local(Weak<()>),
    Restored,
}

impl PendingApproval {
    fn live(&self) -> bool {
        match &self.waiter {
            ApprovalWaiter::Backend | ApprovalWaiter::Folder { .. } => true,
            ApprovalWaiter::Local(waiter) => waiter.strong_count() != 0,
            ApprovalWaiter::Restored => false,
        }
    }
}

pub(crate) fn worker_approval(request_id: &str, actor: &Actor) -> bool {
    request_id.starts_with(fuzukue_proto::DELEGATE_JOB)
        || request_id.starts_with(crate::codex::WORKER_APPROVAL_PREFIX)
        || matches!(actor, Actor::Codex { parent_tool_use: Some(job), .. }
            if job.starts_with(fuzukue_proto::DELEGATE_JOB))
}

impl Hub {
    pub fn new() -> Self {
        Self::with_tool_limits(Limits::configured())
    }

    pub(crate) fn with_tool_limits(tool_limits: Limits) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let (worker_events, _) = broadcast::channel(1024);
        Self { inner: Mutex::new(Inner { next_seq: 1, log: ReplayLog::default(), writer: None, threads: None, folder_grants: None, partner_started: Default::default(), worker_tool_guards: HashMap::new(), next_partner_generation: 0, handoffs_ready: HashSet::new(), recent_handoffs: HashMap::new(), last_partner_progress: None, running: HashSet::new(), github_auth_requests: HashMap::new(), compactions: HashMap::new(), workers: BTreeMap::new(), background: BTreeMap::new(), chat_pauses: HashMap::new(), notifications: Default::default(), activity: None, approvals: HashMap::new(), resolved_approvals: HashSet::new(), questions: HashMap::new(), question_clients: 0, question_clients_offline_since: Instant::now(),
            payload_holds: HashMap::new(),
        }), tx,
            worker_events,
            epoch: format!("{:032x}", rand::random::<u128>()),
            tool_limits,
            approval_queue: Default::default(),
            last_payload_skip: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn disable_journal_writes(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.inner.lock().unwrap().writer.as_mut().unwrap().disable_writes(dir)
    }

    pub(crate) fn subscribe_worker(&self) -> broadcast::Receiver<WorkerEvent> {
        self.worker_events.subscribe()
    }

    /// ディスクの記録から始める。以後のイベントもそこへ追記する。
    pub fn with_timeline(dir: &std::path::Path) -> Self {
        let hub = Self::new();
        {
            let mut inner = hub.inner.lock().unwrap();
            let mut workers = BTreeMap::new();
            inner.writer = crate::state::TimelineWriter::open(dir).ok();
            let timeline = inner.writer.as_ref().map_or_else(
                || crate::state::Timeline::open(dir), crate::state::TimelineWriter::timeline,
            );
            timeline.restore(|envelope, size| {
                observe_approval(&mut inner, &envelope, ApprovalWaiter::Restored);
                observe_question(&mut inner.questions, &envelope);
                observe_worker(&mut workers, &envelope.event);
                inner.next_seq = inner.next_seq.max(envelope.seq.saturating_add(1));
                if let Some(size) = size {
                    inner.log.push(envelope, size);
                }
            });
            // Approval owners from the previous process cannot answer after restart.
            hub.close_approvals(&mut inner, "cancelled", |_| true);
            let held = crate::self_update::drain::Journal::default().records(dir).unwrap_or_default();
            for ((session, item), worker) in workers {
                let pending = held.iter().find(|record| record.id == item && record.session == session);
                if worker.started_at.is_none() && pending.is_some_and(|record| !record.claimed && record.review.is_none()) {
                    // The queue will republish this assignment under the same ID.
                    inner.workers.insert((session, item), worker);
                    continue;
                }
                let report = DelegateReport::interrupted(0);
                hub.publish_locked(&mut inner, ServerEvent::ToolResult {
                    session, item, ok: false, output: serde_json::to_string(&report).unwrap(),
                }, None, ApprovalWaiter::Backend);
            }
        }
        hub
    }

    pub(crate) fn restore_review_decks(&self, project: &std::path::Path) -> (HashSet<(String, String)>, Vec<String>) {
        let mut inner = self.inner.lock().unwrap();
        let mut keys = HashMap::new();
        let mut failures = HashSet::new();
        inner.log.retain_mut(|env| {
            let ServerEvent::ReviewStatus { deck, kind, state, .. } = &mut env.event else {
                return true;
            };
            let key = keys.entry(deck.clone()).or_insert_with(|| {
                crate::workspace::resolve_deck(project, std::path::Path::new(deck))
                    .ok().map(|(_, key)| key)
            });
            let Some(key) = key else { return false };
            deck.clone_from(key);
            let entry = (deck.clone(), kind.clone());
            if state == "failed" {
                failures.insert(entry);
            } else {
                failures.remove(&entry);
            }
            true
        });
        let pruned = keys.into_iter().filter_map(|(deck, key)| {
            (key.as_deref() != Some(deck.as_str())).then_some(deck)
        }).collect();
        (failures, pruned)
    }

    pub fn watch_activity(&self, changed: tokio::sync::watch::Sender<()>) {
        self.inner.lock().unwrap().activity = Some(changed);
    }

    pub fn has_running_turns(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.running.is_empty() || !inner.compactions.is_empty()
    }

    pub(crate) fn thread_running(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.running.contains(id) || inner.compactions.contains_key(id)
    }

    pub(crate) fn chat_activity(&self) -> (HashSet<String>, HashMap<String, i64>) {
        let inner = self.inner.lock().unwrap();
        (inner.running.clone(), inner.chat_pauses.clone())
    }

    pub(crate) fn compaction_sessions(&self) -> Vec<String> {
        self.inner.lock().unwrap().compactions.keys().cloned().collect()
    }

    pub(crate) fn compaction(&self, session: &str) -> Option<Arc<crate::cancellation::Cancellation>> {
        self.inner.lock().unwrap().compactions.get(session).map(|(cancel, _)| cancel.clone())
    }

    pub(crate) fn set_github_auth_request(&self, session: &str, request: Option<crate::github_auth::mcp::Tool>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(request) = request {
            inner.github_auth_requests.insert(session.into(), request);
        } else {
            inner.github_auth_requests.remove(session);
        }
    }

    pub(crate) fn github_auth_request(&self, session: &str) -> Option<crate::github_auth::mcp::Tool> {
        self.inner.lock().unwrap().github_auth_requests.get(session).copied()
    }

    pub(crate) fn set_compaction(&self, session: &str, cancel: Option<Arc<crate::cancellation::Cancellation>>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(cancel) = cancel {
            inner.compactions.insert(session.into(), (cancel, chrono::Utc::now().timestamp_millis()));
        } else {
            inner.compactions.remove(session);
        }
        if let Some(threads) = &mut inner.threads {
            threads.mark_payload_activity(session);
        }
        if let Some(changed) = &inner.activity {
            changed.send_replace(());
        }
    }

    pub(crate) fn has_seen_approval(&self, request_id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.approvals.contains_key(request_id) || inner.resolved_approvals.contains(request_id)
    }

    // The drain gate waits for live approvals and question replies.
    #[cfg(test)]
    pub(crate) fn live_approval_threads(&self) -> BTreeSet<String> {
        let inner = self.inner.lock().unwrap();
        inner.approvals.values().filter(|approval| approval.live()).filter_map(|approval| {
            match &approval.envelope.event {
                ServerEvent::ApprovalRequested { session, .. } => Some(session.clone()),
                _ => None,
            }
        }).chain(inner.questions.values().filter(|question| question.live()).filter_map(|question| {
            match &question.envelope.event {
                ServerEvent::QuestionAsked { session, .. } => Some(session.clone()),
                _ => None,
            }
        })).collect()
    }

    pub(crate) fn drain_blockers(&self, workspace: &str) -> Vec<SelfUpdateBlocker> {
        let inner = self.inner.lock().unwrap();
        let thread_title = |session: &str| inner.threads.as_ref().and_then(|store| store.title(session));
        let blocker = |kind, session: Option<&str>, id: &str, title, started_at| SelfUpdateBlocker {
            workspace: workspace.into(), session: session.map(str::to_owned), id: id.into(),
            title, kind, started_at,
        };
        let mut blockers = Vec::new();
        for (session, started) in &inner.partner_started {
            blockers.push(blocker(BlockerKind::Turn, Some(session), session, thread_title(session), started.observed.timestamp));
        }
        for (session, (_, started_at)) in &inner.compactions {
            if !inner.running.contains(session) {
                blockers.push(blocker(BlockerKind::Turn, Some(session), session, thread_title(session), *started_at));
            }
        }
        for ((session, id), worker) in &inner.workers {
            if let Some(started_at) = worker.started_at {
                blockers.push(blocker(BlockerKind::Worker, Some(session), id, worker.title.clone(), started_at));
            }
        }
        for approval in inner.approvals.values().filter(|approval| approval.live() && !matches!(approval.waiter, ApprovalWaiter::Folder { .. })) {
            if let ServerEvent::ApprovalRequested { session, request_id, .. } = &approval.envelope.event {
                blockers.push(blocker(BlockerKind::Approval, Some(session), request_id,
                    thread_title(session), approval.observed.timestamp));
            }
        }
        for question in inner.questions.values().filter(|question| question.live()) {
            if let ServerEvent::QuestionAsked { session, request_id, .. } = &question.envelope.event {
                blockers.push(blocker(BlockerKind::Question, Some(session), request_id,
                    thread_title(session), question.observed.timestamp));
            }
        }
        for ((kind, id), started_at) in &inner.background {
            blockers.push(blocker(*kind, None, id, Some(id.clone()), *started_at));
        }
        blockers.sort_by(|a, b| (a.kind, &a.session, &a.id).cmp(&(b.kind, &b.session, &b.id)));
        blockers
    }

    pub(crate) fn expire_orphaned_approvals(&self, age: Duration) {
        let mut inner = self.inner.lock().unwrap();
        self.close_approvals(&mut inner, "expired", |approval| {
            !approval.live() && approval.observed.instant.elapsed() >= age
        });
        let offline = inner.question_clients == 0 && inner.question_clients_offline_since.elapsed() >= age;
        self.close_questions(&mut inner, true, |question| {
            question.observed.instant.elapsed() >= age && (!question.live() || offline)
        });
    }

    pub(crate) fn register_question_client(&self) -> QuestionClient<'_> {
        self.inner.lock().unwrap().question_clients += 1;
        QuestionClient(self)
    }

    pub(crate) fn ask_question(
        &self, session: String, actor: Actor, backend_id: Value, questions: Vec<Question>,
    ) -> oneshot::Receiver<QuestionReply> {
        let request_id = crate::user_item().replacen("user:", "question:", 1);
        let (reply, response) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        self.publish_locked(&mut inner, ServerEvent::QuestionAsked {
            session, request_id: request_id.clone(), actor, questions,
        }, None, ApprovalWaiter::Backend);
        inner.questions.get_mut(&request_id).unwrap().backend = Some((backend_id, reply));
        if let ServerEvent::QuestionAsked { session, .. } = &inner.questions[&request_id].envelope.event {
            notifications::send_notice(&inner, session, fuzukue_proto::NotificationKind::Question);
        }
        response
    }

    pub(crate) fn answer_question(
        &self, request_id: &str, answers: BTreeMap<String, QuestionAnswer>, dismiss: bool,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(pending) = inner.questions.get(request_id).filter(|pending| pending.live()) else {
            // Another client or the backend may already have resolved it.
            return Ok(());
        };
        let ServerEvent::QuestionAsked { session, questions, .. } = &pending.envelope.event else { unreachable!() };
        let session = session.clone();
        let reply = if dismiss { None } else { Some(questions::validate(questions, &answers)?) };
        let answers = reply.as_ref().map(|answers| questions::display(questions, answers)).unwrap_or_default();
        let (_, sender) = inner.questions.get_mut(request_id).unwrap().backend.take().unwrap();
        let outcome = match sender.send(reply) {
            Ok(()) if dismiss => QuestionOutcome::Dismissed,
            Ok(()) => QuestionOutcome::Answered,
            Err(_) => QuestionOutcome::Cancelled,
        };
        self.publish_locked(&mut inner, ServerEvent::QuestionResolved {
            session, request_id: request_id.into(), outcome,
            answers: if outcome == QuestionOutcome::Answered { answers } else { BTreeMap::new() },
        }, None, ApprovalWaiter::Backend);
        Ok(())
    }

    pub(crate) fn cancel_questions(
        &self, session: &str, actor: &Actor, backend_id: Option<&Value>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        self.close_questions(&mut inner, false, |pending| {
            matches!(&pending.envelope.event, ServerEvent::QuestionAsked { session: owner, actor: asker, .. }
                if owner == session && asker == actor)
                && backend_id.is_none_or(|id| pending.backend.as_ref().is_some_and(|(stored, _)| stored == id))
        });
    }

    fn close_questions(&self, inner: &mut Inner, release_backend: bool, close: impl Fn(&PendingQuestion) -> bool) {
        let ids: Vec<_> = inner.questions.iter().filter_map(|(id, pending)| {
            let ServerEvent::QuestionAsked { session, .. } = &pending.envelope.event else { return None };
            close(pending).then(|| (id.clone(), session.clone()))
        }).collect();
        for (request_id, session) in ids {
            if release_backend {
                if let Some((_, sender)) = inner.questions.get_mut(&request_id).unwrap().backend.take() {
                    let _ = sender.send(None);
                }
            }
            self.publish_locked(inner, ServerEvent::QuestionResolved {
                session, request_id, outcome: QuestionOutcome::Cancelled, answers: BTreeMap::new(),
            }, None, ApprovalWaiter::Backend);
        }
    }

    pub(crate) fn cancel_worker_approvals(&self, job: &str) {
        let mut inner = self.inner.lock().unwrap();
        self.close_approvals(&mut inner, "cancelled", |approval| {
            matches!(&approval.envelope.event, ServerEvent::ApprovalRequested { request_id, actor, .. }
                if crate::delegate::belongs_to_job(request_id, actor, job))
        });
    }

    fn close_approvals(&self, inner: &mut Inner, decision: &str, close: impl Fn(&PendingApproval) -> bool) {
        let ids: Vec<_> = inner.approvals.iter().filter(|(_, approval)| close(approval))
            .map(|(id, _)| id.clone()).collect();
        for request_id in ids {
            self.publish_locked(inner, ServerEvent::ApprovalResolved {
                request_id, decision: decision.into(), automatic: false,
            }, None, ApprovalWaiter::Backend);
        }
    }

    pub(crate) fn swap_threads(
        &self,
        workspace: &str,
        sessions: Option<&HashMap<String, std::sync::Arc<crate::ClaudeSession>>>,
        workers: &BTreeSet<String>,
    ) -> Vec<fuzukue_proto::SelfUpdateBlockingThread> {
        let inner = self.inner.lock().unwrap();
        let ids: BTreeSet<_> = inner.running.iter().chain(inner.compactions.keys())
            .chain(sessions.into_iter().flat_map(|sessions| sessions.keys()))
            .chain(workers).collect();
        ids.into_iter().filter_map(|id| {
            let activity = sessions.and_then(|sessions| sessions.get(id))
                .map(|session| session.swap_activity());
            let checking = activity == Some(None);
            let (active, queued, paused) = activity.flatten().unwrap_or_default();
            let running = active || inner.running.contains(id) || inner.compactions.contains_key(id);
            let worker = workers.contains(id);
            (running || worker || queued || paused || checking).then(|| fuzukue_proto::SelfUpdateBlockingThread {
                workspace: workspace.into(),
                id: id.clone(),
                title: inner.threads.as_ref().and_then(|store| store.title(id)),
                running,
                worker,
                queued,
                paused,
                checking,
            })
        }).collect()
    }

    pub(crate) fn rename_delegate(
        &self, session: &str, job: &str, label: String, label_source: Option<String>, source: &str,
    ) -> anyhow::Result<fuzukue_proto::DelegateNameChange> {
        let mut inner = self.inner.lock().unwrap();
        let change = inner.threads.as_mut().expect("Workspace initializes the thread store")
            .rename_delegate(session, job, label, label_source, source)?;
        self.publish_locked(&mut inner, ServerEvent::DelegateRenamed {
            session: session.into(), job: job.into(), change: change.clone(),
        }, None, ApprovalWaiter::Backend);
        Ok(change)
    }

    pub(crate) fn dismiss_delegate(&self, session: &str, job: &str) -> anyhow::Result<()> {
        anyhow::ensure!(job.starts_with(fuzukue_proto::DELEGATE_JOB), "Job not found in this thread");
        let mut inner = self.inner.lock().unwrap();
        if inner.threads.as_mut().expect("Workspace initializes the thread store").dismiss_delegate(session, job)? {
            self.publish_locked(&mut inner, ServerEvent::DelegateDismissed {
                session: session.into(), job: job.into(),
            }, None, ApprovalWaiter::Backend);
        }
        Ok(())
    }

    pub fn publish(&self, event: ServerEvent) {
        self.publish_event(event, None);
    }

    pub(crate) fn worker_tool_guard<'a>(&'a self, session: &str, job: &'a str) -> WorkerToolGuard<'a> {
        let guard = Arc::new(Guard::new(self.tool_limits, true));
        self.inner.lock().unwrap().worker_tool_guards.insert(job.into(), (session.into(), guard.clone()));
        WorkerToolGuard { hub: self, job, guard }
    }

    pub(crate) fn record_tool_call(&self, session: &str, job: Option<&str>, name: &str, native_item: Option<&str>) -> Option<Refusal> {
        let mut inner = self.inner.lock().unwrap();
        let guard = match job {
            Some(job) => {
                let (owner, guard) = inner.worker_tool_guards.get(job)?;
                if owner != session {
                    return None;
                }
                guard
            }
            None => &inner.partner_started.get(session)?.tool_guard,
        };
        let refusal = guard.record(name, native_item)?;
        if job.is_none() && refusal.first {
            self.publish_locked(&mut inner, crate::tool_guard::desk_notice(session),
                None, ApprovalWaiter::Backend);
        }
        Some(refusal)
    }

    pub(crate) fn last_partner_progress(&self) -> Option<tokio::time::Instant> {
        self.inner.lock().unwrap().last_partner_progress
    }

    pub(crate) fn update_request_generation(&self, session: &str, proof: &str) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        let turn = inner.partner_started.get(session)?;
        (turn.update_proof.as_deref() == Some(proof)).then_some(turn.generation)
    }

    pub(crate) fn partner_is_only_turn(&self, session: &str, generation: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.compactions.contains_key(session)
            && inner.partner_started.get(session).is_some_and(|turn| turn.generation == generation)
    }

    pub(crate) fn partner_elapsed(&self, session: &str) -> Option<u64> {
        self.inner.lock().unwrap().partner_started.get(session)
            .map(|started| started.observed.instant.elapsed().as_millis() as u64)
    }

    pub(crate) fn publish_partner_turn_started(
        &self, session: &str, request: Option<crate::front_desk::Request>, escalated: bool,
        update_proof: Option<String>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_partner_generation += 1;
        let generation = inner.next_partner_generation;
        inner.partner_started.insert(session.into(), PartnerTurn {
            generation,
            update_proof,
            observed: Observed::now(),
            request,
            request_revision: 0,
            handoff: None,
            escalated,
            external_approval_input: false,
            tool_guard: Arc::new(Guard::new(self.tool_limits, false)),
        });
        self.publish_locked(&mut inner, ServerEvent::TurnActivity {
            session: session.into(), running: true,
        }, None, ApprovalWaiter::Backend);
    }

    pub(crate) fn mark_external_approval_input(&self, session: &str) {
        if let Some(turn) = self.inner.lock().unwrap().partner_started.get_mut(session) {
            turn.external_approval_input = true;
        }
    }

    pub(crate) fn external_approval_input(&self, session: &str) -> bool {
        self.inner.lock().unwrap().partner_started.get(session)
            .is_some_and(|turn| turn.external_approval_input)
    }

    pub(crate) fn escalation_turn(&self, session: &str) -> anyhow::Result<(Instant, u64)> {
        let inner = self.inner.lock().unwrap();
        let turn = inner.partner_started.get(session)
            .ok_or_else(|| anyhow::anyhow!("Escalation requires an active front desk turn"))?;
        anyhow::ensure!(turn.request.is_some(), "Escalation requires an ordinary user turn");
        anyhow::ensure!(turn.handoff.is_none(), "Escalation is available once per user request");
        Ok((turn.observed.instant, turn.request_revision))
    }

    pub(crate) fn update_partner_request(&self, session: &str, request: crate::front_desk::Request) {
        if let Some(turn) = self.inner.lock().unwrap().partner_started.get_mut(session) {
            if turn.request.is_some() && turn.handoff.is_none() {
                turn.request = Some(request);
                turn.request_revision += 1;
            }
        }
    }

    pub(crate) fn request_escalation(
        &self, session: &str, request_turn: (Instant, u64), handoff: crate::front_desk::Escalation,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Inner { partner_started, recent_handoffs, .. } = &mut *inner;
        let turn = partner_started.get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("The front desk turn has ended"))?;
        anyhow::ensure!((turn.observed.instant, turn.request_revision) == request_turn,
            "The front desk request has changed");
        anyhow::ensure!(turn.request.is_some() && turn.handoff.is_none(),
            "Escalation is available once per user request");
        let now = tokio::time::Instant::now();
        recent_handoffs.retain(|_, attempts| {
            while attempts.front().is_some_and(|at| now.duration_since(*at) >= HANDOFF_WINDOW) {
                attempts.pop_front();
            }
            !attempts.is_empty()
        });
        let attempts = recent_handoffs.entry(session.into()).or_default();
        anyhow::ensure!(attempts.len() < HANDOFF_LIMIT,
            "This thread already used {HANDOFF_LIMIT} model handoffs in the past hour. The user can still send /escalate.");
        attempts.push_back(now);
        turn.handoff = Some(handoff);
        Ok(())
    }

    pub(crate) fn partner_handoff_pending(&self, session: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.handoffs_ready.contains(session)
            || inner.partner_started.get(session).is_some_and(|turn| turn.escalated || turn.handoff.is_some())
    }

    pub(crate) fn finish_partner_handoff(&self, session: &str) {
        self.inner.lock().unwrap().handoffs_ready.remove(session);
    }

    pub(crate) fn publish_partner_turn_ended(
        &self, session: &str, actor: Actor, completed: bool,
    ) -> (Option<(usize, String)>, Option<(crate::front_desk::Escalation, crate::front_desk::Request)>) {
        let mut inner = self.inner.lock().unwrap();
        let _held = transcript_timing::LockHold::start("turn_end");
        let handoff = inner.partner_started.get_mut(session)
            .and_then(|turn| turn.handoff.take().zip(turn.request.take()))
            .filter(|_| completed);
        if handoff.is_some() {
            // Reserve the gap between TurnEnded and the replacement turn's start.
            inner.handoffs_ready.insert(session.into());
        }
        let event = ServerEvent::TurnEnded { session: session.into(), actor };
        let title = self.publish_locked(&mut inner, event,
            (completed && handoff.is_none()).then_some(session), ApprovalWaiter::Backend);
        (title, handoff)
    }

    fn publish_event(&self, event: ServerEvent, completed_session: Option<&str>) -> Option<(usize, String)> {
        let mut timing = crate::send_latency::Publish::start(&event);
        let result = {
            let mut inner = self.inner.lock().unwrap();
            let _held = transcript_timing::LockHold::start("publish");
            let timing = timing.as_mut().map(|timing| (timing, Instant::now()));
            self.publish_locked_timed(&mut inner, event, completed_session, ApprovalWaiter::Backend, timing)
        };
        if let Some(timing) = timing {
            timing.log();
        }
        result
    }

    pub(crate) fn publish_local_approval(&self, event: ServerEvent, waiter: &Arc<()>) {
        let mut inner = self.inner.lock().unwrap();
        self.publish_locked(&mut inner, event, None, ApprovalWaiter::Local(Arc::downgrade(waiter)));
    }

    fn publish_locked(&self, inner: &mut Inner, event: ServerEvent, completed_session: Option<&str>,
        waiter: ApprovalWaiter,
    ) -> Option<(usize, String)> {
        self.publish_locked_timed(inner, event, completed_session, waiter, None)
    }

    fn publish_locked_timed(&self, inner: &mut Inner, mut event: ServerEvent, completed_session: Option<&str>,
        waiter: ApprovalWaiter, timing: Option<(&mut crate::send_latency::Publish, Instant)>,
    ) -> Option<(usize, String)> {
        match &event {
            ServerEvent::TurnEnded { session, actor } => {
                self.close_questions(inner, false, |question| {
                    matches!(&question.envelope.event, ServerEvent::QuestionAsked { session: owner, actor: asker, .. }
                        if session == owner && actor == asker)
                });
            }
            ServerEvent::TextDelta { session, actor, .. }
            | ServerEvent::ToolCall { session, actor, .. }
                if inner.running.contains(session) && matches!(actor, Actor::Claude | Actor::Codex { .. }) =>
            {
                inner.last_partner_progress = Some(tokio::time::Instant::now());
            }
            ServerEvent::Paused { session, until, .. } => {
                inner.chat_pauses.insert(session.clone(), *until);
            }
            ServerEvent::Resumed { session } => {
                inner.chat_pauses.remove(session);
            }
            _ => {}
        }
        let activity = match &event {
            ServerEvent::TurnActivity { session, running } => Some((session, *running)),
            ServerEvent::TurnEnded { session, actor }
                if matches!(actor, fuzukue_proto::Actor::Claude | fuzukue_proto::Actor::Codex { parent_tool_use: None, .. }) =>
            {
                Some((session, false))
            }
            _ => None,
        };
        if let Some((session, running)) = activity {
            let was_idle = inner.running.is_empty();
            if running {
                inner.next_partner_generation += 1;
                let generation = inner.next_partner_generation;
                inner.partner_started.entry(session.clone()).or_insert_with(|| PartnerTurn {
                    generation,
                    update_proof: None,
                    observed: Observed::now(), request: None, request_revision: 0, handoff: None, escalated: false,
                    external_approval_input: false,
                    tool_guard: Arc::new(Guard::new(self.tool_limits, false)),
                });
                inner.running.insert(session.clone());
            } else {
                inner.running.remove(session);
                inner.github_auth_requests.remove(session);
                inner.partner_started.remove(session);
                if inner.folder_grants.as_mut().is_some_and(|store| store.end_turn(session)) {
                    let _ = self.tx.send(crate::snapshot(ServerEvent::FolderGrantsChanged { session: session.clone() }));
                }
                self.close_approvals(inner, "cancelled", |approval| {
                    matches!(approval.waiter, ApprovalWaiter::Backend)
                        && matches!(&approval.envelope.event, ServerEvent::ApprovalRequested {
                            session: owner, request_id, actor, ..
                        } if owner == session && !worker_approval(request_id, actor))
                });
            }
            if was_idle != inner.running.is_empty() {
                if let Some(changed) = &inner.activity { changed.send_replace(()); }
            }
        }
        if let ServerEvent::ToolCall { session, item, name, input, .. } = &mut event {
            if name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB) {
                if let Some((saved, _, _)) = inner.threads.as_ref().and_then(|store| store.delegate_record(session, item)) {
                    if saved.get("rename_history").is_some() {
                        input["label"] = saved["label"].clone();
                        input["label_source"] = saved["label_source"].clone();
                    }
                    if saved["dismissed_from_panel"].as_bool() == Some(true) {
                        input["dismissed_from_panel"] = true.into();
                    }
                }
            }
        }
        self.bind_approval_tool(inner, &event);
        let replayable = retain_replay_event(&mut event);
        let artifact_paths = inner.threads.as_ref().and_then(|threads| threads.event_artifact_paths(&event));
        let worker_listening = self.worker_events.receiver_count() != 0;
        let failure_tail = match &event {
            ServerEvent::ToolResult { ok: false, output, .. } if worker_listening => {
                let start = output.char_indices().rev().nth(599).map_or(0, |(i, _)| i);
                Some(Arc::from(&output[start..]))
            }
            _ => None,
        };
        let mut env = Envelope { workspace: None, seq: if replayable { inner.next_seq } else { 0 }, event };
        let stored = inner.threads.as_ref().and_then(|threads| threads.event_blobs(&env.event))
            .map_or_else(|| Ok(Refs::default()), |blobs| blobs.compact(&mut env.event));
        let (refs, sidecar_error) = match stored {
            Ok(refs) => (refs, None),
            Err(error) => (Refs::default(), Some(error)),
        };
        let notice = notifications::before_publish(inner, &env.event);
        observe_approval(inner, &env, waiter);
        observe_question(&mut inner.questions, &env);
        observe_worker(&mut inner.workers, &env.event);
        observe_background(&mut inner.background, &env.event);
        let journal_started_at = timing.as_ref().map(|_| Instant::now());
        if let Some(threads) = inner.threads.as_ref() {
            threads.prepare_event(&env.event);
        }
        let mut payload_source = None;
        if replayable {
            inner.next_seq += 1;
            let mut size = append_timeline(&mut inner.writer, &env, &refs);
            if sidecar_error.is_some() {
                // Store the original journal record if the sidecar volume is full.
                let journal = inner.writer.as_mut().and_then(|writer| writer.last_record(size));
                let mut preview = env.event.clone();
                payloads::bound_replay(&mut preview);
                let original = std::mem::replace(&mut env.event, preview);
                payload_source = Some(match journal {
                    Some(record) => {
                        tracing::warn!(error=?sidecar_error, "preserved tool payload in journal after sidecar write failed");
                        Source::Journal(record)
                    }
                    None => {
                        tracing::error!(error=?sidecar_error, "retaining unsaved tool payload in memory");
                        Source::Memory(Arc::new(payloads::MemoryRecord {
                            event: original,
                            journal_prefixes: inner.threads.as_ref().map_or([None; 2], crate::threads::Store::journal_prefixes),
                        }))
                    }
                });
                size = serde_json::to_vec(&env).expect("serializable preview").len() as u64 + 1;
            }
            inner.log.push(env.clone(), size);
        }
        // 送信は lock を握ったまま行い、backlog と live の順序を揃える。
        let journal_done_at = timing.as_ref().map(|_| Instant::now());
        let journaled = replayable && inner.writer.as_ref().is_some_and(|writer| writer.appends_synced());
        let saved = inner.threads.as_mut().map(|threads| {
            let unsaved = matches!(payload_source, Some(Source::Memory(_)));
            let saved = threads.observe_persisted(&env.event, refs, payload_source, completed_session.is_some(), journaled, artifact_paths.as_deref())
                .and_then(|saved| {
                    if let Some(error) = sidecar_error.filter(|_| unsaved) {
                        Err(error.context("tool payload retained in memory after storage failed"))
                    } else {
                        Ok(saved)
                    }
                });
            if matches!(env.event, ServerEvent::TurnEnded { .. }) {
                threads.trim_transcripts();
            }
            saved
        });
        let transcript_done_at = timing.as_ref().map(|_| Instant::now());
        let seq = env.seq;
        if worker_listening && matches!(env.event, ServerEvent::TextDelta { .. }
            | ServerEvent::ToolCall { .. } | ServerEvent::ToolResult { .. }
            | ServerEvent::ApprovalRequested { .. } | ServerEvent::ApprovalResolved { .. })
        {
            let _ = self.worker_events.send(WorkerEvent {
                event: env.event.clone(), artifact_paths: artifact_paths.map(Arc::from), failure_tail,
            });
        }
        let _ = self.tx.send(env);
        if let Some((timing, locked_at)) = timing {
            timing.stages = Some(crate::send_latency::Stages {
                locked_at,
                journal_started_at: journal_started_at.unwrap(),
                journal_done_at: journal_done_at.unwrap(),
                transcript_done_at: transcript_done_at.unwrap(),
                broadcast_at: Instant::now(),
                seq,
                transcript_error: saved.as_ref().is_some_and(Result::is_err),
            });
        }
        if let Some((session, kind)) = notice {
            notifications::send_notice(inner, &session, kind);
        }
        match saved {
            Some(Ok(true)) => {
                let threads = inner.threads.as_ref()?;
                let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
                threads.title_summary_input(completed_session?)
            }
            Some(Err(e)) => {
                tracing::error!("could not save thread: {e:#}");
                let event = ServerEvent::Error { message: format!("スレッドを保存できない: {e:#}") };
                let _ = self.tx.send(Envelope { workspace: None, seq: 0, event });
                None
            }
            _ => None,
        }
    }

    /// seq を振らず、backlog にも残さずに配る（状態のスナップショット用）。
    pub fn publish_snapshot(&self, event: ServerEvent) {
        let mut inner = self.inner.lock().unwrap();
        let notice = inner.notifications.goal_change(&event);
        observe_background(&mut inner.background, &event);
        let _ = self.tx.send(Envelope { workspace: None, seq: 0, event });
        if let Some(session) = notice {
            notifications::send_notice(&inner, &session, fuzukue_proto::NotificationKind::GoalBlocked);
        }
    }

    pub fn load_threads(
        &self,
        project: &std::path::Path,
        state_dir: &std::path::Path,
    ) -> std::collections::BTreeMap<String, crate::state::SessionRecord> {
        let mut records = crate::state::load_sessions(state_dir);
        let mut inner = self.inner.lock().unwrap();
        let timeline = inner.writer.as_ref().map_or_else(
            || crate::state::Timeline::open(state_dir), crate::state::TimelineWriter::timeline,
        );
        let mut store = crate::threads::Store::load_indexed(project, state_dir, &records, timeline);
        store.restore_sessions(&mut records);
        inner.threads = Some(store);
        inner.folder_grants = Some(crate::folder_grants::Store::load(state_dir));
        records
    }

    pub(crate) fn library_thread_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().threads.as_ref()
            .map(|store| store.library_thread_ids()).unwrap_or_default()
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(crate) fn try_cache_size(&self) -> Option<(u64, crate::threads::CacheSize)> {
        let inner = self.inner.try_lock().ok()?;
        Some((inner.log.bytes, inner.threads.as_ref().map_or_else(Default::default, |store| store.cache_size())))
    }

    pub(crate) fn flush_dirty_transcripts(&self, idle_only: bool) -> anyhow::Result<()> {
        let mut inner = self.inner.try_lock()
            .map_err(|_| anyhow::anyhow!("transcript store is busy"))?;
        let _held = transcript_timing::LockHold::start("flush");
        let Inner { threads, running, compactions, payload_holds, .. } = &mut *inner;
        let trigger = if idle_only { "idle_flush" } else { "shutdown_flush" };
        threads.as_mut().map_or(Ok(()), |threads| threads.flush_dirty_transcripts(trigger, |id| {
            idle_only && (running.contains(id) || compactions.contains_key(id) || payload_holds.contains_key(id))
        }))
    }

    // Pair a live writer with its task lifetime. Replay and late events can
    // leave display state behind, and queued work has no live writer yet.
    pub(crate) fn hold_payload_conversion(self: &Arc<Self>, session: &str) -> PayloadHold {
        let mut inner = self.inner.lock().unwrap();
        *inner.payload_holds.entry(session.into()).or_default() += 1;
        if let Some(threads) = &mut inner.threads {
            threads.mark_payload_activity(session);
        }
        PayloadHold { hub: self.clone(), session: session.into() }
    }

    pub(crate) fn trim_idle_transcripts(&self, idle: std::time::Duration) {
        if let Ok(mut inner) = self.inner.try_lock() {
            if let Some(threads) = &mut inner.threads {
                threads.trim_idle_transcripts(idle);
            }
        }
    }

    fn log_payload_skip(&self, thread: Option<&str>, reason: &'static str) {
        let Ok(mut last) = self.last_payload_skip.try_lock() else { return };
        if last.is_none_or(|last| last.elapsed() >= Duration::from_secs(60)) {
            tracing::info!(target: PAYLOAD_CONVERSION_LOG, thread, reason, "idle payload conversion skipped");
            *last = Some(Instant::now());
        }
    }

    pub(crate) async fn migrate_idle_payloads(self: &Arc<Self>, idle: Duration) {
        let (candidate, skipped) = {
            let Ok(inner) = self.inner.try_lock() else {
                self.log_payload_skip(None, "transcript store is busy");
                return;
            };
            let Some(threads) = &inner.threads else { return };
            let (candidate, skipped) = threads.payload_candidate(idle, |id| inner.payload_busy(id));
            (candidate, skipped.map(|(id, reason)| (id.to_owned(), reason)))
        };
        if let Some((id, reason)) = skipped {
            self.log_payload_skip(Some(&id), reason);
        }
        let Some(candidate) = candidate else { return };
        let id = candidate.id.clone();
        let started = Instant::now();
        tracing::info!(target: PAYLOAD_CONVERSION_LOG, thread = %id, "idle payload conversion started");
        let hub = self.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut staged = candidate.stage()?;
            let mut inner = hub.inner.try_lock().map_err(|_| anyhow::anyhow!("transcript store is busy"))?;
            let id = &staged.candidate.id;
            if let Some(reason) = inner.payload_busy(id) {
                anyhow::bail!("{reason}");
            }
            let threads = inner.threads.as_mut().ok_or_else(|| anyhow::anyhow!("transcript store is unavailable"))?;
            anyhow::ensure!(threads.publish_payloads(&mut staged)?, "thread changed since staging started");
            drop(inner);
            staged.finish()
        }).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result.map_err(anyhow::Error::from).and_then(|result| result) {
            Ok(()) => tracing::info!(target: PAYLOAD_CONVERSION_LOG, thread = %id, elapsed_ms, "idle payload conversion finished"),
            Err(error) => tracing::warn!(target: PAYLOAD_CONVERSION_LOG, thread = %id, elapsed_ms, %error, "idle payload conversion aborted"),
        }
    }

    pub(crate) fn tool_detail(&self, session: &str, item: &str) -> Option<crate::threads::ToolDetail> {
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_mut()?;
        let detail = threads.tool_detail(session, item);
        threads.trim_transcripts();
        detail
    }

    pub(crate) fn library_thread(
        &self,
        id: &str,
        read_messages: impl FnOnce(&crate::threads::LibraryStamp) -> bool,
    ) -> Option<crate::threads::LibraryThread> {
        self.inner.lock().unwrap().threads.as_mut()?.library_thread(id, read_messages)
    }

    pub(crate) fn history_cursor(&self, session: &str) -> usize {
        self.inner.lock().unwrap().threads.as_ref()
            .map_or(0, |threads| threads.history_cursor(session))
    }

    pub(crate) fn history_messages(&self, session: &str, cursor: usize) -> (Vec<String>, Option<String>) {
        self.inner.lock().unwrap().threads.as_ref()
            .map_or_else(Default::default, |threads| threads.history_messages(session, cursor))
    }

    pub fn has_thread(&self, id: &str) -> bool {
        self.inner.lock().unwrap().threads.as_ref().is_some_and(|threads| threads.contains(id))
    }

    pub fn thread_active(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.compactions.contains_key(id) || inner.threads.as_ref().is_some_and(|store| store.active(id))
    }

    pub fn plan_sessions(&self) -> Vec<String> {
        self.inner.lock().unwrap().threads.as_ref().map(|store| store.plan_sessions()).unwrap_or_default()
    }

    pub fn recent_requests(&self, id: &str) -> Vec<String> {
        self.inner.lock().unwrap().threads.as_ref().map(|store| store.recent_requests(id)).unwrap_or_default()
    }

    pub fn recovery_summary(&self, id: &str) -> Option<String> {
        self.inner.lock().unwrap().threads.as_ref()?.recovery_summary(id)
    }

    pub fn role_settings(&self, id: &str) -> Option<(fuzukue_proto::ChatSettings, fuzukue_proto::DelegateSettings)> {
        self.inner.lock().unwrap().threads.as_ref()?.role_settings(id)
    }

    pub fn agent_override(&self, id: &str, name: &str) -> Option<fuzukue_proto::AgentSelection> {
        self.inner.lock().unwrap().threads.as_ref()?.agent_override(id, name)
    }

    pub(crate) fn refresh_model_defaults(&self) {
        let mut inner = self.inner.lock().unwrap();
        let Some(store) = inner.threads.as_mut() else { return };
        if store.reload_model_defaults() {
            for event in [
                store.snapshot(),
                ServerEvent::PartnerDefaultChanged { settings: store.partner_default.clone() },
                ServerEvent::DelegateSettingsChanged { settings: store.worker_default.clone() },
            ] {
                let _ = self.tx.send(crate::snapshot(event));
            }
        }
    }

    pub fn role_defaults(&self) -> (fuzukue_proto::ChatSettings, fuzukue_proto::DelegateSettings) {
        let inner = self.inner.lock().unwrap();
        let store = inner.threads.as_ref().expect("Workspace initializes the thread store");
        (store.partner_default.clone(), store.worker_default.clone())
    }

    pub(crate) fn recover_thread_metadata(&self, id: Option<&str>) {
        let candidate = self.inner.lock().unwrap().threads.as_mut()
            .and_then(|threads| threads.metadata_recovery(id));
        let Some(candidate) = candidate else { return };
        let result = candidate.read().and_then(|recovered| {
            let mut inner = self.inner.lock().unwrap();
            let Some(threads) = &mut inner.threads else { return Ok(()) };
            if threads.apply_metadata_recovery(recovered)? {
                let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
            }
            threads.trim_transcripts();
            Ok(())
        });
        if let Err(error) = result {
            tracing::warn!(%error, "could not recover saved thread metadata");
        }
    }

    pub fn thread_artifacts(&self, id: &str) -> Vec<String> {
        self.recover_thread_metadata(Some(id));
        self.inner.lock().unwrap().threads.as_ref().map(|store| store.artifacts(id)).unwrap_or_default()
    }

    pub(crate) fn thread_inbox_paths(&self, id: &str) -> Vec<String> {
        self.inner.lock().unwrap().threads.as_ref()
            .map(|store| store.inbox_paths(id)).unwrap_or_default()
    }

    pub fn thread_attachments(&self, id: &str) -> Vec<String> {
        self.inner.lock().unwrap().threads.as_ref().map(|store| store.attached(id)).unwrap_or_default()
    }

    pub fn set_thread_session(&self, id: &str, session: Option<(&str, fuzukue_proto::ClaudeSessionStart)>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(threads) = inner.threads.as_mut() {
            if threads.set_session(id, session) {
                let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
            }
        }
    }

    pub fn refresh_threads(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(threads) = inner.threads.as_ref() {
            let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
        }
    }

    pub(crate) fn routing_context(&self, id: &str) -> Option<(Vec<String>, Vec<String>)> {
        self.inner.lock().unwrap().threads.as_ref()?.routing_context(id)
    }

    pub(crate) fn apply_routing(
        &self, id: &str, clear: bool, candidates: Vec<fuzukue_proto::DeckRoute>,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(threads) = inner.threads.as_mut() {
            if threads.apply_routing(id, clear, candidates)? {
                let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
            }
        }
        Ok(())
    }

    pub(crate) fn thread_path(&self, id: &str) -> anyhow::Result<std::path::PathBuf> {
        self.inner.lock().unwrap().threads.as_ref().unwrap().path(id)
    }

    pub(crate) fn restore_thread(&self, path: std::path::PathBuf) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let generation = inner.next_seq;
        let store = inner.threads.as_mut().expect("Workspace initializes the thread store");
        let event = store.restore(path, generation)?;
        let snapshot = store.snapshot();
        let _ = self.tx.send(crate::snapshot(snapshot));
        self.publish_locked(&mut inner, event, None, ApprovalWaiter::Backend);
        Ok(())
    }

    pub fn read_threads<T>(&self, read: impl FnOnce(&crate::threads::Store) -> T) -> T {
        let inner = self.inner.lock().unwrap();
        read(inner.threads.as_ref().expect("Workspace initializes the thread store"))
    }

    pub fn deliver_github(
        &self, session: &str, link: &fuzukue_proto::GitHubLink, events: &[fuzukue_proto::GitHubActivity],
        informational: bool,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
        if let Some(event) = threads.github_deliver(session, link, events, informational)? {
            let snapshot = threads.snapshot();
            self.broadcast_saved(&mut inner, event, snapshot);
        }
        Ok(())
    }

    pub(crate) fn deliver_system_note(&self, session: &str, item: &str, text: String) -> anyhow::Result<()> {
        self.save_system_note(session, item, text, false).map(|_| ())
    }

    pub(crate) fn deliver_update_note(&self, session: &str, item: &str, text: String) -> anyhow::Result<bool> {
        self.save_system_note(session, item, text, true)
    }

    fn save_system_note(&self, session: &str, item: &str, text: String, allow_archived: bool) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
        let Some(event) = threads.system_note(session, item, text, allow_archived)? else { return Ok(false) };
        let snapshot = threads.snapshot();
        self.broadcast_saved(&mut inner, event, snapshot);
        Ok(true)
    }

    fn broadcast_saved(&self, inner: &mut Inner, event: ServerEvent, snapshot: ServerEvent) {
        let env = Envelope { workspace: None, seq: inner.next_seq, event };
        inner.next_seq += 1;
        let size = append_timeline(&mut inner.writer, &env, &Refs::default());
        inner.log.push(env.clone(), size);
        let _ = self.tx.send(env);
        let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: snapshot });
    }

    pub fn edit_threads<T>(&self, edit: impl FnOnce(&mut crate::threads::Store) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
        let result = edit(threads);
        threads.trim_transcripts();
        let result = result?;
        let _ = self.tx.send(Envelope { workspace: None, seq: 0, event: threads.snapshot() });
        Ok(result)
    }

    pub(crate) fn trash_thread(
        &self, app: &crate::Workspace, trash: &crate::trash::Store, session: &str,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let store = inner.threads.as_mut().expect("Workspace initializes the thread store");
        anyhow::ensure!(store.contains(session), "Thread not found");
        self.end_folder_grants(&mut inner, session, true)?;
        let store = inner.threads.as_mut().unwrap();
        store.trash(app, trash, session)?;
        let snapshot = store.snapshot();
        let _ = self.tx.send(crate::snapshot(snapshot));
        self.publish_locked(&mut inner, ServerEvent::ThreadHistoryInvalidated {
            session: session.into(),
        }, None, ApprovalWaiter::Backend);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn thread_tail(&self, session: String, request: u64) -> ServerEvent {
        self.thread_tail_mode(session, request, false)
    }

    pub(crate) fn thread_tail_mode(&self, session: String, request: u64, compact: bool) -> ServerEvent {
        use fuzukue_proto::ThreadHistoryFailure::*;
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_ref().expect("Workspace initializes the thread store");
        let Some(generation) = threads.history_generation(&session) else {
            return ServerEvent::ThreadHistoryFailed { session, request, reason: Some(NotFound) };
        };
        let workers = inner.workers.keys().filter(|(owner, _)| owner == &session)
            .map(|(_, job)| job.clone()).collect();
        let tools: HashMap<_, _> = inner.log.iter().filter_map(|env| {
            let key = tool_key(&env.event)?;
            (key.0 == session && env.seq > generation).then_some((key, &env.event))
        }).collect();
        let tail = threads.history_tail(&session, &workers, compact, |event| {
            let event = tool_key(&event).and_then(|key| tools.get(&key)).map(|&event| event.clone()).unwrap_or(event);
            crate::threads::bound_history_tool(event)
        });
        if let Some(event) = threads.thread_damage(&session) {
            let _ = self.tx.send(crate::snapshot(event));
        }
        inner.threads.as_mut().unwrap().trim_transcripts();
        match tail {
            Some((events, workers, next_before)) => ServerEvent::ThreadTail {
                events: self.history_with_approval_receipts(&session, events),
                session, request, generation, through: inner.next_seq - 1, workers, next_before,
            },
            None => ServerEvent::ThreadHistoryFailed { session, request, reason: Some(Unavailable) },
        }
    }

    pub(crate) fn thread_history_page_mode(
        &self, session: String, request: u64, before: u64, generation: Option<u64>, scoped: bool, compact: bool,
    ) -> ServerEvent {
        use fuzukue_proto::ThreadHistoryFailure::Unavailable;
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_ref().expect("Workspace initializes the thread store");
        let page = if scoped {
            threads.scoped_history_page(&session, before, generation, compact)
        } else {
            threads.history_page(&session, before).ok_or(Unavailable)
        };
        if let Some(event) = threads.thread_damage(&session) {
            let _ = self.tx.send(crate::snapshot(event));
        }
        inner.threads.as_mut().unwrap().trim_transcripts();
        match page {
            Ok((events, next_before)) => ServerEvent::ThreadHistoryPage {
                events: self.history_with_approval_receipts(&session, events),
                session, request, next_before,
                generation: if scoped { generation } else { None },
                through: scoped.then_some(inner.next_seq - 1),
            },
            Err(reason) => ServerEvent::ThreadHistoryFailed { session, request, reason: scoped.then_some(reason) },
        }
    }

    pub(crate) fn thread_history_from(&self, session: String, request: u64, start: u64, generation: u64) -> ServerEvent {
        let mut inner = self.inner.lock().unwrap();
        let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
        let page = threads.history_from(&session, start, generation);
        if let Some(event) = threads.thread_damage(&session) {
            let _ = self.tx.send(crate::snapshot(event));
        }
        threads.trim_transcripts();
        match page {
            Ok((events, next_after)) => ServerEvent::ThreadHistoryFrom {
                events: self.history_with_approval_receipts(&session, events),
                session, request, start, next_after, generation, through: inner.next_seq - 1,
            },
            Err(reason) => ServerEvent::ThreadHistoryFailed { session, request, reason: Some(reason) },
        }
    }

    pub(crate) fn thread_tool_details(&self, session: String, item: String, request: u64) -> ServerEvent {
        let detail = self.tool_detail(&session, &item);
        let input = detail.as_ref().map(|detail| detail.input()).transpose();
        let output = detail.as_ref().map(|detail| detail.output()).transpose();
        let unavailable = detail.is_none() || input.is_err() || output.is_err();
        ServerEvent::ThreadToolDetails {
            session, item, request, input: input.ok().flatten().flatten(),
            output: output.ok().flatten().flatten(), unavailable,
        }
    }

    pub(crate) fn thread_job_details(
        &self, session: String, job: String, request: u64, before: Option<u64>, full_report: bool,
    ) -> ServerEvent {
        let (mut events, next_before, through, detail) = {
            let mut inner = self.inner.lock().unwrap();
            let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
            let (events, next_before) = threads.job_details(&session, &job, before);
            let detail = (before.is_none() && full_report).then(|| threads.tool_detail(&session, &job)).flatten();
            threads.trim_transcripts();
            (self.history_with_approval_receipts(&session, events), next_before, inner.next_seq - 1, detail)
        };
        let mut error = None;
        if let Some(detail) = detail {
            match detail.output() {
                Ok(Some(full)) => {
                    if let Some(output) = events.iter_mut().find_map(|event| match event {
                        ServerEvent::ToolResult { item, output, .. } if *item == job => Some(output),
                        _ => None,
                    }) {
                        *output = full;
                    }
                }
                Ok(None) => {}
                Err(_) => error = Some("The full worker report is unavailable.".into()),
            }
        }
        ServerEvent::ThreadJobDetails { session, job, request, through, before, next_before, events, error }
    }

    pub(crate) fn thread_job_summary(&self, event: &ServerEvent) -> Option<(String, u64, fuzukue_proto::JobSummary)> {
        let mut inner = self.inner.lock().unwrap();
        let through = inner.next_seq - 1;
        let threads = inner.threads.as_mut()?;
        let summary = threads.job_summary_for_event(event);
        summary.map(|(session, summary)| (session.into(), through, summary))
    }

    pub(crate) fn thread_jobs(&self, session: &str) -> (u64, Vec<fuzukue_proto::JobSummary>) {
        let mut inner = self.inner.lock().unwrap();
        let through = inner.next_seq - 1;
        let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
        let jobs = threads.job_summaries(session);
        threads.trim_transcripts();
        (through, jobs)
    }

    pub(crate) fn thread_job_report(&self, session: String, job: String, request: u64) -> ServerEvent {
        let result = {
            let mut inner = self.inner.lock().unwrap();
            let threads = inner.threads.as_mut().expect("Workspace initializes the thread store");
            let ok = threads.delegate_record(&session, &job).and_then(|(_, ok, _)| ok);
            let detail = ok.and_then(|_| threads.tool_detail(&session, &job));
            threads.trim_transcripts();
            ok.zip(detail)
        }.and_then(|(ok, detail)| Some((ok, serde_json::from_str(&detail.output().ok()??).ok()?)));

        ServerEvent::ThreadJobReport { session, job, request, result }
    }

    pub(crate) fn subscribe_transcripts(
        &self, after: u64, resume: &TranscriptResume,
    ) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        let inner = self.inner.lock().unwrap();
        let live = self.tx.subscribe();
        let threads = inner.threads.as_ref().expect("Workspace initializes the thread store");
        let through = inner.next_seq - 1;
        let missed = inner.log.iter().filter(|env| env.seq > after);
        let can_resume = resume.epoch.as_deref() == Some(&self.epoch)
            && after <= through
            && missed.clone().zip(after + 1..).all(|(env, seq)| env.seq == seq)
            && missed.clone().next_back().map_or(after, |env| env.seq) == through;

        let mut valid = HashSet::new();
        let mut reset_sessions = BTreeSet::new();
        for cache in &resume.cached_sessions {
            if can_resume && threads.history_generation(&cache.session) == Some(cache.generation) {
                valid.insert(cache.session.as_str());
            } else {
                reset_sessions.insert(cache.session.clone());
            }
        }

        let mut initial: Vec<_> = inner.log.iter().filter(|env| {
            if let Some(session) = env.event.transcript_session() {
                return can_resume && env.seq > after && valid.contains(session)
                    && !threads.hidden_replay_event(&env.event);
            }
            (!can_resume || env.seq > after)
                && !matches!(env.event, ServerEvent::ThreadRestored { .. }
                    | ServerEvent::ThreadHistoryInvalidated { .. }
                    | ServerEvent::TurnActivity { .. } | ServerEvent::Paused { .. } | ServerEvent::Resumed { .. }
                    | ServerEvent::QuestionAsked { .. } | ServerEvent::QuestionResolved { .. }
                    | ServerEvent::GitHubConnectCard { .. } | ServerEvent::Error { .. })
                && keep_with_thread_tails(threads, &env.event)
        }).cloned().collect();
        initial.retain_mut(|env| retain_replay_event(&mut env.event));

        let mut approvals_and_questions: Vec<_> = inner.approvals.values().filter(|pending| pending.live())
            .map(|pending| &pending.envelope)
            .chain(inner.questions.values().filter(|pending| pending.live()).map(|pending| &pending.envelope))
            .filter(|env| match &env.event {
                ServerEvent::ApprovalRequested { session, .. } | ServerEvent::QuestionAsked { session, .. } =>
                    threads.contains(session) || !crate::threads::is_thread(session),
                _ => false,
            }).collect();
        approvals_and_questions.sort_unstable_by_key(|env| env.seq);
        initial.extend(approvals_and_questions.into_iter().map(|env| Envelope { seq: 0, ..env.clone() }));
        let mut workers = BTreeMap::<String, BTreeSet<String>>::new();
        for (session, job) in inner.workers.keys() {
            workers.entry(session.clone()).or_default().insert(job.clone());
        }
        initial.push(crate::snapshot(ServerEvent::ChatActivity {
            turns: inner.running.iter().cloned().collect(), workers,
        }));
        initial.push(crate::snapshot(threads.snapshot()));
        initial.extend(threads.damage_notices().map(crate::snapshot));
        initial.push(crate::snapshot(ServerEvent::ThreadSync {
            epoch: self.epoch.clone(), through, reset_sessions: reset_sessions.into_iter().collect(),
        }));
        (initial, live)
    }

    pub fn subscribe_client(&self, after: u64, include_threads: bool) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        let mut inner = self.inner.lock().unwrap();
        let rx = self.tx.subscribe();
        let threads = inner.threads.as_ref().filter(|_| include_threads);
        let cold_threads = threads.filter(|_| after == 0);
        let unseen = inner.log.iter().filter(|env| env.seq > after);
        let cursor = unseen.clone().next_back().map_or(0, |env| env.seq);
        let mut backlog: Vec<_> = unseen
            .filter(|env| cold_threads.is_none_or(|threads| keep_with_thread_tails(threads, &env.event)))
            .cloned().collect();
        // Older journal entries may contain one-time actions or transient notices.
        backlog.retain_mut(|envelope| retain_replay_event(&mut envelope.event));
        if include_threads {
            let present: HashSet<_> = backlog.iter().filter_map(|envelope| match &envelope.event {
                ServerEvent::QuestionAsked { request_id, .. } => Some(request_id.as_str()),
                _ => None,
            }).collect();
            let resolved: HashSet<_> = backlog.iter().filter_map(|envelope| match &envelope.event {
                ServerEvent::QuestionResolved { request_id, .. } => Some(request_id.clone()),
                _ => None,
            }).collect();
            let mut live: Vec<_> = inner.questions.iter().filter(|(id, pending)| {
                pending.live() && !present.contains(id.as_str())
            }).map(|(_, pending)| &pending.envelope).collect();
            live.sort_unstable_by_key(|envelope| envelope.seq);
            let live = live.into_iter().map(|envelope| Envelope { seq: 0, ..envelope.clone() }).collect::<Vec<_>>();
            if after == 0 {
                backlog.retain(|envelope| match &envelope.event {
                    ServerEvent::QuestionAsked { request_id, .. } =>
                        resolved.contains(request_id.as_str()) || inner.questions.get(request_id).is_some_and(PendingQuestion::live),
                    _ => true,
                });
            }
            backlog.extend(live);
        }
        if let Some(threads) = threads {
            let mut workers = BTreeMap::<String, BTreeSet<String>>::new();
            for (session, job) in inner.workers.keys() {
                workers.entry(session.clone()).or_default().insert(job.clone());
            }
            if after == 0 {
                let mut history = replay_thread_tails(threads, &inner.log, &workers);
                let mut approvals: Vec<_> = inner.approvals.values().filter(|approval| approval.live())
                    .map(|approval| &approval.envelope)
                    .filter(|envelope| matches!(&envelope.event, ServerEvent::ApprovalRequested { session, .. }
                        if threads.contains(session) || !crate::threads::is_thread(session)))
                    .collect();
                approvals.sort_unstable_by_key(|envelope| envelope.seq);
                history.extend(approvals.into_iter().map(|envelope| Envelope {
                    seq: 0, ..envelope.clone()
                }));
                backlog.splice(..0, history);
            } else {
                backlog.retain_mut(|envelope| {
                    if let ServerEvent::ThreadRestored { events, .. } = &mut envelope.event {
                        events.retain(|event| !threads.hidden_replay_event(event));
                    }
                    !threads.hidden_replay_event(&envelope.event)
                });
            }
            // Filtered events still advance the reconnect cursor.
            let seq = (cursor > backlog.last().map(|env| env.seq).unwrap_or(0)).then_some(cursor).unwrap_or(0);
            if after != 0 {
                // A reconnecting client may still have these rows in memory.
                backlog.extend(threads.hide_replies_on_reconnect()
                    .map(|event| Envelope { workspace: None, seq: 0, event }));
            }
            backlog.push(Envelope { workspace: None, seq: 0, event: ServerEvent::ChatActivity {
                turns: inner.running.iter().cloned().collect(), workers,
            } });
            backlog.push(Envelope { workspace: None, seq, event: threads.snapshot() });
            backlog.extend(threads.damage_notices().map(crate::snapshot));
        }
        if include_threads {
            if let Some(threads) = inner.threads.as_mut() {
                threads.trim_transcripts();
            }
        }
        (backlog, rx)
    }

    /// `after` より後のログと、以降のライブ購読を同時に返す（取りこぼし無し）。
    pub fn subscribe(&self, after: u64) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        self.subscribe_client(after, false)
    }
}

fn observe_question(questions: &mut HashMap<String, PendingQuestion>, envelope: &Envelope) {
    match &envelope.event {
        ServerEvent::QuestionAsked { request_id, .. } => {
            questions.insert(request_id.clone(), PendingQuestion {
                envelope: envelope.clone(), observed: Observed::now(), backend: None,
            });
        }
        ServerEvent::QuestionResolved { request_id, .. } => {
            questions.remove(request_id);
        }
        _ => {}
    }
}

fn observe_approval(inner: &mut Inner, envelope: &Envelope, waiter: ApprovalWaiter) {
    match &envelope.event {
        ServerEvent::ApprovalRequested { request_id, .. } => {
            inner.approvals.insert(request_id.clone(), PendingApproval {
                envelope: envelope.clone(), observed: Observed::now(), waiter, full: None,
            });
        }
        ServerEvent::ApprovalResolved { request_id, .. } => {
            inner.approvals.remove(request_id);
            inner.resolved_approvals.insert(request_id.clone());
        }
        _ => {}
    }
}

fn observe_worker(workers: &mut BTreeMap<(String, String), WorkerActivity>, event: &ServerEvent) {
    match event {
        ServerEvent::ToolCall { session, item, name, input, .. }
            if name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
            workers.insert((session.clone(), item.clone()), WorkerActivity {
                started_at: input["started_at_ms"].as_i64(),
                title: input["label"].as_str().or_else(|| input["title"].as_str())
                    .filter(|title| !title.trim().is_empty()).map(str::to_owned),
            });
        }
        ServerEvent::DelegateRenamed { session, job, change } => {
            if let Some(worker) = workers.get_mut(&(session.clone(), job.clone())) {
                worker.title = Some(change.label.clone());
            }
        }
        ServerEvent::ToolResult { session, item, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
            workers.remove(&(session.clone(), item.clone()));
        }
        _ => {}
    }
}

fn observe_background(background: &mut BTreeMap<(BlockerKind, String), i64>, event: &ServerEvent) {
    let (key, running) = match event {
        ServerEvent::BuildStatus { deck, state, .. } => ((BlockerKind::Build, deck.clone()), state == "running"),
        ServerEvent::ReviewStatus { deck, kind, state, .. } =>
            ((BlockerKind::Review, format!("{deck} / {kind}")), state == "running"),
        ServerEvent::TestRun { run } =>
            ((BlockerKind::Test, run.suite.clone()), run.state == fuzukue_proto::TestState::Running),
        _ => return,
    };
    if running {
        background.entry(key).or_insert_with(|| chrono::Utc::now().timestamp_millis());
    } else {
        background.remove(&key);
    }
}

fn retain_replay_event(event: &mut ServerEvent) -> bool {
    if let ServerEvent::ThreadRestored { events, .. } = event {
        events.retain_mut(retain_replay_event);
    }
    // Before composer notices carried a thread ID, these were journaled as workspace errors.
    if let ServerEvent::Error { message } = event {
        if message.starts_with("受付への連絡: ")
            || matches!(message.as_str(), "Codex: 止めた" | crate::PARTNER_SWITCH_WAIT | crate::claude::PARTNER_SWITCH_WAIT)
        {
            return false;
        }
    }
    !event.is_live_only() && !crate::planner::is_transient_notice(event)
}

fn tool_key(event: &ServerEvent) -> Option<(&str, &str, bool)> {
    match event {
        // The transcript has current job metadata, including edits made after the call.
        ServerEvent::ToolCall { item, name, .. }
            if name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB) => None,
        ServerEvent::ToolCall { session, item, .. } => Some((session, item, false)),
        ServerEvent::ToolResult { session, item, .. } => Some((session, item, true)),
        _ => None,
    }
}

/// Use the transcript tail for messages and borrow the recent journal for tool details.
fn replay_thread_tails(
    threads: &crate::threads::Store,
    log: &ReplayLog,
    workers: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Envelope> {
    let tools: HashMap<_, _> = log.iter().filter_map(|env| {
        Some((tool_key(&env.event)?, &env.event))
    }).collect();
    threads.replay_tails(workers, |event| {
        let recorded = tool_key(&event).and_then(|key| tools.get(&key));
        crate::threads::bound_history_tool(recorded.map(|e| (*e).clone()).unwrap_or(event))
    }).map(|event| Envelope { workspace: None, seq: 0, event }).collect()
}

fn keep_with_thread_tails(threads: &crate::threads::Store, event: &ServerEvent) -> bool {
    match event {
        ServerEvent::Paused { session, .. } | ServerEvent::Resumed { session }
        | ServerEvent::SessionStarted { session, .. } | ServerEvent::SessionStats { session, .. }
        | ServerEvent::ChatSettingsChanged { session, .. }
            if crate::threads::is_legacy_session(session) => false,
        ServerEvent::ApprovalRequested { .. } | ServerEvent::ApprovalResolved { .. } => false,
        ServerEvent::Paused { session, .. } | ServerEvent::Resumed { session } if threads.contains(session) => {
            threads.active(session)
        }
        event => !crate::threads::history_session(event)
            .is_some_and(|id| crate::threads::is_thread(id) || crate::threads::is_legacy_session(id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[ignore = "generates a workspace-sized synthetic journal; run in release mode"]
    fn synthetic_thread_performance() -> anyhow::Result<()> {
        use std::io::{BufWriter, Write};
        use std::time::Instant;

        fn rss() -> String {
            std::fs::read_to_string("/proc/self/status").ok()
                .and_then(|status| status.lines().find(|line| line.starts_with("VmRSS:")).map(str::to_owned))
                .unwrap_or_default()
        }

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        std::fs::create_dir(project.path().join("threads"))?;
        let payload = "synthetic ".repeat(61);
        let path = state.path().join("thread-timeline.jsonl");
        let mut journal = BufWriter::new(std::fs::File::create(&path)?);
        let mut seq = 0;
        for thread in 0..192 {
            let id = format!("thread-perf-{thread}");
            let mut transcript = BufWriter::new(std::fs::File::create(
                project.path().join("threads").join(format!("{id}.md")),
            )?);
            writeln!(transcript, "---\nid: {id}\ntitle: Synthetic test {thread}\ncreated: 1\nupdated: 1\n---")?;
            for message in 0..1906 {
                seq += 1;
                let item = format!("synthetic-{seq}");
                let event = if message % 16 == 6 {
                    ServerEvent::ToolCall {
                        session: id.clone(), item, actor: Actor::Claude,
                        name: "synthetic_tool".into(), input: json!({"data":payload}),
                    }
                } else if message % 16 == 7 {
                    ServerEvent::ToolResult {
                        session: id.clone(), item: format!("synthetic-{}", seq - 1),
                        ok: true, output: payload.clone(),
                    }
                } else {
                    let actor = if message % 2 == 0 { Actor::User } else { Actor::Claude };
                    let role = if actor == Actor::User { "User" } else { "Claude" };
                    writeln!(transcript, "<!-- labo-thread {} -->\n## {role}\n\n{payload}\n",
                        json!({"item":item,"actor":actor}))?;
                    ServerEvent::TextDelta {
                        session: id.clone(), item, actor, text: payload.clone(),
                    }
                };
                serde_json::to_writer(&mut journal, &Envelope { workspace: None, seq, event })?;
                writeln!(journal)?;
            }
            transcript.flush()?;
        }
        journal.flush()?;
        drop(journal);
        eprintln!("journal_bytes={} events={seq} fixture_rss={}", path.metadata()?.len(), rss());
        for run in 0..2 {
            let start = Instant::now();
            let hub = Hub::with_timeline(state.path());
            let hub_ms = start.elapsed().as_secs_f64() * 1000.0;
            hub.load_threads(project.path(), state.path());
            let start_ms = start.elapsed().as_secs_f64() * 1000.0;
            let (initial, _) = hub.subscribe_transcripts(0, &TranscriptResume::default());
            let initial_bytes: usize = initial.iter().map(|env| serde_json::to_vec(env).unwrap().len()).sum();
            eprintln!("run={run} hub_ms={hub_ms:.2} workspace_ms={start_ms:.2} initial_bytes={initial_bytes} {}", rss());
            let mut observations = Vec::new();
            let mut response_bytes = 0;
            for (request, thread) in [0, 191, 96, 1, 2, 3, 4, 190, 0, 191, 96, 189].into_iter().enumerate() {
                let start = Instant::now();
                let event = hub.thread_tail_mode(format!("thread-perf-{thread}"), request as u64, true);
                assert!(matches!(&event, ServerEvent::ThreadTail { events, .. } if !events.is_empty()));
                response_bytes += serde_json::to_vec(&Envelope { workspace: None, seq: 0, event })?.len();
                observations.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            observations.sort_by(f64::total_cmp);
            eprintln!("run={run} switch_median_ms={:.2} switch_max_ms={:.2} mean_switch_bytes={} after_switch_rss={}",
                observations[observations.len()/2], observations[observations.len()-1],
                response_bytes/observations.len(), rss());
        }
        Ok(())
    }

    fn sample_question() -> Vec<Question> {
        questions::parse(&json!({"questions": [
            {"id": "value", "question": "Value?", "options": null, "isSecret": true}
        ]}), false).unwrap()
    }

    fn latest_question(hub: &Hub) -> Envelope {
        hub.subscribe(0).0.into_iter().rev().find(|env|
            matches!(env.event, ServerEvent::QuestionAsked { .. })).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn question_reconnect_and_racing_clients_resolve_once_with_masked_journal() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let hub = Arc::new(Hub::with_timeline(dir.path()));
        let actor = Actor::Codex { thread_id: "thread".into(), parent_tool_use: None };
        let reply = hub.ask_question("session".into(), actor, json!(9), sample_question());
        let asked = latest_question(&hub);
        let ServerEvent::QuestionAsked { request_id, .. } = asked.event else { unreachable!() };
        let reconnect = hub.subscribe_client(asked.seq, true).0;
        assert!(matches!(&reconnect[..], [Envelope {
            seq: 0, event: ServerEvent::QuestionAsked { request_id: id, .. }, ..
        }] if id == &request_id));
        assert!(hub.answer_question(&request_id, BTreeMap::new(), false).is_err());
        let start = Arc::new(std::sync::Barrier::new(2));
        let clients: Vec<_> = ["first-secret", "second-secret"].into_iter().map(|text| {
            let (hub, start, request_id) = (hub.clone(), start.clone(), request_id.clone());
            tokio::task::spawn_blocking(move || {
                start.wait();
                hub.answer_question(&request_id, BTreeMap::from([("value".into(), QuestionAnswer {
                    selected: Vec::new(), text: Some(text.into()),
                })]), false)
            })
        }).collect();
        for client in clients { client.await??; }
        let rpc = questions::codex_response(json!(9), reply.await?);
        assert!(matches!(rpc["result"]["answers"]["value"]["answers"][0].as_str(),
            Some("user_note: first-secret" | "user_note: second-secret")));
        assert!(hub.live_approval_threads().is_empty());
        drop(hub);
        let mut resolved = Vec::new();
        crate::state::read_timeline(dir.path(), |env| {
            let wire = serde_json::to_string(&env).unwrap();
            assert!(!wire.contains("first-secret") && !wire.contains("second-secret"));
            if matches!(env.event, ServerEvent::QuestionResolved { .. }) { resolved.push(env.event); }
        });
        assert!(matches!(&resolved[..], [ServerEvent::QuestionResolved {
            outcome: QuestionOutcome::Answered, answers, ..
        }] if answers["value"] == [fuzukue_proto::SECRET_ANSWER_MASK]));
        let restored = Hub::with_timeline(dir.path());
        assert_eq!(restored.subscribe_client(0, true).0.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn questions_cancel_on_turn_end_backend_cancellation_and_orphan_expiry() -> anyhow::Result<()> {
        let hub = Hub::new();
        let actor = Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some("delegate:a".into()) };
        let reply = hub.ask_question("session".into(), actor.clone(), json!(1), sample_question());
        let second = hub.ask_question("session".into(), actor.clone(), json!(2), sample_question());
        let client = hub.register_question_client();
        hub.expire_orphaned_approvals(Duration::ZERO);
        assert_eq!(hub.live_approval_threads(), BTreeSet::from(["session".into()]));
        hub.cancel_questions("session", &actor, Some(&json!(1)));
        assert!(reply.await.is_err());
        hub.publish(ServerEvent::TurnEnded { session: "session".into(), actor: Actor::Claude });
        assert!(!hub.live_approval_threads().is_empty());
        hub.publish(ServerEvent::TurnEnded { session: "session".into(), actor });
        assert!(second.await.is_err());
        drop(client);
        let orphan = hub.ask_question("session".into(), Actor::Claude, json!("control"), sample_question());
        hub.expire_orphaned_approvals(Duration::ZERO);
        assert!(orphan.await?.is_none());
        assert!(hub.live_approval_threads().is_empty());
        assert_eq!(hub.subscribe(0).0.iter().filter(|env| matches!(env.event,
            ServerEvent::QuestionResolved { outcome: QuestionOutcome::Cancelled, .. })).count(), 3);
        Ok(())
    }

    #[test]
    fn resume_returns_only_newer() {
        let hub = Hub::new();
        for i in 0..3 {
            hub.publish(ServerEvent::FileChanged { path: i.to_string() });
        }
        let (backlog, _) = hub.subscribe(1);
        assert_eq!(backlog.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn job_summary_updates_with_no_transcript_ranges_open() -> anyhow::Result<()> {
        use fuzukue_proto::{Actor, JobStatus};

        let project = crate::test_support::TestDir::new();
        let state = project.path().join("state");
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let session = hub.edit_threads(|store| store.create(None))?;
        let (initial, _) = hub.subscribe_transcripts(0, &TranscriptResume::default());
        assert!(initial.iter().all(|env| env.event.transcript_session().is_none()));
        let job = "delegate:wire";
        let call = |started| ServerEvent::ToolCall {
            session: session.clone(), item: job.into(), name: "delegate".into(), actor: Actor::System,
            input: serde_json::json!({
                "model":"gpt-5.5", "queued_at_ms":10, "started_at_ms":started,
                "selection":{"backend":"codex","model":"gpt-5.5","effort":"high"},
                "routing":{"tier":"heavy","reason":"test"},
            }),
        };
        let mut previous = 0;
        let mut publish = |event: ServerEvent| {
            hub.publish(event.clone());
            let (_, through, summary) = hub.thread_job_summary(&event).unwrap();
            assert!(through > previous);
            previous = through;
            summary
        };
        let queued = publish(call(None));
        assert_eq!(queued.status, JobStatus::Queued);
        assert_eq!(queued.elapsed_at(50), Some(40));
        let running = publish(call(Some(25)));
        assert_eq!(running.status, JobStatus::Running);
        assert_eq!(running.job.routing.unwrap().tier, fuzukue_proto::Tier::Heavy);
        assert_eq!(running.job.selection.unwrap().effort.as_deref(), Some("high"));
        for path in ["early.md", "last.md"] {
            let summary = publish(ServerEvent::ToolCall {
                session: session.clone(), item: "worker-call".into(), name: "Read".into(),
                actor: Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some(job.into()) },
                input: serde_json::json!({"path":path}),
            });
            assert_eq!(summary.tools, 1);
            assert_eq!(summary.last_tool, Some(format!("Read {path}")));
        }
        let finished = publish(ServerEvent::ToolResult {
            session: session.clone(), item: job.into(), ok: true,
            output: serde_json::json!({
                "elapsed_ms":75, "text":"Job report", "files":{"source":"declared","changed_files":["notes.md"]},
            }).to_string(),
        });
        assert_eq!(finished.status, JobStatus::Succeeded);
        assert_eq!(finished.elapsed_at(999), Some(75));
        assert_eq!(finished.changed_file_count, Some(1));
        hub.rename_delegate(&session, job, "New name".into(), None, "user")?;
        hub.dismiss_delegate(&session, job)?;
        let (_, saved) = hub.thread_jobs(&session);
        let [saved] = saved.as_slice() else { panic!("expected one job") };
        assert!(saved.job.dismissed_from_panel);
        assert_eq!(saved.job.display.label.as_deref(), Some("New name"));
        let interrupted = "delegate:interrupted";
        let ServerEvent::ToolCall { actor, name, input, .. } = call(Some(50)) else { unreachable!() };
        hub.publish(ServerEvent::ToolCall { session: session.clone(), item: interrupted.into(), actor, name, input });
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: format!("{interrupted}:tool"), actor: Actor::Claude,
            name: "Read".into(), input: serde_json::json!({"path":"README.md"}),
        });
        drop(hub);

        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let (_, restored) = hub.thread_jobs(&session);
        let [restored, interrupted] = restored.as_slice() else { panic!("expected two jobs") };
        assert_eq!(serde_json::to_value(restored)?, serde_json::to_value(saved)?);
        assert_eq!(interrupted.status, JobStatus::Interrupted);
        assert_eq!(interrupted.tools, 1);
        assert_eq!(interrupted.last_tool.as_deref(), Some("Read README.md"));
        Ok(())
    }

    #[test]
    fn reattach_skips_navigation_and_client_commands() -> anyhow::Result<()> {
        use fuzukue_proto::{ClientCommand, ThreadGroup};

        let project = crate::test_support::TestDir::new();
        let state = project.path().join("state");
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let session = hub.edit_threads(|store| store.create(None))?;
        let commands = [
            ServerEvent::OpenFile { session: session.clone(), path: "card.md".into(), lines: Some((2, 4)) },
            ServerEvent::OpenPaper { session: session.clone(), deck: "paper".into(), card: None },
            ServerEvent::ClientCommand { command: ClientCommand::SetGroupState { group: ThreadGroup::Current, open: false } },
        ];
        let call = ServerEvent::ToolCall {
            session: session.clone(), item: "call".into(), actor: Actor::Claude,
            name: "open_file".into(), input: serde_json::json!({"path": "card.md"}),
        };
        let result = ServerEvent::ToolResult {
            session: session.clone(), item: "call".into(), ok: true, output: "card.md".into(),
        };
        hub.publish(call.clone());
        hub.publish(result.clone());
        hub.publish(ServerEvent::TurnEnded { session: session.clone(), actor: Actor::Claude });
        drop(hub);

        // Seed sequenced journal entries from an unguarded publisher.
        let mut writer = crate::state::TimelineWriter::open(&state)?;
        writer.append(&Envelope { workspace: None, seq: 4, event: ServerEvent::ThreadRestored {
            session: session.clone(), events: [commands.to_vec(), vec![call, result]].concat(),
        }});
        for (seq, event) in (5..).zip(commands.iter().cloned()) {
            writer.append(&Envelope { workspace: None, seq, event });
        }
        drop(writer);

        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        for after in [0, 0, 3, 4] {
            let (history, _) = hub.subscribe_client(after, true);
            assert!(!history.iter().any(|env| matches!(env.event,
                ServerEvent::OpenFile { .. } | ServerEvent::OpenPaper { .. } | ServerEvent::ClientCommand { .. }
            )), "client actions replayed after cursor {after}");
            assert!(matches!(history.last(), Some(Envelope { seq: 7, event: ServerEvent::Threads { .. }, .. })));
            if after == 0 {
                assert!(history.iter().any(|env| matches!(&env.event, ServerEvent::ToolCall { item, .. } if item == "call")));
                assert!(history.iter().any(|env| matches!(&env.event, ServerEvent::ToolResult { item, .. } if item == "call")));
            }
            if after == 3 {
                assert!(matches!(history[0].event,
                    ServerEvent::ThreadRestored { ref events, .. }
                        if matches!(events.as_slice(), [ServerEvent::ToolCall { .. }, ServerEvent::ToolResult { .. }])
                ));
            }
        }

        let (_, mut live) = hub.subscribe_client(7, true);
        for event in &commands {
            hub.publish(event.clone());
            let received = live.try_recv()?;
            assert_eq!(received.seq, 0);
            assert_eq!(serde_json::to_value(received.event)?, serde_json::to_value(event)?);
        }
        hub.publish(ServerEvent::ThreadRestored { session, events: commands.to_vec() });
        let restored = live.try_recv()?;
        assert_eq!(restored.seq, 8);
        assert!(matches!(restored.event, ServerEvent::ThreadRestored { events, .. } if events.is_empty()));
        assert!(matches!(crate::state::load_timeline(&state, BACKLOG).last(),
            Some(Envelope { seq: 8, event: ServerEvent::ThreadRestored { events, .. }, .. }) if events.is_empty()
        ));
        Ok(())
    }

    fn planner_notice(session: &str, burst: &str, key: &str) -> ServerEvent {
        ServerEvent::LocalNotice {
            session: session.into(),
            item: format!("planner-notice:{burst}:1"),
            text: key.into(),
            localized: Some(fuzukue_proto::LocalizedMessage::new(key, [("burst", burst.into()), ("count", "1".into())])),
        }
    }

    #[test]
    fn composer_notices_are_live_and_legacy_notices_are_excluded_from_replay() -> anyhow::Result<()> {
        let project = crate::test_support::TestDir::new();
        let state = project.path().join("state");
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let session = hub.edit_threads(|store| store.create(None))?;
        let path = hub.thread_path(&session)?;
        let before = std::fs::read(&path)?;
        let (_, mut live) = hub.subscribe_client(0, true);
        let other_workspace = Hub::new();
        let (_, mut other_live) = other_workspace.subscribe(0);

        let wait = crate::backend_switch_notice(&session, &anyhow::anyhow!(crate::claude::PARTNER_SWITCH_WAIT))
            .expect("switch wait should use the composer's status slot");
        hub.publish_snapshot(wait);
        crate::publish_codex_turn_error(&hub, &session, crate::cancellation::Cancelled.into());
        for key in ["server-claude-switch-wait", "server-codex-stopped"] {
            let envelope = live.try_recv()?;
            assert_eq!(envelope.seq, 0);
            assert!(matches!(&envelope.event, ServerEvent::LocalNotice { session: owner, item, localized: Some(message), .. }
                if owner == &session && item == fuzukue_proto::COMPOSER_NOTICE_ITEM && message.key == key));
            let wire = crate::wire_event(&envelope, 60, Some("origin"), fuzukue_i18n::Locale::JAPANESE);
            let wire: serde_json::Value = serde_json::from_str(wire.to_text()?)?;
            assert_eq!(wire["workspace"], "origin");
            assert_eq!(wire["event"]["type"], "local_notice");
            assert_eq!(wire["event"]["session"], session);
        }
        assert!(matches!(other_live.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert_eq!(std::fs::read(path)?, before);
        assert!(crate::state::load_timeline(&state, BACKLOG).is_empty());
        assert!(!hub.subscribe_client(0, true).0.iter().any(|env| env.event.is_live_only()));
        drop(hub);

        let mut writer = crate::state::TimelineWriter::open(&state)?;
        let messages = [
            "受付への連絡: 相手を替えるには Claude のターンの完了を待つ",
            "Codex: 止めた",
            crate::PARTNER_SWITCH_WAIT,
            "Codex: app-server exited",
        ];
        for (index, message) in messages.iter().enumerate() {
            writer.append(&Envelope { workspace: None, seq: index as u64 + 1,
                event: ServerEvent::Error { message: (*message).into() },
            });
        }
        drop(writer);
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        for after in [0, 1] {
            let (backlog, _) = hub.subscribe_client(after, true);
            let errors = backlog.iter().filter_map(|env| match &env.event {
                ServerEvent::Error { message } => Some(message.as_str()),
                _ => None,
            }).collect::<Vec<_>>();
            assert_eq!(errors, [messages[3]]);
        }
        Ok(())
    }

    #[test]
    fn attaching_again_does_not_append_old_planner_failures() -> anyhow::Result<()> {
        let project = crate::test_support::TestDir::new();
        let state = project.path().join("state");
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let session = hub.edit_threads(|store| store.create(None))?;
        let path = hub.thread_path(&session)?;
        // Seed a portable transcript written before failure notices became live-only.
        hub.publish(planner_notice(&session, "old", "chat-plan-updated"));
        let markdown = std::fs::read_to_string(&path)?.replace("chat-plan-updated", "chat-plan-update-rejected");
        std::fs::write(&path, markdown)?;
        hub.publish(ServerEvent::TextDelta {
            session: session.clone(), item: "reply".into(), actor: Actor::Claude, text: "Latest reply".into(),
        });
        let mut writer = crate::state::TimelineWriter::open(&state)?;
        for (seq, message) in [(3, "方針の更新"), (4, "目標の継続")] {
            let event = ServerEvent::Error {
                message: format!("{message}: Verification rejected the attempt: unknown field `goal`"),
            };
            writer.append(&Envelope { workspace: None, seq, event });
        }
        writer.append(&Envelope {
            workspace: None, seq: 5, event: planner_notice(&session, "journal", "chat-goal-continuation-failed"),
        });
        drop(writer);
        drop(hub);

        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let latest_reply = |envelope: &Envelope| matches!(&envelope.event,
            ServerEvent::TextDelta { session: id, item, .. } if id == &session && item == "reply");
        for _ in 0..2 {
            // A fresh attachment after switching back starts at zero again.
            let (history, _) = hub.subscribe_client(0, true);
            assert_eq!(history.iter().filter(|event| latest_reply(event)).count(), 1);
            assert!(!history.iter().any(|env| matches!(&env.event,
                ServerEvent::Error { .. } | ServerEvent::LocalNotice { .. })));
            assert!(matches!(history.last(), Some(Envelope { seq: 5, event: ServerEvent::Threads { .. }, .. })));
        }
        let (resumed, _) = hub.subscribe_client(2, true);
        assert!(matches!(resumed.as_slice(), [
            Envelope { seq: 0, event: ServerEvent::ChatActivity { .. }, .. },
            Envelope { seq: 5, event: ServerEvent::Threads { .. }, .. },
        ]));
        Ok(())
    }

    #[test]
    fn planner_failures_are_live_and_worker_diagnostics_are_durable() -> anyhow::Result<()> {
        let project = crate::test_support::TestDir::new();
        let state = project.path().join("state");
        let hub = Hub::with_timeline(&state);
        hub.load_threads(project.path(), &state);
        let session = hub.edit_threads(|store| store.create(None))?;
        let (_, mut live) = hub.subscribe_client(0, true);

        let diagnostic = "delegate:fixture:planner-error:live";
        hub.publish(ServerEvent::TextDelta {
            session: session.clone(), item: diagnostic.into(), actor: Actor::System, text: "Raw rejection".into(),
        });
        hub.publish(planner_notice(&session, "live", "chat-plan-update-rejected"));

        assert!(matches!(live.try_recv()?.event, ServerEvent::TextDelta { item, .. } if item == diagnostic));
        let failure = live.try_recv()?;
        assert_eq!(failure.seq, 0);
        assert!(crate::planner::is_transient_notice(&failure.event));
        let markdown = std::fs::read_to_string(hub.thread_path(&session)?)?;
        assert!(markdown.contains("Raw rejection"));
        assert!(!markdown.contains("planner-notice:live:1"));

        hub.publish(planner_notice(&session, "success", "chat-plan-updated"));
        let journal = crate::state::load_timeline(&state, BACKLOG);
        assert_eq!(journal.len(), 2);
        assert!(!journal.iter().any(|env| crate::planner::is_transient_notice(&env.event)));
        let (history, _) = hub.subscribe_client(0, true);
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::LocalNotice { item, .. } if item == "planner-notice:success:1")));
        assert!(!history.iter().any(|env| crate::planner::is_transient_notice(&env.event)));
        Ok(())
    }
}
