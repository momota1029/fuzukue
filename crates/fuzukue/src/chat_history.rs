use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use fuzukue_proto::{CachedSession, ClientMsg, JobSummary, ServerEvent, ThreadHistoryFailure};
use gpui_kit::{Context, ListOffset};

use super::{CHAT_LEADING_PADDING, Chat, Content, Row, RowKind, Workbench};
use crate::ConnectionStatus;

#[derive(Default)]
pub(super) struct JobSummaries {
    pub(super) requested: bool,
    pub(super) received: bool,
    pub(super) report_request: Option<(u64, String)>,
    through: u64,
    jobs: HashMap<String, (u64, JobSummary)>,
}

impl JobSummaries {
    pub(super) fn update(&mut self, through: u64, summary: JobSummary) {
        if through >= self.through && self.jobs.get(&summary.id).is_none_or(|(seq, _)| *seq <= through) {
            self.jobs.insert(summary.id.clone(), (through, summary));
        }
    }

    pub(super) fn snapshot(&mut self, through: u64, first: bool, jobs: Vec<JobSummary>) {
        if through < self.through {
            return;
        }
        if first {
            self.jobs.retain(|_, (seq, _)| *seq > through);
            self.through = through;
            self.received = true;
        }
        for summary in jobs {
            self.update(through, summary);
        }
    }

    pub(super) fn get(&self, id: &str) -> Option<&JobSummary> {
        self.jobs.get(id).map(|(_, summary)| summary)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &JobSummary> {
        self.jobs.values().map(|(_, summary)| summary)
    }
}

#[derive(Default)]
pub(super) struct History {
    pub(super) before: Option<u64>,
    pub(super) pending: Option<u64>,
    pub(super) sparse_workers: Vec<String>,
    generation: Option<u64>,
    through: u64,
    tail: Option<PendingTail>,
    deferred_page_events: Vec<(u64, ServerEvent)>,
    page_through: HashMap<String, u64>,
    reset_position: Option<super::TranscriptPosition>,
    jump: Option<Jump>,
    forward: Option<PendingForward>,
    pub(super) next_after: Option<u64>,
    pages: VecDeque<LoadedPage>,
    last_scroll: Option<ListOffset>,
    direction: Option<Direction>,

}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Jump {
    History(u64),
    Latest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Older,
    Newer,
}

struct PendingForward {
    request: u64,
    start: u64,
    jump: bool,
    events: Vec<(u64, ServerEvent)>,
}

struct LoadedPage {
    start: u64,
    rows: usize,
}

struct PendingTail {
    request: u64,
    events: Vec<(u64, ServerEvent)>,
}

#[derive(Default)]
pub(crate) struct Transcripts {
    pub(crate) scoped: bool,
    replay: Vec<(u64, ServerEvent)>,
    workers: BTreeMap<String, BTreeSet<String>>,
    initial_activity: Option<ServerEvent>,
    initial_waiting: Option<ServerEvent>,
    initial_threads: Option<ServerEvent>,
    controls: BTreeMap<String, ServerEvent>,
    initial_controls: BTreeMap<String, ServerEvent>,
    advertised: Option<Vec<CachedSession>>,
}

pub(super) fn next_request() -> u64 {
    static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
    NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
}

fn history_item(event: &ServerEvent) -> Option<Cow<'_, str>> {
    match event {
        ServerEvent::TextDelta { item, .. } | ServerEvent::UserPrompt { item, .. }
        | ServerEvent::ToolCall { item, .. } | ServerEvent::ToolResult { item, .. }
        | ServerEvent::MessageDetails { item, .. } | ServerEvent::LocalNotice { item, .. }
        | ServerEvent::GitHubActivity { item, .. } => Some(Cow::Borrowed(item)),
        ServerEvent::DelegateProgress { job, .. } | ServerEvent::DelegateRenamed { job, .. }
        | ServerEvent::DelegateDismissed { job, .. } | ServerEvent::ThreadJobActivity { job, .. } => Some(Cow::Borrowed(job)),
        ServerEvent::CommentApplication { application, .. } => Some(Cow::Owned(format!("comments-{}", application.thread))),
        _ => None,
    }
}


impl Workbench {
    pub(crate) fn update_thread_job(&mut self, session: &str, through: u64, summary: JobSummary, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        let row = chat.by_item.get(&summary.id).copied();
        chat.jobs.update(through, summary);
        if let Some(row) = row {
            chat.remeasure(row, cx);
        }
        cx.notify();
    }

    pub(crate) fn update_thread_jobs(&mut self, session: &str, through: u64, first: bool, jobs: Vec<JobSummary>, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        let rows: Vec<_> = jobs.iter().filter_map(|job| chat.by_item.get(&job.id).copied()).collect();
        chat.jobs.snapshot(through, first, jobs);
        for row in rows {
            chat.remeasure(row, cx);
        }
        cx.notify();
    }

    pub(crate) fn request_thread_tail(&mut self) {
        if !self.transcripts.scoped || self.receiving_backlog
            || !matches!(self.status, ConnectionStatus::Connected)
            || self.workspaces.active.is_none()
            || !self.threads.iter().any(|thread| thread.id == self.active)
        {
            return;
        }
        let session = self.active.clone();
        let Some(chat) = self.chats.get_mut(&session) else { return };
        if self.protocol >= fuzukue_proto::JOB_SUMMARY_PROTOCOL && !chat.jobs.requested {
            chat.jobs.requested = self.out.send(ClientMsg::GetThreadJobs { session: session.clone() }).is_ok();
        }
        if let Some(generation) = chat.history.generation {
            match chat.history.jump {
                Some(Jump::History(start)) => {
                    self.request_history_from(&session, start, generation, true);
                    return;
                }
                Some(Jump::Latest) => {}
                None => return,
            }
        }
        if chat.history.tail.is_some() {
            return;
        }
        let request = next_request();
        if self.out.send(ClientMsg::GetThreadTail { session, request }).is_ok() {
            chat.history.tail = Some(PendingTail { request, events: Vec::new() });
        }
    }

    fn request_history_from(&mut self, session: &str, start: u64, generation: u64, jump: bool) {
        let history = &mut self.chats.get_mut(session).unwrap().history;
        if history.forward.is_some() {
            return;
        }
        let request = next_request();
        if self.out.send(ClientMsg::GetThreadHistoryFrom { session: session.into(), request, start, generation }).is_ok() {
            history.forward = Some(PendingForward { request, start, jump, events: Vec::new() });
        }
    }

    pub(super) fn jump_thread_history(&mut self, latest: bool, cx: &mut Context<Self>) -> bool {
        if !self.transcripts.scoped || self.protocol < fuzukue_proto::THREAD_WINDOW_PROTOCOL
            || !self.threads.iter().any(|thread| thread.id == self.active)
        {
            return false;
        }
        let chat = self.chats.get_mut(&self.active).unwrap();
        chat.cancel_history_prefetch(cx);
        chat.history.forward = None;
        chat.history.direction = None;
        chat.history.reset_position = None;
        chat.history.jump = Some(if latest { Jump::Latest } else { Jump::History(0) });
        self.request_thread_tail();
        cx.notify();
        true
    }

