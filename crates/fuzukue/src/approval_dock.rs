//! The selected workspace's opt-in approval queue and its controls above the composer.

use std::collections::{HashMap, HashSet};

use fuzukue_proto::{
    ApprovalDecision, ApprovalGrantScope, ApprovalRisk, ApprovalState, ApprovalSummary,
    ApprovalUnattended, ClientMsg, NotificationKind, ServerEvent,
};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    div, point, prelude::*, px, Context, FocusHandle, Focusable as _, Role, ScrollHandle, Window,
};
use serde_json::Value;

use crate::Workbench;

const ALLOW: ApprovalDecision = ApprovalDecision::Grant { scope: ApprovalGrantScope::Once };
const DENY: ApprovalDecision = ApprovalDecision::Deny;

#[derive(Default)]
pub(super) struct ApprovalDock {
    pub(super) available: bool,
    next_request: u64,
    fetching: Option<u64>,
    ready: bool,
    queue_error: Option<String>,
    // The server supplies registration order, including ties in created_at.
    pending: Vec<ApprovalSummary>,
    notices: HashMap<String, Vec<Resolution>>,
    collapsed: bool,
    detail: Option<String>,
    selected: HashSet<String>,
    review: Option<Review>,
    submitted: Option<Submission>,
    feedback: HashMap<String, Feedback>,
    scroll: ScrollHandle,
    focus: HashMap<(String, &'static str), FocusHandle>,
    last_focus: Option<FocusHandle>,
}

struct Resolution {
    id: String,
    thread: String,
    tool: String,
    state: ApprovalState,
    updated_at: i64,
    local: bool,
}

struct Review {
    decision: ApprovalDecision,
    requests: Vec<ApprovalSummary>,
}

struct Submission {
    thread: String,
    outstanding: HashSet<u64>,
    feedback: Feedback,
}

#[derive(Default)]
struct Feedback {
    applied: usize,
    conflicts: usize,
    errors: Vec<String>,
    interrupted: usize,
    bulk: bool,
}

impl ApprovalDock {
    fn request(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    pub(super) fn ready(&self) -> bool {
        self.ready
    }

    pub(super) fn clear_controls(&mut self) {
        self.collapsed = false;
        self.detail = None;
        self.selected.clear();
        self.review = None;
        self.scroll = ScrollHandle::new();
        self.focus.clear();
        self.last_focus = None;
    }

    pub(super) fn disconnect(&mut self) {
        self.available = false;
        self.ready = false;
        self.fetching = None;
        self.queue_error = None;
        self.pending.clear();
        self.review = None;
        self.detail = None;
        self.selected.clear();
        self.interrupt_submission();
    }

    fn interrupt_submission(&mut self) {
        if let Some(mut submission) = self.submitted.take() {
            submission.feedback.interrupted += submission.outstanding.len();
            self.feedback.insert(submission.thread, submission.feedback);
        }
    }

    pub(super) fn snapshot(&mut self, request: u64, result: Result<Vec<ApprovalSummary>, String>) {
        if self.fetching != Some(request) {
            return;
        }
        self.fetching = None;
        match result {
            Ok(items) => {
                self.pending = items.into_iter().filter(|item| item.state == ApprovalState::Pending).collect();
                self.ready = true;
            }
            Err(error) => self.queue_error = Some(error),
        }
    }

    fn thread<'a, 'b>(&'a self, thread: &'b str) -> impl Iterator<Item = &'a ApprovalSummary> + use<'a, 'b> {
        self.pending.iter().filter(move |item| item.owner.thread == thread)
    }

    fn groups(&self, thread: &str) -> Vec<Vec<&ApprovalSummary>> {
        let mut jobs = HashMap::new();
        let mut groups = Vec::<Vec<_>>::new();
        for item in self.thread(thread) {
            let index = *jobs.entry(&item.owner.job).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[index].push(item);
        }
        groups
    }

    fn pending_change(&mut self, approval: ApprovalSummary) -> bool {
        if let Some(previous) = self.pending.iter_mut().find(|item| item.approval_id == approval.approval_id) {
            if approval.updated_at > previous.updated_at {
                *previous = approval;
            }
            return false;
        }
        self.pending.push(approval);
        true
    }

    fn resolve(&mut self, approval: ApprovalSummary, local: bool) -> bool {
        let id = &approval.approval_id;
        self.pending.retain(|item| &item.approval_id != id);
        self.selected.remove(id);
        self.focus.retain(|(key, _), _| key != id);
        if self.detail.as_ref() == Some(id) {
            self.detail = None;
        }
        let notices = self.notices.entry(approval.workspace).or_default();
        if let Some(previous) = notices.iter().rev().find(|notice| &notice.id == id) {
            if previous.state == approval.state || previous.updated_at > approval.updated_at {
                return false;
            }
        }
        notices.push(Resolution {
            id: approval.approval_id,
            thread: approval.owner.thread,
            tool: approval.action.tool,
            state: approval.state,
            updated_at: approval.updated_at,
            local,
        });
        true
    }

