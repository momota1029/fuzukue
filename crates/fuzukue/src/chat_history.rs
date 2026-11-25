use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use fuzukue_proto::{CachedSession, ClientMsg, ServerEvent, ThreadHistoryFailure};
use gpui_kit::{Context, ListOffset};

use super::{CHAT_LEADING_PADDING, Chat, Content, Row, RowKind, Workbench};
use crate::ConnectionStatus;

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

fn next_request() -> u64 {
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
        | ServerEvent::DelegateDismissed { job, .. } => Some(Cow::Borrowed(job)),
        ServerEvent::CommentApplication { application, .. } => Some(Cow::Owned(format!("comments-{}", application.thread))),
        _ => None,
    }
}


impl Workbench {
    pub(crate) fn request_thread_tail(&mut self) {
        if !self.transcripts.scoped || self.receiving_backlog
            || !matches!(self.status, ConnectionStatus::Connected)
            || self.workspaces.active.is_none()
            || !self.threads.iter().any(|thread| thread.id == self.active)
        {
            return;
        }
        let Some(chat) = self.chats.get_mut(&self.active) else { return };
        if chat.history.generation.is_some() || chat.history.tail.is_some() {
            return;
        }
        let request = next_request();
        if self.out.send(ClientMsg::GetThreadTail { session: self.active.clone(), request }).is_ok() {
            chat.history.tail = Some(PendingTail { request, events: Vec::new() });
        }
    }

    pub(crate) fn finish_thread_tail(
        &mut self, event: ServerEvent, cx: &mut Context<Self>,
    ) {
        let ServerEvent::ThreadTail { session, request, generation, through, events, workers, next_before } = event
            else { unreachable!() };
        let Some(chat) = self.chats.get_mut(&session) else { return };
        let Some(pending) = chat.history.tail.take_if(|pending| pending.request == request) else { return };
        chat.history.generation = Some(generation);
        chat.history.through = through;
        chat.history.before = next_before;
        chat.history.sparse_workers = workers;
        chat.prepend_history(events, true, cx);
        chat.replay_updates(pending.events.into_iter().filter(|(seq, _)| *seq > through)
            .map(|(_, event)| event), cx);
        chat.reconcile_activity(chat.busy, self.transcripts.workers.get(&session), cx);
        if let Some(position) = chat.history.reset_position.take() {
            chat.restore_transcript_position(position);
        }
        self.out.cache_transcript(&session, Some(generation));
        self.restore_approval_notices(&session, cx);
        if session == self.active {
            self.github_view();
        }
    }

    pub(crate) fn reset_thread_history(&mut self, session: &str, cx: &mut Context<Self>) {
        self.out.cache_transcript(session, None);
        let Some(chat) = self.chats.get_mut(session) else { return };
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
            chat.history.tail = None;
            chat.history.pending = None;
            let deferred = std::mem::take(&mut chat.history.deferred_page_events);
            chat.replay_updates(deferred.into_iter().map(|(_, event)| event), cx);
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
        if chat.history.generation.is_none() || (seq != 0 && seq <= chat.history.through) {
            return Some(false);
        }
        let item = history_item(event);
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
                if chat.history.generation.is_some() && seq > chat.history.through.max(item_through) {
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
        chat.history.before =
            next_before.filter(|next| chat.history.before.is_some_and(|before| *next < before));
        let items: HashSet<_> = if through.is_some() {
            events.iter().filter_map(history_item).filter(|item| !chat.contains_history_item(item))
                .map(Cow::into_owned).collect()
        } else {
            HashSet::new()
        };
        let deferred = std::mem::take(&mut chat.history.deferred_page_events);
        chat.prepend_history(events, false, cx);
        if let Some(through) = through {
            chat.history.page_through.extend(items.iter().map(|item| (item.clone(), through)));
            chat.replay_updates(deferred.into_iter().filter_map(|(seq, event)| {
                let included = history_item(&event).is_some_and(|item| items.contains(item.as_ref()));
                (!included || seq > through).then_some(event)
            }), cx);
        }
    }

    pub(crate) fn thread_history_failed(
        &mut self, session: &str, request: u64, reason: Option<ThreadHistoryFailure>, cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        if chat.history.tail.take_if(|pending| pending.request == request).is_some() {
            cx.notify();
            return;
        }
        if chat.history.pending != Some(request) {
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
                let deferred = std::mem::take(&mut chat.history.deferred_page_events);
                chat.replay_updates(deferred.into_iter().map(|(_, event)| event), cx);
            }
            None => chat.history.before = None,
        }
    }
}

impl Chat {
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

    fn reset_transcript(&mut self) {
        let position = self.transcript_position().or_else(|| self.history.reset_position.take());
        self.history = History { reset_position: position, ..History::default() };
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

    fn replay_updates(&mut self, events: impl Iterator<Item = ServerEvent>, cx: &mut Context<Workbench>) {
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