    pub(super) fn history_scrolled(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get_mut(&self.active) else { return };
        let scroll = chat.scroller.logical_scroll_top();
        let previous = chat.history.last_scroll.replace(scroll);
        let Some(previous) = previous else { return };
        let direction = match (scroll.item_ix, scroll.offset_in_item)
            .partial_cmp(&(previous.item_ix, previous.offset_in_item))
        {
            Some(std::cmp::Ordering::Less) => Direction::Older,
            Some(std::cmp::Ordering::Greater) => Direction::Newer,
            _ => return,
        };
        self.prefetch_history(direction, false, cx);
    }

    pub(super) fn history_scrolled_older(&mut self, cx: &mut Context<Self>) {
        self.prefetch_history(Direction::Older, true, cx);
    }

    fn prefetch_history(&mut self, direction: Direction, at_edge: bool, cx: &mut Context<Self>) {
        const PREFETCH_ROWS: usize = 32;

        if !matches!(self.status, ConnectionStatus::Connected) || self.receiving_backlog {
            return;
        }
        let session = self.active.clone();
        let Some(chat) = self.chats.get_mut(&session) else { return };
        if chat.history.jump.is_some() {
            return;
        }
        if chat.history.direction.replace(direction) != Some(direction) {
            chat.cancel_history_prefetch(cx);
        }
        let top = chat.scroller.logical_scroll_top().item_ix;
        let remaining = chat.scroller.max_offset_for_scrollbar().y + chat.scroller.scroll_px_offset_for_scrollbar().y;
        match direction {
            Direction::Older if at_edge || top <= PREFETCH_ROWS => self.request_older_history(&session),
            Direction::Newer if remaining <= chat.scroller.viewport_bounds().size.height.max(gpui_kit::px(48.)) => {
                if let (Some(start), Some(generation)) = (chat.history.next_after, chat.history.generation) {
                    self.request_history_from(&session, start, generation, false);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn finish_thread_tail(
        &mut self, event: ServerEvent, cx: &mut Context<Self>,
    ) {
        let ServerEvent::ThreadTail { session, request, generation, through, events, workers, next_before } = event
            else { unreachable!() };
        let Some(chat) = self.chats.get_mut(&session) else { return };
        let Some(pending) = chat.history.tail.take_if(|pending| pending.request == request) else { return };
        let latest = chat.history.jump == Some(Jump::Latest);
        if latest {
            chat.clear_transcript_rows();
            chat.history = History::default();
        }
        chat.history.generation = Some(generation);
        chat.history.through = through;
        chat.history.before = next_before;
        chat.history.sparse_workers = workers;
        let rows = chat.rows.len();
        chat.prepend_history(events, true, cx);
        if chat.history.pages.is_empty() {
            chat.history.pages.push_back(LoadedPage {
                start: next_before.unwrap_or(0), rows: (chat.rows.len() - rows).saturating_sub(chat.sparse_history_rows()),
            });
        }
        chat.replay_updates(pending.events.into_iter().filter(|(seq, _)| *seq > through)
            .map(|(_, event)| event), cx);
        chat.reconcile_activity(chat.busy, self.transcripts.workers.get(&session), cx);
        if !matches!(chat.history.jump, Some(Jump::History(_))) {
            if let Some(position) = chat.history.reset_position.take() {
                chat.restore_transcript_position(position);
            }
        }
        if latest {
            chat.scroll_to_end();
        } else if chat.history.jump.is_none() {
            chat.reveal_latest_conversation();
        }
        chat.history.last_scroll = Some(chat.scroller.logical_scroll_top());
        self.out.cache_transcript(&session, Some(generation));
        self.restore_approval_notices(&session, cx);
        if session == self.active {
            self.github_view();
            self.request_thread_tail();
        }
    }

    pub(crate) fn reset_thread_history(&mut self, session: &str, cx: &mut Context<Self>) {
        self.out.cache_transcript(session, None);
        let Some(chat) = self.chats.get_mut(session) else { return };
        chat.jobs = JobSummaries::default();
        if chat.history.generation.is_none() && chat.history.tail.is_none() {
            return;
        }
        chat.reset_transcript();
        let controls: Vec<_> = self.transcripts.controls.values().filter(|event| match event {
            ServerEvent::ApprovalRequested { session: owner, .. } | ServerEvent::QuestionAsked { session: owner, .. }
                => owner == session,
            _ => false,
        }).cloned().collect();
        for event in controls {
            self.apply_event(event, true, cx);
        }
        cx.notify();
    }

    pub(crate) fn clear_scoped_workspace(&mut self) {
        self.out.clear_transcript_resume();
        self.transcripts = Transcripts { scoped: self.transcripts.scoped, ..Transcripts::default() };
    }

    pub(crate) fn advertised_transcript_cache(&mut self, cached: Vec<CachedSession>) {
        self.transcripts.advertised = Some(cached);
    }

    pub(crate) fn disconnect_transcripts(&mut self, cx: &mut Context<Self>) {
        self.transcripts.advertised = None;
        self.transcripts.replay.clear();
        self.transcripts.initial_activity = None;
        self.transcripts.initial_waiting = None;
        self.transcripts.initial_threads = None;
        self.transcripts.initial_controls.clear();
        for chat in self.chats.values_mut() {
            chat.jobs.requested = false;
            chat.jobs.report_request = None;
            chat.history.tail = None;
            chat.history.forward = None;
            chat.cancel_history_prefetch(cx);
        }
    }

    /// Records control state independently of the saved rows, and buffers
    /// reconnect deltas until the server has validated the cached generations.
    pub(crate) fn scoped_event(&mut self, seq: u64, event: &ServerEvent, cx: &mut Context<Self>) -> Option<bool> {
        match event {
            ServerEvent::Threads { .. } if self.receiving_backlog => {
                self.transcripts.initial_threads = Some(event.clone());
                return None;
            }
            ServerEvent::ChatWaiting { .. } if self.receiving_backlog => {
                self.transcripts.initial_waiting = Some(event.clone());
                return None;
            }
            ServerEvent::ChatActivity { workers, .. } => {
                self.transcripts.workers.clone_from(workers);
                if self.receiving_backlog {
                    self.transcripts.initial_activity = Some(event.clone());
                    return None;
                }
            }
            ServerEvent::ApprovalRequested { request_id, .. } | ServerEvent::QuestionAsked { request_id, .. } => {
                if self.receiving_backlog {
                    self.transcripts.initial_controls.insert(request_id.clone(), event.clone());
                    return None;
                }
                self.transcripts.controls.insert(request_id.clone(), event.clone());
            }
            ServerEvent::ApprovalResolved { request_id, .. } | ServerEvent::QuestionResolved { request_id, .. } => {
                self.transcripts.controls.remove(request_id);
            }
            _ => {}
        }
        let Some(session) = event.transcript_session() else { return Some(true) };
        let received = self.out.prompt_received(seq, event);
        if self.receiving_backlog {
            self.transcripts.replay.push((seq, event.clone()));
            return None;
        }
        match event {
            ServerEvent::ToolCall { name, item, .. } if name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                self.transcripts.workers.entry(session.into()).or_default().insert(item.clone());
                cx.notify();
            }
            ServerEvent::ToolResult { item, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                if let Some(workers) = self.transcripts.workers.get_mut(session) {
                    workers.remove(item);
                }
                cx.notify();
            }
            _ => {}
        }
        let Some(chat) = self.chats.get_mut(session) else { return Some(false) };
        if let Some(tail) = &mut chat.history.tail {
            tail.events.push((seq, event.clone()));
        }
        if let Some(page) = &mut chat.history.forward {
            page.events.push((seq, event.clone()));
        }
        if chat.history.generation.is_none() || (seq != 0 && seq <= chat.history.through) {
            return Some(false);
        }
        if chat.buffer_job_event(seq, event) {
            return Some(false);
        }
        let item = history_item(event);
        if chat.history.next_after.is_some()
            && item.as_ref().is_some_and(|item| !chat.contains_history_item(item))
        {
            return Some(false);
        }
        if seq != 0 && chat.history.page_through.values().all(|&through| seq > through) {
            chat.history.page_through.clear();
        }
        if item.as_ref().and_then(|id| chat.history.page_through.get(id.as_ref()))
            .is_some_and(|&through| seq != 0 && seq <= through)
        {
            return Some(false);
        }
        if chat.history.pending.is_some() {
            let superseded = matches!(event, ServerEvent::ReplySuperseded { .. });
            let missing = item.as_ref().is_some_and(|id| !chat.contains_history_item(id));
            if missing || superseded {
                chat.history.deferred_page_events.push((seq, event.clone()));
            }
            if missing {
                return Some(false);
            }
        }
        if let Some((at, threshold)) = received.filter(|_| session == self.active) {
            // Run after on_conn applies the event and invalidates the view.
            cx.defer(move |_| {
                let elapsed = at.elapsed();
                if elapsed >= threshold {
                    eprintln!("send_latency seq={seq} receive_to_apply_us={}", elapsed.as_micros());
                }
            });
        }
        Some(true)
    }

    pub(crate) fn scoped_worker(&self, session: &str) -> Option<&str> {
        self.transcripts.workers.get(session)?.first().map(String::as_str)
    }

    pub(crate) fn sync_thread_transcripts(
        &mut self, workspace: &str, epoch: String, through: u64, reset_sessions: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(threads) = self.transcripts.initial_threads.take() {
            self.apply_event(threads, true, cx);
        }
        let applied = self.out.transcript_through(workspace, &epoch);
        if applied == 0 {
            for chat in self.chats.values_mut() {
                chat.jobs = JobSummaries::default();
            }
        }
        let mut reset_sessions: HashSet<_> = reset_sessions.into_iter().collect();
        if let Some(advertised) = self.transcripts.advertised.take() {
            let advertised: HashMap<_, _> = advertised.into_iter()
                .map(|cache| (cache.session, cache.generation)).collect();
            // The socket can send Hello before the UI has finished applying the
            // preceding connection's last tail response.
            reset_sessions.extend(self.chats.iter().filter(|(session, chat)| {
                chat.history.generation.is_some() && chat.history.generation != advertised.get(*session).copied()
            }).map(|(session, _)| session.clone()));
        }
        let controls = std::mem::take(&mut self.transcripts.initial_controls);
        let previous_controls = std::mem::replace(&mut self.transcripts.controls, controls);
        for (request_id, event) in previous_controls {
            if self.transcripts.controls.contains_key(&request_id) {
                continue;
            }
            match event {
                ServerEvent::ApprovalRequested { .. } => self.apply_event(ServerEvent::ApprovalResolved {
                    request_id, decision: "cancelled".into(), automatic: false,
                }, true, cx),
                ServerEvent::QuestionAsked { session, .. } => self.apply_event(ServerEvent::QuestionResolved {
                    session, request_id, outcome: fuzukue_proto::QuestionOutcome::Cancelled, answers: BTreeMap::new(),
                }, true, cx),
                _ => {}
            }
        }
        for session in reset_sessions {
            self.reset_thread_history(&session, cx);
        }

        // Workbench commits this cursor only after applying the validated deltas.
        for (seq, event) in std::mem::take(&mut self.transcripts.replay) {
            if seq <= applied {
                continue;
            }
            if let Some(chat) = event.transcript_session().and_then(|session| self.chats.get_mut(session)) {
                let item_through = history_item(&event).and_then(|item|
                    chat.history.page_through.get(item.as_ref()).copied()).unwrap_or(0);
                let belongs = chat.history.next_after.is_none()
                    || history_item(&event).is_none_or(|item| chat.contains_history_item(&item));
                if belongs && chat.history.generation.is_some() && seq > chat.history.through.max(item_through) {
                    chat.replay_updates(std::iter::once(event), cx);
                }
            }
        }
        for chat in self.chats.values_mut() {
            // The reconnect waiting snapshot replaces any last Paused signal.
            chat.paused = None;
        }
        if let Some(waiting) = self.transcripts.initial_waiting.take() {
            self.apply_event(waiting, true, cx);
        }
        for event in self.transcripts.controls.values().cloned().collect::<Vec<_>>() {
            self.apply_event(event, true, cx);
        }
        if let Some(activity) = self.transcripts.initial_activity.take() {
            self.apply_event(activity, true, cx);
        }
        self.out.sync_transcripts(workspace, epoch, through);
    }

    pub(crate) fn prune_thread_history(&mut self, threads: &[fuzukue_proto::ThreadView]) {
        let ids: HashSet<_> = threads.iter().map(|thread| thread.id.as_str()).collect();
        let removed: Vec<_> = self.threads.iter().filter(|thread| !ids.contains(thread.id.as_str()))
            .map(|thread| thread.id.clone()).collect();
        for session in removed {
            self.chats.remove(&session);
            self.recent_transcripts.retain(|id| id != &session);
            self.transcripts.workers.remove(&session);
            self.out.cache_transcript(&session, None);
        }
    }

    pub(super) fn request_older_history(&mut self, session: &str) {
        if !matches!(self.status, ConnectionStatus::Connected)
            || self.receiving_backlog
            || session != self.active
            || self.workspaces.active.is_none()
            || self
                .threads
                .iter()
                .any(|thread| thread.id == session && thread.archived)
        {
            return;
        }
        let Some(chat) = self.chats.get_mut(session) else {
            return;
        };
        let Some(before) = chat
            .history
            .before
            .filter(|_| chat.history.pending.is_none())
        else {
            return;
        };
        let request = next_request();
        if self
            .out
            .send(ClientMsg::GetThreadHistory {
                session: session.into(),
                request,
                before,
                generation: chat.history.generation,
            })
            .is_ok()
        {
            chat.history.pending = Some(request);
        }
    }

    pub(crate) fn start_thread_history(
        &mut self,
        session: &str,
        before: u64,
        workers: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.chat(session, cx).history = History {
            before: Some(before),
            pending: None,
            sparse_workers: workers,
            ..History::default()
        };
    }

    pub(crate) fn finish_thread_history(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        let ServerEvent::ThreadHistoryPage { session, request, next_before, events, generation, through } = event
            else { unreachable!() };
        let Some(chat) = self.chats.get_mut(&session) else {
            return;
        };
        if chat.history.pending != Some(request) || chat.history.generation != generation
            || self.transcripts.scoped != through.is_some() {
            return;
        }
        chat.history.pending = None;
        let previous_before = chat.history.before;
        let rows = chat.rows.len();
        let sparse = chat.sparse_history_rows();
        chat.history.before = next_before.filter(|next| previous_before.is_some_and(|before| *next < before));
        let items: HashSet<_> = if through.is_some() {
            events.iter().filter_map(history_item).filter(|item| !chat.contains_history_item(item))
                .map(Cow::into_owned).collect()
        } else {
            HashSet::new()
        };
        let deferred = std::mem::take(&mut chat.history.deferred_page_events);
        chat.prepend_history(events, false, cx);
        if !chat.history.pages.is_empty() && chat.history.before != previous_before {
            chat.history.pages.push_front(LoadedPage {
                start: chat.history.before.unwrap_or(0),
                rows: chat.rows.len() - rows + sparse.saturating_sub(chat.sparse_history_rows()),
            });
            chat.trim_history_window(Direction::Older);
        }
        if let Some(through) = through {
            chat.history.page_through.extend(items.iter().map(|item| (item.clone(), through)));
            chat.replay_updates(deferred.into_iter().filter_map(|(seq, event)| {
                let included = history_item(&event).is_some_and(|item| items.contains(item.as_ref()));
                (!included || seq > through).then_some(event)
            }), cx);
        }
        chat.history.last_scroll = Some(chat.scroller.logical_scroll_top());
    }

    pub(crate) fn finish_thread_history_from(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        let ServerEvent::ThreadHistoryFrom { session, request, start, next_after, generation, through, events } = event
            else { unreachable!() };
        let Some(chat) = self.chats.get_mut(&session) else { return };
        if chat.history.generation != Some(generation) {
            return;
        }
        let Some(pending) = chat.history.forward.take_if(|pending| pending.request == request && pending.start == start)
            else { return };
        if pending.jump {
            chat.clear_transcript_rows();
            chat.history.before = (start != 0).then_some(start);
            chat.history.pages.clear();
            chat.history.sparse_workers.clear();
            chat.history.page_through.clear();
        }

        let rows = chat.rows.len();
        chat.history.next_after = next_after.filter(|&next| next > start);
        chat.scroller.set_follow_mode(gpui_kit::FollowMode::Normal);
        chat.grouping = Default::default();
        chat.delegate_anchor = None;
        chat.replay_updates(events.into_iter(), cx);
        chat.history.pages.push_back(LoadedPage { start, rows: chat.rows.len().saturating_sub(rows) });
        if chat.history.next_after.is_none() {
            chat.history.through = through;
            chat.replay_updates(pending.events.into_iter().filter_map(|(seq, event)| (seq > through).then_some(event)), cx);
        }
        if pending.jump {
            if chat.rows.is_empty() && chat.history.next_after.is_some() {
                chat.history.jump = chat.history.next_after.map(Jump::History);
                if self.active == session {
                    self.request_thread_tail();
                }
                return;
            }
            chat.history.jump = None;
            if let Some(position) = chat.history.reset_position.take() {
                chat.restore_transcript_position(position);
            } else {
                chat.scroller.scroll_to(ListOffset::default());
            }
        } else {
            chat.trim_history_window(Direction::Newer);
        }
        if chat.history.next_after.is_none() {
            let anchor = chat.scroller.logical_scroll_top();
            chat.scroller.set_follow_mode(gpui_kit::FollowMode::Tail);
            chat.scroller.scroll_to(anchor);
        }
        chat.history.last_scroll = Some(chat.scroller.logical_scroll_top());
        if pending.jump {
            self.restore_approval_notices(&session, cx);
        }
        cx.notify();
    }

    pub(crate) fn thread_history_failed(
        &mut self, session: &str, request: u64, reason: Option<ThreadHistoryFailure>, cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        if chat.history.tail.take_if(|pending| pending.request == request).is_some() {
            cx.notify();
            return;
        }
        let from = chat.history.forward.take_if(|pending| pending.request == request);
        if from.is_none() && chat.history.pending != Some(request) {
            return;
        }
        chat.history.pending = None;
        match reason {
            Some(ThreadHistoryFailure::Stale | ThreadHistoryFailure::NotFound) => {
                self.reset_thread_history(session, cx);
                if reason == Some(ThreadHistoryFailure::Stale) && session == self.active {
                    self.request_thread_tail();
                }
            }
            Some(ThreadHistoryFailure::Unavailable) => {
                if from.is_some_and(|pending| pending.jump) {
                    chat.history.jump = None;
                }
                chat.cancel_history_prefetch(cx);
            }
            None => chat.history.before = None,
        }
    }
}

impl Chat {
    fn cancel_history_prefetch(&mut self, cx: &mut Context<Workbench>) {
        self.history.pending = None;
        if self.history.forward.as_ref().is_some_and(|pending| !pending.jump) {
            self.history.forward = None;
        }
        let deferred = std::mem::take(&mut self.history.deferred_page_events);
        self.replay_updates(deferred.into_iter().map(|(_, event)| event), cx);
    }

    fn trim_history_window(&mut self, direction: Direction) {
        const KEPT_PAGES: usize = 4;
        if self.history.pages.len() <= KEPT_PAGES {
            return;
        }

        let anchor = self.scroller.logical_scroll_top();
        let sparse = self.sparse_history_rows();
        let range = match direction {
            Direction::Older => {
                let newest = self.history.pages.back().unwrap();
                let offset = sparse + self.history.pages.iter().take(self.history.pages.len() - 1)
                    .map(|page| page.rows).sum::<usize>();
                if anchor.item_ix.saturating_add(32) >= offset || self.scroller.is_following_tail() {
                    return;
                }
                self.history.next_after = Some(newest.start);
                self.history.pages.pop_back();
                self.scroller.set_follow_mode(gpui_kit::FollowMode::Normal);
                offset..self.rows.len()
            }
            Direction::Newer => {
                let oldest = self.history.pages.front().unwrap();
                let end = sparse + oldest.rows;
                if anchor.item_ix <= end {
                    return;
                }
                self.history.pages.pop_front();
                self.history.before = self.history.pages.front().map(|page| page.start);
                sparse..end.min(self.rows.len())
            }
        };
        self.remove_history_rows(range);
    }

    fn remove_history_rows(&mut self, range: std::ops::Range<usize>) {
        let shift = |row: &mut usize| {
            if range.contains(row) {
                return false;
            }
            if *row >= range.end {
                *row -= range.len();
            }
            true
        };
        self.by_item.retain(|_, row| shift(row));
        self.pending_delegates.retain(|job| self.by_item.contains_key(job));
        self.by_tool.retain(|_, (row, _)| shift(row));
        self.by_approval.retain(|_, (row, _)| shift(row));
        self.delegate_items.retain(|_, (row, _)| shift(row));
        self.delegate_anchor = self.delegate_anchor.take().and_then(|mut row| shift(&mut row).then_some(row));
        self.history.page_through.retain(|item, _| self.by_item.contains_key(item)
            || self.by_tool.contains_key(item) || self.delegate_items.contains_key(item));
        let removed = self.rows.drain(range.clone()).map(|row| row.id).collect();
        self.scroller.splice(range, 0);
        self.transcript.forget_rows(&removed);
        self.clear_transcript_hover();
    }

    fn sparse_history_rows(&self) -> usize {
        self.history.sparse_workers.iter().filter(|job| self.by_item.contains_key(*job)).count()
    }

    fn contains_history_item(&self, item: &str) -> bool {
        self.by_item.contains_key(item)
            || self.by_tool.contains_key(item)
            || self.delegate_items.contains_key(item)
    }

    fn prepend_history(&mut self, events: Vec<ServerEvent>, initial: bool, cx: &mut Context<Workbench>) {
        if events.is_empty() {
            return;
        }
        let leading = self
            .transcript
            .rows
            .borrow()
            .as_ref()
            .and_then(|rows| rows.first_visible);
        // Replay into an independent row builder so old turn endings and tool
        // results cannot change the live turn or replace a running worker.
        let mut page = Chat::new(cx);
        page.markdown = self.markdown.clone();
        if initial {
            page.history.sparse_workers.clone_from(&self.history.sparse_workers);
        }
        let mut anchors = BTreeMap::new();
        let mut covered_workers = HashSet::new();
        let mut worker_end = 0;
        for item in self.history.sparse_workers.iter().filter(|_| !initial) {
            let Some(&row) = self.by_item.get(item) else {
                continue;
            };
            let Content::Delegate(worker) = &self.rows[row].content else {
                continue;
            };
            let page_row = page.rows.len();
            page.push_tool(
                item.clone(),
                self.rows[row].actor.clone(),
                "delegate",
                serde_json::to_value(&worker.job).expect("serializable delegate job"),
                cx,
            );
            page.delegate_anchor = None;
            anchors.insert(page_row, row);
            worker_end = row + 1;
        }
        // The sparse workers were inserted in transcript order. Entries in the
        // page go between the workers they cross, after any still-older worker.
        let mut slots = BTreeMap::new();
        let mut before = worker_end;
        for event in events {
            let item = history_item(&event);
            if let ServerEvent::ToolCall { item, .. } = &event {
                if let Some(page_row) = page.by_item.get(item).filter(|row| anchors.contains_key(*row)) {
                    // Keep the live worker's row at its original history ordinal.
                    covered_workers.insert(item.clone());
                    slots.entry(page.rows.len()).or_insert(anchors[page_row]);
                    page.delegate_anchor = None;
                    continue;
                }
            }
            if item.is_none_or(|item| !self.contains_history_item(&item))
                || matches!(event, ServerEvent::MessageDetails { .. })
            {
                page.apply_history_row(event, cx);
            }
        }
        if initial {
            for row in &mut page.rows {
                row.at_ms = None;
            }
        }

        for (&page_row, &live_row) in &anchors {
            let Content::Delegate(older) = &mut page.rows[page_row].content else {
                continue;
            };
            let Content::Delegate(live) = &mut self.rows[live_row].content else {
                continue;
            };
            live.job.dismissed_from_panel |= older.job.dismissed_from_panel;
            if older.progress.is_empty() {
                continue;
            }
            for (_, index) in self
                .delegate_items
                .values_mut()
                .filter(|(row, _)| *row == live_row)
            {
                *index += older.progress.len();
            }
            live.progress.splice(..0, older.progress.drain(..));
            page.delegate_items.retain(|item, (row, index)| {
                if *row != page_row {
                    return true;
                }
                self.delegate_items.insert(item.clone(), (live_row, *index));
                false
            });
            self.remeasure(live_row, cx);
        }
        self.history
            .sparse_workers
            .retain(|item| !covered_workers.contains(item));

        let mut batches = BTreeMap::<usize, Vec<(usize, Row)>>::new();
        for (index, row) in page.rows.into_iter().enumerate().rev() {
            if let Some(&slot) = slots.get(&(index + 1)) {
                before = slot;
            }
            if !anchors.contains_key(&index) {
                batches.entry(before).or_default().push((index, row));
            }
        }
        if batches.is_empty() {
            return;
        }
        if !initial && self.scroller.is_following_tail()
            && self.scroller.scroll_px_offset_for_scrollbar().y.as_f32() == 0.
        {
            self.scroller.scroll_to(ListOffset::default());
        }
        let mut preserve_leading_padding = false;
        let preserve_leading_row = !self.scroller.is_following_tail()
            && leading == Some(self.scroller.logical_scroll_top().item_ix);
        let show_worker_messages = self
            .transcript
            .current_style()
            .is_some_and(|style| style.show_worker_messages);
        let mut destinations = vec![0; page.scroller.item_count()];
        let mut inserted = 0;
        for (before, batch) in batches {
            let start = before + inserted;
            let count = batch.len();
            preserve_leading_padding |= preserve_leading_row
                && start <= self.scroller.logical_scroll_top().item_ix
                && batch
                    .iter()
                    .any(|(_, row)| row.kind(show_worker_messages) != RowKind::Hidden);
            let rows = batch
                .into_iter()
                .rev()
                .enumerate()
                .map(|(offset, (source, row))| {
                    destinations[source] = start + offset;
                    row
                });
            self.insert_history_rows(start, count, rows);
            inserted += count;
        }
        if initial {
            self.grouping = page.grouping;
            self.delegate_anchor = page.delegate_anchor.map(|row| destinations[row]);
            self.pending_delegates.extend(page.pending_delegates);
        }
        self.by_item
            .extend(page.by_item.into_iter().filter_map(|(item, row)| {
                (!anchors.contains_key(&row)).then(|| (item, destinations[row]))
            }));
        self.by_tool.extend(
            page.by_tool
                .into_iter()
                .map(|(item, (row, call))| (item, (destinations[row], call))),
        );
        self.delegate_items.extend(
            page.delegate_items
                .into_iter()
                .map(|(item, (row, entry))| (item, (destinations[row], entry))),
        );
        if preserve_leading_padding {
            // The original first row loses its 16 px leading inset once older
            // content is present. Retain the position of its visible content.
            let mut anchor = self.scroller.logical_scroll_top();
            anchor.offset_in_item -= CHAT_LEADING_PADDING;
            self.scroller.scroll_to(anchor);
        }
        self.clear_transcript_hover();
        cx.notify();
    }

    pub(super) fn prepend_job_details(&mut self, job: &str, events: Vec<ServerEvent>, cx: &mut Context<Workbench>) {
        let sparse = std::mem::replace(&mut self.history.sparse_workers, vec![job.into()]);
        self.prepend_history(events, false, cx);
        self.history.sparse_workers = sparse;
    }

    fn reset_transcript(&mut self) {
        let position = self.transcript_position().or_else(|| self.history.reset_position.take());
        let jump = self.history.jump.or_else(|| {
            self.history.next_after?;
            let item = self.scroller.logical_scroll_top().item_ix;
            let mut offset = self.sparse_history_rows();
            for page in &self.history.pages {
                offset += page.rows;
                if item < offset {
                    return Some(Jump::History(page.start));
                }
            }
            self.history.pages.front().map(|page| Jump::History(page.start))
        });
        self.history = History { reset_position: position, jump, ..History::default() };
        self.clear_transcript_rows();
    }

    fn clear_transcript_rows(&mut self) {
        self.rows.clear();
        self.by_item.clear();
        self.by_tool.clear();
        self.by_approval.clear();
        self.delegate_items.clear();
        self.pending_delegates.clear();
        self.delegate_anchor = None;
        self.grouping = Default::default();
        self.scroller.reset(0);
        self.scroller.set_follow_mode(gpui_kit::FollowMode::Tail);
        self.discard_transcript_layout();
    }

    pub(super) fn replay_updates(&mut self, events: impl Iterator<Item = ServerEvent>, cx: &mut Context<Workbench>) {
        let busy = self.busy;
        let turn = self.turn.take();
        let notice = self.composer_notice.take();
        for event in events {
            self.apply_history_row(event, cx);
        }
        self.busy = busy;
        self.turn = turn;
        self.composer_notice = notice;
    }

    fn apply_history_row(&mut self, event: ServerEvent, cx: &mut Context<Workbench>) {
        match event {
            ServerEvent::ToolCall { item, actor, name, input, .. } =>
                self.push_replayed_tool(item, actor, &name, input, cx),
            ServerEvent::TextDelta { item, actor, text, .. } => self.push_text(item, actor, text, cx),
            ServerEvent::UserPrompt { item, text, paths, to_worker, .. } =>
                self.user_prompt(item, text, paths, to_worker, cx),
            ServerEvent::ToolResult { item, ok, output, .. } => self.tool_result(&item, ok, output, cx),
            ServerEvent::DelegateProgress { job, summary, .. } => self.delegate_progress(&job, summary, cx),
            ServerEvent::ThreadJobActivity { job, tools, .. } => self.update_job_tool_count(&job, tools, cx),
            ServerEvent::DelegateRenamed { job, change, .. } => self.delegate_renamed(&job, &change, cx),
            ServerEvent::DelegateDismissed { job, .. } => self.delegate_dismissed(&job),
            ServerEvent::LocalNotice { item, text, localized, .. } if item != fuzukue_proto::COMPOSER_NOTICE_ITEM =>
                self.local_notice(item, text, localized, cx),
            ServerEvent::GitHubActivity { item, events, .. } => self.github_activity(item, events, cx),
            ServerEvent::CommentApplication { application, .. } => self.comment_application(application, cx),
            ServerEvent::MessageDetails { item, details, .. } => self.set_message_details(&item, details, cx),
            ServerEvent::TurnEnded { actor, .. } => self.end_turn(&actor, cx),
            ServerEvent::ReplySuperseded { items, .. } => self.supersede(&items, cx),
            _ => {}
        }
    }

    fn insert_history_rows(&mut self, start: usize, count: usize, rows: impl Iterator<Item = Row>) {
        for row in self
            .by_item
            .values_mut()
            .chain(self.by_tool.values_mut().map(|(row, _)| row))
            .chain(self.by_approval.values_mut().map(|(row, _)| row))
            .chain(self.delegate_items.values_mut().map(|(row, _)| row))
            .chain(self.delegate_anchor.iter_mut())
        {
            if *row >= start {
                *row += count;
            }
        }
        self.rows.splice(start..start, rows);
        let end = start + count;
        let heights = (start..end)
            .map(|row| self.height_estimate(row))
            .collect::<Vec<_>>();
        self.scroller
            .splice_with_height_estimates(start..start, heights);
        if end < self.rows.len() {
            self.remeasure_group_near(end);
        } else {
            self.transcript.rows.borrow_mut().take();
        }
    }
}


#[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
mod tests {
    use super::*;
    use crate::conn::ConnEvent;
    use fuzukue_proto::{Actor, Envelope, PROTOCOL_VERSION};
    use gpui_kit::{AppContext as _, Window};

    const WORKSPACE: &str = "scoped-history";
    const A: &str = "thread-a";
    const B: &str = "thread-b";

    fn deliver(view: &mut Workbench, window: &mut Window, cx: &mut Context<Workbench>, seq: u64, event: ServerEvent) {
        let workspace = (!matches!(event, ServerEvent::Welcome { .. } | ServerEvent::WorkspaceSelected { .. }))
            .then(|| WORKSPACE.into());
        view.on_conn(ConnEvent::Event(Envelope { workspace, seq, event }), window, cx);
    }

    fn text(session: &str, item: &str, text: &str) -> ServerEvent {
        ServerEvent::TextDelta { session: session.into(), item: item.into(), actor: Actor::Claude, text: text.into() }
    }

    fn body<'a>(view: &'a Workbench, session: &str, item: &str) -> &'a str {
        let chat = &view.chats[session];
        let Content::Message(message) = &chat.rows[chat.by_item[item]].content else { panic!("missing message") };
        &message.text
    }

    #[test]
    fn home_end_prefetch_and_job_details_preserve_the_transcript() {
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            crate::math::init(cx);
            let (window, view) = gpui_kit::open_window(gpui_kit::component::TitleBar::window_options(),
                cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("No socket fixture"))))).unwrap();
            let (out, sent) = std::sync::mpsc::channel();
            window.update(cx, |_, window, cx| {
                window.draw(cx).clear(cx);
            }).unwrap();
            let job = "delegate:synthetic";
            let summary = window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                view.out = out.into();
                view.on_conn(ConnEvent::Connected, window, cx);
                deliver(view, window, cx, 0, ServerEvent::Welcome {
                    protocol: PROTOCOL_VERSION, server: "fixture".into(), client_manifest: None, server_tailnet_url: None,
                    selection_scoped_transcripts: true,
                });
                deliver(view, window, cx, 0, ServerEvent::WorkspaceSelected { id: WORKSPACE.into(), replay: true });
                deliver(view, window, cx, 0, ServerEvent::Threads {
                    threads: vec![serde_json::from_value(serde_json::json!({"id": A, "title": A, "updated": 1})).unwrap()],
                });
                deliver(view, window, cx, 0, ServerEvent::ThreadSync {
                    epoch: "fixture".into(), through: 10, reset_sessions: vec![],
                });
                let request = view.chats[A].history.tail.as_ref().unwrap().request;
                let summary: JobSummary = serde_json::from_value(serde_json::json!({
                    "id":job, "position":20, "status":"running", "tools":500, "had_tool_error":false,
                    "job":{"model":"fixture","started_at_ms":0}, "last_tool":"Read notes.md",
                })).unwrap();
                deliver(view, window, cx, 0, ServerEvent::ThreadJobs {
                    session: A.into(), through: 10, first: true, jobs: vec![summary.clone()],
                });
                assert!(view.chats[A].rows.is_empty());
                assert_eq!(view.chats[A].active_worker(), Some("fixture"));
                assert_eq!(view.chats[A].jobs.get(job).unwrap().tools, 500);
                let events = vec![
                    text(A, "latest", "Newest conversation reply"),
                    ServerEvent::ToolCall {
                        session: A.into(), item: job.into(), actor: Actor::System, name: "delegate".into(),
                        input: serde_json::json!({"title":"Synthetic job", "model":"fixture", "started_at_ms":0,
                            "details_deferred":true,"tool_count":500}),
                    }
                ];
                deliver(view, window, cx, 0, ServerEvent::ThreadTail {
                    session: A.into(), request, generation: 0, through: 10,
                    events, workers: vec![], next_before: Some(600),
                });
                assert_eq!(body(view, A, "latest"), "Newest conversation reply");
                let job_row = view.chats[A].by_item[job];
                let Content::Delegate(block) = &view.chats[A].rows[job_row].content else { panic!("missing job") };
                assert!(block.progress.is_empty());
                assert_eq!(block.deferred.as_ref().unwrap().tools, 500);
                view.toggle_thread_job(A, job_row, cx);
                let detail = sent.try_iter().find_map(|event| match event {
                    ClientMsg::InWorkspace { request, .. } => match *request {
                        ClientMsg::GetThreadJob { request, .. } => request,
                        _ => None,
                    },
                    _ => None,
                }).expect("expansion requested details");
                deliver(view, window, cx, 0, ServerEvent::ThreadJobDetails {
                    session: A.into(), job: job.into(), request: detail, through: 10,
                    before: None, next_before: None,
                    events: vec![ServerEvent::TextDelta {
                        session: A.into(), item: format!("{job}:detail"), text: "Synthetic activity".into(),
                        actor: Actor::Codex { thread_id: "fixture".into(), parent_tool_use: Some(job.into()) },
                    }],
                });
                let Content::Delegate(block) = &view.chats[A].rows[job_row].content else { panic!("missing job") };
                assert_eq!(block.progress.len(), 1);

                view.history_scrolled_older(cx);
                let older = view.chats[A].history.pending.unwrap();
                view.prefetch_history(Direction::Newer, false, cx);
                assert!(view.chats[A].history.pending.is_none());
                deliver(view, window, cx, 0, ServerEvent::ThreadHistoryPage {
                    session: A.into(), request: older, events: vec![text(A, "ignored", "Cancelled page")],
                    next_before: Some(300), generation: Some(0), through: Some(10),
                });
                assert!(!view.chats[A].by_item.contains_key("ignored"));
                summary
            })).unwrap();

            window.update(cx, |_, window, cx| {
                window.blur(cx);
                assert!(window.dispatch_keystroke(gpui_kit::Keystroke::parse("home").unwrap(), cx));
            }).unwrap();
            window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                let request = view.chats[A].history.forward.as_ref().expect("Home should request history from row zero").request;
                deliver(view, window, cx, 0, ServerEvent::ThreadHistoryFrom {
                    session: A.into(), request, generation: 0, start: 0, next_after: Some(1), through: 11,
                    events: vec![text(A, "first", "First conversation message")],
                });
                assert_eq!(body(view, A, "first"), "First conversation message");
                assert_eq!(view.chats[A].scroller.logical_scroll_top(), ListOffset::default());
                view.prefetch_history(Direction::Newer, false, cx);
                let request = view.chats[A].history.forward.as_ref().unwrap().request;
                let anchor = view.chats[A].scroller.logical_scroll_top();
                deliver(view, window, cx, 0, ServerEvent::ThreadHistoryFrom {
                    session: A.into(), request, generation: 0, start: 1, next_after: Some(2), through: 11,
                    events: vec![text(A, "second", "Second conversation message")],
                });
                assert_eq!(view.chats[A].scroller.logical_scroll_top(), anchor);

                for start in 2..=4 {
                    view.request_history_from(A, start, 0, false);
                    let request = view.chats[A].history.forward.as_ref().unwrap().request;
                    if start == 4 {
                        let chat = &view.chats[A];
                        chat.scroller.scroll_to(ListOffset { item_ix: chat.by_item["page-2-10"], offset_in_item: gpui_kit::px(3.) });
                    }
                    deliver(view, window, cx, 0, ServerEvent::ThreadHistoryFrom {
                        session: A.into(), request, generation: 0, start, next_after: Some(start + 1), through: 11,
                        events: (0..32).map(|item| text(A, &format!("page-{start}-{item}"), "Synthetic message")).collect(),
                    });
                }
                let chat = &view.chats[A];
                assert!(!chat.by_item.contains_key("first"));
                assert!(!chat.by_item.contains_key(job));
                assert_eq!(chat.jobs.get(job).unwrap().tools, 500);
                assert_eq!(chat.history.pages.len(), 4);
                assert_eq!(chat.scroller.logical_scroll_top(), ListOffset {
                    item_ix: chat.by_item["page-2-10"], offset_in_item: gpui_kit::px(3.),
                });
                view.input.update(cx, |input, cx| input.focus(window, cx));
            })).unwrap();

            window.update(cx, |_, window, cx| {
                window.draw(cx).clear(cx);
                assert!(window.dispatch_keystroke(gpui_kit::Keystroke::parse("end").unwrap(), cx));
            }).unwrap();
            window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                let request = view.chats[A].history.tail.as_ref().unwrap().request;
                deliver(view, window, cx, 12, text(A, "latest", " + streamed"));
                deliver(view, window, cx, 0, ServerEvent::ThreadTail {
                    session: A.into(), request, generation: 0, through: 11, workers: vec![], next_before: Some(8),
                    events: vec![text(A, "latest", "Newest conversation reply")],
                });
                assert_eq!(body(view, A, "latest"), "Newest conversation reply + streamed");
                assert!(view.chats[A].scroller.is_following_tail());
                deliver(view, window, cx, 0, ServerEvent::ThreadJobUpdated {
                    session: A.into(), through: 13, summary: JobSummary { tools: 501, ..summary },
                });
                assert_eq!(view.chats[A].jobs.get(job).unwrap().tools, 501);
            })).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn selected_tail_cache_and_epoch_reset() {
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            crate::math::init(cx);
            let (window, view) = gpui_kit::open_window(gpui_kit::component::TitleBar::window_options(),
                cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("No socket fixture"))))).unwrap();
            window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                let (out, sent) = std::sync::mpsc::channel();
                view.out = out.into();
                let requests = || -> Vec<(String, u64)> {
                    sent.try_iter().filter_map(|event| match event {
                        ClientMsg::InWorkspace { id, request } => {
                            assert_eq!(id, WORKSPACE);
                            match *request {
                                ClientMsg::GetThreadTail { session, request } => Some((session, request)),
                                _ => None,
                            }
                        }
                        ClientMsg::GetThreadTail { .. } => panic!("tail request lost its workspace"),
                        _ => None,
                    }).collect()
                };
                let welcome = || ServerEvent::Welcome {
                    protocol: PROTOCOL_VERSION, server: "fixture".into(), client_manifest: None, server_tailnet_url: None,
                    selection_scoped_transcripts: true,
                };
                let select = |replay| ServerEvent::WorkspaceSelected { id: WORKSPACE.into(), replay };
                let threads = || ServerEvent::Threads {
                    threads: [(A, 2), (B, 1)].into_iter().map(|(id, updated)| {
                        serde_json::from_value(serde_json::json!({ "id": id, "title": id, "updated": updated })).unwrap()
                    }).collect(),
                };
                let sync = |epoch: &str, through, reset_sessions| ServerEvent::ThreadSync { epoch: epoch.into(), through, reset_sessions };
                let tail = |session: &str, request, through, events, next_before| ServerEvent::ThreadTail {
                    session: session.into(), request, generation: 0, through, events, workers: vec![], next_before,
                };
                view.on_conn(ConnEvent::Connected, window, cx);
                deliver(view, window, cx, 0, welcome());
                deliver(view, window, cx, 0, select(true));
                deliver(view, window, cx, 0, threads());
                assert!(view.active.is_empty());
                assert!(requests().is_empty());
                deliver(view, window, cx, 0, ServerEvent::ChatActivity {
                    turns: BTreeSet::new(), workers: BTreeMap::from([(B.into(), BTreeSet::from(["delegate:job".into()]))]),
                });
                deliver(view, window, cx, 0, sync("first", 10, vec![]));
                assert_eq!(view.active, A);
                assert_eq!(view.scoped_worker(B), Some("delegate:job"));
                let [(session, a)] = requests().try_into().unwrap();
                assert_eq!(session, A);

                deliver(view, window, cx, 11, text(B, "unopened", "discarded"));
                assert!(!view.chats.contains_key(B));
                deliver(view, window, cx, 12, text(A, "reply", "already included"));
                deliver(view, window, cx, 13, text(A, "reply", " + live"));
                view.select_thread(B.into(), cx);
                let [(session, b)] = requests().try_into().unwrap();
                assert_eq!(session, B);
                view.select_thread(A.into(), cx);
                assert!(requests().is_empty(), "reselecting A must wait for its pending response");
                view.select_thread(B.into(), cx);
                assert!(requests().is_empty(), "reselecting B must wait for its pending response");
                deliver(view, window, cx, 0, tail(A, a, 12, vec![text(A, "reply", "tail")], Some(5)));
                assert_eq!(view.active, B);
                assert_eq!(body(view, A, "reply"), "tail + live");
                assert!(view.chats[B].rows.is_empty());
                deliver(view, window, cx, 0, tail(B, b, 13, vec![], None));
                view.select_thread(A.into(), cx);
                view.select_thread(B.into(), cx);
                view.select_thread(A.into(), cx);
                assert!(requests().is_empty(), "both nonempty and empty tails must be reused");

                view.request_older_history(A);
                let page = view.chats[A].history.pending.unwrap();
                deliver(view, window, cx, 14, text(A, "older", "already included in page"));
                deliver(view, window, cx, 15, text(A, "older", " + update"));
                deliver(view, window, cx, 0, ServerEvent::ThreadHistoryPage {
                    session: A.into(), request: page, events: vec![text(A, "older", "old")],
                    next_before: None, generation: Some(0), through: Some(14),
                });
                assert_eq!(body(view, A, "older"), "old + update");
                assert_eq!(view.chats[A].by_item["older"], 0);

                let cache = [A, B].into_iter().map(|session| CachedSession { session: session.into(), generation: 0 }).collect();
                view.on_conn(ConnEvent::Disconnected("reconnect fixture".into()), window, cx);
                view.on_conn(ConnEvent::Connected, window, cx);
                deliver(view, window, cx, 0, welcome());
                view.on_conn(ConnEvent::AdvertisedTranscriptCache(cache), window, cx);
                deliver(view, window, cx, 0, select(false));
                deliver(view, window, cx, 0, threads());
                deliver(view, window, cx, 16, text(A, "reply", " + resume"));
                assert_eq!(body(view, A, "reply"), "tail + live");
                deliver(view, window, cx, 0, sync("first", 16, vec![]));
                assert_eq!(body(view, A, "reply"), "tail + live + resume");
                assert!(requests().is_empty());

                view.on_conn(ConnEvent::Disconnected("restart fixture".into()), window, cx);
                view.on_conn(ConnEvent::Connected, window, cx);
                deliver(view, window, cx, 0, welcome());
                deliver(view, window, cx, 0, select(false));
                deliver(view, window, cx, 0, threads());
                deliver(view, window, cx, 0, sync("second", 2, vec![A.into(), B.into()]));
                let [(session, replacing)] = requests().try_into().unwrap();
                assert_eq!(session, A);
                assert_ne!(replacing, a);
                assert!(view.chats[A].rows.is_empty());
                deliver(view, window, cx, 0, tail(A, a, 12, vec![text(A, "reply", "stale response")], None));
                assert!(view.chats[A].rows.is_empty());
                deliver(view, window, cx, 0, tail(A, replacing, 2, vec![text(A, "reply", "new epoch")], None));
                assert_eq!(body(view, A, "reply"), "new epoch");
                view.select_thread(B.into(), cx);
                assert_eq!(requests().iter().map(|(session, _)| session.as_str()).collect::<Vec<_>>(), [B]);
            })).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }
}