    fn selected_requests(&self, thread: &str) -> Vec<&ApprovalSummary> {
        self.thread(thread)
            .filter(|item| self.selected.contains(&item.approval_id))
            .collect()
    }

    fn decision_candidates(&self, thread: &str, reviewed: &[ApprovalSummary]) -> Vec<String> {
        self.thread(thread)
            .filter(|current| reviewed.iter().any(|item| {
                item.approval_id == current.approval_id && item.action == current.action && same_batch(item, current)
            }))
            .map(|item| item.approval_id.clone())
            .collect()
    }
}

fn same_batch(first: &ApprovalSummary, second: &ApprovalSummary) -> bool {
    [first, second].iter().all(|item| {
        item.risk != ApprovalRisk::High && item.unattended != ApprovalUnattended::NeverUnattended
    }) && first.workspace == second.workspace
        && first.owner.thread == second.owner.thread
        && first.action.tool == second.action.tool
        && first.action.scope == second.action.scope
}

fn label(ja: &'static str, en: &'static str) -> &'static str {
    if crate::i18n::current() == fuzukue_i18n::Locale::JAPANESE { ja } else { en }
}

fn risk_labels(risk: ApprovalRisk) -> (&'static str, &'static str) {
    match risk {
        ApprovalRisk::Low => ("低リスク", "Low risk"),
        ApprovalRisk::Mid => ("中リスク", "Medium risk"),
        ApprovalRisk::High => ("高リスク", "High risk"),
    }
}

fn decision_label(decision: ApprovalDecision) -> &'static str {
    if decision == ALLOW { label("許可", "Allow") } else { label("拒否", "Deny") }
}

fn json(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())
}

fn target(item: &ApprovalSummary) -> String {
    let keys = ["target", "paths", "path", "destination", "url", "repo", "cwd"];
    [&item.action.args, &item.action.scope]
        .into_iter()
        .find_map(|value| {
            keys.iter().find_map(|key| value.get(key).filter(|value| !value.is_null()))
        })
        .map(json)
        .unwrap_or_else(|| label("対象の指定なし", "No target specified").to_owned())
}

fn expiry(item: &ApprovalSummary) -> String {
    chrono::DateTime::from_timestamp_millis(item.expires_at)
        .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_else(|| item.expires_at.to_string())
}

impl Resolution {
    fn event(&self) -> ServerEvent {
        let state = match self.state {
            ApprovalState::Granted => label("許可", "Allowed"),
            ApprovalState::Denied => label("拒否", "Denied"),
            ApprovalState::Withdrawn => label("撤回", "Withdrawn"),
            ApprovalState::Expired => label("期限切れ", "Expired"),
            ApprovalState::Pending => unreachable!(),
        };
        let by = if self.local { label("（あなた）", " by you") } else { "" };
        ServerEvent::LocalNotice {
            session: self.thread.clone(),
            item: format!("approval-dock:{}:{:?}", self.id, self.state),
            text: format!("{state}{by} · {} · {}", self.tool, self.id),
            localized: None,
        }
    }
}

impl Workbench {
    pub(super) fn refresh_approvals(&mut self) {
        if !self.approvals.available || self.workspaces.active.is_none() {
            return;
        }
        self.approvals.interrupt_submission();
        self.approvals.ready = false;
        self.approvals.pending.clear();
        self.approvals.queue_error = None;
        let request = self.approvals.request();
        if self.out.send(ClientMsg::ListPendingApprovals { request }).is_ok() {
            self.approvals.fetching = Some(request);
        } else {
            self.approvals.fetching = None;
            self.approvals.queue_error = Some(label("承認一覧を取得できなかった", "Could not request approvals").to_owned());
        }
    }

    pub(super) fn approval_changed(
        &mut self, approval: ApprovalSummary, local: bool, window: &Window, cx: &mut Context<Self>,
    ) {
        if !self.approvals.ready || self.workspaces.active.as_ref() != Some(&approval.workspace) {
            return;
        }
        let thread = approval.owner.thread.clone();
        if approval.state == ApprovalState::Pending {
            let workspace = approval.workspace.clone();
            if self.approvals.pending_change(approval) {
                self.notify_thread(workspace, thread, NotificationKind::Approval, None, window, cx);
            }
            return;
        }
        let workspace = approval.workspace.clone();
        if self.approvals.resolve(approval, local) && self.chats.contains_key(&thread) {
            let notice = self.approvals.notices[&workspace].last().unwrap().event();
            self.on_chat_event(notice, cx);
        }
    }

    pub(super) fn restore_approval_notices(&mut self, thread: &str, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.active.as_ref() else { return };
        let Some(notices) = self.approvals.notices.get(workspace) else { return };
        let events: Vec<_> = notices.iter().filter(|notice| notice.thread == thread)
            .map(Resolution::event).collect();
        for event in events {
            self.on_chat_event(event, cx);
        }
    }

    pub(super) fn approval_result(
        &mut self, request: u64, result: Result<ApprovalSummary, String>, window: &Window, cx: &mut Context<Self>,
    ) {
        let Some(submission) = self.approvals.submitted.as_mut() else { return };
        if !submission.outstanding.remove(&request) {
            return;
        }
        match &result {
            Ok(_) => submission.feedback.applied += 1,
            Err(error) if error == "approval request has already been decided or consumed" => {
                submission.feedback.conflicts += 1;
            }
            Err(error) => submission.feedback.errors.push(error.clone()),
        }
        if submission.outstanding.is_empty() {
            let submission = self.approvals.submitted.take().unwrap();
            self.approvals.feedback.insert(submission.thread, submission.feedback);
        }
        if let Ok(approval) = result {
            self.approval_changed(approval, true, window, cx);
        }
    }

    fn can_decide(&self) -> bool {
        self.approvals.ready && self.approvals.submitted.is_none() && !self.workspaces.pending
    }

    fn submit_approvals(&mut self, ids: Vec<String>, decision: ApprovalDecision, mut feedback: Feedback) {
        if !self.can_decide() {
            return;
        }
        let thread = self.active.clone();
        self.approvals.feedback.remove(&thread);
        let mut outstanding = HashSet::new();
        for approval_id in ids {
            let request = self.approvals.request();
            let message = ClientMsg::DecideApproval { request, approval_id, decision, reason: None };
            if self.out.send(message).is_ok() {
                outstanding.insert(request);
            } else {
                feedback.interrupted += 1;
            }
        }
        if outstanding.is_empty() {
            self.approvals.feedback.insert(thread, feedback);
        } else {
            self.approvals.submitted = Some(Submission { thread, outstanding, feedback });
        }
    }

    fn toggle_selection(&mut self, id: &str) {
        if self.approvals.selected.remove(id) {
            return;
        }
        let Some(item) = self.approvals.thread(&self.active).find(|item| item.approval_id == id) else { return };
        if same_batch(item, item) && self.approvals.selected_requests(&self.active)
            .iter().all(|selected| same_batch(selected, item))
        {
            self.approvals.selected.insert(id.to_owned());
        }
    }

    fn review_batch(&mut self, decision: ApprovalDecision, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.approvals.selected_requests(&self.active);
        let [first, _, ..] = selected.as_slice() else { return };
        if !self.can_decide() || !selected.iter().all(|item| same_batch(first, item)) {
            return;
        }
        self.approvals.review = Some(Review {
            requests: selected.into_iter().cloned().collect(),
            decision,
        });
        self.approvals.scroll.set_offset(point(px(0.), px(0.)));
        let focus = self.approval_focus("", "back", cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    fn cancel_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(review) = self.approvals.review.take() else { return false };
        if self.approvals.selected_requests(&self.active).len() >= 2 {
            let button = if review.decision == ALLOW { "bulk-allow" } else { "bulk-deny" };
            self.focus_dock_button(button, window, cx);
        } else {
            self.focus_dock_button("toggle", window, cx);
        }
        true
    }

    fn focus_dock_button(&mut self, button: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let focus = if self.approval_count(&self.active) > 0 {
            self.approval_focus("", button, cx)
        } else {
            self.input.read(cx).focus_handle(cx)
        };
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    fn confirm_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_decide() {
            return;
        }
        if let Some(review) = self.approvals.review.take() {
            let ids = self.approvals.decision_candidates(&self.active, &review.requests);
            let feedback = Feedback { bulk: true, conflicts: review.requests.len() - ids.len(), ..Feedback::default() };
            self.approvals.selected.clear();
            self.submit_approvals(ids, review.decision, feedback);
            self.focus_dock_button("toggle", window, cx);
        }
    }

    pub(super) fn approval_count(&self, thread: &str) -> usize {
        self.approvals.thread(thread).count()
    }

    pub(super) fn approval_badge(&self, count: usize, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().id("thread-approval-badge").role(Role::Group)
            .aria_label(format!("{count} {}", label("件の承認待ち", "pending approvals")))
            .flex_none().px_1().rounded_md().bg(theme.secondary)
            .text_color(theme.warning).text_xs().child(count.to_string())
    }

    pub(super) fn reveal_approval_dock(&mut self) {
        self.approvals.collapsed = false;
    }

    fn approval_focus(&mut self, id: &str, button: &'static str, cx: &mut Context<Self>) -> FocusHandle {
        self.approvals.focus.entry((id.to_owned(), button)).or_insert_with(|| cx.focus_handle().tab_stop(true)).clone()
    }
}

mod view;
#[cfg(all(target_os = "linux", feature = "headless-shot"))]
mod shot_fixture;
#[cfg(test)]
mod tests;
