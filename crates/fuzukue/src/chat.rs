//! Chat rows, tool disclosures, and their virtual-list measurements.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};

use gpui_kit::base::{Disableable as _, ElementExt as _, ScrollBounce, Selectable, StyledExt as _, TextSelection, TextSelectionScopeId};
use gpui_kit::base::actions::{SelectDown, SelectUp};
use gpui_kit::base::input::{Enter, MoveDown, MoveUp, RopeExt as _};
use gpui_kit::base::text::editor_host::{parse_markdown, BlockNode};
use gpui_kit::base::text::MarkdownExtensions;
use gpui_kit::component::{
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    h_flex,
    input::Textarea,
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    popover::Popover,
    progress::ProgressCircle,
    scroll::ScrollableElement as _,
    text::{SelectionFormat, TextView},
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::{
    canvas, div, fill, linear_color_stop, linear_gradient, list_with_optional_items, point, prelude::*, px, relative, rems,
    Anchor, Animation, AnimationExt, AnyElement, Context,
    ElementId, Entity, EntityInputHandler as _, ExternalPaths, Focusable as _, FollowMode,
    HoverListenerMode, ListAlignment, ListState, PathPromptOptions, Pixels, Point, Rems,
    SharedString, Task, Window,
};
use fuzukue_i18n::STATUS_SEPARATOR;
use fuzukue_proto::{
    Actor, AgentBackend, AgentSelection, ChatSettings, ClientMsg, CommentApplication, CommentApplicationState, MathMacro, PermissionMode, ServerEvent, ATTACHMENT_CHUNK_BYTES,
    ApprovalReceipt, ApprovalState, BackgroundCommand, BackgroundCommandState,
    ChatWaitReason, ChatWaiting, DeckRoute, GoalTail, InboxProposal, ThreadView, CLAUDE_MODELS, DelegateJob, DelegateReport, DELEGATE_JOB, MAX_ATTACHMENTS, MAX_ATTACHMENT_BYTES,
    JobStatus, JobSummary, WorkerApprovalPolicy,
};
use fuzukue_view::chat::{
    is_json_document, paper_batch_failure_summary, tool_result_summary, tool_summary, ChatGrouping,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::appearance::{ThemeSurfaces, UI_LINE_HEIGHT};
use super::chat_quote::quote_insertion;
use super::chat_question::QuestionCard;
use super::composer_markdown::{command_candidates, continue_line, visual_edge_target, CommandKey};
use super::icons::Symbol;
use super::settings_file;
use super::{composing, scroll_list_by_key, Workbench};
#[cfg(test)]
use super::math;

#[path = "clipboard_attach.rs"]
mod clipboard_attach;
#[path = "attachment_drop.rs"]
mod attachment_drop;
#[path = "attachment_image.rs"]
mod attachment_image;
#[path = "chat_activity.rs"]
pub(super) mod chat_activity;
#[path = "chat_code.rs"]
mod chat_code;
#[path = "chat_history.rs"]
mod chat_history;
pub(super) use chat_history::Transcripts;
#[path = "chat_transcript.rs"]
mod chat_transcript;
#[path = "chat_tool_runs.rs"]
mod chat_tool_runs;
use chat_tool_runs::ToolRuns;

use chat_activity::{status_row, status_speech, STATUS_OPTICAL_OFFSET};
use chat_transcript::{MarkdownPart, RowEstimate, TranscriptRows, TranscriptStyle, TranscriptView};
pub(crate) use chat_transcript::TranscriptPosition;

use attachment_drop::DropState;
use attachment_image::{decode_attachment_image, AttachmentImage, AttachmentThumbnail};

const LONG_PARTNER_TOOL_RUN: usize = 4;
const CHAT_WIDTH: f32 = 800.;
const CHAT_COLUMN_INSET: Rems = rems(1.25);
const CHAT_GROUP_GAP: Pixels = px(24.);
const CHAT_GROUP_ROW_GAP: Pixels = px(8.);
const CHAT_LEADING_PADDING: Pixels = px(16.);
// Logical height of the overlay ending at the transcript's clip edge.
const CHAT_BOTTOM_FADE: Pixels = px(28.);
const CHAT_STATUS_TEXT_INSET: Pixels = px(2.);
const CHAT_TAIL_CLEARANCE: Pixels = px(4.);
// Four smoothstep samples across the ramp. The final logical pixel is solid,
// so the last physical pixel is also fully covered at fractional scale factors.
const CHAT_FADE_STOPS: [(Pixels, f32); 4] = [
    (px(0.), 0.),
    (px(9.), 7. / 27.),
    (px(18.), 20. / 27.),
    (px(27.), 1.),
];
const QUOTE_PILL_WIDTH: Rems = rems(7.5);
const QUOTE_PILL_HEIGHT: Rems = rems(1.75);
const QUOTE_PILL_GAP: Pixels = px(5.);
pub(super) const QUOTE_REPLY_SHORTCUT: &str = "Ctrl+Shift+Q";
const CHAT_JUMP_SIZE: Pixels = px(32.);
const CHAT_JUMP_MARGIN: Pixels = px(8.);

fn snap_chat_height(height: Pixels, scale: f32) -> Pixels {
    px((f32::from(height) * scale).round() / scale)
}

fn chat_tail_padding(scale: f32) -> Pixels {
    snap_chat_height(CHAT_JUMP_SIZE + CHAT_JUMP_MARGIN * 2., scale)
        .max(CHAT_BOTTOM_FADE + CHAT_TAIL_CLEARANCE)
}

pub(super) fn chat_column() -> gpui_kit::Div {
    div().w_full().max_w(px(CHAT_WIDTH)).mx_auto().px(CHAT_COLUMN_INSET)
}

pub(super) struct Chat {
    rows: Vec<Row>,
    history: chat_history::History,
    jobs: chat_history::JobSummaries,
    by_item: HashMap<String, usize>,
    by_tool: HashMap<String, (usize, usize)>,
    approval_receipts: HashMap<String, ApprovalReceipt>,
    by_approval: HashMap<String, (usize, usize)>,
    delegate_items: HashMap<String, (usize, usize)>,
    pending_delegates: HashSet<String>,
    delegate_anchor: Option<usize>,
    delegate_to_partner: bool,
    worker_target: Option<String>,
    grouping: ChatGrouping,
    pub(super) scroller: ListState,
    transcript: TranscriptView,
    selection_scope: TextSelectionScopeId,
    /// 休憩中なら (再開の時刻, 理由, ためているメッセージ数)
    pub paused: Option<(i64, String, usize)>,
    pub busy: bool,
    turn: Option<TurnStatus>,
    composer_notice: Option<String>,
    composer_bounds: Rc<Cell<gpui_kit::Bounds<Pixels>>>,
    pub(super) codex_thread_id: Option<String>,
    pub(super) settings: ChatSettings,
    attachments: Vec<DraftAttachment>,
    inbox: BTreeMap<String, InboxEntry>,
    hovered_message_row: Option<(usize, u64)>,
    quote_position: Option<Point<Pixels>>,
    disclosure_press: Rc<Cell<Option<Point<Pixels>>>>,
    math_macros: Vec<MathMacro>,
    markdown: MarkdownExtensions,
}

#[derive(Default)]
pub(super) struct ChatWaitingState {
    workspace: Option<TimedWait>,
    threads: BTreeMap<String, Option<TimedWait>>,
    goal_status: BTreeMap<String, GoalTail>,
    background_commands: BTreeMap<String, Vec<BackgroundCommand>>,
}

#[derive(Clone, Copy)]
struct TimedWait {
    reason: ChatWaitReason,
    since: Instant,
}

impl TimedWait {
    fn update(previous: Option<Self>, reason: ChatWaitReason, now: Instant) -> Self {
        let since = previous.filter(|previous| {
            previous.reason == reason || matches!(
                (previous.reason, reason),
                (ChatWaitReason::UsagePaused { .. }, ChatWaitReason::UsagePaused { .. })
            )
        }).map_or(now, |previous| previous.since);
        Self { reason, since }
    }
}

impl ChatWaitingState {
    fn update(&mut self, waiting: ChatWaiting, now: Instant) {
        let threads = waiting.threads.into_iter().map(|(session, reason)| {
            let next = reason.map(|reason| TimedWait::update(self.reason(&session), reason, now));
            (session, next)
        }).collect();
        self.workspace = waiting.workspace.map(|reason| TimedWait::update(self.workspace, reason, now));
        self.threads = threads;
        self.goal_status = waiting.goal_status;
    }

    fn reason(&self, session: &str) -> Option<TimedWait> {
        if self.goal_status.contains_key(session) { return None; }
        self.threads.get(session).copied().unwrap_or(self.workspace)
    }

    pub(super) fn usage_paused(&self, session: &str) -> bool {
        self.reason(session).is_some_and(|wait| matches!(wait.reason, ChatWaitReason::UsagePaused { .. }))
    }

    fn static_goal_status(&self, session: &str, chat: &Chat) -> Option<&GoalTail> {
        if chat.busy || chat.pending_jobs().next().is_some() { return None; }
        self.goal_status.get(session)
    }

    fn background_commands(&self, session: &str) -> &[BackgroundCommand] {
        self.background_commands.get(session).map_or(&[], Vec::as_slice)
    }

    fn has_background_commands(&self, session: &str) -> bool {
        self.background_commands(session).iter().any(|command| command.state == BackgroundCommandState::Running)
    }
}

struct Row {
    id: u64,
    actor: Actor,
    content: Content,
    at_ms: Option<u64>,
    estimate: RowEstimate,
}

impl Row {
    fn new(actor: Actor, content: Content) -> Self {
        static NEXT_ROW_ID: AtomicU64 = AtomicU64::new(0);
        let estimate = RowEstimate::for_content(&actor, &content);
        Self {
            id: NEXT_ROW_ID.fetch_add(1, Ordering::Relaxed),
            actor, content, at_ms: Some(unix_millis()), estimate,
        }
    }

    fn kind(&self, show_worker_messages: bool) -> RowKind {
        match &self.content {
            Content::Message(message) if message.superseded || message.hidden
                || (message.text.trim().is_empty() && message.attachments.is_empty() && !message.to_worker) => RowKind::Hidden,
            Content::InternalNote { .. } => internal_note_row_kind(show_worker_messages),
            Content::Message(message) if is_worker_message(&self.actor, message.item.as_deref()) =>
                internal_note_row_kind(show_worker_messages),
            Content::Message(_) if is_partner(&self.actor) => RowKind::AssistantText,
            Content::Delegate(block) if block.progress.iter().any(|entry| matches!(entry, DelegateEntry::Steer { .. })) => {
                RowKind::SpeakerBoundary
            }
            Content::GitHub { events, open, .. } if events.is_empty() || (!open && events.len() <= 3) => RowKind::Hidden,
            Content::Tools(group) if group.calls.is_empty() && group.approvals.is_empty() => RowKind::Hidden,
            Content::GitHub { .. } | Content::GitHubConnect { .. } | Content::Notice { .. } | Content::PlanNotice { .. } => RowKind::SpeakerBoundary,
            Content::Message(_) if self.actor == Actor::User => {
                RowKind::SpeakerBoundary
            }
            _ => RowKind::AssistantDetail,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Hidden,
    SpeakerBoundary,
    AssistantText,
    AssistantDetail,
}

fn internal_note_row_kind(show_worker_messages: bool) -> RowKind {
    if show_worker_messages { RowKind::SpeakerBoundary } else { RowKind::Hidden }
}

fn merge_plan_notice(content: &mut Content, count: usize, text: String) -> bool {
    match content {
        Content::PlanNotice { count: current, text: label } if *current < count => {
            *current = count;
            *label = text;
            true
        }
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AssistantGroup {
    first: usize,
    last: usize,
    last_text: Option<usize>,
}

/// Adjacent visible assistant rows start a new group at this interval.
const ASSISTANT_GROUP_GAP_MINUTES: u64 = 5;

fn assistant_groups(rows: impl IntoIterator<Item = (RowKind, Option<u64>)>) -> Vec<AssistantGroup> {
    let mut groups: Vec<AssistantGroup> = Vec::new();
    let mut previous = None::<Option<u64>>;
    for (index, (kind, at_ms)) in rows.into_iter().enumerate() {
        match kind {
            RowKind::Hidden => continue,
            RowKind::SpeakerBoundary => {
                previous = None;
                continue;
            }
            RowKind::AssistantText | RowKind::AssistantDetail => {}
        }
        let last_text = (kind == RowKind::AssistantText).then_some(index);
        let continues = previous.is_some_and(|before| {
            before.zip(at_ms).is_none_or(|(before, now)| before.abs_diff(now) < ASSISTANT_GROUP_GAP_MINUTES * 60_000)
        });
        if continues {
            let group = groups.last_mut().unwrap();
            group.last = index;
            group.last_text = last_text.or(group.last_text);
        } else {
            groups.push(AssistantGroup { first: index, last: index, last_text });
        }
        previous = Some(at_ms);
    }
    groups
}

// The last row owns the whole boundary budget, including any action strip.
fn chat_row_spacing(first: bool, ends_group: bool, action_height: Pixels) -> (Pixels, Pixels) {
    let top = if first { CHAT_LEADING_PADDING } else { px(0.) };
    let bottom = if ends_group {
        CHAT_GROUP_GAP - action_height
    } else {
        CHAT_GROUP_ROW_GAP
    };
    (top, bottom)
}

fn hidden_message_actions_row(
    has_activity: bool,
    latest_message: Option<usize>,
    groups: &[AssistantGroup],
) -> Option<usize> {
    let message = latest_message.filter(|_| has_activity)?;
    let group = groups.get(groups.partition_point(|group| group.last < message))
        .filter(|group| group.first <= message);
    Some(group.map_or(message, |group| group.last))
}

enum Content {
    GitHub { item: String, events: Vec<fuzukue_proto::GitHubActivity>, open: bool },
    GitHubConnect { attempt: String, state: fuzukue_proto::GitHubConnectState },
    InternalNote { kind: InternalNote, text: String, open: bool },
    PlanNotice { count: usize, text: String },
    Message(Message),
    Notice { text: String, divider: bool },
    Tools(ToolGroup),
    Question { card: Entity<QuestionCard>, lines: usize },
    CommentApplication(CommentApplication),
    Delegate(DelegateBlock),
    AutoAttached(DeckRoute),
}

#[derive(Clone, Copy)]
enum InternalNote {
    Plan,
    DrainInstruction,
    ReasoningSummary,
}

impl InternalNote {
    fn for_item(actor: &Actor, item: &str) -> Option<Self> {
        match actor {
            Actor::System if item.starts_with(fuzukue_proto::PLAN_NOTE) => Some(Self::Plan),
            // Checkpoints use a distinct `drain-` item and stay visible for recovery.
            Actor::System if item.starts_with("drain:") => Some(Self::DrainInstruction),
            Actor::Codex { .. } if item.contains(":summary:") => Some(Self::ReasoningSummary),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plan => tr!("chat-planner"),
            Self::DrainInstruction | Self::ReasoningSummary => tr!("tiers-details"),
        }
    }
}

struct InboxEntry {
    proposal: Option<InboxProposal>,
    pending: bool,
    destination: Option<String>,
    error: Option<String>,
    preview: Option<Arc<AttachmentImage>>,
}

struct DelegateBlock {
    id: String,
    job: DelegateJob,
    summary: Option<String>,
    wire_summary: Option<JobSummary>,
    progress: Vec<DelegateEntry>,
    text_open: Option<bool>,
    tools_open: bool,
    earlier_tools_open: bool,
    report: Option<(bool, DelegateReport)>,
    deferred: Option<DeferredJob>,
}

#[derive(Default)]
struct DeferredJob {
    open: bool,
    tools: usize,
    request: Option<u64>,
    next_before: Option<u64>,
    pending: Vec<(u64, ServerEvent)>,
}

const VISIBLE_DELEGATE_TOOLS: usize = 12;

enum DelegateEntry {
    Text(String),
    Tool(ToolCall),
    /// 作業中に利用者が作業役へ送った文。送った時点の位置に出す。
    Steer { text: String, attachments: Vec<String> },
}

struct TurnStatus {
    started: Instant,
    phase_started: Instant,
    writing: bool,
    consulting: Option<Instant>,
    tools: Vec<String>,
    pending_approvals: Vec<String>,
}

#[derive(PartialEq, Eq)]
enum TurnPhase {
    Thinking,
    Writing,
    Tool(String),
    Approval,
    Consulting,
}

impl TurnStatus {
    fn phase(&self) -> TurnPhase {
        if self.consulting.is_some() {
            TurnPhase::Consulting
        } else if !self.pending_approvals.is_empty() {
            TurnPhase::Approval
        } else if let Some(tool) = self.tools.first() {
            TurnPhase::Tool(tool.clone())
        } else if self.writing {
            TurnPhase::Writing
        } else {
            TurnPhase::Thinking
        }
    }

    fn update_phase(&mut self, update: impl FnOnce(&mut Self)) {
        let previous = self.phase();
        update(self);
        if self.phase() != previous {
            self.phase_started = Instant::now();
        }
    }
}

struct TurnActivity<'a> {
    writing: bool,
    phase_elapsed: Duration,
    elapsed: Duration,
    tool: Option<ToolActivity<'a>>,
}

enum ToolActivity<'a> {
    Running { name: &'a str, elapsed: Duration, additional: usize },
    AwaitingApproval,
    Consulting { elapsed: Duration },
}

struct Message {
    details: Option<fuzukue_proto::RunDetails>,
    item: Option<String>,
    memo_saved: Option<bool>,
    superseded: bool,
    hidden: bool,
    text: String,
    markdown: bool,
    json_open: bool,
    worker_text_open: Option<bool>,
    attachments: Vec<String>,
    to_worker: bool,
}

/// A live approval shown in the dock until it is resolved.
pub(super) struct PendingLiveApproval {
    pub request_id: String,
    pub summary: crate::i18n::Message,
    pub shell: bool,
    pub remember_shell: bool,
}

struct Approval {
    request_id: String,
    summary: crate::i18n::Message,
    resolved: Option<String>,
    automatic: bool,
    shell: bool,
    remember_shell: bool,
}

#[derive(Default)]
struct ToolGroup {
    calls: Vec<ToolCall>,
    approvals: Vec<Approval>,
    decided_open: bool,
    open: bool,
    ended: bool,
}

struct ToolCall {
    item: String,
    name: String,
    summary: String,
    input: Value,
    started: Instant,
    input_open: bool,
    result: Option<ToolResult>,
    result_open: bool,
    detail: ToolDetail,
}

#[derive(Default)]
struct ToolDetail {
    request: Option<u64>,
    input: Option<Value>,
    output: Option<String>,
    unavailable: bool,
}

impl ToolDetail {
    fn status(&self) -> Option<&'static str> {
        if self.request.is_some() {
            Some(tr!("chat-tool-details-loading"))
        } else if self.unavailable {
            Some(tr!("chat-tool-details-unavailable"))
        } else {
            None
        }
    }
}

struct ToolResult {
    ok: bool,
    output: String,
}

#[derive(Clone, Copy, Debug)]
enum Disclosure {
    GitHub,
    InternalNote,
    Json,
    WorkerText,
    DelegateTools,
    DelegateDetails,
    MoreDelegateDetails,
    DelegateEarlierTools,
    DelegateTool(usize),
    Group,
    Input(usize),
    Result(usize),
    DecidedApprovals,
}

impl Chat {
    pub(super) fn new(cx: &mut Context<Workbench>) -> Self {
        // Estimates support PageUp and scrollbar seeks before those rows have
        // been visited. Only lay out the viewport, even on a tall display.
        let scroller = ListState::new(0, ListAlignment::Top, px(0.));
        scroller.set_follow_mode(FollowMode::Tail);
        let workbench = cx.weak_entity();
        let markdown = super::chat_components::extensions(&[], workbench.clone());
        scroller.set_scroll_handler(move |_, _, cx| {
            let workbench = workbench.clone();
            cx.defer(move |cx| {
                let _ = workbench.update(cx, |view, cx| {
                    view.history_scrolled(cx);
                    cx.notify();
                });
            });
        });
        Self {
            rows: Vec::new(),
            history: Default::default(),
            jobs: Default::default(),
            by_item: HashMap::new(),
            by_tool: HashMap::new(),
            approval_receipts: HashMap::new(),
            by_approval: HashMap::new(),
            delegate_items: HashMap::new(),
            pending_delegates: HashSet::new(),
            delegate_anchor: None,
            delegate_to_partner: false,
            worker_target: None,
            grouping: ChatGrouping::default(),
            scroller,
            transcript: TranscriptView::default(),
            selection_scope: TextSelectionScopeId::new(),
            paused: None,
            busy: false,
            turn: None,
            composer_notice: None,
            composer_bounds: Default::default(),
            codex_thread_id: None,
            settings: ChatSettings::default(),
            attachments: Vec::new(),
            inbox: BTreeMap::new(),
            hovered_message_row: None,
            quote_position: None,
            disclosure_press: Rc::default(),
            math_macros: Vec::new(),
            markdown,
        }
    }

    pub(super) fn codex_selected(&self) -> bool {
        AgentBackend::for_model(self.settings.model.as_deref()) == AgentBackend::Codex
    }

    pub(super) fn push_text(
        &mut self,
        item: String,
        actor: Actor,
        text: String,
        cx: &mut Context<Workbench>,
    ) {
        if let Some(job) = delegate_job(&item, &actor) {
            if let Some((row, entry)) =
                self.delegate_entry(job, &item, || DelegateEntry::Text(String::new()))
            {
                if let DelegateEntry::Text(content) = entry {
                    content.push_str(&text);
                    self.remeasure(row, cx);
                }
                return;
            }
        }
        if let Some(kind) = InternalNote::for_item(&actor, &item) {
            if let Some(&row) = self.by_item.get(&item) {
                // Reasoning summaries can arrive in chunks. Plan and drain notices arrive whole.
                if matches!(kind, InternalNote::ReasoningSummary) {
                    if let Content::InternalNote { text: current, .. } = &mut self.rows[row].content {
                        current.push_str(&text);
                        self.remeasure(row, cx);
                    }
                }
            } else {
                self.push_system_row(item, Content::InternalNote { kind, text, open: false }, cx);
            }
            return;
        }
        if is_partner(&actor) && !is_worker_message(&actor, Some(&item)) {
            self.set_busy(true, cx);
            if let Some(turn) = &mut self.turn {
                turn.update_phase(|turn| turn.writing = true);
            }
        }
        // 送った発言は 1 通で丸ごと届く。古いサーバーは再起動のたびに `user:0` から振り直していたので、
        // 同じ ID でも別の行にする。
        let whole = item.starts_with("user:");
        if let Some(&row) = self.by_item.get(&item).filter(|_| !whole) {
            if let Content::Message(message) = &mut self.rows[row].content {
                message.text.push_str(&text);
                message.hidden = is_assistant(&actor) && message_has_no_visible_text(&message.text, &self.markdown);
                self.remeasure(row, cx);
            }
        } else {
            self.append_message(actor, Some(item), text, true, cx);
        }
    }

    pub(super) fn push_error(&mut self, text: String, cx: &mut Context<Workbench>) {
        self.append_message(Actor::System, None, text, false, cx);
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn first_pending_approval(&self) -> Option<&str> {
        self.rows.iter().find_map(|row| match &row.content {
            Content::Tools(group) => group.approvals.iter()
                .find(|approval| approval.resolved.is_none())
                .map(|approval| approval.request_id.as_str()),
            _ => None,
        })
    }

    /// Live approvals waiting for the user, in arrival order. The dock shows them.
    pub(super) fn pending_live_approvals(&self) -> Vec<PendingLiveApproval> {
        let mut pending: Vec<_> = self.by_approval.iter().filter_map(|(id, &(row, index))| {
            let Content::Tools(group) = &self.rows.get(row)?.content else { return None };
            let approval = group.approvals.get(index).filter(|approval| approval.resolved.is_none())?;
            Some((row, index, PendingLiveApproval {
                request_id: id.clone(),
                summary: approval.summary.clone(),
                shell: approval.shell,
                remember_shell: approval.remember_shell,
            }))
        }).collect();
        pending.sort_by_key(|(row, index, _)| (*row, *index));
        pending.into_iter().map(|(_, _, approval)| approval).collect()
    }

    /// Unanswered question cards, in arrival order. The dock shows them.
    pub(super) fn pending_questions(&self, cx: &gpui_kit::App) -> Vec<Entity<QuestionCard>> {
        self.rows.iter().filter_map(|row| match &row.content {
            Content::Question { card, .. } if !card.read(cx).is_resolved() => Some(card.clone()),
            _ => None,
        }).collect()
    }

    pub(super) fn busy(&self) -> bool {
        self.busy
    }

    pub(super) fn request_approval(
        &mut self,
        request_id: String,
        actor: Actor,
        summary: crate::i18n::Message,
        remember_shell: bool,
        cx: &mut Context<Workbench>,
    ) {
        if self.by_approval.contains_key(&request_id) {
            return;
        }
        if is_partner(&actor) && delegate_job(&request_id, &actor).is_none() {
            self.set_busy(true, cx);
            if let Some(turn) = &mut self.turn {
                turn.update_phase(|turn| turn.pending_approvals.push(request_id.clone()));
            }
        }
        let (row, joined) = self.tool_group(actor, true);
        if let Content::Tools(group) = &mut self.rows[row].content {
            let shell = summary.fallback.starts_with("Bash:")
                || summary.localized.as_ref().is_some_and(|message| message.key == "approval-run-command");
            self.by_approval
                .insert(request_id.clone(), (row, group.approvals.len()));
            group.approvals.push(Approval {
                request_id,
                summary,
                resolved: None,
                automatic: false,
                shell,
                remember_shell,
            });
        }
        self.update_tool_group(row, joined, cx);
    }

    pub(super) fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: &str,
        automatic: bool,
        cx: &mut Context<Workbench>,
    ) {
        if let Some(&(row, index)) = self.by_approval.get(request_id) {
            if let Content::Tools(group) = &mut self.rows[row].content {
                let approval = &mut group.approvals[index];
                approval.resolved = Some(decision.to_owned());
                approval.automatic = automatic;
                if let Some(turn) = &mut self.turn {
                    turn.update_phase(|turn| turn.pending_approvals.retain(|id| id != request_id));
                }
                self.remeasure(row, cx);
            }
        }
    }

    pub(super) fn add_question(
        &mut self, request_id: String, actor: Actor, card: Entity<QuestionCard>, lines: usize,
        cx: &mut Context<Workbench>,
    ) {
        if self.by_item.contains_key(&request_id) {
            return;
        }
        self.grouping.push(&actor, false);
        self.by_item.insert(request_id, self.rows.len());
        self.rows.push(Row::new(actor, Content::Question { card, lines }));
        self.append_row(cx);
    }

    pub(super) fn question_resolved(&mut self, request_id: &str, cx: &mut Context<Workbench>) {
        if let Some(&row) = self.by_item.get(request_id) {
            if let Content::Question { lines, .. } = &mut self.rows[row].content {
                *lines = 1;
                self.remeasure(row, cx);
            }
        }
    }

    fn append_message(
        &mut self,
        actor: Actor,
        item: Option<String>,
        text: String,
        markdown: bool,
        cx: &mut Context<Workbench>,
    ) {
        let previous_reply = self.transcript.current_style().and_then(|_| self.latest_reply());
        self.grouping.push(&actor, false);
        let tail = self.rows.len();
        if actor == Actor::User && !self.busy {
            self.delegate_anchor = None;
        }
        // Use the first delegation in this partner turn as the insertion point.
        // Later user turns and worker updates stay at their original anchors.
        let index = if is_partner(&actor) && !is_worker_message(&actor, item.as_deref()) {
            self.delegate_anchor.unwrap_or(tail)
        } else {
            tail
        };
        if index < tail {
            for row in self.by_item.values_mut()
                .chain(self.by_tool.values_mut().map(|(row, _)| row))
                .chain(self.by_approval.values_mut().map(|(row, _)| row))
                .chain(self.delegate_items.values_mut().map(|(row, _)| row))
            {
                *row += usize::from(*row >= index);
            }
            self.delegate_anchor = Some(index + 1);
        }
        if let Some(item) = &item {
            self.by_item.insert(item.clone(), index);
        }
        let hidden = is_assistant(&actor) && message_has_no_visible_text(&text, &self.markdown);
        self.rows.insert(index, Row::new(
            actor,
            Content::Message(Message {
                details: None,
                item,
                memo_saved: None,
                superseded: false,
                hidden,
                text,
                markdown,
                json_open: false,
                worker_text_open: None,
                attachments: Vec::new(),
                to_worker: false,
            }),
        ));
        self.insert_row_geometry(index);
        self.remeasure_group_near(index);
        if let Some(previous) = previous_reply {
            self.remeasure(previous + usize::from(previous >= index), cx);
        }
        cx.notify();
    }

    fn latest_reply(&self) -> Option<usize> {
        let user = self.rows.iter().rposition(|row| {
            row.actor == Actor::User && matches!(row.content, Content::Message(_))
        })?;
        self.rows.iter().enumerate().skip(user + 1).rev()
            .find_map(|(index, row)| {
                (row.kind(false) == RowKind::AssistantText
                    && matches!(&row.content, Content::Message(message) if message.item.is_some()))
                    .then_some(index)
            })
    }

    pub(super) fn supersede(&mut self, items: &[String], cx: &mut Context<Workbench>) {
        for item in items {
            if let Some(&row) = self.by_item.get(item) {
                if let Content::Message(message) = &mut self.rows[row].content {
                    message.superseded = true;
                    self.remeasure(row, cx);
                }
            }
        }
    }

    pub(super) fn comment_application(&mut self, application: CommentApplication, cx: &mut Context<Workbench>) {
        let item = format!("comments-{}", application.thread);
        if let Some(&row) = self.by_item.get(&item) {
            self.rows[row].content = Content::CommentApplication(application);
            self.remeasure(row, cx);
        } else {
            self.grouping.push(&Actor::System, false);
            self.by_item.insert(item, self.rows.len());
            self.rows.push(Row::new(Actor::System, Content::CommentApplication(application)));
            self.append_row(cx);
        }
    }

    fn github_activity(&mut self, item: String, events: Vec<fuzukue_proto::GitHubActivity>, cx: &mut Context<Workbench>) {
        let open = events.len() <= 3;
        self.push_system_row(item.clone(), Content::GitHub { item, events, open }, cx);
    }

    fn row_for_approval_anchor(&self, item: &str) -> Option<usize> {
        self.by_item.get(item).copied().or_else(|| self.by_tool.get(item).map(|&(row, _)| row))
    }

    pub(super) fn record_approval_receipt(&mut self, receipt: ApprovalReceipt, cx: &mut Context<Workbench>) {
        let previous = self.approval_receipts.get(&receipt.id);
        if previous.is_some_and(|old| old == &receipt || old.updated_at > receipt.updated_at
            || (old.updated_at == receipt.updated_at && old.tool_item.is_some() && receipt.tool_item.is_none()))
        {
            return;
        }
        let old_row = previous.and_then(|old| old.history_anchor())
            .and_then(|item| self.row_for_approval_anchor(item));
        let row = receipt.job().or_else(|| receipt.history_anchor())
            .and_then(|item| self.row_for_approval_anchor(item));
        self.approval_receipts.insert(receipt.id.clone(), receipt);
        for row in old_row.into_iter().chain(row) {
            self.remeasure(row, cx);
        }
    }

    pub(super) fn local_notice(
        &mut self, item: String, text: String, localized: Option<fuzukue_proto::LocalizedMessage>, cx: &mut Context<Workbench>,
    ) {
        if let Some(receipt) = localized.as_ref().and_then(ApprovalReceipt::from_message) {
            self.record_approval_receipt(receipt, cx);
            return;
        }
        if item == fuzukue_proto::COMPOSER_NOTICE_ITEM {
            if self.composer_notice.as_deref() != Some(&text) {
                self.composer_notice = Some(text);
                cx.notify();
            }
            return;
        }
        let burst = localized.as_ref().filter(|_| item.starts_with("planner-notice:")).and_then(|message| {
            Some((message.args.get("burst")?, message.args.get("count")?.parse().ok()?))
        });
        if let Some((burst, count)) = burst {
            self.push_plan_notice(burst, count, text, cx);
        } else {
            let divider = localized.is_some_and(|message| message.key == "command-cleared");
            self.push_system_row(item, Content::Notice { text, divider }, cx);
        }
    }

    fn user_prompt(
        &mut self, item: String, text: String, paths: Vec<String>, to_worker: bool,
        cx: &mut Context<Workbench>,
    ) {
        if to_worker {
            let row = if let Some(job) = delegate_job(&item, &Actor::User) {
                self.by_item.get(job).copied()
            } else {
                self.rows.iter().rposition(|row| {
                    matches!(&row.content, Content::Delegate(block) if block.report.is_none())
                })
            };
            if let Some(row) = row {
                if let Content::Delegate(block) = &mut self.rows[row].content {
                    self.delegate_items.entry(item).or_insert_with(|| {
                        block.progress.push(DelegateEntry::Steer { text, attachments: paths });
                        (row, block.progress.len() - 1)
                    });
                }
                self.remeasure(row, cx);
                return;
            }
        }
        self.append_message(Actor::User, Some(item), text, true, cx);
        if to_worker || !paths.is_empty() {
            if let Some(Row { content: Content::Message(message), .. }) = self.rows.last_mut() {
                message.attachments = paths;
                message.to_worker = to_worker;
            }
            self.remeasure(self.rows.len() - 1, cx);
        }
    }

    fn push_system_row(&mut self, key: String, content: Content, cx: &mut Context<Workbench>) {
        if self.by_item.contains_key(&key) {
            return;
        }
        self.grouping.push(&Actor::System, false);
        self.by_item.insert(key, self.rows.len());
        self.rows.push(Row::new(Actor::System, content));
        self.append_row(cx);
    }

    fn push_plan_notice(&mut self, burst: &str, count: usize, text: String, cx: &mut Context<Workbench>) {
        let key = format!("planner-notice:{burst}");
        if let Some(&row) = self.by_item.get(&key) {
            if merge_plan_notice(&mut self.rows[row].content, count, text) {
                self.remeasure(row, cx);
            }
        } else {
            self.push_system_row(key, Content::PlanNotice { count, text }, cx);
        }
    }

    fn file_pending(&self, path: &str) -> bool {
        self.inbox.get(path).is_some_and(|entry| entry.pending)
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_uploaded_attachment(&self, name: &str) -> bool {
        self.attachments.iter().any(|draft| draft.name == name
            && matches!(draft.state, AttachmentState::Saved(_)))
    }

    pub(super) fn inbox_files(&self) -> impl Iterator<Item = &str> {
        self.inbox.iter().filter_map(|(path, entry)| {
            entry.destination.as_deref().is_none_or(|path| path.starts_with("inbox/"))
                .then_some(path.as_str())
        })
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_inbox_proposal(&self, to: &str) -> bool {
        self.inbox.values().any(|entry| entry.proposal.as_ref().is_some_and(|proposal| proposal.to == to))
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_partner_text(&self, needle: &str) -> bool {
        self.rows.iter().any(|row| {
            matches!(row.actor, Actor::Claude | Actor::Codex { parent_tool_use: None, .. })
                && matches!(&row.content, Content::Message(message) if !message.superseded && message.text.contains(needle))
        })
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn delegate_details_id(&self) -> Option<String> {
        self.rows.iter().rev().find_map(|row| {
            let Content::Delegate(block) = &row.content else { return None; };
            Some(format!("delegate-details-{}", block.id))
        })
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_delegate_report(&self, model: &str) -> bool {
        self.jobs.iter().any(|summary| summary.job.model == model && summary.status == JobStatus::Succeeded)
            || self.rows.iter().any(|row| matches!(&row.content, Content::Delegate(block)
            if block.job.model == model && block.report.as_ref().is_some_and(|(ok, report)| *ok && !report.text.is_empty())))
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_live_delegate_text(&self, needle: &str) -> bool {
        self.jobs.iter().any(|summary| summary.status.active()
            && summary.latest_message.as_ref().is_some_and(|text| text.contains(needle)))
            || self.rows.iter().any(|row| matches!(&row.content, Content::Delegate(block)
            if block.report.is_none() && block.progress.iter().any(|entry|
                matches!(entry, DelegateEntry::Text(text) if text.contains(needle)))))
    }

    pub(super) fn delegate_renamed(&mut self, job: &str, change: &fuzukue_proto::DelegateNameChange, cx: &mut Context<Workbench>) {
        if let Some(&row) = self.by_item.get(job) {
            if let Content::Delegate(block) = &mut self.rows[row].content {
                block.job.display.apply_rename(change);
                self.remeasure(row, cx);
            }
        }
    }

    pub(super) fn delegate_dismissed(&mut self, job: &str) {
        if let Some(&row) = self.by_item.get(job) {
            if let Content::Delegate(block) = &mut self.rows[row].content {
                block.job.dismissed_from_panel = true;
            }
        }
    }

    pub(super) fn delegate_progress(&mut self, job: &str, summary: String, cx: &mut Context<Workbench>) {
        if summary.trim().is_empty() {
            return;
        }
        if let Some(&row) = self.by_item.get(job) {
            if let Content::Delegate(block) = &mut self.rows[row].content {
                block.summary = Some(summary);
                self.remeasure(row, cx);
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn has_live_delegate_summaries(&self, count: usize) -> bool {
        let summaries = self.jobs.iter().filter(|summary|
            summary.status == JobStatus::Running && summary.last_activity.is_some()).count();
        let legacy = self.rows.iter().filter(|row| matches!(&row.content, Content::Delegate(block)
            if self.jobs.get(&block.id).is_none() && block.job.started_at_ms.is_some()
                && block.report.is_none() && block.summary.is_some())).count();
        summaries + legacy >= count
    }

    fn delegate_entry(
        &mut self, job: &str, item: &str, new: impl FnOnce() -> DelegateEntry,
    ) -> Option<(usize, &mut DelegateEntry)> {
        let row = *self.by_item.get(job)?;
        let Content::Delegate(block) = &mut self.rows[row].content else { return None };
        let index = *self.delegate_items.entry(item.into()).or_insert_with(|| {
            block.progress.push(new());
            (row, block.progress.len() - 1)
        });
        Some((row, &mut block.progress[index.1]))
    }

    pub(super) fn push_replayed_tool(
        &mut self, item: String, actor: Actor, name: &str, input: Value, cx: &mut Context<Workbench>,
    ) {
        if self.history.sparse_workers.contains(&item) {
            let anchor = self.delegate_anchor;
            self.push_tool(item, actor, name, input, cx);
            self.delegate_anchor = anchor;
        } else {
            self.push_tool(item, actor, name, input, cx);
        }
    }

    pub(super) fn push_tool(
        &mut self,
        item: String,
        actor: Actor,
        name: &str,
        input: Value,
        cx: &mut Context<Workbench>,
    ) {
        if item.starts_with(DELEGATE_JOB) && name == "delegate" {
            if let Ok(mut job) = serde_json::from_value::<DelegateJob>(input.clone()) {
                let compact = input["details_deferred"].as_bool() == Some(true);
                let count = input["tool_count"].as_u64().unwrap_or(0) as usize;
                let wire_summary = serde_json::from_value::<JobSummary>(input["job_summary"].clone()).ok();
                if job.display.label.is_none() && job.display.card_id.is_none() {
                    job.display = fuzukue_proto::DelegateDisplay::from_title(job.title.as_deref(), None);
                }
                let mut started = job.started_at_ms.is_some();
                if let Some(&row) = self.by_item.get(&item) {
                    if let Content::Delegate(block) = &mut self.rows[row].content {
                        if block.report.as_ref().is_some_and(|(_, report)| report.interrupted) {
                            block.report = None;
                            self.pending_delegates.insert(item.clone());
                        }
                        started &= block.job.started_at_ms.is_none() && block.report.is_none();
                        job.dismissed_from_panel |= block.job.dismissed_from_panel;
                        block.job = job;
                        if compact {
                            block.deferred.get_or_insert_with(DeferredJob::default).tools = count;
                            block.wire_summary = wire_summary;
                        }
                        self.remeasure(row, cx);
                    }
                } else {
                    self.grouping.push(&Actor::System, false);
                    self.delegate_anchor.get_or_insert(self.rows.len());
                    if let Some(row) = self.latest_reply() {
                        self.remeasure(row, cx);
                    }
                    self.pending_delegates.insert(item.clone());
                    self.by_item.insert(item.clone(), self.rows.len());
                    self.rows.push(Row::new(
                        Actor::System,
                        Content::Delegate(DelegateBlock {
                            id: item.clone(),
                            job,
                            summary: None,
                            wire_summary,
                            progress: Vec::new(),
                            text_open: None,
                            tools_open: false,
                            earlier_tools_open: false,
                            report: None,
                            deferred: compact.then_some(DeferredJob { tools: count, ..DeferredJob::default() }),
                        }),
                    ));
                    self.append_row(cx);
                }
                if started {
                    self.delegate_to_partner = false;
                }
                return;
            }
        }
        let summary = match name {
            "Codex" => tool_display_name(name).to_owned(),
            "worker_approval" => automatic_approval_label(input["decision"].as_str() == Some("accept")).into(),
            _ => tool_summary(name, &input),
        };
        let job = delegate_job(&item, &actor);
        if let Some(job) = job {
            if let Some((row, entry)) = self.delegate_entry(job, &item, || {
                DelegateEntry::Tool(ToolCall::new(&item, name, summary.clone(), input.clone()))
            }) {
                if let DelegateEntry::Tool(call) = entry {
                    call.summary = summary;
                    call.input = input;
                    self.remeasure(row, cx);
                }
                return;
            }
        }
        if let Some(&(row, call)) = self.by_tool.get(&item) {
            if let Content::Tools(group) = &mut self.rows[row].content {
                group.calls[call].summary = summary;
                group.calls[call].input = input;
                self.remeasure(row, cx);
            }
            return;
        }

        if is_partner(&actor) && job.is_none() {
            self.set_busy(true, cx);
            if let Some(turn) = &mut self.turn {
                turn.update_phase(|turn| {
                    turn.writing = false;
                    turn.tools.push(item.clone());
                });
            }
        }
        let call = ToolCall::new(&item, name, summary, input);
        let (row, joined) = self.tool_group(actor, false);
        if let Content::Tools(group) = &mut self.rows[row].content {
            self.by_tool.insert(item, (row, group.calls.len()));
            group.calls.push(call);
        }
        self.update_tool_group(row, joined, cx);
    }

    fn tool_group(&mut self, actor: Actor, approval: bool) -> (usize, bool) {
        let placement = self.grouping.push(&actor, true);
        let joined = placement.join_tools && self.rows.last().is_some_and(|row| {
            matches!(&row.content, Content::Tools(group) if group.approvals.is_empty() != approval)
        });
        if !joined {
            self.rows.push(Row::new(actor, Content::Tools(ToolGroup::default())));
        }
        (self.rows.len() - 1, joined)
    }

    fn update_tool_group(&mut self, row: usize, joined: bool, cx: &mut Context<Workbench>) {
        if joined {
            self.remeasure(row, cx);
        } else {
            self.append_row(cx);
        }
    }

    pub(super) fn tool_result(
        &mut self,
        item: &str,
        ok: bool,
        output: String,
        cx: &mut Context<Workbench>,
    ) {
        if let Some(&row) = self.by_item.get(item) {
            if matches!(&self.rows[row].content, Content::Delegate(_)) {
                if let Ok(report) = serde_json::from_str::<DelegateReport>(&output) {
                    self.finish_delegate(item, ok, report, cx);
                }
                return;
            }
        }
        if let Some(&(row, progress)) = self.delegate_items.get(item) {
            if let Content::Delegate(block) = &mut self.rows[row].content {
                if let DelegateEntry::Tool(call) = &mut block.progress[progress] {
                    call.result = Some(ToolResult { ok, output });
                    self.remeasure(row, cx);
                }
                return;
            }
        }
        if let Some(turn) = &mut self.turn {
            turn.update_phase(|turn| turn.tools.retain(|id| id != item));
        }
        if let Some(&(row, call)) = self.by_tool.get(item) {
            if let Content::Tools(group) = &mut self.rows[row].content {
                group.calls[call].result = Some(ToolResult { ok, output });
                self.remeasure(row, cx);
            }
        }
    }

    fn finish_delegate(&mut self, item: &str, ok: bool, report: DelegateReport, cx: &mut Context<Workbench>) {
        let Some(&row) = self.by_item.get(item) else { return };
        let Content::Delegate(block) = &mut self.rows[row].content else { return };
        block.report = Some((ok, report));
        block.text_open = None;
        self.pending_delegates.remove(item);
        if self.worker_target.as_deref() == Some(item) {
            self.worker_target = None;
        }
        if let Some(reply) = self.latest_reply() {
            self.remeasure(reply, cx);
        }
        self.remeasure(row, cx);
    }

    fn reconcile_activity(&mut self, running: bool, workers: Option<&BTreeSet<String>>, cx: &mut Context<Workbench>) {
        self.set_busy(running, cx);
        let stale: Vec<_> = self.pending_delegates.iter()
            .filter(|id| workers.is_none_or(|workers| !workers.contains(*id)))
            .cloned().collect();
        for id in stale {
            self.finish_delegate(&id, false, DelegateReport::interrupted(0), cx);
        }
    }

    pub(super) fn set_message_details(&mut self, item: &str, details: fuzukue_proto::RunDetails, cx: &mut Context<Workbench>) {
        if let Some(&index) = self.by_item.get(item) {
            if let Content::Message(message) = &mut self.rows[index].content {
                message.details = Some(details);
                self.remeasure(index, cx);
            }
        }
    }

    pub(super) fn end_turn(&mut self, actor: &Actor, cx: &mut Context<Workbench>) {
        self.grouping.end_turn(actor);
        if is_partner(actor) {
            self.delegate_anchor = None;
        }
        self.finish_turn_activity(actor, cx);
    }

    pub(super) fn finish_turn_activity(&mut self, actor: &Actor, cx: &mut Context<Workbench>) {
        if matches!(
            actor,
            Actor::Claude
                | Actor::Codex {
                    parent_tool_use: None,
                    ..
                }
        ) {
            self.clear_composer_notice(cx);
            self.set_busy(false, cx);
        }
    }

    fn clear_composer_notice(&mut self, cx: &mut Context<Workbench>) {
        if self.composer_notice.take().is_some() {
            cx.notify();
        }
    }

    fn set_busy(&mut self, running: bool, cx: &mut Context<Workbench>) {
        if self.busy == running {
            return;
        }
        self.busy = running;
        if running {
            let now = Instant::now();
            self.turn = Some(TurnStatus {
                started: now,
                phase_started: now,
                writing: false,
                consulting: None,
                tools: Vec::new(),
                pending_approvals: Vec::new(),
            });
        } else if let Some(turn) = self.turn.take() {
            for item in turn.tools {
                if let Some(&(row, _)) = self.by_tool.get(&item) {
                    if let Content::Tools(group) = &mut self.rows[row].content {
                        group.ended = true;
                        self.scroller.remeasure_items(row..row + 1);
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn set_consulting(&mut self, active: bool) {
        if let Some(turn) = &mut self.turn {
            turn.update_phase(|turn| turn.consulting = active.then(Instant::now));
        }
    }

    fn delegate_block(&self, id: &str) -> Option<&DelegateBlock> {
        match &self.rows[*self.by_item.get(id)?].content {
            Content::Delegate(block) => Some(block),
            _ => None,
        }
    }

    fn job_summary<'a>(&'a self, block: &'a DelegateBlock) -> Option<&'a JobSummary> {
        self.jobs.get(&block.id).or(block.wire_summary.as_ref())
    }

    fn pending_jobs(&self) -> impl Iterator<Item = (usize, &str, &DelegateJob)> {
        let summaries = self.jobs.iter().filter(|summary| summary.status.active())
            .map(|summary| (summary.position, summary.id.as_str(), &summary.job));
        let rows = self.pending_delegates.iter().filter_map(|item| {
            if self.jobs.received || self.jobs.get(item).is_some() {
                return None;
            }
            let row = *self.by_item.get(item)?;
            match &self.rows[row].content {
                Content::Delegate(block) => Some((row, item.as_str(), &block.job)),
                _ => None,
            }
        });
        summaries.chain(rows)
    }

    fn finished_panel_jobs(&self) -> impl Iterator<Item = (usize, &DelegateBlock)> {
        self.rows.iter().enumerate().rev().filter_map(|(row, entry)| match &entry.content {
            Content::Delegate(block) if block.report.is_some() && !block.job.dismissed_from_panel => Some((row, block)),
            _ => None,
        })
    }

    pub(super) fn active_worker(&self) -> Option<&str> {
        self.pending_jobs().filter(|(_, _, job)| job.started_at_ms.is_some())
            .min_by_key(|(position, _, _)| *position).map(|(_, _, job)| job.model.as_str())
    }

    pub(super) fn working(&self) -> bool {
        self.busy || self.active_worker().is_some()
    }

    fn has_activity(&self, waiting: Option<TimedWait>) -> bool {
        (self.turn.is_some() && self.paused.is_none()) || self.pending_jobs().next().is_some()
            || self.visible_wait(waiting).is_some() || self.paused.is_some()
    }

    fn visible_wait(&self, waiting: Option<TimedWait>) -> Option<TimedWait> {
        visible_wait(waiting, self.busy, self.pending_jobs().next().is_some())
    }

    fn turn_activity(&self) -> Option<TurnActivity<'_>> {
        let turn = self.turn.as_ref().filter(|_| self.paused.is_none())?;
        let tool = if let Some(started) = turn.consulting {
            Some(ToolActivity::Consulting { elapsed: started.elapsed() })
        } else if turn.pending_approvals.is_empty() {
            let mut running = turn.tools.iter().filter_map(|item| {
                let &(row, index) = self.by_tool.get(item)?;
                match &self.rows[row].content {
                    Content::Tools(group) => group.calls.get(index).filter(|call| call.result.is_none()),
                    _ => None,
                }
            });
            running.next().map(|call| ToolActivity::Running {
                name: tool_display_name(&call.name),
                elapsed: call.started.elapsed(),
                additional: running.count(),
            })
        } else {
            // Approval requests identify the turn's actor and carry their own request ID.
            Some(ToolActivity::AwaitingApproval)
        };
        Some(TurnActivity {
            writing: turn.writing,
            phase_elapsed: turn.phase_started.elapsed(),
            elapsed: turn.started.elapsed(),
            tool,
        })
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn turn_status(&self) -> Option<String> {
        let turn = self.turn_activity()?;
        let jobs: Vec<_> = self.pending_jobs().map(|(_, _, job)| job).collect();
        Some(status_speech(&activity_summary(Some(&turn), &jobs, None, unix_millis()).0))
    }

    fn remeasure(&mut self, row: usize, cx: &mut Context<Workbench>) {
        let data = &mut self.rows[row];
        data.estimate = RowEstimate::for_content(&data.actor, &data.content);
        self.remeasure_group_near(row);
        cx.notify();
    }

    fn height_estimate(&self, row: usize) -> Pixels {
        let data = &self.rows[row];
        let style = self.transcript.current_style();
        if data.kind(style.as_ref().is_some_and(|style| style.show_worker_messages)) == RowKind::Hidden {
            return px(0.);
        }
        let (rem, code_scale) = style.map_or((px(16.), 1.0), |style| (style.rem, style.code_scale));
        let width = self.scroller.viewport_bounds().size.width;
        let width = if width > px(0.) { width } else { px(CHAT_WIDTH) };
        data.estimate.height(width, rem, code_scale)
    }

    fn remeasure_group_near(&self, row: usize) {
        self.transcript.rows.borrow_mut().take();
        let height = self.height_estimate(row);
        if self.scroller.viewport_bounds().size.height == px(0.) {
            self.scroller.remeasure_items_with_height_estimate(row..row + 1, row, height);
            return;
        }
        // Include adjacent speaker boundaries. A visibility change can join groups
        // or move the transcript's leading or trailing padding to a different row.
        let before = self.rows[..row].iter().rposition(|row| row.kind(false) == RowKind::SpeakerBoundary);
        let after = self.rows[row + 1..].iter().position(|row| row.kind(false) == RowKind::SpeakerBoundary);
        let range = before.unwrap_or(0)..after.map_or(self.rows.len(), |i| row + 2 + i);
        self.scroller.remeasure_items_with_height_estimate(range, row, height);
    }

    fn insert_row_geometry(&self, row: usize) {
        self.scroller.splice_with_height_estimates(row..row, [self.height_estimate(row)]);
    }

    fn append_row(&self, cx: &mut Context<Workbench>) {
        let index = self.rows.len() - 1;
        self.insert_row_geometry(index);
        self.remeasure_group_near(index);
        cx.notify();
    }

    pub(super) fn remeasure_github(&self, event: &str) {
        for (index, row) in self.rows.iter().enumerate() {
            if matches!(&row.content, Content::GitHub { events, .. } if events.iter().any(|item| item.id == event)) {
                self.scroller.remeasure_items(index..index + 1);
            }
        }
    }

    pub(super) fn recent_github_ids(&self, count: usize) -> impl Iterator<Item = &str> {
        self.rows.iter().rev().filter_map(|row| match &row.content {
            Content::GitHub { events, .. } => Some(events.iter().rev()),
            _ => None,
        }).flatten().take(count).map(|event| event.id.as_str())
    }

    pub(super) fn remeasure_history<'a>(&self, items: impl Iterator<Item = &'a str>) {
        for row in items.filter_map(|item| self.by_item.get(item).copied()) {
            self.remeasure_group_near(row);
        }
    }

    pub(super) fn scroll_to_end(&self) {
        self.scroller.set_follow_mode(FollowMode::Tail);
        self.scroller.scroll_to_end();
    }

    pub(super) fn reveal_latest_conversation(&self) {
        if !self.scroller.is_following_tail() {
            return;
        }
        let latest = self.rows.iter().rposition(|row| {
            matches!(row.content, Content::Message(_))
                && (row.actor == Actor::User || is_partner(&row.actor))
        });
        if let Some(latest) = latest.filter(|&row| self.rows.len().saturating_sub(row) > 4) {
            self.scroller.scroll_to(gpui_kit::ListOffset {
                item_ix: latest.saturating_sub(1), offset_in_item: px(0.),
            });
        }
    }

    pub(super) fn reveal_message(&self, item: &str) {
        if let Some(&index) = self.by_item.get(item) {
            self.scroller.scroll_to(gpui_kit::ListOffset { item_ix: index, offset_in_item: px(0.) });
        }
    }

    pub(super) fn reveal_delegate(&self, job: &str) {
        if let Some(&index) = self.by_item.get(job) {
            if matches!(self.rows[index].content, Content::Delegate(_)) {
                self.scroller.scroll_to(gpui_kit::ListOffset { item_ix: index, offset_in_item: px(0.) });
            }
        }
    }

    pub(super) fn buffer_job_event(&mut self, seq: u64, event: &ServerEvent) -> bool {
        let Some((_, job)) = event.worker_child() else { return false };
        let Some(&row) = self.by_item.get(job) else { return false };
        let Content::Delegate(block) = &mut self.rows[row].content else { return false };
        let Some(deferred) = &mut block.deferred else { return false };
        if deferred.request.is_some() {
            deferred.pending.push((seq, event.clone()));
            return true;
        }
        !deferred.open
    }

    pub(super) fn update_job_tool_count(&mut self, job: &str, tools: usize, cx: &mut Context<Workbench>) {
        let Some(&row) = self.by_item.get(job) else { return };
        let Content::Delegate(block) = &mut self.rows[row].content else { return };
        let Some(deferred) = &mut block.deferred else { return };
        deferred.tools = tools;
        self.remeasure(row, cx);
    }

    fn toggle(&mut self, row: usize, disclosure: Disclosure, show_worker_messages: bool, cx: &mut Context<Workbench>) {
        if matches!(disclosure, Disclosure::Group | Disclosure::DecidedApprovals) {
            // Act on the runs that were drawn.
            let drawn = self.transcript.rows.borrow().clone();
            let computed;
            let runs = match &drawn {
                Some(drawn) => &drawn.tool_runs,
                None => {
                    computed = ToolRuns::new(self, |row| row.kind(show_worker_messages) != RowKind::Hidden);
                    &computed
                }
            };
            if let Some(run) = runs.at(row) {
                let decided = matches!(disclosure, Disclosure::DecidedApprovals);
                let open = !if decided { run.decided_open(&self.rows) } else { run.open(&self.rows) };
                for &index in &run.rows {
                    if let Content::Tools(group) = &mut self.rows[index].content {
                        if decided { group.decided_open = open } else { group.open = open }
                    }
                }
                self.remeasure(row, cx);
                return;
            }
        }
        let Some(row_data) = self.rows.get_mut(row) else {
            return;
        };
        let open = match (&mut row_data.content, disclosure) {
            (Content::GitHub { open, .. }, Disclosure::GitHub) => open,
            (Content::InternalNote { open, .. }, Disclosure::InternalNote) => open,
            (Content::Message(message), Disclosure::Json) => &mut message.json_open,
            (Content::Message(message), Disclosure::WorkerText) => message.worker_text_open
                .get_or_insert(show_worker_messages),
            (Content::Delegate(block), Disclosure::WorkerText) => block.text_open
                .get_or_insert(show_worker_messages),
            (Content::Delegate(block), Disclosure::DelegateTools) => &mut block.tools_open,
            (Content::Delegate(block), Disclosure::DelegateEarlierTools) => &mut block.earlier_tools_open,
            (Content::Tools(group), Disclosure::Group) => &mut group.open,
            (Content::Tools(group), Disclosure::DecidedApprovals) => &mut group.decided_open,
            _ => return,
        };
        *open = !*open;
        self.remeasure(row, cx);
    }

    pub(super) fn reset_worker_disclosures(&mut self) {
        for row in &mut self.rows {
            match &mut row.content {
                Content::InternalNote { open, .. } => *open = false,
                Content::Delegate(block) => block.text_open = None,
                Content::Message(message) => message.worker_text_open = None,
                _ => {}
            }
        }
        self.transcript.rows.borrow_mut().take();
        self.scroller.remeasure();
    }

    pub(super) fn clear_transcript_hover(&mut self) {
        self.hovered_message_row = None;
        self.quote_position = None;
    }

    pub(super) fn activate_transcript(&self) {
        self.transcript.activate();
    }

    pub(super) fn discard_transcript_layout(&self) {
        self.transcript.invalidate_markdown();
        self.transcript.rows.borrow_mut().take();
        let style = self.transcript.current_style();
        self.scroller.discard_layout(self.rows.iter().map(|row| {
            if row.kind(style.as_ref().is_some_and(|style| style.show_worker_messages)) == RowKind::Hidden {
                px(0.)
            } else {
                row.estimate.height(px(CHAT_WIDTH), px(16.), 1.0)
            }
        }));
        self.scroll_to_end();
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn transcript_parse_requests(&self) -> usize {
        self.transcript.parse_requests()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as u64
}

fn is_partner(actor: &Actor) -> bool {
    matches!(actor, Actor::Claude | Actor::Codex { parent_tool_use: None, .. })
}

fn is_assistant(actor: &Actor) -> bool {
    matches!(actor, Actor::Claude | Actor::Codex { .. })
}

fn message_has_no_visible_text(text: &str, extensions: &MarkdownExtensions) -> bool {
    fn block_is_empty(block: &BlockNode) -> bool {
        match block {
            BlockNode::Root { children, .. } => children.iter().all(block_is_empty),
            BlockNode::Paragraph(paragraph) | BlockNode::Heading { children: paragraph, .. } => {
                paragraph.children().iter().all(|run| {
                    run.text().trim().is_empty() && run.as_image().is_none() && run.as_custom().is_none()
                })
            }
            BlockNode::Break { .. } | BlockNode::Definition { .. } | BlockNode::Unknown => true,
            _ => false,
        }
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Only constructs which can disappear from the document need a parse
    // during replay. Ordinary prose, fenced code and lists wait for layout.
    let may_be_empty = trimmed.starts_with(['<', '[', '&'])
        || trimmed.chars().all(|ch| ch.is_whitespace() || matches!(ch, '*' | '-' | '_' | '#'));
    if !may_be_empty {
        return false;
    }
    // Use TextView's parser so literal tags in code and visible HTML contents still count.
    parse_markdown(&fuzukue_view::link_card_refs(text), extensions.clone().into())
        .is_ok_and(|blocks| blocks.iter().all(block_is_empty))
}

fn delegate_job<'a>(item: &'a str, actor: &'a Actor) -> Option<&'a str> {
    match actor {
        Actor::Codex { parent_tool_use: Some(job), .. } if job.starts_with(DELEGATE_JOB) => Some(job),
        _ => {
            let (id, _) = item.strip_prefix(DELEGATE_JOB)?.split_once(':')?;
            item.get(..DELEGATE_JOB.len() + id.len())
        }
    }
}

fn is_worker_message(actor: &Actor, item: Option<&str>) -> bool {
    matches!(actor, Actor::Codex { parent_tool_use: Some(_), .. })
        || item.is_some_and(|item| item.starts_with(fuzukue_proto::WORKSHOP_REPORT)
            || delegate_job(item, actor).is_some())
}

impl ToolCall {
    fn new(item: &str, name: &str, summary: String, input: Value) -> Self {
        Self {
            item: item.into(), name: name.into(), summary, input, started: Instant::now(),
            input_open: false, result: None, result_open: false, detail: ToolDetail::default(),
        }
    }

    fn pretty_input(&self) -> String {
        let input = self.detail.input.as_ref().unwrap_or(&self.input);
        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
    }

    fn batch_failure_summary(&self) -> Option<&str> {
        paper_batch_failure_summary(&self.name, &self.result.as_ref()?.output)
    }
}

fn tool_display_name(name: &str) -> &str {
    if name == "Codex" { tr!("chat-worker") } else { name }
}

fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        tr!("chat-elapsed-seconds", seconds = seconds.to_string())
    } else {
        tr!("chat-elapsed-minutes-seconds", minutes = (seconds / 60).to_string(), seconds = (seconds % 60).to_string())
    }
}

pub(super) fn activity_spinner(id: impl Into<ElementId>, color: gpui_kit::Hsla) -> gpui_kit::Stateful<gpui_kit::Div> {
    const BOX_SIZE: Pixels = px(14.);
    v_flex().id(id).flex_none().size(BOX_SIZE).items_center().justify_center()
        .pt(STATUS_OPTICAL_OFFSET * 2.)
        .child(Icon::new(IconName::Loader).size(px(12.)).text_color(color)
            .with_animation(
                "rotate",
                Animation::new(Duration::from_millis(800)).repeat(),
                // GPUI rotates SVGs around the center of their bounds.
                |icon, phase| icon.transform(gpui_kit::Transformation::rotate(gpui_kit::percentage(phase))),
            ))
}

pub(super) fn tier_label(tier: fuzukue_proto::Tier) -> &'static str {
    match tier {
        fuzukue_proto::Tier::Light => tr!("tiers-light"),
        fuzukue_proto::Tier::Standard => tr!("tiers-standard"),
        fuzukue_proto::Tier::Heavy => tr!("tiers-heavy"),
    }
}

pub(super) fn job_details(id: String, lines: Vec<String>) -> impl IntoElement {
    Popover::new(format!("{id}-popover"))
        .anchor(Anchor::TopRight)
        .trigger(message_action(id.clone(), Symbol::Info, tr!("tiers-details")))
        .content(move |_, _, cx| {
            let color = cx.theme().muted_foreground;
            v_flex().id(SharedString::from(format!("{id}-content")))
                .p_2().gap_1().min_w(px(220.)).max_w(px(460.))
                .text_sm().whitespace_normal().children(lines.iter().map(|line| status_row(line, color)))
        })
}

pub(super) fn selection_details(selection: &AgentSelection) -> String {
    tr!("tiers-selection", backend = selection.backend.label(), model = selection.model.as_deref().unwrap_or(tr!("chat-cli-default")),
        effort = effort_label(selection.effort.as_deref()))
}

pub(super) fn run_details(details: &fuzukue_proto::RunDetails) -> Vec<String> {
    let mut lines = vec![selection_details(&details.selection)];
    if let Some(tokens) = details.tokens {
        lines.push(tr!("agents-tokens", tokens = tokens.to_string()));
    }
    if let Some(cost) = details.cost_usd {
        lines.push(tr!("tiers-cost", cost = format!("${cost:.4}")));
    }
    if let Some(duration) = details.duration_ms {
        lines.push(tr!("tiers-duration", duration = elapsed_label(Duration::from_millis(duration))));
    }
    lines
}

pub(super) fn attempt_details(attempt: &fuzukue_proto::TierAttempt) -> String {
    let tokens = attempt.tokens.map_or_else(|| "—".into(), |tokens| tokens.to_string());
    let cost = attempt.cost_usd.map_or_else(|| "—".into(), |cost| format!("${cost:.4}"));
    let outcome = match attempt.outcome {
        fuzukue_proto::AgentOutcome::Ok => tr!("agents-succeeded"),
        fuzukue_proto::AgentOutcome::Failed => tr!("agents-failed"),
        fuzukue_proto::AgentOutcome::Cancelled => tr!("agents-interrupted"),
        fuzukue_proto::AgentOutcome::Stalled => tr!("tiers-stalled"),
    };
    tr!("tiers-attempt", tier = tier_label(attempt.tier), outcome = outcome,
        selection = selection_details(&attempt.selection), tokens = tokens, cost = cost,
        duration = elapsed_label(Duration::from_millis(attempt.duration_ms)))
}

fn delegate_details(job: &DelegateJob, report: Option<&DelegateReport>) -> Vec<String> {
    let name = job.agent.as_deref().unwrap_or(tr!("chat-worker"));
    let mut lines = vec![tr!("tiers-role", role = name)];
    lines.extend(job.title.iter().cloned());
    lines.extend(job.display.label_source.iter().filter(|text| Some(*text) != job.title.as_ref()).cloned());
    lines.push(job.selection.as_ref().map_or_else(|| job.model.clone(), selection_details));
    if let Some(decision) = &job.routing {
        lines.push(tr!("tiers-decision", tier = tier_label(decision.tier), reason = decision.reason.as_str()));
    }
    if let Some(report) = report {
        lines.push(tr!("tiers-duration", duration = elapsed_label(Duration::from_millis(report.elapsed_ms))));
        for attempt in &report.attempts {
            lines.push(attempt_details(attempt));
            if let Some(reason) = &attempt.escalation_reason {
                lines.push(tr!("tiers-escalated", reason = reason.as_str()));
            }
        }
    }
    lines
}

fn join_status_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut parts = parts.into_iter();
    let mut label = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        label.push_str(STATUS_SEPARATOR);
        label.push_str(part);
    }
    label
}

fn worker_heading(status: &str, title: Option<&str>, elapsed: Option<&str>) -> String {
    join_status_parts(std::iter::once(status)
        .chain(title.map(str::trim).filter(|title| !title.is_empty()))
        .chain(elapsed))
}

fn turn_activity_label(turn: &TurnActivity<'_>) -> (String, Duration) {
    match turn.tool {
        Some(ToolActivity::Running { name, elapsed, additional }) => {
            let mut label = tr!("chat-tool-state-running", name = name);
            if additional > 0 {
                label.push_str(&format!(" +{additional}"));
            }
            (label, elapsed)
        }
        Some(ToolActivity::AwaitingApproval) => (tr!("chat-approval-pending").into(), turn.elapsed),
        Some(ToolActivity::Consulting { elapsed }) => (tr!("chat-consulting").into(), elapsed),
        None => {
            let label = if turn.writing { tr!("chat-writing") } else { tr!("chat-thinking") };
            (label.into(), turn.elapsed)
        }
    }
}

fn visible_wait(waiting: Option<TimedWait>, turn_running: bool, has_jobs: bool) -> Option<TimedWait> {
    if waiting.is_some_and(|wait| wait.reason == ChatWaitReason::Compacting) || !turn_running && !has_jobs {
        waiting
    } else {
        None
    }
}

fn activity_time(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn goal_tail_label(status: &GoalTail) -> String {
    match status {
        GoalTail::PendingVerification { reason } => tr!("chat-goal-verification", reason = reason.as_str()),
        GoalTail::Paused => tr!("chat-goal-paused").into(),
    }
}

fn waiting_label(waiting: TimedWait, now_ms: u64) -> String {
    use fuzukue_proto::SelfUpdateStepKind as Step;
    use ChatWaitReason::*;

    let reason = match waiting.reason {
        UsagePaused { resume_at: Some(until) } if until > (now_ms / 1_000) as i64 => {
            let seconds = (until - (now_ms / 1_000) as i64) as u64;
            tr!("chat-wait-usage-resume", remaining = activity_time(seconds))
        }
        reason => match reason {
            UsagePaused { .. } => tr!("chat-wait-usage"),
            SelfUpdate { stage: Step::Check } => tr!("chat-wait-update-check"),
            SelfUpdate { stage: Step::Build } => tr!("chat-wait-update-build"),
            SelfUpdate { stage: Step::Test } => tr!("chat-wait-update-test"),
            SelfUpdate { stage: Step::Shot } => tr!("chat-wait-update-shot"),
            SelfUpdate { stage: Step::Publish } => tr!("chat-wait-update-publish"),
            SelfUpdate { stage: Step::Wait } => tr!("chat-wait-update-wait"),
            SelfUpdate { stage: Step::Swap } => tr!("chat-wait-update-swap"),
            SelfUpdateCanaryApproval => tr!("chat-wait-update-canary"),
            Worker => tr!("chat-wait-worker"),
            BackgroundTest => tr!("chat-wait-test"),
            BackgroundBuild => tr!("chat-wait-build"),
            QueuedTurn => tr!("chat-wait-queued-turn"),
            Compacting => return tr!("chat-wait-compacting").into(),
            Goal => tr!("chat-wait-goal"),
            Watch => tr!("chat-wait-watch"),
        }.to_owned(),
    };
    let elapsed = activity_time(waiting.since.elapsed().as_secs());
    worker_heading(tr!("chat-activity-waiting"), Some(&reason), Some(&elapsed))
}

fn delegate_activity_name(job: &DelegateJob) -> &str {
    job.display.label.as_deref()
        .or_else(|| job.title.as_deref().map(str::trim).filter(|title| !title.is_empty() && Some(*title) != job.display.card_id.as_deref()))
        .or_else(|| job.agent.as_deref().map(str::trim).filter(|agent| !agent.is_empty()))
        .unwrap_or(tr!("chat-worker"))
}

fn delegate_tools(block: &DelegateBlock) -> (usize, bool) {
    block.progress.iter().filter_map(|entry| match entry {
        DelegateEntry::Tool(call) => Some(call),
        _ => None,
    }).fold((0, false), |(count, failed), call| {
        (count + 1, failed || call.result.as_ref().is_some_and(|result| !result.ok))
    })
}

fn delegate_elapsed(job: &DelegateJob, now_ms: u64) -> Option<u64> {
    job.started_at_ms.map(|started| now_ms.saturating_sub(started.max(0) as u64) / 1000)
}

fn job_status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => tr!("chat-waiting"),
        JobStatus::Running => tr!("chat-working"),
        JobStatus::Succeeded => tr!("chat-done"),
        JobStatus::Failed => tr!("chat-failed"),
        JobStatus::Interrupted => tr!("agents-interrupted"),
    }
}

fn turn_activity_detail(turn: &TurnActivity<'_>) -> String {
    let (label, elapsed) = turn_activity_label(turn);
    worker_heading(&label, None, Some(&activity_time(elapsed.as_secs())))
}

fn activity_summary(
    turn: Option<&TurnActivity<'_>>,
    jobs: &[&DelegateJob],
    waiting: Option<TimedWait>,
    now_ms: u64,
) -> (String, Option<String>) {
    let active_turn = turn.filter(|turn| !matches!(turn.tool, Some(ToolActivity::AwaitingApproval)));
    let (label, elapsed) = if let Some(turn) = active_turn {
        (turn_activity_label(turn).0, Some(turn.phase_elapsed.as_secs()))
    } else if let Some(seconds) = jobs.iter().filter_map(|job| delegate_elapsed(job, now_ms)).max() {
        (tr!("chat-status-working").to_owned(), Some(seconds))
    } else {
        let elapsed = turn.map(|turn| turn.phase_elapsed.as_secs())
            .or_else(|| waiting.map(|wait| wait.since.elapsed().as_secs()));
        (tr!("chat-status-waiting").to_owned(), elapsed)
    };
    let elapsed = elapsed.map(activity_time);
    let (label, metadata) = match (active_turn, jobs.len()) {
        (_, 0) => (label, elapsed),
        (Some(_), count) => {
            let workers = tr!("chat-status-workers", count = count);
            (join_status_parts([label.as_str(), workers.as_str()]), elapsed)
        }
        (None, count) => {
            let jobs = tr!("chat-status-count", count = count);
            let metadata = join_status_parts(std::iter::once(jobs.as_str()).chain(elapsed.as_deref()));
            (label, Some(metadata))
        }
    };
    (label, metadata)
}

fn chat_distance_to_bottom(scroller: &ListState) -> Pixels {
    scroller.max_offset_for_scrollbar().y + scroller.scroll_px_offset_for_scrollbar().y
}

fn chat_at_bottom(scroller: &ListState) -> bool {
    // GPUI resumes following when the scroll position is within one pixel of the end.
    scroller.is_following_tail() || chat_distance_to_bottom(scroller) <= px(1.)
}

fn is_command_echo(actor: &Actor, item: Option<&str>) -> bool {
    *actor == Actor::User && item.is_some_and(|item| item.starts_with(fuzukue_proto::slash::ECHO))
}

fn chat_transcript_id(session: &str) -> ElementId {
    ElementId::from(SharedString::from(format!("chat-{session}")))
}

impl Workbench {
    fn toggle_tool_payload(&mut self, session: &str, row: usize, part: Disclosure, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        let call = match (chat.rows.get_mut(row).map(|row| &mut row.content), part) {
            (Some(Content::Tools(group)), Disclosure::Input(i) | Disclosure::Result(i)) => group.calls.get_mut(i),
            (Some(Content::Delegate(job)), Disclosure::DelegateTool(i)) => match job.progress.get_mut(i) {
                Some(DelegateEntry::Tool(call)) => Some(call),
                _ => None,
            },
            _ => None,
        };
        let Some(call) = call else { return };
        let open = if matches!(part, Disclosure::Result(_)) { &mut call.result_open } else { &mut call.input_open };
        *open = !*open;
        let wants_output = matches!(part, Disclosure::Result(_) | Disclosure::DelegateTool(_)) && call.result.is_some();
        if !call.input_open && !call.result_open {
            call.detail = ToolDetail::default();
        } else if self.protocol >= fuzukue_proto::TOOL_DETAILS_PROTOCOL && self.transcripts.scoped
            && call.detail.request.is_none()
            && (call.detail.input.is_none() || wants_output && call.detail.output.is_none())
        {
            let request = chat_history::next_request();
            if self.out.send(ClientMsg::GetThreadToolDetails {
                session: session.into(), item: call.item.clone(), request,
            }).is_ok() {
                call.detail.request = Some(request);
                call.detail.unavailable = false;
            }
        }
        chat.remeasure(row, cx);
    }

    pub(super) fn finish_tool_payload(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        let ServerEvent::ThreadToolDetails { session, item, request, input, output, unavailable } = event else { return };
        let Some(chat) = self.chats.get_mut(&session) else { return };
        let (row, call) = if let Some(&(row, index)) = chat.by_tool.get(&item) {
            let Content::Tools(group) = &mut chat.rows[row].content else { return };
            (row, &mut group.calls[index])
        } else if let Some(&(row, index)) = chat.delegate_items.get(&item) {
            let Content::Delegate(job) = &mut chat.rows[row].content else { return };
            let DelegateEntry::Tool(call) = &mut job.progress[index] else { return };
            (row, call)
        } else { return };
        if call.detail.request == Some(request) {
            call.detail = ToolDetail { request: None, input, output, unavailable };
            chat.remeasure(row, cx);
        }
    }

    fn toggle_thread_job(&mut self, session: &str, row: usize, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        let Some(Row { content: Content::Delegate(block), .. }) = chat.rows.get_mut(row) else { return };
        let Some(deferred) = &mut block.deferred else { return };
        let request = (!deferred.open).then(chat_history::next_request);
        if self.out.send(ClientMsg::GetThreadJob {
            session: session.into(), job: block.id.clone(), request, before: None,
        }).is_err() {
            return;
        }
        deferred.open = request.is_some();
        deferred.request = request;
        deferred.pending.clear();
        if !deferred.open {
            deferred.tools = deferred.tools.max(block.progress.iter().filter(|entry| matches!(entry, DelegateEntry::Tool(_))).count());
            block.progress.clear();
            block.summary = None;
            chat.delegate_items.retain(|_, (owner, _)| *owner != row);
            if let Some((_, report)) = &mut block.report {
                report.text.clear();
                report.changes.clear();
                report.attempts.clear();
                report.files = None;
            }
        }
        chat.remeasure(row, cx);
    }

    fn request_older_job_details(&mut self, session: &str, row: usize) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        let Some(Row { content: Content::Delegate(block), .. }) = chat.rows.get_mut(row) else { return };
        let Some(deferred) = &mut block.deferred else { return };
        let Some(before) = deferred.next_before.filter(|_| deferred.open && deferred.request.is_none()) else { return };
        let request = chat_history::next_request();
        if self.out.send(ClientMsg::GetThreadJob {
            session: session.into(), job: block.id.clone(), request: Some(request), before: Some(before),
        }).is_ok() {
            deferred.request = Some(request);
        }
    }

    pub(crate) fn finish_thread_job(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        let ServerEvent::ThreadJobDetails { session, job, request, through, before, next_before, events, error } = event
            else { unreachable!() };
        let Some(chat) = self.chats.get_mut(&session) else { return };
        let Some(&row) = chat.by_item.get(&job) else { return };
        let Content::Delegate(block) = &mut chat.rows[row].content else { return };
        let Some(deferred) = &mut block.deferred else { return };
        if deferred.request != Some(request) {
            return;
        }
        deferred.request = None;
        if error.is_none() || !events.is_empty() {
            deferred.next_before = next_before;
        }
        let buffered = std::mem::take(&mut deferred.pending);
        if let Some(message) = error.as_ref() {
            chat.push_error(message.clone(), cx);
            if events.is_empty() {
                chat.replay_updates(buffered.into_iter().map(|(_, event)| event), cx);
                return;
            }
        }
        if before.is_some() {
            chat.prepend_job_details(&job, events, cx);
        } else {
            chat.replay_updates(events.into_iter(), cx);
        }
        chat.replay_updates(buffered.into_iter().filter_map(|(seq, event)| {
            (before.is_some() || seq > through).then_some(event)
        }), cx);
        let Content::Delegate(block) = &mut chat.rows[row].content else { return };
        block.text_open = Some(true);
        block.tools_open = true;
        chat.remeasure(row, cx);
    }

    pub(super) fn has_chat_quote(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        TextSelection::quote_bounds(&chat_transcript_id(&self.active), window, cx).is_some()
    }

    pub(super) fn dismiss_chat_quote(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.has_chat_quote(window, cx) {
            return false;
        }
        TextSelection::clear(window, cx);
        cx.notify();
        true
    }

    pub(super) fn quote_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if window.has_active_dialog(cx) || composing(&self.input, window, cx) {
            return false;
        }
        let selected = TextSelection::quote_text(&chat_transcript_id(&self.active), window, cx);
        let input = self.input.read(cx);
        let caret = input.cursor();
        let Some((edit, reply)) = quote_insertion(&input.value(), caret, &selected) else {
            return false;
        };
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.replace_range_atomically(caret..caret, &edit, window, cx);
            input.move_caret_to(caret + reply, false, cx);
        });
        TextSelection::clear(window, cx);
        cx.notify();
        true
    }

    pub(super) fn thread_navigation_key(
        &mut self,
        event: &gpui_kit::KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        let plain = modifiers.number_of_modifiers() == 0;
        let control = modifiers.control && modifiers.number_of_modifiers() == 1;
        let endpoint = match key.key.as_str() {
            "home" if plain || control => Some(false),
            "end" if plain || control => Some(true),
            "pageup" | "pagedown" if plain => None,
            _ => return false,
        };
        let in_context = |name| event.context_stack.iter().any(|context| context.contains(name));
        if window.has_active_dialog(cx)
            || self.workspace_browser_open()
            || self.palette.is_some()
            || self.app_menu.is_expanded()
            || self.history.is_open()
            || self.screenshots.preview_open()
            || (self.paper_pdf_visible() && self.paper_pdf.popup.is_some())
            || in_context("PopupMenu")
            || in_context("WorkbenchJobList")
        {
            return false;
        }
        let composer_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let thread_focused = composer_focused || in_context("ThreadConversation")
            || self.sidebar.thread_focus.get(&self.active).is_some_and(|focus| focus.is_focused(window));
        // In a split view, only route keys from the conversation side.
        if !thread_focused && (self.right != super::RightPane::None || self.settings_open
            || self.symbols.is_open() || self.thread_info_visible())
        {
            return false;
        }

        if composer_focused {
            if composing(&self.input, window, cx) {
                return false;
            }
            let text = self.input.read(cx).text();
            let input_uses_key = if control { text.lines_len() > 1 } else { text.len() > 0 };
            if input_uses_key {
                return false;
            }
        } else if in_context("Input") {
            let title = self.thread_title_edit.as_ref()
                .map(|(_, input)| input)
                .filter(|input| input.read(cx).focus_handle(cx).is_focused(window));
            let input = title.or_else(|| self.question_drafts.values()
                .find_map(|card| card.read(cx).focused_input(window, cx)));
            if input.is_none_or(|input| !control && input.read(cx).text().len() > 0) {
                return false;
            }
        }

        if let Some(latest) = endpoint {
            if self.jump_thread_history(latest, cx) {
                return true;
            }
            let chat = &self.chats[&self.active];
            if latest {
                chat.scroll_to_end();
            } else {
                chat.scroller.scroll_to(gpui_kit::ListOffset::default());
            }
        } else {
            scroll_list_by_key(&self.chats[&self.active].scroller, key, window);
        }
        self.history_scrolled(cx);
        cx.notify();
        true
    }

    fn chat_key_down(&mut self, event: &gpui_kit::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let key = &event.keystroke;
        let chat = &self.chats[&self.active];
        if self.chat_focus.is_focused(window) {
            let modifier_count = key.modifiers.number_of_modifiers();
            let plain = modifier_count == 0;
            let text = key.key_char.as_deref().filter(|text| {
                (modifier_count == u8::from(key.modifiers.shift) || event.prefer_character_input)
                    && text.chars().next().is_some_and(|c| !c.is_control())
            });
            let down_at_bottom = plain && key.key == "down" && chat.history.next_after.is_none()
                && (chat_at_bottom(&chat.scroller)
                    || chat_distance_to_bottom(&chat.scroller) <= window.line_height() + px(1.));
            if down_at_bottom || text.is_some() || (plain && matches!(key.key.as_str(), "enter" | "escape")) {
                if down_at_bottom {
                    chat.scroll_to_end();
                }
                self.input.update(cx, |input, cx| {
                    input.focus(window, cx);
                    // The native input handler is installed at paint, after this key.
                    if let Some(text) = text {
                        input.replace_text_in_range(None, text, window, cx);
                    }
                });
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        if matches!(key.key.as_str(), "up" | "down")
            && scroll_list_by_key(&chat.scroller, key, window)
        {
            self.history_scrolled(cx);
            cx.stop_propagation();
            cx.notify();
        }
    }

    pub(super) fn update_chat_macros(&mut self, threads: &[ThreadView], cx: &mut Context<Self>) {
        if !self.threads_loaded {
            // The first thread snapshot follows replay. Transcript events have no message times.
            for chat in self.chats.values_mut() {
                for row in &mut chat.rows {
                    row.at_ms = None;
                }
                chat.transcript.rows.borrow_mut().take();
                chat.scroller.remeasure_items(0..chat.rows.len());
            }
        }
        for thread in threads {
            if thread.math_macros.is_empty() && !self.chats.contains_key(&thread.id) {
                continue;
            }
            let chat = self.chat(&thread.id, cx);
            if let Some(settings) = &thread.partner_settings {
                chat.settings.clone_from(settings);
            }
            if chat.math_macros != thread.math_macros {
                chat.math_macros.clone_from(&thread.math_macros);
                chat.markdown = super::chat_components::extensions(&chat.math_macros, cx.weak_entity());
                chat.transcript.invalidate_markdown();
                chat.scroller.remeasure_items(0..chat.scroller.item_count());
            }
        }
    }

    pub(super) fn chat_markdown(&self, id: SharedString, source: String, chat: &Chat, cx: &gpui_kit::App) -> TextView {
        self.style_chat_markdown(TextView::markdown(id, source), chat, cx)
    }

    fn style_chat_markdown(&self, view: TextView, chat: &Chat, cx: &gpui_kit::App) -> TextView {
        self.chat_code_style(view, cx)
            .selection_format(SelectionFormat::PlainWithSourceSelectAll)
            .markdown_extensions(chat.markdown.clone())
            .on_link_click(self.markdown_link_handler())
    }

    fn turn_history_action(
        &self,
        session: &str,
        item: &str,
        turn: &str,
        entity: &Entity<Self>,
    ) -> Option<Button> {
        if !self.has_turn_changes(session, turn) {
            return None;
        }
        self.history_button(
            format!("turn-history-{item}"),
            fuzukue_proto::HistoryScope::Turn { thread: session.to_owned(), turn: turn.to_owned() },
            entity.downgrade(),
        ).map(|button| button.max_h(CHAT_GROUP_GAP))
    }

    pub(super) fn render_chat(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let session = self.active.clone();
        let chat = &self.chats[&session];
        let scroller = chat.scroller.clone();
        let quote_position = chat.quote_position;
        let quote_session = session.clone();
        let has_activity = chat.has_activity(self.chat_waiting.reason(&session))
            || self.chat_waiting.has_background_commands(&session);
        let thread = self.threads.iter().find(|thread| thread.id == session);
        let show_worker_messages = self.appearance.settings.show_worker_messages;
        let message_action_height = (window.rem_size() * 1.5).min(CHAT_GROUP_GAP);
        let tail_padding = chat_tail_padding(window.scale_factor());
        let style = TranscriptStyle {
            rem: window.rem_size(),
            ui_font: self.appearance.ui_font.clone(),
            code_font: self.appearance.code_font.clone(),
            code_scale: self.appearance.settings.code_font_scale(),
            math_scale: self.appearance.settings.math_scale,
            dark: cx.theme().mode.is_dark(),
            show_worker_messages,
            tail_padding,
        };
        let style_revision = chat.transcript.style(style.clone());
        let geometry = {
            let mut cached = chat.transcript.rows.borrow_mut();
            if cached.as_ref().is_none_or(|rows| rows.show_worker_messages != show_worker_messages) {
                let shown = |row: &Row| match &row.content {
                    Content::AutoAttached(route) if thread.is_none_or(|thread| !thread.auto_attached.contains(route)) => {
                        RowKind::Hidden
                    }
                    _ => row.kind(show_worker_messages),
                };
                let tool_runs = ToolRuns::new(chat, |row| shown(row) != RowKind::Hidden);
                let kinds: Vec<_> = chat.rows.iter().enumerate().map(|(index, row)| {
                    if tool_runs.hides(index) { RowKind::Hidden } else { shown(row) }
                }).collect();
                let latest_message = chat.rows.iter().zip(&kinds).rposition(|(row, &kind)| {
                    matches!(row.content, Content::Message(..)) && match kind {
                        RowKind::AssistantText => true,
                        RowKind::SpeakerBoundary => row.actor == Actor::User,
                        _ => false,
                    }
                });
                *cached = Some(Rc::new(TranscriptRows {
                    estimates: chat.rows.iter().enumerate().map(|(index, row)| {
                        tool_runs.at(index).filter(|run| run.root() == index && run.rows.len() + run.calls.len() > 1)
                            .map_or(row.estimate, |run| {
                                let open_calls = if run.open(&chat.rows) {
                                    run.calls.len() + usize::from(run.unlinked > 0)
                                } else { 0 };
                                let open_decided = if run.decided_open(&chat.rows) { run.decided.len() } else { 0 };
                                let summaries = usize::from(!run.calls.is_empty()) + usize::from(!run.decided.is_empty());
                                RowEstimate::Disclosure(summaries + open_calls + open_decided)
                            })
                    }).collect(),
                    tool_runs,
                    first_visible: kinds.iter().position(|&kind| kind != RowKind::Hidden),
                    last_visible: kinds.iter().rposition(|&kind| kind != RowKind::Hidden),
                    groups: assistant_groups(kinds.iter().zip(&chat.rows).map(|(&kind, row)| (kind, row.at_ms))),
                    kinds,
                    latest_message,
                    latest_reply: chat.latest_reply(),
                    show_worker_messages,
                }));
            }
            cached.as_ref().unwrap().clone()
        };
        let hidden_actions_row = hidden_message_actions_row(has_activity, geometry.latest_message, &geometry.groups);
        let estimates = geometry.clone();
        let chat_id = chat_transcript_id(&session);
        let quote_region = chat_id.clone();
        // The list holds a mutable borrow of its layout state while rendering items.
        let hovered_row = chat.hovered_message_row.and_then(|(index, id)| {
            (chat.rows.get(index)?.id == id).then_some(index)
        }).and_then(|index| {
            let bounds = scroller.bounds_for_item(index)?.intersect(&scroller.viewport_bounds());
            Some((index, bounds))
        });
        let rows = list_with_optional_items(
            scroller.clone(),
            move |ix, window, cx| {
                let TranscriptRows { kinds, groups, first_visible, last_visible, .. } = &*geometry;
                let (first_visible, last_visible) = (*first_visible, *last_visible);
                let latest_message = geometry.latest_message;
                if kinds[ix] == RowKind::Hidden {
                    return None;
                }
                let theme = cx.theme();
                let this = entity.read(cx);
                let chat = &this.chats[&session];
                // A list item is measured on its own. Its full-width root gives
                // the chat column room to resolve its automatic side margins.
                let row = &chat.rows[ix];
                let group = groups.get(groups.partition_point(|group| group.last < ix))
                    .filter(|group| group.first <= ix);
                let pointer = window.mouse_position();
                let covered = this.screenshots.preview_open();
                let keyboard_focus = !covered && window.last_input_was_keyboard();
                let group_hovered = !covered && hovered_row.is_some_and(|(hovered, bounds)| {
                    bounds.contains(&pointer)
                        && group.map_or(hovered == ix, |group| (group.first..=group.last).contains(&hovered))
                });
                let hovered_chat = entity.downgrade();
                let hovered_session = session.clone();
                let row_id = row.id;
                let hover_group: SharedString = format!("chat-message-row-{row_id}").into();
                let list_row = div().id(("chat-row", row_id)).w_full().group(hover_group.clone())
                    .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
                    .on_hover(move |&hovered, _, cx| {
                        let _ = hovered_chat.update(cx, |this, cx| {
                            let Some(chat) = this.chats.get_mut(&hovered_session) else { return };
                            if hovered && chat.rows.get(ix).is_some_and(|row| row.id == row_id) {
                                chat.hovered_message_row = Some((ix, row_id));
                            } else if chat.hovered_message_row.is_some_and(|(_, id)| id == row_id) {
                                chat.hovered_message_row = None;
                            }
                            cx.notify();
                        });
                    });
                let markdown = |part, text: &str| {
                    let entity = entity.clone();
                    let session = session.clone();
                    div().w_full().min_w_0().text_base().line_height(UI_LINE_HEIGHT)
                        .child(chat.transcript.markdown(
                            row.id, part, text, chat.markdown.clone(), move |view, cx| {
                                let this = entity.read(cx);
                                this.style_chat_markdown(view, &this.chats[&session], cx)
                            },
                        ))
                };
                let summary_toggle_at = |target: usize, id: SharedString, part, open, label: String, color| {
                    let entity = entity.clone();
                    let session = session.clone();
                    let press = chat.disclosure_press.clone();
                    disclosure_row(id, open, &label, color, press, cx, move |cx| {
                        entity.update(cx, |this, cx| {
                            match part {
                                Disclosure::Input(_) | Disclosure::Result(_) | Disclosure::DelegateTool(_) =>
                                    this.toggle_tool_payload(&session, target, part, cx),
                                Disclosure::DelegateDetails => this.toggle_thread_job(&session, ix, cx),
                                Disclosure::MoreDelegateDetails => this.request_older_job_details(&session, ix),
                                _ => if let Some(chat) = this.chats.get_mut(&session) {
                                    chat.toggle(target, part, this.appearance.settings.show_worker_messages, cx);
                                },
                            }
                        });
                    })
                };
                let summary_toggle = |id, part, open, label, color| {
                    summary_toggle_at(ix, id, part, open, label, color)
                };
                let disclosure = |part, open, label: String| {
                    summary_toggle(format!("chat-{session}-{ix}-{part:?}").into(),
                        part, open, label, theme.muted_foreground)
                };
                let code = |part: &str, text: &str| {
                    let entity = entity.clone();
                    chat.transcript.markdown(
                        row.id,
                        MarkdownPart::Code(part.into()),
                        text,
                        MarkdownExtensions::default(),
                        move |view, cx| entity.read(cx).chat_code_style(view.text_xs(), cx),
                    )
                };
                let user = matches!(row.actor, Actor::User);
                let mut body = v_flex().min_w_0().gap_2();
                match &row.content {
                    Content::Question { card, .. } => {
                        // An open question is answered in the dock. The answer stays here.
                        body = if card.read(cx).is_resolved() {
                            body.child(card.clone())
                        } else {
                            body.child(div().id(SharedString::from(format!("question-in-dock-{ix}")))
                                .w_full().min_w_0().text_sm().text_color(theme.muted_foreground).truncate()
                                .child(tr!("chat-question-in-dock")))
                        };
                    }
                    Content::GitHubConnect { attempt, state } => {
                        body = body.child(crate::github_report::render_github_connect_card(
                            &format!("github-chat-{attempt}"), state, true, cx,
                        ));
                    }
                    Content::GitHub { item, events, open } => {
                        if events.len() > 3 {
                            body = body.child(disclosure(Disclosure::GitHub, *open, tr!("github-batch", count = events.len())));
                        }
                        if *open {
                            body = body.children(events.iter().map(|event| this.github_card(&session, item, event, &entity, cx)));
                        }
                    }
                    Content::InternalNote { kind, text, open } => {
                        body = body.child(summary_toggle(
                            format!("chat-internal-note-{ix}").into(), Disclosure::InternalNote,
                            *open, kind.label().into(), theme.muted_foreground,
                        )).children(open.then(|| div()
                            .id(SharedString::from(format!("chat-internal-note-text-{ix}")))
                            .pl_3().text_sm().text_color(theme.muted_foreground)
                            .whitespace_normal().child(text.clone())));
                    }
                    Content::Notice { text, .. } | Content::PlanNotice { text, .. } => {
                        let divider = matches!(row.content, Content::Notice { divider: true, .. });
                        body = body.child(v_flex()
                            .id(format!("chat-local-notice-{ix}"))
                            .min_w_0().gap_0p5()
                            .when(divider, |notice| notice.border_t_1().border_color(theme.border).pt_2())
                            .text_xs().text_color(theme.muted_foreground)
                            .children(text.lines().map(|line| div().child(line.to_owned()))));
                    }
                    Content::Message(message) => {
                        let worker_message = is_worker_message(&row.actor, message.item.as_deref());
                        let text_open = message.worker_text_open.unwrap_or(show_worker_messages);
                        if worker_message {
                            body = body.child(summary_toggle(
                                format!("chat-worker-text-{ix}").into(), Disclosure::WorkerText,
                                text_open, tr!("chat-worker-messages").into(), theme.muted_foreground,
                            ));
                        }
                        body = body.children(message.to_worker.then(|| {
                            div().text_xs().text_color(theme.muted_foreground).child(tr!("chat-to-worker"))
                        }));
                        body = if worker_message && !text_open {
                            body
                        } else if is_command_echo(&row.actor, message.item.as_deref()) {
                            body.child(div().id(format!("chat-command-echo-{ix}"))
                                .font(this.appearance.code_font.clone())
                                .whitespace_normal().child(message.text.clone()))
                        } else if !message.markdown {
                            body.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(message.text.clone()),
                            )
                        } else if matches!(row.actor, Actor::Claude | Actor::Codex { .. })
                            && is_json_document(&message.text)
                        {
                            body.child(disclosure(
                                Disclosure::Json,
                                message.json_open,
                                "JSON".into(),
                            ))
                            .children(message.json_open.then(|| code("json", &message.text)))
                        } else if !message.text.trim().is_empty() {
                            body.child(markdown(MarkdownPart::Message, &message.text))
                        } else {
                            body
                        };
                        if !message.attachments.is_empty() {
                            body = body.child(this.message_attachments(chat, ix.to_string(), &message.attachments));
                        }
                    }
                    Content::AutoAttached(route) => {
                        let kind = if route.kind == "research" { tr!("chat-research") } else { tr!("chat-paper") };
                        let deck = route.deck.clone();
                        let id = session.clone();
                        let out = this.out.clone();
                        body = body.child(h_flex().gap_1().items_center().text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().min_w_0().truncate().child(tr!("chat-attached-deck", kind = kind.to_string(), title = route.title.to_string())))
                            .child(inline_button(SharedString::from(format!("undo-auto-attachment-{deck}")))
                                .flex_none().label(tr!("chat-undo"))
                                .on_click(move |_, _, _| {
                                    let _ = out.send(ClientMsg::DetachThread { id: id.clone(), deck: deck.clone() });
                                })));
                    }
                    Content::CommentApplication(application) => {
                        let thread = application.thread.clone();
                        let (label, detail) = match &application.state {
                            CommentApplicationState::Running => (
                                tr!("chat-applying-comments", count = application.count),
                                String::new(),
                            ),
                            CommentApplicationState::Applied { cards } => {
                                let cards = if cards.is_empty() { tr!("chat-none").into() } else { cards.join("、") };
                                (tr!("chat-applied-cards-changed", cards = cards.to_string()), String::new())
                            }
                            CommentApplicationState::Failed { reason } => (tr!("chat-failed").into(), format!("
{reason}")),
                        };
                        let title = this.threads.iter().find(|t| t.id == thread)
                            .map_or(&application.title, |t| &t.title);
                        let text = format!("{label} → {title}");
                        let button = Button::new(SharedString::from(format!(
                            "open-comment-thread-{ix}"
                        )))
                            .text()
                            .xsmall()
                            .min_w_0()
                            .w_full()
                            .h_auto()
                            .text_color(theme.link)
                            .accessibility_label(text.clone())
                            .child(div().w_full().min_w_0().whitespace_normal().child(text.clone()))
                            .tooltip(format!("{text}{detail}"));
                        let entity = entity.clone();
                        body = body.child(button.on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.select_thread(thread.clone(), cx));
                        }));
                    }
                    Content::Delegate(block) => {
                        let summary = chat.job_summary(block);
                        let job = summary.map_or(&block.job, |summary| &summary.job);
                        let status = summary.map(|summary| summary.status);
                        let (status_label, elapsed) = if let Some(summary) = summary {
                            (job_status_label(summary.status), summary.elapsed_at(unix_millis()))
                        } else { match &block.report {
                            Some((ok, report)) => (
                                if report.interrupted { tr!("agents-interrupted") }
                                else if *ok { tr!("chat-done") } else { tr!("chat-failed") },
                                (!report.interrupted).then_some(report.elapsed_ms),
                            ),
                            None => match block.job.started_at_ms {
                                Some(started) => (tr!("chat-working"), Some(unix_millis().saturating_sub(started.max(0) as u64))),
                                None => (tr!("chat-waiting"), None),
                            },
                        }};
                        let elapsed = elapsed.map(|ms| elapsed_label(Duration::from_millis(ms)));
                        let finished = status.map_or(block.report.is_some(), |status| !status.active());
                        let label = if finished {
                            worker_heading(status_label, Some(delegate_activity_name(job)), elapsed.as_deref())
                        } else {
                            delegate_activity_name(job).to_owned()
                        };
                        if let Some(deferred) = &block.deferred {
                            let calls = tr!("chat-tool-calls", calls = summary.map_or(deferred.tools, |summary| summary.tools));
                            let label = [delegate_activity_name(job), status_label, &job.model, elapsed.as_deref().unwrap_or(""), &calls]
                                .into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(STATUS_SEPARATOR);
                            body = body.child(summary_toggle(
                                format!("delegate-expand-{}", block.id).into(),
                                Disclosure::DelegateDetails, deferred.open, label, theme.muted_foreground,
                            ));
                            if let Some(activity) = summary.and_then(|summary| summary.last_activity.as_ref()) {
                                body = body.child(div().truncate().text_xs().text_color(theme.muted_foreground).child(activity.clone()));
                            }
                        }
                        if block.deferred.as_ref().is_none_or(|job| job.open) {
                        let failed = status.map_or(matches!(&block.report, Some((false, _))), |status|
                            matches!(status, JobStatus::Failed | JobStatus::Interrupted));
                        let header = h_flex().flex_wrap().gap_2().items_center()
                            .line_height(UI_LINE_HEIGHT)
                            .child(status_row(&label, theme.muted_foreground).text_xs().text_color(if failed {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            }))
                            .when_some(job.display.card_id.clone(), |row, id| row.child(div().text_xs().child(id)))
                            .when(this.protocol >= fuzukue_proto::JOB_LABEL_PROTOCOL, |row| {
                                row.child(this.rename_job_button(&session, &block.id, job, SharedString::from(format!("rename-{}", block.id))))
                            })
                            .child(job_details(format!("delegate-details-{}", block.id),
                                delegate_details(job, block.report.as_ref().map(|(_, report)| report))));
                        body = body.child(header.id(SharedString::from(format!("delegate-block-{}", block.id))));
                        if let Some(summary) = block.summary.as_ref().filter(|_| block.wire_summary.is_none()) {
                            body = body.child(div().id(SharedString::from(format!("delegate-summary-{}", block.id)))
                                .text_sm().text_color(theme.muted_foreground).child(summary.clone()));
                        }
                        let (calls, tool_failed) = delegate_tools(block);
                        if let Some(job) = block.deferred.as_ref().filter(|job| job.next_before.is_some()) {
                            body = body.child(summary_toggle(
                                format!("delegate-more-{}", block.id).into(), Disclosure::MoreDelegateDetails,
                                false, tr!("chat-earlier-worker-tools", calls = summary.map_or(job.tools, |summary| summary.tools).saturating_sub(calls)),
                                theme.muted_foreground,
                            ));
                        }
                        let tools_open = block.tools_open;
                        let earlier_calls = calls.saturating_sub(VISIBLE_DELEGATE_TOOLS);
                        if calls > 0 {
                            let worker = tr!("chat-worker");
                            let status = if block.job.started_at_ms.is_some() && block.report.is_none() {
                                tr!("chat-tool-running")
                            } else {
                                status_label
                            };
                            let label = tr!("chat-worker-tools", worker = worker, calls = calls, status = status);
                            let label = if tool_failed && !failed {
                                tr!("chat-tool-group-error", label = label)
                            } else {
                                label
                            };
                            let color = if tool_failed || failed { theme.danger } else { theme.muted_foreground };
                            body = body.child(summary_toggle(
                                format!("delegate-tools-{}", block.id).into(),
                                Disclosure::DelegateTools,
                                tools_open,
                                label,
                                color,
                            ));
                        }
                        if tools_open && earlier_calls > 0 {
                            body = body.child(summary_toggle(
                                format!("delegate-earlier-tools-{}", block.id).into(),
                                Disclosure::DelegateEarlierTools,
                                block.earlier_tools_open,
                                tr!("chat-earlier-worker-tools", calls = earlier_calls),
                                theme.muted_foreground,
                            ));
                        }
                        let text_open = block.text_open.unwrap_or(show_worker_messages);
                        if block.report.is_some() || block.progress.iter().any(|entry| {
                            matches!(entry, DelegateEntry::Text(text) if !text.trim().is_empty())
                        }) {
                            body = body.child(summary_toggle(
                                format!("delegate-text-{}", block.id).into(), Disclosure::WorkerText,
                                text_open, tr!("chat-worker-messages").into(), theme.muted_foreground,
                            ));
                        }
                        let mut tools_to_skip = if block.earlier_tools_open { 0 } else { earlier_calls };
                        let report_text = block.report.as_ref()
                            .map(|(_, report)| report.text.trim());
                        for (index, entry) in block.progress.iter().enumerate() {
                            match entry {
                                DelegateEntry::Text(text) if text_open && !text.trim().is_empty()
                                    && Some(text.trim()) != report_text => {
                                    body = body.child(v_flex().min_w_0().gap_1()
                                        .child(div().id(SharedString::from(format!("chat-worker-message-{ix}-{index}")))
                                            .child(markdown(MarkdownPart::WorkerText(index), text)))
                                        .child(message_actions().children(copy_message_action(&format!("{ix}-worker-{index}"), text))));
                                }
                                DelegateEntry::Tool(call) if tools_open => {
                                    if tools_to_skip > 0 {
                                        tools_to_skip -= 1;
                                        continue;
                                    }
                                    let status = match &call.result {
                                        Some(result) if result.ok => tr!("chat-done"),
                                        Some(_) => tr!("chat-failed"),
                                        None if block.report.is_some() => tr!("chat-ended"),
                                        None => tr!("chat-tool-running"),
                                    };
                                    let allowed = chat.approval_receipts.values().any(|receipt| {
                                        receipt.job() == Some(block.id.as_str())
                                            && receipt.state == ApprovalState::Granted && receipt.automatic
                                            && receipt.tool_item.as_ref().and_then(|item| chat.delegate_items.get(item)) == Some(&(ix, index))
                                    });
                                    let label = format!("{}{STATUS_SEPARATOR}{status}", call.summary);
                                    let label = if allowed {
                                        format!("{label}{STATUS_SEPARATOR}{}", tr!("chat-automatically-allowed"))
                                    } else { label };
                                    body = body.child(
                                        disclosure(Disclosure::DelegateTool(index), call.input_open, label)
                                            .font(this.appearance.code_font.clone())
                                            .text_color(theme.muted_foreground),
                                    );
                                    if call.input_open {
                                        body = body.child(code(&format!("worker-input-{index}"), &call.pretty_input()))
                                            .children(call.detail.status().map(|status| div().text_xs().child(status)));
                                        if let Some(result) = &call.result {
                                            body = body.child(code(&format!("worker-output-{index}"),
                                                call.detail.output.as_deref().unwrap_or(&result.output)));
                                        }
                                    }
                                }
                                DelegateEntry::Steer { text, attachments } => {
                                    body = body.child(
                                        v_flex().self_end().min_w_0().max_w(relative(0.85)).gap_1()
                                            .child(
                                                v_flex().id(SharedString::from(format!("chat-steer-bubble-{ix}-{index}")))
                                                    .gap_1().px_3().py_2()
                                                    .map(|bubble| style_user_bubble(bubble, rems(0.5)))
                                                    .bg(theme.secondary)
                                                    .child(div().text_xs().text_color(theme.muted_foreground).child(tr!("chat-to-worker")))
                                                    .children((!text.trim().is_empty()).then(|| div().child(text.clone())))
                                                    .children((!attachments.is_empty()).then(|| {
                                                        this.message_attachments(chat, format!("{ix}-steer-{index}"), attachments)
                                                    })),
                                            )
                                            .child(message_actions()
                                                .children(copy_message_action(&format!("{ix}-steer-{index}"), text))),
                                    );
                                }
                                DelegateEntry::Text(_) | DelegateEntry::Tool(_) => {}
                            }
                        }
                        body = body.children(chat.approval_receipts.values().filter(|receipt| {
                            receipt.job() == Some(block.id.as_str()) && receipt.state == ApprovalState::Denied
                        }).map(|receipt| div().text_xs().text_color(theme.danger)
                            .child(format!("{}{STATUS_SEPARATOR}{}", tr!("chat-deny"), receipt.tool))));
                        if let Some((ok, report)) = block.report.as_ref().filter(|_| text_open) {
                            let text = if report.interrupted { tr!("chat-worker-recovery") } else { report.text.as_str() };
                            body = body.child(v_flex().id(SharedString::from(format!("delegate-report-{}", block.id)))
                                .min_w_0().gap_1()
                                .child(div().text_xs().text_color(theme.muted_foreground)
                                    .child(if *ok { tr!("chat-final-report") } else { tr!("chat-failure-details") }))
                                .child(markdown(MarkdownPart::WorkerReport, text))
                                .children((!report.changes.is_empty()).then(|| {
                                    div().text_xs().text_color(theme.muted_foreground).child(report.changes.clone())
                                }))
                                .child(message_actions()
                                    .children(copy_message_action(&format!("{ix}-report"), text))
                                    .children(this.turn_history_action(&session, &block.id, &block.id, &entity))));
                        }
                        }
                    }
                    Content::Tools(group) => {
                        let run = geometry.tool_runs.at(ix);
                        let open = run.map_or(group.open, |run| run.open(&chat.rows));
                        let calls = run.map_or_else(|| (0..group.calls.len()).map(|call| (ix, call)).collect(),
                            |run| run.calls.clone());
                        let call_at = |(row, call): (usize, usize)| {
                            let Content::Tools(group) = &chat.rows[row].content else { unreachable!() };
                            &group.calls[call]
                        };
                        let batch_failure = calls.iter().map(|&position| call_at(position)).find_map(ToolCall::batch_failure_summary);
                        let failed = batch_failure.is_some() || calls.iter().map(|&position| call_at(position))
                            .any(|call| call.result.as_ref().is_some_and(|r| !r.ok));
                        if !calls.is_empty() {
                            let label = tr!("chat-tool-calls", calls = calls.len());
                            let color = if failed { theme.danger } else { theme.muted_foreground };
                            let long_partner_run = calls.len() >= LONG_PARTNER_TOOL_RUN
                                && calls.iter().all(|&(row, _)| matches!(chat.rows[row].actor,
                                    Actor::Claude | Actor::Codex { parent_tool_use: None, .. }));
                            let label = if long_partner_run {
                                let pending = calls.iter().any(|&position| call_at(position).result.is_none());
                                // Only calls whose own turn is still running keep a merged run running.
                                let running = calls.iter().any(|&(row, call)| matches!(&chat.rows[row].content,
                                    Content::Tools(group) if !group.ended && group.calls[call].result.is_none()));
                                let status = if !pending {
                                    tr!("chat-done")
                                } else if !running {
                                    tr!("chat-ended")
                                } else {
                                    tr!("chat-tool-running")
                                };
                                let label = tr!("chat-tool-group-status", label = label, status = status);
                                if failed { tr!("chat-tool-group-error", label = label) } else { label }
                            } else {
                                label
                            };
                            let label = if let Some(count) = run.map(|run| run.approvals()).filter(|count| *count > 0) {
                                format!("{label}{STATUS_SEPARATOR}{}", tr!("chat-auto-approved-count", count = count))
                            } else { label };
                            let collapsed_failure = batch_failure.filter(|_| !open);
                            let label = match collapsed_failure {
                                Some(summary) => format!("{summary}{STATUS_SEPARATOR}{label}"),
                                None => label,
                            };
                            let toggle = summary_toggle(
                                format!("chat-{session}-{ix}-Group").into(),
                                Disclosure::Group,
                                open,
                                label,
                                color,
                            );
                            body = body.child(if collapsed_failure.is_some() {
                                toggle.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            } else {
                                toggle
                            });
                        }
                        let decided_positions = run.map_or_else(
                            || (0..group.approvals.len()).map(|approval| (ix, approval)).collect::<Vec<_>>(),
                            |run| run.decided.clone(),
                        );
                        let decided: Vec<_> = decided_positions.into_iter().filter_map(|(row, approval)| {
                            let Content::Tools(group) = &chat.rows[row].content else { unreachable!() };
                            Some(&group.approvals[approval]).filter(|approval| approval.resolved.is_some())
                        }).collect();
                        let decided_open = run.map_or(group.decided_open, |run| run.decided_open(&chat.rows));
                        if let Some(first) = decided.first() {
                            body = body.child(summary_toggle(
                                format!("decided-approvals-{}", first.request_id).into(),
                                Disclosure::DecidedApprovals,
                                decided_open,
                                tr!("chat-decided-approvals", count = decided.len()),
                                theme.muted_foreground,
                            ));
                            if decided_open {
                                for approval in decided {
                                    let accepted = approval.resolved.as_deref() == Some("accept");
                                    let status = if approval.automatic {
                                        automatic_approval_label(accepted)
                                    } else if accepted {
                                        tr!("chat-allow")
                                    } else {
                                        tr!("chat-deny")
                                    };
                                    body = body.child(
                                        div().id(SharedString::from(format!("decided-approval-{}", approval.request_id)))
                                            .pl_3().text_xs().text_color(theme.muted_foreground)
                                            .whitespace_normal()
                                            .child(status_row(&format!("{status}{STATUS_SEPARATOR}{}", approval.summary.text()), theme.muted_foreground)),
                                    );
                                }
                            }
                        }
                        // Pending cards live in the dock above the composer. The transcript
                        // keeps one line at the request's place and the decision afterwards.
                        for approval in group.approvals.iter().filter(|approval| approval.resolved.is_none()) {
                            let first = approval.summary.text().lines().next().unwrap_or_default().to_owned();
                            body = body.child(
                                div().id(SharedString::from(format!("approval-in-dock-{}", approval.request_id)))
                                    .w_full().min_w_0().text_sm().text_color(theme.muted_foreground).truncate()
                                    .child(format!("{} · {first}", tr!("chat-approval-in-dock"))),
                            );
                        }
                        if open {
                            for (row, i) in calls {
                                let call = call_at((row, i));
                                let input = Disclosure::Input(i);
                                let mut tool = v_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        summary_toggle_at(row, format!("chat-{session}-{row}-{input:?}").into(),
                                            input, call.input_open, call.summary.clone(), theme.muted_foreground,
                                        )
                                        .font(this.appearance.code_font.clone()),
                                    )
                                    .children(call.input_open.then(|| {
                                        code(&format!("input-{row}-{i}"), &call.pretty_input())
                                    }))
                                    .children(run.and_then(|run| run.attached.get(&(row, i))).map(|_| {
                                        div().pl_3().text_xs().text_color(theme.muted_foreground)
                                            .child(tr!("chat-automatically-allowed"))
                                    }));
                                if let Some(result) = &call.result {
                                    let batch_failure = call.batch_failure_summary();
                                    let label = if let Some(summary) = batch_failure {
                                        format!("✗ {summary}")
                                    } else {
                                        let summary = tool_result_summary(&result.output);
                                        let summary = if summary.is_empty() { tr!("chat-no-output") } else { &summary };
                                        let mark = if result.ok { "→" } else { "✗" };
                                        format!("{mark} {summary}")
                                    };
                                    let color = if result.ok && batch_failure.is_none() { theme.muted_foreground } else { theme.danger };
                                    let part = Disclosure::Result(i);
                                    let toggle = summary_toggle_at(row,
                                        format!("chat-{session}-{row}-{part:?}").into(),
                                        part,
                                        call.result_open,
                                        label,
                                        color,
                                    ).font(this.appearance.code_font.clone());
                                    tool = tool.child(
                                        v_flex()
                                            .min_w_0()
                                            .pl_3()
                                            .gap_1()
                                            .text_color(color)
                                            .child(if batch_failure.is_some() {
                                                toggle.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            } else {
                                                toggle
                                            })
                                            .children(
                                                call.result_open.then(|| code(&format!("output-{row}-{i}"),
                                                    call.detail.output.as_deref().unwrap_or(&result.output))),
                                            ),
                                    );
                                }
                                body = body.child(tool.pl_3().children(
                                    call.detail.status().map(|status| div().text_xs().child(status))
                                ));
                            }
                            if let Some(count) = run.map(|run| run.unlinked).filter(|count| *count > 0) {
                                body = body.child(div().pl_3().text_xs().text_color(theme.muted_foreground)
                                    .child(tr!("chat-auto-approved-count", count = count)));
                            }
                        }
                    }
                }
                if let Some(&count) = geometry.tool_runs.standalone.get(&ix) {
                    body = body.child(div().text_xs().text_color(theme.muted_foreground)
                        .child(tr!("chat-auto-approved-count", count = count)));
                }
                if let Some(tools) = geometry.tool_runs.denials.get(&ix) {
                    body = body.children(tools.iter().map(|tool| {
                        div().text_xs().text_color(theme.danger)
                            .child(format!("{}{STATUS_SEPARATOR}{tool}", tr!("chat-deny")))
                    }));
                }
                let action_index = match group {
                    Some(group) if group.last == ix => group.last_text,
                    Some(_) => None,
                    None => Some(ix),
                };
                let mut actions_below = None;
                if let Some(ix) = action_index {
                    let row = &chat.rows[ix];
                    let message = match &row.content {
                        Content::Message(message) if !is_worker_message(&row.actor, message.item.as_deref())
                            || message.worker_text_open.unwrap_or(show_worker_messages) => Some(message),
                        _ => None,
                    };
                    if let Some(message) = message {
                        let history = message.item.as_ref().filter(|_| {
                            matches!(row.actor, Actor::Claude | Actor::Codex { .. })
                        }).and_then(|item| {
                            let turn = match &row.actor {
                                Actor::Codex { parent_tool_use: Some(job), .. } => job,
                                _ => item,
                            };
                            this.turn_history_action(&session, item, turn, &entity)
                        });
                        let copy = if let Some(group) = group {
                            let text = chat.rows[group.first..=group.last].iter().filter_map(|row| {
                                match &row.content {
                                    Content::Message(message) if row.kind(show_worker_messages) == RowKind::AssistantText => {
                                        Some(message.text.as_str())
                                    }
                                    _ => None,
                                }
                            }).collect::<Vec<_>>().join("\n\n");
                            copy_message_action(&ix.to_string(), &text)
                        } else {
                            copy_message_action(&ix.to_string(), &message.text)
                        };
                        let memo = message.item.as_ref().filter(|_| {
                            this.protocol >= fuzukue_proto::MEMOS_PROTOCOL && !message.text.trim().is_empty()
                        }).map(|item| {
                            let (session, item, text) = (session.clone(), item.clone(), message.text.clone());
                            let entity = entity.clone();
                            let saved = message.memo_saved == Some(true);
                            let (label, icon) = if saved {
                                (tr!("chat-memo-saved"), Symbol::Check)
                            } else {
                                (tr!("chat-save-memo"), Symbol::FilePenLine)
                            };
                            message_action(format!("chat-save-memo-{ix}"), icon, label)
                                .disabled(saved)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.save_chat_memo(session.clone(), item.clone(), text.clone(), cx);
                                    });
                                })
                        });
                        let rewrite = message.item.as_ref().filter(|_| {
                            this.protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL
                                && !chat.busy && chat.pending_jobs().next().is_none() && geometry.latest_reply == Some(ix)
                        }).map(|item| {
                            let out = this.out.clone();
                            let command = ClientMsg::RewriteReply { session: session.clone(), item: item.clone() };
                            message_action("chat-rewrite", Symbol::RotateCcw, tr!("chat-rewrite"))
                                .on_click(move |_, _, _| { let _ = out.send(command.clone()); })
                        });
                        let runtime = match &row.actor {
                            Actor::Claude | Actor::Codex { .. } => {
                                let backend = if matches!(row.actor, Actor::Claude) { AgentBackend::Claude } else { AgentBackend::Codex };
                                let mut lines = vec![tr!("tiers-role", role = tr!("thread-details-partner"))];
                                if let Some(details) = &message.details {
                                    lines.extend(run_details(details));
                                } else {
                                    lines.push(backend.label().into());
                                }
                                Some(job_details(format!("message-details-{ix}"), lines))
                            }
                            _ => None,
                        };
                        if hidden_actions_row != Some(group.map_or(ix, |group| group.last))
                            && (copy.is_some() || memo.is_some() || history.is_some() || rewrite.is_some() || runtime.is_some())
                        {
                            let actions = message_actions().h(message_action_height).items_center()
                                .id(SharedString::from(if geometry.latest_reply == Some(ix) {
                                    "chat-partner-reply".into()
                                } else {
                                    format!("chat-message-actions-{ix}")
                                }))
                                .when(latest_message != Some(ix), |actions| {
                                    older_message_actions(actions, group_hovered, keyboard_focus, (!covered).then_some(hover_group))
                                })
                                .children((message.memo_saved == Some(false)).then(|| {
                                    div().text_xs().text_color(theme.danger).child(tr!("library-memo-save-failed"))
                                }))
                                .children(copy)
                                .children(memo)
                                .children(history)
                                .children(rewrite)
                                .children(runtime);
                            actions_below = Some(actions.into_any_element());
                        }
                    }
                }
                let action_height = if actions_below.is_some() { message_action_height } else { px(0.) };
                let (top, bottom) = chat_row_spacing(
                    first_visible == Some(ix),
                    group.is_none_or(|group| group.last == ix),
                    action_height,
                );
                let row_el = chat_column().pt(top).pb(bottom);
                let row = if user {
                    row_el.flex().justify_end().child(
                        // TextView needs a definite width to measure wrapped text and display math.
                        v_flex().w(relative(0.8)).max_w(px(CHAT_WIDTH * 0.8))
                            .child(body.id(SharedString::from(format!("chat-message-bubble-{ix}")))
                                .w_full().px_4().py_3()
                                .map(|bubble| style_user_bubble(bubble, rems(0.75)))
                                .bg(theme.secondary))
                            .children(actions_below),
                    )
                } else if matches!(row.content, Content::Delegate(_)) || matches!(
                    row.actor,
                    Actor::Codex {
                        parent_tool_use: Some(_),
                        ..
                    }
                ) {
                    let (group_actions, row_actions) = if group.is_some() {
                        (actions_below, None)
                    } else {
                        (None, actions_below)
                    };
                    row_el.child(
                        div()
                            .ml_3()
                            .border_l_2()
                            .border_color(theme.border)
                            .pl_3()
                            .child(body.id(SharedString::from(format!("chat-message-bubble-{ix}"))))
                            .children(row_actions),
                    ).children(group_actions)
                } else {
                    let escalation = match &row.content {
                        Content::Message(message) => message.details.as_ref().filter(|details| details.escalated),
                        _ => None,
                    };
                    row_el.child(body.id(SharedString::from(format!("chat-message-bubble-{ix}"))))
                        .children(escalation.map(|details| {
                            div().text_xs().text_color(theme.muted_foreground).child(tr!("chat-escalated-reply",
                                model = details.selection.model.as_deref().unwrap_or_else(|| details.selection.backend.label()).to_owned()))
                        }))
                        .children(actions_below)
                };
                Some(list_row
                    // Include this space in the last visible item's scroll extent, so the
                    // latest message can scroll clear of the fade and the jump button.
                    .when(last_visible == Some(ix), |row| row.pb(tail_padding))
                    .child(row)
                    .into_any_element())
            },
        )
        .with_height_estimates(style_revision, move |ix, width| {
            if estimates.kinds[ix] == RowKind::Hidden {
                return px(0.);
            }
            let content = estimates.estimates[ix].height(width, style.rem, style.code_scale);
            content + if estimates.last_visible == Some(ix) { style.tail_padding } else { px(0.) }
        })
        .size_full()
        .min_h_0();
        let at_bottom = chat_at_bottom(&scroller);
        let at_top = scroller.scroll_px_offset_for_scrollbar().y >= -px(1.);
        let start = chat.history.at_true_start(thread.is_some() && !self.transcripts.scoped
            && self.threads_loaded && !self.receiving_backlog);
        let loading_older = chat.history.pending.is_some();
        let loading_newer = chat.history.loading_newer();
        let workbench = cx.weak_entity();
        let layout_scroller = scroller.clone();
        let layout_workbench = workbench.clone();
        let quote_workbench = workbench.clone();
        let scroll_workbench = workbench.clone();
        let bounce = ScrollBounce::new("bounce", &scroller, rows);
        let bounce = if chat.history.before.is_some() || chat.history.next_after.is_some() || chat.history.needs_trim() {
            let session = self.active.clone();
            bounce.on_vertical_wheel(move |delta, _, cx| {
                let _ = scroll_workbench.update(cx, |view, cx| view.history_wheel(&session, delta, cx));
            })
        } else {
            bounce
        };
        let background = cx.theme().background;
        let disclosure_press = chat.disclosure_press.clone();
        div()
            .child(cancel_disclosure_drag(disclosure_press))
            .on_children_prepainted(move |_, window, cx| {
                if chat_at_bottom(&layout_scroller) != at_bottom
                    || start && (layout_scroller.scroll_px_offset_for_scrollbar().y >= -px(1.)) != at_top
                {
                    let workbench = layout_workbench.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = workbench.update(cx, |_, cx| cx.notify());
                    });
                }
            })
            .id(chat_id)
            .track_focus(&self.chat_focus)
            .tab_index(0)
            .relative()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .on_key_down(cx.listener(Self::chat_key_down))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_attachments(this.active.clone(), paths.paths().to_vec(), cx);
            }))
            .role(gpui_kit::Role::Log)
            .child(bounce)
            .when(loading_older || loading_newer || start && at_top, |pane| {
                let (id, label) = if loading_older {
                    ("chat-loading-older", tr!("chat-loading-older"))
                } else if loading_newer {
                    ("chat-loading-newer", tr!("chat-loading-newer"))
                } else {
                    ("chat-start-of-thread", tr!("chat-start-of-thread"))
                };
                let label = div().id(id).absolute().left_0().w_full().flex().justify_center()
                    .when(loading_newer, |row| row.bottom(CHAT_BOTTOM_FADE))
                    .when(!loading_newer, |row| row.top_0())
                    .child(div().px_2().rounded_md().bg(background)
                        .text_xs().line_height(CHAT_LEADING_PADDING)
                        .text_color(cx.theme().muted_foreground).child(label));
                pane.child(label)
            })
            .child(div().id("chat-bottom-fade").absolute().left_0().bottom_0().w_full().h(CHAT_BOTTOM_FADE)
                .child(canvas(|_, _, _| (), move |bounds, _, window, _| {
                    // GPUI gradients take two stops, so paint one quad per segment.
                    for (index, stops) in CHAT_FADE_STOPS.windows(2).enumerate() {
                        let [(start, from), (end, to)] = [stops[0], stops[1]];
                        let mut segment = bounds;
                        segment.origin.y += start;
                        segment.size.height = if index == CHAT_FADE_STOPS.len() - 2 {
                            bounds.size.height - start
                        } else {
                            end - start
                        };
                        let fade = linear_gradient(
                            180.,
                            linear_color_stop(background.opacity(from), 0.),
                            linear_color_stop(background.opacity(to), (end - start) / segment.size.height),
                        );
                        window.paint_quad(fill(segment, fade));
                    }
                }).size_full()))
            .child(canvas(|_, _, _| (), move |bounds, _, window, cx| {
                // Read after the transcript has painted at its current scroll offset.
                let position = TextSelection::quote_bounds(&quote_region, window, cx)
                    .filter(|edge| bounds.contains(&edge.center()))
                    .map(|edge| {
                        let width = QUOTE_PILL_WIDTH.to_pixels(window.rem_size());
                        let height = QUOTE_PILL_HEIGHT.to_pixels(window.rem_size());
                        let x = edge.left() - bounds.left();
                        let above = edge.top() - bounds.top() - height - QUOTE_PILL_GAP;
                        let y = if above >= QUOTE_PILL_GAP {
                            above
                        } else {
                            edge.bottom() - bounds.top() + QUOTE_PILL_GAP
                        };
                        point(
                            x.clamp(QUOTE_PILL_GAP, (bounds.size.width - width - QUOTE_PILL_GAP).max(QUOTE_PILL_GAP)),
                            y.clamp(QUOTE_PILL_GAP, (bounds.size.height - height - QUOTE_PILL_GAP).max(QUOTE_PILL_GAP)),
                        )
                    });
                if position != quote_position {
                    let workbench = quote_workbench.clone();
                    let session = quote_session.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = workbench.update(cx, |this, cx| {
                            if let Some(chat) = this.chats.get_mut(&session) {
                                chat.quote_position = position;
                                cx.notify();
                            }
                        });
                    });
                }
            }).absolute().left_0().top_0().size_full())
            .vertical_scrollbar(&scroller)
            .children(quote_position.map(|position| {
                let theme = cx.theme();
                div().absolute().left(position.x).top(position.y).block_mouse_except_scroll()
                    .keep_text_selection("keep-chat-quote")
                    .child(Button::new("chat-quote-reply")
                        .secondary().small()
                        .accessibility_label(tr!("chat-quote-reply"))
                        .w(QUOTE_PILL_WIDTH).h(QUOTE_PILL_HEIGHT)
                        .rounded(theme.radius_full())
                        .border_1().border_color(theme.border)
                        .bg(theme.background).text_color(theme.foreground)
                        .shadow_sm()
                        .child(h_flex().gap_1().items_center()
                            .child(div().text_base().child("❞"))
                            .child(tr!("chat-quote-reply")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.quote_reply(window, cx);
                        })))
            }))
            .text_selection_region(chat.selection_scope)
    }

    pub(super) fn render_chat_footer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = &self.chats[&self.active];
        let show_latest = chat.history.next_after.is_some() || !chat_at_bottom(&chat.scroller);
        let theme = cx.theme();
        let height = snap_chat_height(window.rem_size() * 1.5, window.scale_factor());
        let has_commands = self.chat_waiting.has_background_commands(&self.active);
        let has_finished_jobs = self.protocol >= fuzukue_proto::JOB_LABEL_PROTOCOL
            && chat.finished_panel_jobs().next().is_some();
        let goal_status = self.chat_waiting.static_goal_status(&self.active, chat)
            .filter(|_| !has_commands && !has_finished_jobs);
        chat_column().id("chat-status-strip").relative().flex_none().h(height).flex().items_start().gap_2()
            .pt(CHAT_STATUS_TEXT_INSET).bg(theme.background)
            .children(goal_status.map(|status| div().id("chat-tail-activity").min_w_0().text_size(rems(0.75))
                .line_height(UI_LINE_HEIGHT).text_color(theme.muted_foreground)
                .child(chat_activity::goal_activity_row(status, "chat-tail-activity-label".into(), cx))))
            .when(has_commands || has_finished_jobs || goal_status.is_none() && chat.has_activity(self.chat_waiting.reason(&self.active)), |strip| {
                strip.child(self.render_chat_activity(&self.active, cx.weak_entity(), cx))
            })
            .children(chat.composer_notice.clone().map(|text| {
                let notice = div().id("chat-composer-notice").flex_1().min_w_0().truncate()
                    .text_size(rems(0.75)).line_height(UI_LINE_HEIGHT).text_color(theme.muted_foreground)
                    .child(text.clone());
                status_tooltip(notice, text)
            }))
            .children(self.render_todo_badge(cx))
            .when(show_latest, |strip| {
                strip.child(
                    gpui_kit::deferred(h_flex()
                        .absolute()
                        .left_0()
                        .bottom(height + CHAT_JUMP_MARGIN)
                        .w_full()
                        .justify_center()
                        .child(
                            Button::new("jump-to-latest")
                                .secondary()
                                .icon(IconName::ArrowDown)
                                .tooltip(tr!("chat-jump-to-latest"))
                                .w(CHAT_JUMP_SIZE).h(CHAT_JUMP_SIZE).p_0()
                                .rounded(theme.radius_full())
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .text_color(theme.foreground)
                                .shadow_sm()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.jump_thread_history(true, cx) {
                                        this.chats[&this.active].scroll_to_end();
                                        cx.notify();
                                    }
                                })),
                        )),
                )
            })
    }
}

fn older_message_actions(
    actions: gpui_kit::Stateful<gpui_kit::Div>,
    hovered: bool,
    keyboard: bool,
    hover_group: Option<SharedString>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    actions.focusable().opacity(if hovered { 1. } else { 0. })
        .when_some(hover_group, |actions, group| actions.group_hover(group, |style| style.opacity(1.)))
        .when(keyboard, |actions| actions.focus_within(|style| style.opacity(1.)))
}

fn message_actions() -> gpui_kit::Div {
    h_flex().w_full().justify_end().gap_1()
}

fn message_action(id: impl Into<SharedString>, icon: Symbol, label: &str) -> Button {
    Button::new(id.into()).small().ghost()
        .max_h(CHAT_GROUP_GAP)
        .icon(icon.icon())
        .accessibility_label(label.to_owned())
        .tooltip(label.to_owned())
}

fn copy_message_action(id: &str, text: &str) -> Option<Button> {
    (!text.trim().is_empty()).then(|| {
        let text = text.to_owned();
        message_action(format!("chat-copy-{id}"), Symbol::Copy, tr!("chat-copy-message"))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.clone()));
            })
    })
}

// Clockwise from the top left, matching the physical right-aligned bubble.
fn user_bubble_radii(radius: Rems) -> [Rems; 4] {
    [radius, radius, rems(0.), radius]
}

fn style_user_bubble<E: Styled>(bubble: E, radius: Rems) -> E {
    let [top_left, top_right, bottom_right, bottom_left] = user_bubble_radii(radius);
    bubble.rounded_tl(top_left).rounded_tr(top_right)
        .rounded_br(bottom_right).rounded_bl(bottom_left)
}

#[cfg(test)]
#[test]
fn user_bubbles_only_square_the_bottom_right_corner() {
    for radius in [rems(0.75), rems(0.5)] {
        assert_eq!(user_bubble_radii(radius), [radius, radius, rems(0.), radius]);
    }
}

fn inline_button(id: impl Into<gpui_kit::ElementId>) -> Button {
    Button::new(id).ghost().xsmall().rounded(px(8.)).flex_none()
        .line_height(UI_LINE_HEIGHT)
}

fn composer_text_button(id: impl Into<ElementId>, cx: &Context<Workbench>) -> Button {
    inline_button(id).text_color(cx.theme().muted_foreground)
}

const COMPOSER_BUTTON_SIZE: Rems = rems(2.);
const COMPOSER_ACTION_SIZE: Pixels = px(32.);

#[derive(Debug, PartialEq, Eq)]
enum ComposerAction {
    Empty,
    Stop,
    Send { held: bool },
}

fn composer_action(has_content: bool, running: bool, ime: bool, draining: bool) -> ComposerAction {
    if has_content || ime {
        ComposerAction::Send { held: draining }
    } else if running {
        ComposerAction::Stop
    } else {
        ComposerAction::Empty
    }
}

const COMPOSER_FOOTER_HEIGHT: Rems = rems(1.5);
const COMPOSER_END_PADDING: Rems = rems(0.375);

fn composer_footer() -> gpui_kit::Div {
    h_flex().w_full().justify_end().flex_wrap().gap_1()
}

fn ui_line_height_in_rems(font_size: Rems) -> Rems {
    rems(UI_LINE_HEIGHT.to_pixels(font_size.into(), px(1.)).into())
}

fn composer_picker(
    id: &'static str,
    label: impl Into<SharedString>,
    cx: &Context<Workbench>,
) -> Button {
    composer_text_button(id, cx)
        .h(COMPOSER_FOOTER_HEIGHT)
        .label(label)
        .dropdown_caret(true)
}

const DISCLOSURE_DRAG_THRESHOLD: f64 = 2.;

fn cancel_disclosure_drag(press: Rc<Cell<Option<Point<Pixels>>>>) -> impl IntoElement {
    canvas(|_, _, _| (), move |_, _, window, _| {
        let press = press.clone();
        // Observe the drag even when it leaves the header for selectable content.
        window.on_mouse_event(move |event: &gpui_kit::MouseMoveEvent, phase, _, _| {
            if phase == gpui_kit::DispatchPhase::Capture
                && press.get().is_some_and(|start| {
                    (event.position - start).magnitude() > DISCLOSURE_DRAG_THRESHOLD
                })
            {
                press.set(None);
            }
        });
    }).absolute().size_0()
}

fn disclosure_row(
    id: SharedString,
    open: bool,
    label: &str,
    color: gpui_kit::Hsla,
    press: Rc<Cell<Option<Point<Pixels>>>>,
    cx: &gpui_kit::App,
    toggle: impl Fn(&mut gpui_kit::App) + 'static,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    let hover = theme.muted;
    let ring = theme.ring;
    let action = if open { tr!("chat-collapse") } else { tr!("chat-expand") };
    let tooltip = tr!("chat-toggle-details", label = status_speech(label), action = action.to_string());
    let chevron = if open { Symbol::ChevronDown } else { Symbol::ChevronRight };
    let mouse_down = press.clone();
    h_flex()
        .id(id.clone())
        .role(gpui_kit::Role::Button)
        .aria_label(status_speech(label))
        .aria_expanded(open)
        .tab_index(0)
        .w_full()
        .min_w_0()
        .min_h(px(28.))
        .px_1()
        .gap_1()
        .text_xs()
        .text_color(color)
        .rounded(px(4.))
        .border_1()
        .border_color(gpui_kit::transparent_black())
        .hover(move |style| style.bg(hover))
        .focus_visible(move |style| style.border_color(ring))
        .cursor_pointer()
        .child(div().id(format!("{id}-chevron-{}", if open { "down" } else { "right" }))
            .flex_none().child(chevron.tinted(color)))
        .child(status_row(label, theme.muted_foreground).id(format!("{id}-label")).truncate())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_mouse_down(gpui_kit::MouseButton::Left, move |event, _, _| mouse_down.set(Some(event.position)))
        .on_click(move |event, _, cx| {
            if let gpui_kit::ClickEvent::Mouse(mouse) = event {
                if press.take() != Some(mouse.down.position)
                    || (mouse.up.position - mouse.down.position).magnitude() > DISCLOSURE_DRAG_THRESHOLD
                {
                    return;
                }
            }
            toggle(cx);
        })
}

pub(super) fn folder_approval_buttons(request_id: &str, entity: &Entity<Workbench>, running: bool) -> impl IntoElement {
    use fuzukue_proto::FolderGrantExpiry;
    let button = |expiry, label| {
        let id = match expiry {
            Some(FolderGrantExpiry::OneHour) => format!("accept-{request_id}"),
            Some(other) => format!("folder-access-{other:?}-{request_id}"),
            None => format!("decline-{request_id}"),
        };
        let request_id = request_id.to_owned();
        let entity = entity.clone();
        Button::new(SharedString::from(id)).secondary().xsmall().label(label)
            .on_click(move |_, _, cx| {
                let _ = entity.read(cx).out.send(ClientMsg::DecideFolderAccess { request_id: request_id.clone(), expiry });
            })
    };
    v_flex().gap_1()
        .child(div().text_sm().child(tr!("folder-grant-allow-for")))
        .child(h_flex().flex_wrap().gap_1()
            .children(crate::thread_info::FOLDER_EXPIRIES.into_iter()
                .filter(|expiry| *expiry != FolderGrantExpiry::ThisTurn || running)
                .map(|expiry| button(Some(expiry), crate::thread_info::folder_expiry_label(expiry))))
            .child(button(None, tr!("chat-deny"))))
}

/// `hint` is the tooltip of the allow button.
pub(super) fn approval_buttons(request_id: &str, hint: Option<String>, entity: &Entity<Workbench>) -> impl IntoElement {
    let id = request_id.to_owned();
    approval_decision_buttons(request_id, tr!("chat-deny"), tr!("chat-allow"), hint, entity, move |accept| {
        ClientMsg::Resolve {
            request_id: id.clone(),
            decision: if accept { "accept" } else { "decline" }.into(),
        }
    })
}

pub(super) fn approval_decision_buttons(
    id: &str, decline: &str, accept: &str, hint: Option<String>, entity: &Entity<Workbench>,
    action: impl Fn(bool) -> ClientMsg + Clone + 'static,
) -> impl IntoElement {
    let button = |label: &str, decision: &'static str, primary| {
        let entity = entity.clone();
        let action = action.clone();
        let button = Button::new(SharedString::from(format!("{decision}-{id}"))).label(label.to_owned());
        let button = if primary {
            button.primary().when_some(hint.clone(), |button, hint| button.tooltip(hint))
        } else {
            button.secondary()
        };
        button.on_click(move |_, _, cx| {
            let _ = entity.read(cx).out.send(action(primary));
        })
    };
    h_flex()
        .flex_none()
        .gap_1()
        .child(button(decline, "decline", false))
        .child(button(accept, "accept", true))
}

pub(super) struct ChatCapabilities {
    pub(super) effort_levels: Vec<String>,
    pub(super) mail_restricted: bool,
    pub(super) mcp_tools: Vec<String>,
    pub(super) folder_attachments: bool,
}

struct DraftAttachment {
    id: u64,
    name: String,
    session: String,
    state: AttachmentState,
    preview: Option<Arc<AttachmentImage>>,
}

enum AttachmentState {
    Reading,
    Uploading { bytes: Vec<u8>, sent: u64 },
    Drop(DropState),
    Saved(String),
    Failed(String),
}

const DRAFTS_KEY: &str = "composer_drafts";
const MAX_DRAFT_CHARS: usize = 20_000;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
struct ComposerDraft {
    text: String,
    attachments: Vec<SavedAttachment>,
    #[serde(flatten)]
    extra: toml::Table,
}

impl ComposerDraft {
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty() && self.extra.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SavedAttachment {
    name: String,
    path: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(transparent)]
struct DraftMap(BTreeMap<String, BTreeMap<String, ComposerDraft>>);

impl DraftMap {
    fn get(&self, workspace: &str, thread: &str) -> Option<&ComposerDraft> {
        self.0.get(workspace)?.get(thread)
    }

    fn edit(
        &mut self,
        workspace: &str,
        thread: &str,
        edit: impl FnOnce(&mut ComposerDraft) -> bool,
    ) -> bool {
        let threads = self.0.entry(workspace.to_owned()).or_default();
        let draft = threads.entry(thread.to_owned()).or_default();
        let changed = edit(draft);
        if draft.is_empty() {
            threads.remove(thread);
        }
        if threads.is_empty() {
            self.0.remove(workspace);
        }
        changed
    }

    fn set_text(&mut self, workspace: &str, thread: &str, text: &str) -> bool {
        self.edit(workspace, thread, |draft| {
            let text = &text[..draft_text_end(text)];
            if draft.text == text {
                return false;
            }
            draft.text = text.to_owned();
            true
        })
    }

    fn retain_threads(&mut self, workspace: &str, mut keep: impl FnMut(&str) -> bool) -> bool {
        let Some(drafts) = self.0.get_mut(workspace) else { return false };
        let before = drafts.len();
        drafts.retain(|id, _| keep(id));
        let changed = drafts.len() != before;
        if drafts.is_empty() {
            self.0.remove(workspace);
        }
        changed
    }

    fn forget_attachment(&mut self, workspace: &str, path: &str) -> bool {
        let Some(threads) = self.0.get_mut(workspace) else { return false };
        let mut changed = false;
        threads.retain(|_, draft| {
            let before = draft.attachments.len();
            draft.attachments.retain(|item| item.path != path);
            changed |= draft.attachments.len() != before;
            !draft.is_empty()
        });
        if threads.is_empty() {
            self.0.remove(workspace);
        }
        changed
    }

    fn read(settings: &toml::Table) -> anyhow::Result<Self> {
        let mut drafts: Self = match settings.get(DRAFTS_KEY) {
            Some(value) => value.clone().try_into()?,
            None => Self::default(),
        };
        for draft in drafts.0.values_mut().flat_map(|threads| threads.values_mut()) {
            draft.text.truncate(draft_text_end(&draft.text));
            draft.attachments.truncate(MAX_ATTACHMENTS);
        }
        Ok(drafts)
    }

    fn write(&self, settings: &mut toml::Table) -> anyhow::Result<bool> {
        if self.0.is_empty() {
            return Ok(settings.remove(DRAFTS_KEY).is_some());
        }
        let value = toml::Value::try_from(self)?;
        if settings.get(DRAFTS_KEY) == Some(&value) {
            return Ok(false);
        }
        settings.insert(DRAFTS_KEY.to_owned(), value);
        Ok(true)
    }
}

fn draft_text_end(text: &str) -> usize {
    text.char_indices().nth(MAX_DRAFT_CHARS).map_or(text.len(), |(end, _)| end)
}

pub(super) struct ComposerDrafts {
    drafts: DraftMap,
    // A workspace replay can replace the input even for the same workspace and thread.
    shown: Option<(String, String, u64)>,
    known_threads: Option<(u64, HashSet<String>)>,
    path: Option<PathBuf>,
    pending: bool,
    save_task: Option<Task<()>>,
}

impl ComposerDrafts {
    fn load(settings_path: Option<PathBuf>) -> Self {
        let path = settings_path.map(|path| path.with_extension("state.toml"));
        let drafts = path.as_deref().map(|path| settings_file::read(path).and_then(|settings| DraftMap::read(&settings)));
        let (drafts, path) = match drafts {
            Some(Ok(drafts)) => (drafts, path),
            Some(Err(err)) => {
                eprintln!("Could not read composer drafts: {err:#}");
                (DraftMap::default(), None)
            }
            None => (DraftMap::default(), None),
        };
        Self { drafts, shown: None, known_threads: None, path, pending: false, save_task: None }
    }

    fn reconcile_threads(&mut self, workspace: &str, epoch: u64, threads: &[ThreadView], cx: &mut Context<Workbench>) {
        let previous = self.known_threads.as_ref()
            .filter(|(old_epoch, _)| *old_epoch == epoch)
            .map(|(_, ids)| ids);
        if previous.is_some_and(|ids| {
            ids.len() == threads.len() && threads.iter().all(|thread| ids.contains(&thread.id))
        }) {
            return;
        }
        let ids: HashSet<_> = threads.iter().map(|thread| thread.id.clone()).collect();
        // A ThreadCreated reply can arrive before its list update. Between
        // snapshots only remove drafts for threads that had appeared in the list.
        let changed = self.drafts.retain_threads(workspace, |id| {
            id.is_empty() || ids.contains(id) || previous.is_some_and(|ids| !ids.contains(id))
        });
        self.known_threads = Some((epoch, ids));
        if changed {
            self.schedule_save(cx);
        }
    }

    fn schedule_save(&mut self, cx: &mut Context<Workbench>) {
        if self.path.is_none() {
            return;
        }
        self.pending = true;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(350)).await;
            let _ = this.update(cx, |this, _| this.flush_composer_drafts());
        }));
    }

    fn flush(&mut self) {
        if let Some(path) = self.path.as_deref().filter(|_| self.pending) {
            match settings_file::update(path, |settings| self.drafts.write(settings)) {
                Ok(()) => self.pending = false,
                Err(err) => eprintln!("Could not save composer drafts: {err:#}"),
            }
        }
    }
}

impl Drop for ComposerDrafts {
    fn drop(&mut self) {
        self.flush();
    }
}

fn attachment_label(name: &str, path: &str) -> String {
    if path.starts_with("inbox/folders/ref-") {
        tr!("chat-folder-read-only", name = name.to_owned())
    } else if fuzukue_proto::is_folder_attachment(path) {
        tr!("chat-folder-copied", name = name.to_owned())
    } else {
        name.to_owned()
    }
}

fn attachment_chip(name: &str, cx: &gpui_kit::App) -> gpui_kit::Div {
    h_flex()
        .gap_1()
        .max_w(px(220.))
        .min_w_0()
        .rounded_md()
        .px_2()
        .py_1()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .text_xs()
        .child(Icon::from(IconName::File)
            .xsmall().text_color(cx.theme().muted_foreground))
        .child(status_row(name, cx.theme().muted_foreground).truncate())
}

pub(super) fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => tr!("chat-ask-every-time"),
        PermissionMode::AcceptEdits => tr!("chat-approve-edits-automatically"),
        PermissionMode::Plan => tr!("chat-plan-only"),
        PermissionMode::BypassPermissions => tr!("chat-approve-everything"),
    }
}

pub(super) fn model_label(id: &str) -> &str {
    match id {
        "claude-opus-5-5" => "Opus 5.5",
        "claude-sonnet-5-5" => "Sonnet 5.5",
        "claude-haiku-5-5" => "Haiku 5.5",
        "claude-fable-5-2" => "Fable 5.2",
        _ => id,
    }
}

pub(super) fn effort_label(effort: Option<&str>) -> &str {
    match effort {
        None => tr!("chat-default"),
        Some("low") => tr!("chat-low"),
        Some("medium") => tr!("chat-medium"),
        Some("high") => tr!("chat-high"),
        Some("xhigh") => tr!("chat-extra-high"),
        Some("max") => tr!("chat-maximum"),
        Some(level) => level,
    }
}

#[derive(Clone, Copy, Default)]
struct ContextUsage {
    used: Option<u64>,
    limit: Option<u64>,
}

impl ContextUsage {
    fn percent(self) -> Option<f64> {
        Some(self.used? as f64 / self.limit.filter(|&limit| limit > 0)? as f64 * 100.)
    }

    fn label(self) -> String {
        let display = |tokens: Option<u64>| tokens.map_or_else(|| "—".into(), |n| n.to_string());
        let mut label = tr!("chat-context",
            used = display(self.used),
            limit_label = display(self.limit.filter(|&limit| limit > 0)));
        if let Some(percent) = self.percent() {
            label.push_str(&format!("（{percent:.0}%）"));
        }
        label
    }
}

fn context_usage_tooltip(groups: impl IntoIterator<Item = ContextUsage>) -> (String, f32) {
    let fullest = groups.into_iter()
        .reduce(|first, next| if next.percent() > first.percent() { next } else { first })
        .unwrap_or_default();
    (fullest.label(), fullest.percent().unwrap_or_default() as f32)
}

#[derive(gpui_kit::IntoElement)]
struct ContextRingTrigger {
    button: Button,
    tooltip: String,
    composer_bounds: Rc<Cell<gpui_kit::Bounds<Pixels>>>,
}

impl Selectable for ContextRingTrigger {
    fn selected(self, selected: bool) -> Self {
        Self { button: self.button.selected(selected), ..self }
    }

    fn is_selected(&self) -> bool {
        self.button.is_selected()
    }
}

impl gpui_kit::RenderOnce for ContextRingTrigger {
    fn render(self, _: &mut Window, _: &mut gpui_kit::App) -> impl IntoElement {
        let open = self.is_open();
        let tooltip = self.tooltip;
        let composer_bounds = self.composer_bounds;
        div().id("composer-context")
            .when(!open, |trigger| {
                trigger.tooltip(move |_, cx| {
                    cx.new(|_| ComposerTooltip {
                        label: tooltip.clone(),
                        composer_bounds: composer_bounds.clone(),
                    }).into()
                })
            })
            .child(self.button)
    }
}

fn status_tooltip(
    trigger: gpui_kit::Stateful<gpui_kit::Div>,
    label: String,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let anchor = Rc::new(Cell::new(gpui_kit::Bounds::default()));
    let tracked_anchor = anchor.clone();
    trigger.on_prepaint(move |bounds, _, _| tracked_anchor.set(bounds))
        .tooltip(move |_, cx| {
            cx.new(|_| StatusTooltip {
                label: plain_status_tooltip_text(&label),
                anchor: anchor.clone(),
            }).into()
        })
}

fn plain_status_tooltip_text(label: &str) -> String {
    // Status reasons can contain snippets supplied by the assistant.
    if label.contains('`') {
        if let Ok(markdown) = markdown::to_mdast(label, &markdown::ParseOptions::default()) {
            return markdown.to_string();
        }
    }
    label.to_owned()
}

struct StatusTooltip {
    label: String,
    anchor: Rc<Cell<gpui_kit::Bounds<Pixels>>>,
}

impl gpui_kit::Render for StatusTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::base::{Align, Placement, Positioner};

        div().child(
            Positioner::side(self.anchor.get())
                .placement(Placement::Top)
                .align(Align::Start)
                .offset(px(4.))
                .child(Tooltip::new(self.label.clone()).build(window, cx)),
        )
    }
}

struct ComposerTooltip {
    label: String,
    composer_bounds: Rc<Cell<gpui_kit::Bounds<Pixels>>>,
}

impl gpui_kit::Render for ComposerTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Anchor to the input frame so the tooltip clears a multiline draft as well.
        let composer = self.composer_bounds.get();
        let gap = px(8.);
        let label = self.label.clone();
        let tooltip = Tooltip::element(move |_, _| div().id("composer-context-tooltip").child(label.clone()))
            .build(window, cx);
        div().child(gpui_kit::anchored()
            .anchor(Anchor::BottomRight)
            .position(point(composer.right() - gap, composer.top() - gap))
            .snap_to_window_with_margin(gap)
            .child(tooltip))
    }
}

fn context_usage_details(groups: &[Vec<String>], color: gpui_kit::Hsla) -> impl IntoElement {
    v_flex()
        .id("composer-context-details")
        .gap_2()
        .text_xs()
        .children(groups.iter().map(|lines| {
            v_flex().gap_0p5().children(lines.iter().map(|line| status_row(line, color)))
        }))
        .children(groups.is_empty().then_some(tr!("chat-no-usage-data-yet")))
}

#[derive(Clone)]
pub(super) enum AgentTarget {
    Default(String),
    Thread(String, String),
    Summary,
    Fallback(String),
    PartnerDefault,
    WorkerDefault,
    Role(fuzukue_proto::AgentModelRole),
    Setup,
}

impl AgentTarget {
    fn apply(&self, view: &mut Workbench, selection: Option<AgentSelection>, cx: &mut Context<Workbench>) {
        let msg = match self {
            Self::Default(name) => ClientMsg::SetAgentOverride { name: name.clone(), selection },
            Self::Thread(session, name) => ClientMsg::SetThreadAgentOverride { session: session.clone(), name: name.clone(), selection },
            Self::Summary => ClientMsg::SetSummaryModel { selection },
            Self::Fallback(name) => ClientMsg::SetAgentFallback { name: name.clone(), selection },
            Self::Role(role) => ClientMsg::SetAgentRoleModel { role: *role, selection },
            Self::PartnerDefault => ClientMsg::SetPartnerDefault { settings: selection.map_or_else(ChatSettings::default, |choice| choice.chat_settings()) },
            Self::WorkerDefault => {
                let settings = selection.and_then(|choice| choice.model.map(|model| fuzukue_proto::DelegateSettings {
                    model, effort: choice.effort.unwrap_or_default(),
                })).unwrap_or_default();
                ClientMsg::SetDelegateSettings { settings }
            }
            Self::Setup => {
                view.agent_models.draft = selection;
                cx.notify();
                return;
            }
        };
        let _ = view.out.send(msg);
    }
}

#[derive(Clone)]
enum RoleSettings {
    PartnerDefault,
    PartnerThread(String),
    WorkerDefault,
    WorkerThread(String),
    StyleReviewer,
    MathReviewer,
}

fn worker_choice(settings: &fuzukue_proto::DelegateSettings) -> ChatSettings {
    ChatSettings {
        model: Some(settings.model.clone()),
        effort: (!settings.effort.is_empty()).then(|| settings.effort.clone()),
        ..Default::default()
    }
}

impl Workbench {
    pub(super) fn chat_memo_result(&mut self, session: &str, item: &str, saved: bool, cx: &mut Context<Self>) {
        if let Some(chat) = self.chats.get_mut(session) {
            let Some(&row) = chat.by_item.get(item) else { return };
            if let Some(Row { content: Content::Message(message), .. }) = chat.rows.get_mut(row) {
                message.memo_saved = Some(saved);
                chat.remeasure(row, cx);
            }
        }
    }

    pub(super) fn sync_auto_attachments(&mut self, threads: &[ThreadView], cx: &mut Context<Self>) {
        for thread in threads {
            let removed: Vec<_> = self.threads.iter().find(|old| old.id == thread.id)
                .into_iter().flat_map(|old| &old.auto_attached)
                .filter(|route| !thread.auto_attached.contains(route))
                .map(|route| format!("route:{}", route.deck)).collect();
            if removed.is_empty() && thread.auto_attached.is_empty() {
                continue;
            }
            let chat = self.chat(&thread.id, cx);
            chat.transcript.rows.borrow_mut().take();
            for key in removed {
                if let Some(&row) = chat.by_item.get(&key) {
                    chat.remeasure(row, cx);
                }
            }
            for route in &thread.auto_attached {
                chat.push_system_row(format!("route:{}", route.deck), Content::AutoAttached(route.clone()), cx);
            }
        }
    }

    fn message_attachments(&self, chat: &Chat, id: String, paths: &[String]) -> impl IntoElement {
        h_flex().flex_wrap().gap_1().max_w_full()
            .children(paths.iter().enumerate().map(|(index, path)| {
                let name = path.rsplit('/').next().unwrap_or(path).to_owned();
                AttachmentThumbnail {
                    id: format!("chat-message-image-{id}-{index}").into(),
                    name: name.clone(),
                    label: attachment_label(&name, path),
                    decoded: chat.inbox.get(path).and_then(|entry| entry.preview.clone()),
                    saved: (!fuzukue_proto::is_folder_attachment(path))
                        .then(|| (self.connection_epoch, self.http.clone(), path.clone())),
                }
            }))
    }

    fn file_inbox(&mut self, session: &str, path: &str, to: String, cx: &mut Context<Self>) {
        let Some(entry) = self.chats.get_mut(session).and_then(|chat| chat.inbox.get_mut(path)) else {
            return;
        };
        if entry.pending || entry.destination.is_some() {
            return;
        }
        entry.pending = self.out.send(ClientMsg::FileInbox {
            session: session.into(), path: path.into(), to,
        }).is_ok();
        entry.error = (!entry.pending).then(|| tr!("inbox-destination-send-error").into());
        cx.notify();
    }

    fn attachment_filing_menu(
        &self, session: &str, path: &str, cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        if self.protocol < 13 {
            return None;
        }
        let entry = self.chats[session].inbox.get(path)?;
        if entry.destination.is_some() {
            return None;
        }
        let destination_label = |to: &str| match to {
            "workspace" => tr!("inbox-workspace-root").into(),
            "references" => tr!("inbox-references").into(),
            _ => to.to_owned(),
        };
        let mut choices = vec!["workspace".to_owned(), "references".to_owned()];
        let mut seen = HashSet::new();
        for tab in &self.tabs {
            for deck in Self::tab_attachment(tab).into_iter()
                .chain(tab.papers.iter().flat_map(|paper| &paper.decks).map(|deck| {
                    if deck.is_empty() { "." } else { deck }
                }))
            {
                for to in [deck.to_owned(), format!("{deck}/figures")] {
                    if seen.insert(to.clone()) {
                        choices.push(to);
                    }
                }
            }
        }
        let suggested = entry.proposal.as_ref().filter(|proposal| proposal.to != "inbox");
        let mut destinations = Vec::new();
        if let Some(proposal) = suggested {
            choices.retain(|to| to != &proposal.to);
            destinations.push((
                tr!("inbox-suggested-location", location = destination_label(&proposal.to)),
                proposal.to.clone(),
            ));
        }
        destinations.extend(choices.into_iter().map(|to| (destination_label(&to), to)));
        let entity = cx.weak_entity();
        let name = path.rsplit('/').next().unwrap_or(path);
        let menu_id = SharedString::from(format!("inbox-file-other-{name}"));
        let (session, path) = (session.to_owned(), path.to_owned());
        let label = tr!("inbox-choose-location");
        Some(inline_button(menu_id)
            .icon(Symbol::Ellipsis.icon())
            .accessibility_label(label)
            .tooltip(entry.error.clone().unwrap_or_else(|| {
                if entry.pending { tr!("chat-saving").into() } else { label.into() }
            }))
            .disabled(entry.pending)
            .dropdown_menu(move |mut menu, _, _| {
                for (label, to) in &destinations {
                    let (entity, session, path, to) = (entity.clone(), session.clone(), path.clone(), to.clone());
                    menu = menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.file_inbox(&session, &path, to.clone(), cx));
                    }));
                }
                menu
            }))
    }

    pub(super) fn on_chat_event(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        match event {
            ServerEvent::ChatWaiting { waiting } => {
                self.chat_waiting.update(waiting, Instant::now());
                cx.notify();
            }
            ServerEvent::BackgroundCommands { threads } => {
                self.chat_waiting.background_commands = threads;
                cx.notify();
            }
            ServerEvent::ChatCapabilities {
                effort_levels,
                mail_restricted,
                mcp_tools,
                folder_attachments,
            } => {
                self.chat_capabilities = Some(ChatCapabilities {
                    effort_levels,
                    mail_restricted,
                    mcp_tools,
                    folder_attachments,
                });
            }
            ServerEvent::CodexModels { models } => self.codex_models = models,
            ServerEvent::CodexPartner { session, thread_id } => {
                self.chat(&session, cx).codex_thread_id = thread_id;
            }
            ServerEvent::ChatSettingsChanged { session, settings } => {
                self.chat(&session, cx).settings = settings;
            }
            ServerEvent::GitHubConnectCard { session, attempt, state } => {
                let key = format!("github-auth:{attempt}");
                let chat = self.chat(&session, cx);
                let content = Content::GitHubConnect { attempt, state };
                if let Some(&row) = chat.by_item.get(&key) {
                    chat.rows[row].content = content;
                    chat.remeasure(row, cx);
                } else {
                    chat.push_system_row(key, content, cx);
                }
            }
            ServerEvent::GitHubActivity { session, item, events } => {
                self.chat(&session, cx).github_activity(item, events, cx);
                if session == self.active {
                    self.github_view();
                }
            }
            ServerEvent::LocalNotice { session, item, text, localized } => {
                self.chat(&session, cx).local_notice(item, text, localized, cx);
            }
            ServerEvent::UserPrompt { session, text, paths, to_worker, item } => {
                self.chat(&session, cx).user_prompt(item, text, paths, to_worker, cx);
            }
            ServerEvent::TurnActivity { session, running } => {
                let chat = self.chat(&session, cx);
                if running {
                    chat.clear_composer_notice(cx);
                }
                chat.set_busy(running, cx);
            }
            ServerEvent::ChatActivity { turns, workers } => {
                for (session, chat) in &mut self.chats {
                    chat.reconcile_activity(turns.contains(session), workers.get(session), cx);
                }
                for session in turns {
                    self.chat(&session, cx).set_busy(true, cx);
                }
            }
            ServerEvent::Consulting { session, active } => self.chat(&session, cx).set_consulting(active),
            ServerEvent::FolderReferenceUnavailable { id } => self.folder_reference_unavailable(id, cx),
            ServerEvent::AttachmentProgress { id, offset } => {
                let out = self.out.clone();
                if let Some(draft) = self.drop_draft(id) {
                    draft.send_chunk(offset, &out);
                }
                self.advance_drop(id, offset, cx);
            }
            ServerEvent::AttachmentUploaded { id, path, error, proposal } => {
                let mut saved_for = None;
                if let Some((chat, index)) = self.chats.values_mut().find_map(|chat| {
                    let index = chat.attachments.iter().position(|draft| draft.id == id)?;
                    Some((chat, index))
                }) {
                    if chat.attachments[index].transferring() {
                        if let Some(path) = path.as_ref().filter(|path| !fuzukue_proto::is_folder_attachment(path)) {
                            chat.inbox.insert(path.clone(), InboxEntry {
                                proposal, pending: false, destination: None, error: None,
                                preview: chat.attachments[index].preview.clone(),
                            });
                        }
                        chat.attachments[index].state = match path {
                            Some(path) => {
                                saved_for = Some(chat.attachments[index].session.clone());
                                AttachmentState::Saved(path)
                            }
                            None => AttachmentState::Failed(error.unwrap_or_else(|| tr!("inbox-save-error").into())),
                        };
                    }
                }
                if let Some(thread) = saved_for {
                    self.record_composer_attachments(&thread, cx);
                }
            }
            ServerEvent::InboxFiled { path, destination, error } => {
                if let Some((drafts, workspace)) = self.composer_markdown.drafts.as_mut()
                    .zip(self.workspaces.active.as_deref())
                    .filter(|_| destination.is_some())
                {
                    if drafts.drafts.forget_attachment(workspace, &path) {
                        drafts.schedule_save(cx);
                    }
                }
                for chat in self.chats.values_mut() {
                    if destination.is_some() {
                        chat.attachments.retain(|draft| !matches!(&draft.state, AttachmentState::Saved(saved) if saved == &path));
                    }
                    if let Some(entry) = chat.inbox.get_mut(&path) {
                        if destination.is_some() || entry.pending {
                            entry.destination = destination.clone();
                            entry.error = error.clone();
                            entry.pending = false;
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
        cx.notify();
    }

    pub(super) fn disconnect_chat_uploads(&mut self) {
        self.chat_capabilities = None;
        for chat in self.chats.values_mut() {
            for entry in chat.inbox.values_mut() {
                entry.pending = false;
            }
        }
        for draft in self
            .chats
            .values_mut()
            .flat_map(|chat| &mut chat.attachments)
        {
            if matches!(draft.state, AttachmentState::Reading | AttachmentState::Drop(DropState::Inspecting(_)))
                || draft.transferring()
            {
                if let AttachmentState::Drop(state) = &draft.state {
                    state.cancel();
                }
                draft.state =
                    AttachmentState::Failed(tr!("chat-connection-lost-select-the-attachment-again").into());
            }
        }
    }

    fn choose_attachments(&mut self, folder: bool, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: !folder,
            directories: folder,
            multiple: !folder,
            prompt: Some(if folder { tr!("chat-attach-folder") } else { tr!("chat-choose-attachments") }.into()),
        });
        let session = self.active.clone();
        let epoch = self.connection_epoch;
        cx.spawn(async move |this, cx| {
            let Ok(result) = selection.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch {
                    return;
                }
                match result {
                    Ok(Some(paths)) => this.add_attachments(session, paths, cx),
                    Ok(None) => {}
                    Err(err) => {
                        this.chat(&session, cx)
                            .push_error(tr!("chat-cannot-open-the-file-picker", err = err.to_string()), cx);
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn can_attach_screenshot(&self) -> bool {
        self.chat_capabilities.is_some() && self.threads_loaded
            && self.chats.get(&self.active).is_some_and(|chat| {
                chat.attachments.len() < MAX_ATTACHMENTS && chat.worker_target.is_none()
            })
    }

    pub(super) fn attach_screenshot(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) -> bool {
        if !self.can_attach_screenshot() {
            return false;
        }
        let session = self.active.clone();
        let name = format!("screenshot-{}.png", chrono::Local::now().format("%Y-%m-%d-%H%M%S"));
        self.queue_attachment(&session, name, move || Ok(bytes), cx);
        true
    }

    pub(super) fn add_attachments(&mut self, session: String, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.chat_capabilities.is_none() || paths.is_empty() {
            return;
        }
        if !self.has_attachment_room(&session, paths.len(), cx) {
            return;
        }
        for path in paths {
            self.queue_dropped_path(&session, path, cx);
        }
        cx.notify();
    }

    #[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
    pub(super) fn shot_last_attachment_id(&self) -> u64 {
        self.chats[&self.active].attachments.last().unwrap().id
    }

    pub(super) fn restore_composer_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let drafts = self.composer_markdown.drafts.get_or_insert_with(|| {
            ComposerDrafts::load(self.appearance.settings_path())
        });
        let workspace = self.workspaces.active.as_deref();
        if let Some(workspace) = workspace.filter(|_| self.threads_loaded) {
            drafts.reconcile_threads(workspace, self.connection_epoch, &self.threads, cx);
        }
        let shown = workspace.map(|workspace| (workspace.to_owned(), self.active.clone(), self.connection_epoch));
        if drafts.shown == shown {
            return false;
        }
        drafts.shown = shown;
        let draft = workspace.and_then(|workspace| drafts.drafts.get(workspace, &self.active));
        let text = draft.map_or("", |draft| draft.text.as_str());
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            if !text.is_empty() {
                input.move_caret_to(text.len(), false, cx);
            }
        });
        if let Some((draft, chat)) = draft.zip(self.chats.get_mut(&self.active)) {
            for item in &draft.attachments {
                if !chat.attachments.iter().any(|draft| {
                    matches!(&draft.state, AttachmentState::Saved(path) if path == &item.path)
                }) {
                    chat.attachments.push(DraftAttachment {
                        id: NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed),
                        name: item.name.clone(),
                        session: self.active.clone(),
                        state: AttachmentState::Saved(item.path.clone()),
                        preview: None,
                    });
                }
            }
        }
        true
    }

    pub(super) fn record_composer_text(&mut self, cx: &mut Context<Self>) {
        let Some(drafts) = self.composer_markdown.drafts.as_mut() else { return };
        let Some((workspace, thread, epoch)) = drafts.shown.as_ref() else { return };
        if self.workspaces.active.as_ref() != Some(workspace) || self.active != *thread
            || self.connection_epoch != *epoch
        {
            return;
        }
        if drafts.drafts.set_text(workspace, thread, &self.input.read(cx).value()) {
            drafts.schedule_save(cx);
        }
    }

    pub(super) fn flush_composer_drafts(&mut self) {
        if let Some(drafts) = &mut self.composer_markdown.drafts {
            drafts.flush();
        }
    }

    fn record_composer_attachments(&mut self, thread: &str, cx: &mut Context<Self>) {
        let Some(drafts) = self.composer_markdown.drafts.as_mut() else { return };
        let Some(workspace) = self.workspaces.active.as_deref() else { return };
        let attachments = self.chats[thread].attachments.iter().filter_map(|item| {
            let AttachmentState::Saved(path) = &item.state else { return None };
            Some(SavedAttachment { name: item.name.clone(), path: path.clone() })
        }).collect::<Vec<_>>();
        if drafts.drafts.edit(workspace, thread, |draft| {
            if draft.attachments == attachments {
                return false;
            }
            draft.attachments = attachments;
            true
        }) {
            drafts.schedule_save(cx);
        }
    }

    fn discard_composer_draft(&mut self, cx: &mut Context<Self>) {
        if let Some((drafts, workspace)) = self.composer_markdown.drafts.as_mut()
            .zip(self.workspaces.active.as_deref())
        {
            if drafts.drafts.retain_threads(workspace, |id| id != self.active) {
                drafts.schedule_save(cx);
            }
        }
    }

    pub(super) fn composer_command_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL || !self.threads_loaded
            || window.has_active_dialog(cx) || !self.input.read(cx).focus_handle(cx).is_focused(window)
        {
            return false;
        }
        let preedit = composing(&self.input, window, cx);
        let text = self.input.read(cx).value();
        let backend = self.composer_backend();
        let Some(action) = self.composer_markdown.command_palette.key(&text, backend, key, preedit) else {
            return false;
        };
        match action {
            CommandKey::Move(index) => self.composer_markdown.command_palette.selected = index,
            CommandKey::Dismiss => self.composer_markdown.command_palette.dismissed = true,
            CommandKey::Complete(name) => self.complete_command(name, window, cx),
            CommandKey::Send => self.send_prompt(window, cx),
        }
        cx.notify();
        true
    }

    fn composer_backend(&self) -> AgentBackend {
        AgentBackend::for_model(self.chats[&self.active].settings.model.as_deref())
    }

    fn complete_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let caret = self.input.read(cx).value().len();
        self.input.update(cx, |input, cx| {
            input.replace_range_atomically(0..caret, &format!("/{name}"), window, cx);
        });
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn render_command_palette(&self, text: &str, window: &Window, cx: &Context<Self>) -> Option<impl IntoElement> {
        let palette = &self.composer_markdown.command_palette;
        if self.protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL || !self.threads_loaded
            || palette.dismissed || self.composer_markdown.composing || !self.input.read(cx).focus_handle(cx).is_focused(window)
        {
            return None;
        }
        let candidates = command_candidates(text, self.composer_backend());
        if candidates.is_empty() { return None; }
        let selected = palette.selected.min(candidates.len() - 1);
        let theme = cx.theme();
        let background = theme.background;
        let border = theme.border;
        let muted = theme.muted_foreground;
        let accent = theme.secondary;
        let font = &self.appearance.code_font;
        Some(v_flex().id("composer-command-palette").absolute().bottom(relative(1.)).left_0().max_w_full()
            .rounded_md().border_1().border_color(border).bg(background).shadow_sm().p_1()
            .children(candidates.into_iter().enumerate().map(|(index, spec)| {
                let name = spec.name;
                h_flex().id(format!("composer-command-{name}")).gap_2().px_2().py_0p5()
                    .rounded_sm().cursor_pointer().text_xs()
                    .when(index == selected, |row| row.bg(accent))
                    .hover(|row| row.bg(accent))
                    .child(div().font(font.clone()).child(spec.usage()))
                    .child(div().min_w_0().text_color(muted).truncate().child(tr!(spec.description)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.complete_command(name, window, cx);
                    }))
            })))
    }

    fn composer_enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.secondary || action.shift || window.has_active_dialog(cx) {
            return;
        }
        if composing(&self.input, window, cx) {
            cx.stop_propagation();
            return;
        }
        let input = self.input.read(cx);
        if let Some((range, text)) = continue_line(&input.value(), input.selected_range()) {
            self.input.update(cx, |state, cx| state.replace_range_atomically(range, &text, window, cx));
            cx.stop_propagation();
        }
    }

    fn composer_arrow(&mut self, down: bool, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        if composing(&self.input, window, cx) {
            cx.stop_propagation();
            return;
        }
        let input = self.input.read(cx);
        if !extend && input.vertical_arrow_at_edge(down, cx) {
            let line = window.line_height();
            self.chats[&self.active].scroller.scroll_by(if down { line } else { -line });
            self.history_scrolled(cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let Some((row, rows)) = input.visual_cursor_row(cx) else { return };
        if let Some(target) = visual_edge_target(row, rows, down, input.text().len()) {
            self.input.update(cx, |state, cx| state.move_caret_to(target, extend, cx));
            cx.stop_propagation();
        }
    }

    pub(super) fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.threads_loaded || composing(&self.input, window, cx) {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let command = text.strip_prefix('/').and_then(|body| body.split(char::is_whitespace).next());
        let explicit_escalation = command == Some("escalate") && self.protocol >= fuzukue_proto::LIVE_MODELS_PROTOCOL;
        if self.protocol >= fuzukue_proto::SLASH_COMMAND_PROTOCOL && fuzukue_proto::slash::is_command(&text) && !explicit_escalation {
            if self.out.send(ClientMsg::Prompt { session: self.active.clone(), text }).is_ok() {
                self.input.update(cx, |input, cx| input.clean(window, cx));
                self.record_composer_text(cx);
                cx.notify();
            }
            return;
        }
        let chat = self.chats.get_mut(&self.active).unwrap();
        let worker_running = chat.active_worker().is_some();
        if self.protocol < 14 && chat.codex_selected() && chat.busy && !worker_running {
            return;
        }
        let mut paths = Vec::new();
        for item in &chat.attachments {
            match &item.state {
                AttachmentState::Saved(path) if !chat.file_pending(path) => paths.push(path.clone()),
                _ => return,
            }
        }
        let session = self.active.clone();
        if paths.is_empty() && text.trim().is_empty() {
            return;
        }
        let msg = if let Some(job) = chat.worker_target.clone().filter(|_| self.protocol >= 14 && !explicit_escalation) {
            ClientMsg::WorkerPrompt { session, job, text, paths }
        } else if worker_running && (12..14).contains(&self.protocol) {
            ClientMsg::RolePrompt { session, text, paths, to_worker: !chat.delegate_to_partner }
        } else if paths.is_empty() {
            ClientMsg::Prompt { session, text }
        } else if self.chat_capabilities.is_some() {
            ClientMsg::PromptWithAttachments {
                session,
                text,
                paths,
            }
        } else {
            return;
        };
        if self.out.send(msg).is_ok() {
            chat.attachments.clear();
            self.discard_composer_draft(cx);
            self.input.update(cx, |input, cx| input.clean(window, cx));
            cx.notify();
        }
    }

    fn update_chat_settings(
        &mut self,
        session: &str,
        change: impl FnOnce(&mut ChatSettings),
        cx: &mut Context<Self>,
    ) {
        let Some(capabilities) = &self.chat_capabilities else {
            return;
        };
        let chat = self.chats.get_mut(session).unwrap();
        let mut settings = chat.settings.clone();
        change(&mut settings);
        let levels = self.codex_models.iter()
            .find(|model| Some(model.slug.as_str()) == settings.model.as_deref())
            .map_or(capabilities.effort_levels.as_slice(), |model| &model.effort_levels);
        settings.effort = settings.effort.filter(|level| levels.contains(level));
        if self
            .out
            .send(ClientMsg::SetChatSettings {
                session: session.to_owned(),
                settings: settings.clone(),
            })
            .is_ok()
        {
            chat.settings = settings;
            cx.notify();
        }
    }

    pub(super) fn sync_chat_activity_clock(&mut self, cx: &mut Context<Self>) {
        let chat = &self.chats[&self.active];
        let has_activity = self.chat_waiting.has_background_commands(&self.active)
            || self.chat_waiting.static_goal_status(&self.active, chat).is_none()
                && chat.has_activity(self.chat_waiting.reason(&self.active));
        if !has_activity {
            self.chat_activity_clock = None;
        } else if self.chat_activity_clock.is_none() {
            self.chat_activity_clock = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }));
        }
    }

    pub(super) fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ime = composing(&self.input, window, cx);
        let theme = cx.theme();
        let chat = &self.chats[&self.active];
        let text = self.input.read(cx).value();
        let enabled = self.chat_capabilities.is_some();
        let worker_running = chat.active_worker().is_some();
        let is_command = self.protocol >= fuzukue_proto::SLASH_COMMAND_PROTOCOL && fuzukue_proto::slash::is_command(&text);
        let compacting = self.chat_waiting.reason(&self.active).is_some_and(|wait| wait.reason == ChatWaitReason::Compacting);
        let turn_running = compacting || (chat.busy && !self.thread_usage_paused(&self.active));
        let running_job = chat.pending_jobs()
            .filter(|(_, _, job)| job.started_at_ms.is_some())
            .min_by_key(|(row, id, _)| (chat.worker_target.as_deref() != Some(*id), *row))
            .map(|(_, id, job)| (id, delegate_activity_name(job)))
            .or_else(|| self.scoped_worker(&self.active).map(|id| (id, id)));
        let draining = self.safe_close.update_draining().unwrap_or_else(|| {
            self.self_update.status.as_ref().is_some_and(|status| status.wait.is_some())
        });
        let has_content = !text.trim().is_empty() || !chat.attachments.is_empty();
        let action_state = composer_action(has_content, turn_running || running_job.is_some(), ime, draining);
        let blocked = chat
            .attachments
            .iter()
            .any(|item| !matches!(&item.state, AttachmentState::Saved(path) if !chat.file_pending(path)))
            || !chat.attachments.is_empty() && !enabled
            || self.protocol < 14 && chat.codex_selected() && chat.busy && !worker_running;
        let can_send = self.threads_loaded && (is_command || !blocked && (has_content || ime));
        let attachment_menu = cx.weak_entity();
        let can_choose_files = enabled && chat.attachments.len() < MAX_ATTACHMENTS;
        let can_choose_folder = can_choose_files && self.chat_capabilities.as_ref()
            .is_some_and(|capabilities| capabilities.folder_attachments);
        let copying_folder = self.copying_folder();
        let chips = h_flex()
            .flex_wrap()
            .gap_2()
            .children(chat.attachments.iter().map(|draft| {
                let id = draft.id;
                let (label, hint) = match &draft.state {
                    AttachmentState::Reading => (tr!("chat-loading-attachment", name = draft.name.to_string()), None),
                    AttachmentState::Uploading { bytes, sent } => (
                        format!(
                            "{}{STATUS_SEPARATOR}{}%",
                            draft.name,
                            if bytes.is_empty() {
                                100
                            } else {
                                (sent * 100) / bytes.len() as u64
                            }
                        ),
                        None,
                    ),
                    AttachmentState::Drop(state) => (state.label(&draft.name), None),
                    AttachmentState::Saved(path) => (attachment_label(&draft.name, path), None),
                    AttachmentState::Failed(reason) => {
                        (format!("{}{STATUS_SEPARATOR}{reason}", draft.name), Some(reason.clone()))
                    }
                };
                let remove = inline_button(SharedString::from(format!("remove-attachment-{id}")))
                    .icon(super::icons::Symbol::X.icon())
                    .accessibility_label(tr!("chat-remove-named-attachment", name = draft.name.to_string()))
                    .tooltip(hint.unwrap_or_else(|| tr!("chat-remove-attachment").into()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let chat = this.chats.get_mut(&this.active).unwrap();
                        if let Some(index) =
                            chat.attachments.iter().position(|draft| draft.id == id)
                        {
                            let draft = chat.attachments.remove(index);
                            if let AttachmentState::Drop(state) = &draft.state {
                                state.cancel();
                            }
                            if draft.transferring() {
                                let _ = this.out.send(ClientMsg::CancelAttachment { id });
                            } else if matches!(draft.state, AttachmentState::Saved(_)) {
                                this.record_composer_attachments(&draft.session, cx);
                            }
                        }
                        cx.notify();
                    }));
                let saved_path = match &draft.state {
                    AttachmentState::Saved(path) => Some(path),
                    _ => None,
                };
                let preview = AttachmentThumbnail {
                    id: format!("chat-draft-image-{id}").into(),
                    name: draft.name.clone(),
                    label,
                    decoded: draft.preview.clone().filter(|_| !matches!(draft.state, AttachmentState::Failed(_))),
                    saved: saved_path.filter(|path| !fuzukue_proto::is_folder_attachment(path))
                        .map(|path| (self.connection_epoch, self.http.clone(), path.clone())),
                };
                h_flex().max_w_full().gap_1().items_start()
                    .child(preview)
                    .children(matches!(draft.state, AttachmentState::Drop(DropState::Preview(_))).then(|| {
                        inline_button(SharedString::from(format!("copy-folder-{id}")))
                            .label(tr!("chat-folder-copy"))
                            .disabled(copying_folder)
                            .tooltip(tr!("chat-folder-copy-exclusions"))
                            .on_click(cx.listener(move |this, _, _, cx| this.confirm_folder_copy(id, cx)))
                    }))
                    .children(saved_path.and_then(|path| {
                        self.attachment_filing_menu(&draft.session, path, cx)
                    }))
                    .child(remove)
            }));
        let input = Textarea::new(&self.input)
            .on_paste({
                let workbench = cx.weak_entity();
                move |item, _, cx| workbench.update(cx, |this, cx| this.paste_attachment(item, cx)).unwrap_or(false)
            })
            .appearance(false)
            .xsmall()
            .pl_0()
            .pr(px(6.))
            .aria_label(tr!("chat-enter-a-message"))
            .text_base()
            .line_height(UI_LINE_HEIGHT);
        // Centre one line against the buttons without adding space between wrapped lines.
        let input_padding = (COMPOSER_BUTTON_SIZE - ui_line_height_in_rems(rems(1.))) * 0.5;
        let input = div().flex_1().min_w_0().py(input_padding)
            .capture_action(cx.listener(Self::composer_enter))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                this.composer_arrow(false, false, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                this.composer_arrow(true, false, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectUp, window, cx| {
                this.composer_arrow(false, true, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectDown, window, cx| {
                this.composer_arrow(true, true, window, cx);
            }))
            .child(input);
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        let input = input.id("chat-input");
        let surfaces = ThemeSurfaces::current(cx);
        let (disabled_fill, disabled_icon) = surfaces.composer_disabled(theme.is_dark());
        let (stop_fill, stop_icon) = surfaces.composer_stop(theme.is_dark());
        let action = match action_state {
            ComposerAction::Stop => {
                let interrupt = || ClientMsg::Interrupt { session: self.active.clone() };
                let (command, label) = match running_job {
                    Some((job, title)) if !turn_running => {
                        let command = if self.protocol >= 14 {
                            ClientMsg::StopWorker { session: self.active.clone(), job: job.to_owned() }
                        } else {
                            interrupt()
                        };
                        (command, tr!("chat-stop-worker", title = title))
                    }
                    _ => (
                        interrupt(),
                        tr!("chat-stop-reply").to_owned(),
                    ),
                };
                Button::new("stop-prompt")
                    .icon(IconName::Square)
                    .accessibility_label(label.clone())
                    .tooltip(label)
                    .on_click(cx.listener(move |this, _, _, _| {
                        let _ = this.out.send(command.clone());
                    }))
                    .custom(ButtonCustomVariant::new(cx)
                        .color(stop_fill).foreground(stop_icon).hover(stop_fill).active(stop_fill))
            }
            ComposerAction::Empty | ComposerAction::Send { .. } => {
                let empty = action_state == ComposerAction::Empty;
                let held = matches!(action_state, ComposerAction::Send { held: true, .. });
                let label = if empty { tr!("chat-type-a-message") } else { tr!("chat-send") };
                let tooltip = if empty {
                    label
                } else if held {
                    tr!("chat-send-held-for-update")
                } else {
                    tr!("chat-send-ctrl-enter")
                };
                let button = Button::new("send-prompt")
                    .icon(IconName::ArrowUp)
                    .accessibility_label(label)
                    .tooltip(tooltip)
                    .on_click(cx.listener(|this, _, window, cx| this.send_prompt(window, cx)));
                if can_send && !empty {
                    super::appearance::accent_button(button, true, cx)
                } else {
                    button.ghost().bg(disabled_fill).text_color(disabled_icon).disabled(true)
                }
            }
        };
        let show_role_target = (12..14).contains(&self.protocol) && worker_running;
        let worker_target = chat.worker_target.as_ref().filter(|_| self.protocol >= 14)
            .and_then(|id| {
                let row = chat.by_item.get(id)?;
                let Content::Delegate(_) = &chat.rows[*row].content else { return None };
                Some(
                    composer_text_button("clear-worker-target", cx)
                        .label(tr!("tiers-target-job"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.chats.get_mut(&this.active).unwrap().worker_target = None;
                            cx.notify();
                        })),
                )
            });
        let composer_bounds = chat.composer_bounds.clone();
        v_flex()
            .relative()
            .w_full()
            .flex_none()
            .gap_1()
            .children(self.render_command_palette(&text, window, cx).map(gpui_kit::deferred))
            .children((!chat.attachments.is_empty()).then_some(chips))
            .children((worker_target.is_some() || show_role_target).then(|| {
                h_flex().gap_1().flex_wrap().self_start().max_w_full()
                    .children(worker_target)
                    .children(show_role_target.then(|| {
                        composer_text_button("chat-role-target", cx)
                            .label(if chat.delegate_to_partner { tr!("chat-to-advisor") } else { tr!("chat-to-worker") })
                            .tooltip(tr!("chat-change-recipient"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let chat = this.chats.get_mut(&this.active).unwrap();
                                chat.delegate_to_partner = !chat.delegate_to_partner;
                                cx.notify();
                            }))
                    }))
            }))
            .child(
                h_flex().id("composer-input-frame").w_full()
                    .on_prepaint(move |bounds, _, _| composer_bounds.set(bounds))
                    .items_end()
                    .rounded(px(23.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(surfaces.chat)
                    .pr(COMPOSER_END_PADDING.to_pixels(window.rem_size()) - px(1.))
                    .py_1p5()
                    .child(
                        Button::new("attach-file")
                            .ghost()
                            .w(COMPOSER_BUTTON_SIZE)
                            .h(COMPOSER_BUTTON_SIZE)
                            .pl_2()
                            .rounded(px(16.))
                            .text_color(theme.muted_foreground)
                            .icon(Symbol::Plus.icon())
                            .accessibility_label(tr!("screenshot-attachment-menu"))
                            .tooltip(if enabled {
                                tr!("chat-attach-files-up-to-50-mib-each")
                            } else {
                                tr!("chat-connect-to-a-server-that-supports-attachments")
                            })
                            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                                let files = attachment_menu.clone();
                                let folder = attachment_menu.clone();
                                let screenshot = attachment_menu.clone();
                                menu.item(PopupMenuItem::new(tr!("chat-attach-files"))
                                    .disabled(!can_choose_files)
                                    .on_click(move |_, _, cx| {
                                        let _ = files.update(cx, |this, cx| this.choose_attachments(false, cx));
                                    }))
                                    .item(PopupMenuItem::new(tr!("chat-attach-folder"))
                                        .disabled(!can_choose_folder)
                                        .on_click(move |_, _, cx| {
                                            let _ = folder.update(cx, |this, cx| this.choose_attachments(true, cx));
                                        }))
                                    .item(PopupMenuItem::new(tr!("screenshot-capture")).icon(Symbol::Camera.icon())
                                        .on_click(move |_, window, cx| {
                                            let _ = screenshot.update(cx, |this, cx| this.capture_screenshot(window, cx));
                                        }))
                            }),
                    )
                    .child(input)
                    .child(
                        h_flex().flex_none().ml_1().w(COMPOSER_ACTION_SIZE)
                            .child(action.w(COMPOSER_ACTION_SIZE)
                                .h(COMPOSER_ACTION_SIZE)
                                .rounded(COMPOSER_ACTION_SIZE / 2.)),
                    ),
            )
            .child(
                composer_footer()
                    .pr(COMPOSER_END_PADDING.to_pixels(window.rem_size())
                        + (COMPOSER_ACTION_SIZE - COMPOSER_FOOTER_HEIGHT.to_pixels(window.rem_size())) / 2.)
                    .child(h_flex().max_w_full().justify_end().flex_wrap().gap_1()
                        .children(self.chat_permission_menu(cx))
                        .children(self.worker_approval_menu(cx))
                        .children(self.composer_context_ring(cx))),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_attachments(this.active.clone(), paths.paths().to_vec(), cx);
            }))
    }

    fn composer_context_ring(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.active.is_empty() {
            return None;
        }
        let mut groups = Vec::new();
        let mut contexts = Vec::new();
        for ((session, _), event) in &self.stats {
            if session != &self.active {
                continue;
            }
            let ServerEvent::SessionStats {
                agent, model, effort, cost_usd, context_tokens, context_window,
                input_tokens, output_tokens, turns, ..
            } = event else { continue };
            let backend = match agent.as_str() {
                "claude" => "Claude",
                "codex" => "Codex",
                other => other,
            };
            let model = if model.is_empty() { tr!("chat-unknown-model") } else { model };
            let mut summary = tr!("chat-usage-turns", effort = (effort_label(effort.as_deref())).to_string(), turns = turns);
            if let Some(cost) = cost_usd {
                summary.push_str(&format!("{STATUS_SEPARATOR}${cost:.2}"));
            }
            let context = ContextUsage { used: *context_tokens, limit: *context_window };
            contexts.push(context);
            groups.push(vec![
                format!("{backend}{STATUS_SEPARATOR}{model}"),
                summary,
                tr!("chat-input-output-tokens", input_tokens = input_tokens.to_string(), output_tokens = output_tokens.to_string()),
                context.label(),
            ]);
        }
        // Each backend has its own context window. The ring tracks the fullest one.
        let (tooltip, highest_usage) = context_usage_tooltip(contexts);
        let theme = cx.theme();
        let color = if highest_usage >= 90. {
            theme.danger
        } else if highest_usage >= 75. {
            theme.warning
        } else {
            theme.muted_foreground
        };
        let ring = ProgressCircle::new("context-progress")
            .size(px(14.))
            .color(color)
            .value(highest_usage);
        let button = inline_button("composer-context-ring")
            .w(COMPOSER_FOOTER_HEIGHT)
            .h(COMPOSER_FOOTER_HEIGHT)
            .px_0()
            .rounded(px(999.))
            .accessibility_label(tr!("chat-context-usage"))
            .child(ring);
        let groups = std::rc::Rc::new(groups);
        Some(
            Popover::new(format!("composer-context-popover-{}", self.active))
                .anchor(Anchor::BottomRight)
                .trigger(ContextRingTrigger { button, tooltip, composer_bounds: self.chats[&self.active].composer_bounds.clone() })
                .content(move |_, _, cx| context_usage_details(&groups, cx.theme().muted_foreground))
                .into_any_element(),
        )
    }

    pub(super) fn thread_usage_paused(&self, session: &str) -> bool {
        self.chats.get(session).is_some_and(|chat| chat.paused.is_some())
            || self.transcripts.scoped && self.chat_waiting.usage_paused(session)
    }

    pub(super) fn thread_working(&self, session: &str) -> bool {
        self.chats.get(session).is_some_and(Chat::working) || self.scoped_worker(session).is_some()
    }

    pub(super) fn chat_model_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.active.is_empty() {
            return None;
        }
        let chat = &self.chats[&self.active];
        self.role_model_menu(
            RoleSettings::PartnerThread(self.active.clone()), chat.settings.clone(),
            self.protocol < fuzukue_proto::LIVE_MODELS_PROTOCOL
                && (self.thread_working(&self.active) || self.thread_usage_paused(&self.active)), cx,
        )
    }

    pub(super) fn agent_override_menu(
        &self, id: SharedString, target: AgentTarget, default: AgentSelection,
        selection: Option<&AgentSelection>, cx: &Context<Self>,
    ) -> AnyElement {
        let reset = match &target {
            AgentTarget::Thread(_, _) => tr!("chat-use-shared-settings"),
            AgentTarget::Fallback(_) => tr!("chat-no-fallback-model"),
            AgentTarget::Setup => tr!("model-setup-use-recommended"),
            _ => tr!("chat-use-defaults"),
        };
        let customized = selection.is_some();
        let current = selection.cloned().unwrap_or(default);
        let codex = self.codex_models.clone();
        let claude = self.chat_capabilities.as_ref().map(|cap| cap.effort_levels.clone()).unwrap_or_default();
        let (claude_available, codex_available) = self.model_backends();
        let cli_default = !matches!(target, AgentTarget::WorkerDefault | AgentTarget::Role(fuzukue_proto::AgentModelRole::Escalation));
        let codex_cli_default = cli_default && !matches!(target, AgentTarget::PartnerDefault | AgentTarget::Setup);
        let model = current.model.as_deref().map_or(tr!("chat-cli-default"), |model| {
            codex.iter().find(|entry| entry.slug == model).map_or_else(|| model_label(model), |entry| &entry.label)
        });
        let mut label = format!("{}{STATUS_SEPARATOR}{model}", current.backend.label());
        if let Some(effort) = &current.effort {
            label.push_str(&format!("{STATUS_SEPARATOR}{}", effort_label(Some(effort))));
        }
        if !customized && matches!(target, AgentTarget::Fallback(_)) {
            label = tr!("chat-unspecified").into();
        }
        if !customized && matches!(target, AgentTarget::Role(fuzukue_proto::AgentModelRole::WorkerTier(_))) {
            label = tr!("model-role-automatic-routing").into();
        }
        let entity = cx.weak_entity();
        composer_text_button(id, cx)
            .accessibility_label(status_speech(&label))
            .child(status_row(&label, cx.theme().muted_foreground).truncate())
            .dropdown_caret(true).tooltip(tr!("chat-backend-model-and-reasoning-effort"))
            .dropdown_menu(move |mut menu, _, _| {
                let item = |label: String, checked, choice: Option<AgentSelection>| {
                    let entity = entity.clone();
                    let target = target.clone();
                    PopupMenuItem::new(label).checked(checked).on_click(move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| target.apply(this, choice.clone(), cx));
                    })
                };
                menu = menu.item(item(reset.into(), !customized, None));
                for (backend, has_default, models) in [
                    (AgentBackend::Claude, cli_default, claude_available.then(|| CLAUDE_MODELS.iter()
                        .map(|id| ((*id).into(), model_label(id).into())).collect::<Vec<(String, String)>>())),
                    (AgentBackend::Codex, codex_cli_default, codex_available.then(|| codex.iter()
                        .map(|model| (model.slug.clone(), model.label.clone())).collect())),
                ] {
                    let Some(models) = models else { continue };
                    if models.is_empty() && !has_default { continue; }
                    menu = menu.separator().label(backend.label());
                    for (model, label) in has_default.then(|| (None, tr!("chat-cli-default").into())).into_iter()
                        .chain(models.into_iter().map(|(id, label)| (Some(id), label))) {
                        let checked = backend == current.backend && model == current.model;
                        let effort = checked.then(|| current.effort.clone()).flatten();
                        menu = menu.item(item(label, checked, Some(AgentSelection { backend, model, effort })));
                    }
                }
                let efforts = match current.backend {
                    AgentBackend::Claude if claude_available => &claude[..],
                    AgentBackend::Codex if codex_available => codex.iter().find(|m| Some(m.slug.as_str()) == current.model.as_deref())
                        .map_or(&[][..], |model| &model.effort_levels[..]),
                    _ => &[],
                };
                if !efforts.is_empty() {
                    menu = menu.separator().label(tr!("chat-reasoning-effort"));
                    for effort in std::iter::once(None).chain(efforts.iter().cloned().map(Some)) {
                        let mut next = current.clone();
                        next.effort = effort.clone();
                        menu = menu.item(item(effort.as_deref().map(|effort| effort_label(Some(effort)).to_owned()).unwrap_or_else(|| tr!("chat-cli-default").into()), effort == current.effort, Some(next)));
                    }
                }
                menu
            }).into_any_element()
    }

    fn fallback_menu(&self, role: &str, default: AgentSelection, cx: &Context<Self>) -> AnyElement {
        self.agent_override_menu(format!("agent-fallback-{role}").into(), AgentTarget::Fallback(role.into()),
            default, self.agent_settings.fallbacks.get(role), cx)
    }

    pub(super) fn render_agent_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let roles = v_flex().gap_3()
            .children((self.protocol < fuzukue_proto::AGENT_MODELS_PROTOCOL).then(|| v_flex().gap_1().items_start().child(tr!("chat-progress-summary-model"))
                .child(self.agent_override_menu("summary-model-picker".into(), AgentTarget::Summary,
                    AgentSelection::summary_default(), self.agent_settings.summary.as_ref(), cx))))
            .children((self.protocol >= fuzukue_proto::PLAN_PROTOCOL).then(|| v_flex().gap_1().items_start()
                .children((self.protocol < fuzukue_proto::ONE_VOICE_TIERS_PROTOCOL).then(|| v_flex().gap_1().items_start()
                    .child(tr!("chat-fallback-advisor-same-backend"))
                    .child(self.fallback_menu("front_desk", AgentSelection::from_model(
                        self.partner_default.as_ref().and_then(|settings| settings.model.clone()), None), cx))))
                .child(tr!("chat-fallback-progress-summary"))
                .child(self.fallback_menu("summary", AgentSelection::summary_default(), cx))
                .child(tr!("chat-fallback-default-worker"))
                .child(self.fallback_menu("worker", AgentSelection::from_model(
                    self.delegate_settings.as_ref().map(|settings| settings.model.clone()), None), cx))));
        v_flex().gap_4()
            .children((self.protocol < fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL).then(|| div().text_lg().child(tr!("chat-agents"))))
            .children((self.protocol >= fuzukue_proto::AGENT_MODELS_PROTOCOL).then(|| self.render_model_roles(cx)))
            .children(self.render_tier_totals(cx))
            .children(self.thread_info.agents.iter().map(|agent| {
                let default = agent.default_selection();
                let model = default.model.as_deref().unwrap_or(tr!("chat-cli-default"));
                v_flex().gap_1().items_start()
                    .child(div().font_semibold().child(if agent.name == "planner" { tr!("chat-planner").to_owned() } else { agent.name.clone() }))
                    .child(div().text_sm().child(agent.description.clone()))
                    .children((agent.name == "planner" && self.protocol >= fuzukue_proto::AGENT_MODELS_PROTOCOL)
                        .then(|| div().text_sm().text_color(theme.muted_foreground).child(tr!("model-role-planner-recommendation"))))
                    .children((self.protocol >= fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL).then(|| self.render_agent_policy(agent, cx)))
                    .child(div().text_sm().text_color(theme.muted_foreground)
                        .child(status_row(
                            &tr!("chat-agent-default-model", backend = default.backend.label().to_string(), model = model.to_string()),
                            cx.theme().muted_foreground,
                        )))
                    .child(self.agent_override_menu(format!("agent-model-{}", agent.name).into(),
                        AgentTarget::Default(agent.name.clone()), default.clone(),
                        self.agent_settings.overrides.get(&agent.name), cx))
                    .children((self.protocol >= fuzukue_proto::PLAN_PROTOCOL).then(|| v_flex().items_start().gap_1()
                        .child(tr!("chat-fallback-model")).child(self.fallback_menu(&agent.name, default, cx))))
            }))
            .child(roles)
    }

    pub(super) fn chat_effort_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.active.is_empty() {
            return None;
        }
        let capabilities = self.chat_capabilities.as_ref()?;
        let chat = &self.chats[&self.active];
        let settings = chat.settings.clone();
        let levels = self.codex_models.iter()
            .find(|model| Some(model.slug.as_str()) == settings.model.as_deref())
            .map_or_else(|| capabilities.effort_levels.clone(), |model| model.effort_levels.clone());
        let entity = cx.weak_entity();
        let target = RoleSettings::PartnerThread(self.active.clone());
        Some(
            composer_picker("effort-picker", effort_label(settings.effort.as_deref()).to_owned(), cx)
                .disabled(levels.is_empty() || self.protocol < fuzukue_proto::LIVE_MODELS_PROTOCOL
                    && (self.thread_working(&self.active) || self.thread_usage_paused(&self.active)))
                .tooltip(tr!("chat-reasoning-effort-applies-next-turn"))
                .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                    role_effort_items(menu, &levels, &settings, &target, &entity)
                })
                .into_any_element(),
        )
    }

    pub(super) fn role_defaults(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex().gap_2()
            .children(self.partner_default.clone().filter(|_| self.protocol < fuzukue_proto::AGENT_MODELS_PROTOCOL).map(|settings| {
                h_flex().gap_2().child(tr!("chat-default-advisor"))
                    .children(self.role_model_menu(RoleSettings::PartnerDefault, settings, false, cx))
            }))
            .children(self.delegate_settings.as_ref().filter(|_| self.protocol < fuzukue_proto::AGENT_MODELS_PROTOCOL).map(|settings| {
                h_flex().gap_2().child(tr!("chat-default-worker"))
                    .children(self.role_model_menu(RoleSettings::WorkerDefault, worker_choice(settings), false, cx))
            }))
            .children(self.review_settings.as_ref().map(|settings| {
                let style = ChatSettings { model: settings.style_model.clone(), effort: settings.style_effort.clone(), ..Default::default() };
                let math = worker_choice(&fuzukue_proto::DelegateSettings {
                    model: settings.math_model.clone(), effort: settings.math_effort.clone(),
                });
                v_flex().gap_2().items_start().child(tr!("chat-automatic-review"))
                    .children((self.protocol < 14).then(|| h_flex().gap_2().child(tr!("chat-review-writing"))
                        .children(self.role_model_menu(RoleSettings::StyleReviewer, style, false, cx))))
                    .children((self.protocol < 14).then(|| h_flex().gap_2().child(tr!("chat-math"))
                        .children(self.role_model_menu(RoleSettings::MathReviewer, math, false, cx))))
                    .child(inline_button("review-side-thread").label(if settings.apply_in_side_thread {
                        tr!("chat-apply-findings-in-a-separate-thread")
                    } else {
                        tr!("chat-save-findings-as-a-draft")
                    }).dropdown_caret(true).dropdown_menu({
                        let entity = cx.weak_entity();
                        let selected = settings.apply_in_side_thread;
                        move |mut menu, _, _| {
                            for (value, label) in [(false, tr!("chat-save-as-draft")), (true, tr!("chat-apply-in-a-separate-thread"))] {
                                let entity = entity.clone();
                                menu = menu.item(PopupMenuItem::new(label).checked(selected == value)
                                    .on_click(move |_, _, cx| {
                                        let _ = entity.update(cx, |this, _| {
                                            if let Some(mut settings) = this.review_settings.clone() {
                                                settings.apply_in_side_thread = value;
                                                let _ = this.out.send(ClientMsg::SetReviewSettings { settings });
                                            }
                                        });
                                    }));
                            }
                            menu
                        }
                    }))
            }))
    }

    pub(super) fn thread_worker_menu(&self, thread: &ThreadView, cx: &Context<Self>) -> Option<AnyElement> {
        if self.protocol < 12 {
            return None;
        }
        let settings = thread.worker_settings.as_ref()?;
        self.role_model_menu(
            RoleSettings::WorkerThread(thread.id.clone()), worker_choice(settings),
            self.chats.get(&thread.id).and_then(Chat::active_worker).is_some()
                || self.scoped_worker(&thread.id).is_some(), cx,
        )
    }

    fn change_role(&mut self, target: &RoleSettings, settings: ChatSettings, cx: &mut Context<Self>) {
        let message = match target {
            RoleSettings::PartnerThread(session) => {
                self.update_chat_settings(session, |current| *current = settings, cx);
                return;
            }
            RoleSettings::PartnerDefault => ClientMsg::SetPartnerDefault { settings },
            RoleSettings::StyleReviewer | RoleSettings::MathReviewer => {
                let Some(mut review) = self.review_settings.clone() else { return };
                if matches!(target, RoleSettings::StyleReviewer) {
                    review.style_model = settings.model;
                    review.style_effort = settings.effort;
                } else {
                    let Some(model) = settings.model else { return };
                    review.math_model = model;
                    review.math_effort = settings.effort.unwrap_or_default();
                }
                ClientMsg::SetReviewSettings { settings: review }
            }
            RoleSettings::WorkerDefault | RoleSettings::WorkerThread(_) => {
                let Some(model) = settings.model else { return };
                let settings = fuzukue_proto::DelegateSettings { model, effort: settings.effort.unwrap_or_default() };
                match target {
                    RoleSettings::WorkerThread(session) => ClientMsg::SetThreadWorker { session: session.clone(), settings },
                    _ => ClientMsg::SetDelegateSettings { settings },
                }
            }
        };
        let _ = self.out.send(message);
    }

    fn role_model_menu(&self, target: RoleSettings, settings: ChatSettings, disabled: bool, cx: &Context<Self>) -> Option<AnyElement> {
        let capabilities = self.chat_capabilities.as_ref()?;
        let composer = matches!(&target, RoleSettings::PartnerThread(_));
        let worker = matches!(&target, RoleSettings::WorkerDefault | RoleSettings::WorkerThread(_));
        let style = matches!(target, RoleSettings::StyleReviewer);
        let math = matches!(target, RoleSettings::MathReviewer);
        let id = match &target {
            RoleSettings::PartnerThread(_) => "model-picker",
            RoleSettings::PartnerDefault => "partner-default-picker",
            RoleSettings::WorkerDefault => "worker-default-picker",
            RoleSettings::WorkerThread(_) => "thread-worker-picker",
            RoleSettings::StyleReviewer => "style-reviewer-picker",
            RoleSettings::MathReviewer => "math-reviewer-picker",
        };
        let claude_efforts = capabilities.effort_levels.clone();
        let models = self.codex_models.clone();
        let codex_model = models.iter().find(|model| Some(model.slug.as_str()) == settings.model.as_deref());
        let mut effort_levels = codex_model.map_or_else(|| {
            if math { vec!["medium".into(), "high".into()] } else { capabilities.effort_levels.clone() }
        }, |model| model.effort_levels.clone());
        if math {
            effort_levels.retain(|level| matches!(level.as_str(), "medium" | "high"));
        }
        let label = codex_model.map(|model| model.label.as_str())
            .or_else(|| settings.model.as_deref().map(model_label))
            .unwrap_or(if style { tr!("chat-claude-cli-default") } else { tr!("chat-default-model") });
        let label = match &settings.effort {
            Some(level) if !composer => format!("{label}{STATUS_SEPARATOR}{}", effort_label(Some(level))),
            _ => label.to_owned(),
        };
        let button = if composer {
            composer_picker(id, label, cx)
        } else {
            inline_button(id).accessibility_label(status_speech(&label))
                .child(status_row(&label, cx.theme().muted_foreground).truncate()).dropdown_caret(true)
        };
        let entity = cx.weak_entity();
        let can_inherit = self.protocol >= fuzukue_proto::LIVE_MODELS_PROTOCOL;
        Some(
            button
                .disabled(disabled)
                .tooltip(if composer {
                    tr!("chat-model-applies-next-turn")
                } else {
                    tr!("chat-model-and-reasoning-effort-apply-next-turn")
                })
                .dropdown_menu(move |mut menu, _, _| {
                    let claude = (!worker && !math).then(|| (None, tr!("chat-default-cli-model").into())).into_iter()
                        .chain(CLAUDE_MODELS.iter().filter(|_| !math).map(|id| (Some((*id).into()), model_label(id).into())));
                    let codex = models.iter().filter(|_| !style).map(|m| (Some(m.slug.clone()), m.label.clone()));
                    for (heading, choices) in [
                        ("Claude", claude.collect::<Vec<(Option<String>, String)>>()),
                        ("Codex", codex.collect()),
                    ] {
                        if choices.is_empty() {
                            continue;
                        }
                        if heading == "Codex" {
                            menu = menu.separator();
                        }
                        menu = menu.label(heading);
                        for (choice, label) in choices {
                            let entity = entity.clone();
                            let target = target.clone();
                            let mut next = settings.clone();
                            if next.model != choice {
                                next.model = choice.clone();
                                let levels = models.iter().find(|model| Some(model.slug.as_str()) == choice.as_deref())
                                    .map_or(&claude_efforts, |model| &model.effort_levels);
                                next.effort = if math {
                                    ["medium", "high"].into_iter().find(|level| levels.iter().any(|l| l == level)).map(str::to_owned)
                                } else if worker {
                                    levels.iter().find(|level| level.as_str() == "high").or_else(|| levels.first()).cloned()
                                } else { None };
                            }
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(settings.model == choice)
                                    .on_click(move |_, _, cx| {
                                        let _ = entity.update(cx, |this, cx| {
                                            this.change_role(&target, next.clone(), cx);
                                        });
                                    }),
                            );
                        }
                    }
                    if !composer && !effort_levels.is_empty() {
                        menu = role_effort_items(menu.separator(), &effort_levels, &settings, &target, &entity);
                    }
                    if let RoleSettings::PartnerThread(session) = &target {
                        if can_inherit {
                            let (entity, session) = (entity.clone(), session.clone());
                            menu = menu.separator().item(PopupMenuItem::new(tr!("chat-use-workspace-model"))
                                .on_click(move |_, _, cx| {
                                    let _ = entity.update(cx, |this, _| {
                                        let _ = this.out.send(ClientMsg::UseDefaultThreadModel { session: session.clone() });
                                    });
                                }));
                        }
                    }
                    menu
                })
                .anchor(if composer { Anchor::BottomRight } else { Anchor::TopLeft })
                .into_any_element(),
        )
    }

    fn chat_permission_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.active.is_empty() {
            return None;
        }
        let mail = self.chat_capabilities.as_ref()?.mail_restricted;
        let session = self.active.clone();
        let current = self.chats[&session]
            .settings
            .permission_mode
            .effective(mail);
        let codex = self.chats[&session].codex_selected();
        let tooltip = if current == PermissionMode::BypassPermissions {
            tr!("chat-approve-everything-help")
        } else if codex {
            tr!("chat-codex-approval-mode-help")
        } else if mail {
            tr!("mail-approval-mode-help")
        } else {
            tr!("chat-approval-mode-applies-next-turn")
        };
        let entity = cx.weak_entity();
        Some(
            composer_picker("permission-picker", permission_label(current), cx)
                .tooltip(tooltip)
                .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, _, _| {
                    for mode in [
                        PermissionMode::Default,
                        PermissionMode::AcceptEdits,
                        PermissionMode::Plan,
                        PermissionMode::BypassPermissions,
                    ] {
                        let entity = entity.clone();
                        let session = session.clone();
                        menu = menu.item(
                            PopupMenuItem::new(permission_label(mode))
                                .checked(current == mode)
                                .disabled(mode.effective(mail) != mode)
                                .on_click(move |_, _, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        this.update_chat_settings(
                                            &session,
                                            |s| s.permission_mode = mode,
                                            cx,
                                        );
                                    });
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        )
    }

    fn worker_approval_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let current = self.worker_approval_policy?;
        if self.active.is_empty() || self.protocol < fuzukue_proto::WORKER_APPROVAL_PROTOCOL {
            return None;
        }
        let entity = cx.weak_entity();
        let label = tr!("chat-worker-approval-selection", policy = worker_approval_label(current));
        Some(composer_picker("worker-approval-picker", label, cx)
            .tooltip(tr!("chat-worker-approval-help"))
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, _, _| {
                for policy in [WorkerApprovalPolicy::Ask, WorkerApprovalPolicy::Advisor, WorkerApprovalPolicy::Auto] {
                    let entity = entity.clone();
                    menu = menu.item(PopupMenuItem::new(worker_approval_label(policy))
                        .checked(policy == current)
                        .on_click(move |_, _, cx| {
                            let _ = entity.update(cx, |this, _| {
                                let _ = this.out.send(ClientMsg::SetWorkerApprovalPolicy { policy });
                            });
                        }));
                }
                menu
            }).into_any_element())
    }
}

fn worker_approval_label(policy: WorkerApprovalPolicy) -> &'static str {
    match policy {
        WorkerApprovalPolicy::Ask => tr!("chat-worker-approval-ask"),
        WorkerApprovalPolicy::Advisor => tr!("chat-worker-approval-advisor"),
        WorkerApprovalPolicy::Auto => tr!("chat-worker-approval-auto"),
    }
}

fn automatic_approval_label(accepted: bool) -> &'static str {
    if accepted { tr!("chat-automatically-allowed") } else { tr!("chat-automatically-declined") }
}

fn role_effort_items(
    mut menu: PopupMenu,
    levels: &[String],
    settings: &ChatSettings,
    target: &RoleSettings,
    entity: &gpui_kit::WeakEntity<Workbench>,
) -> PopupMenu {
    menu = menu.label(tr!("chat-reasoning-effort"));
    for choice in std::iter::once(None).chain(levels.iter().cloned().map(Some)) {
        let entity = entity.clone();
        let target = target.clone();
        let mut next = settings.clone();
        next.effort = choice.clone();
        let label = if matches!(&target, RoleSettings::PartnerThread(_)) {
            effort_label(choice.as_deref())
        } else {
            choice.as_deref().unwrap_or(tr!("chat-cli-default-chat"))
        };
        menu = menu.item(
            PopupMenuItem::new(label.to_owned())
                .checked(settings.effort == choice)
                .on_click(move |_, _, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.change_role(&target, next.clone(), cx);
                    });
                }),
        );
    }
    menu
}

static NEXT_ATTACHMENT_ID: AtomicU64 = AtomicU64::new(1);

impl DraftAttachment {
    fn transferring(&self) -> bool {
        match &self.state {
            AttachmentState::Uploading { .. } => true,
            AttachmentState::Drop(state) => state.on_server(),
            _ => false,
        }
    }

    fn send_chunk(&mut self, offset: u64, out: &crate::conn::Sender) {
        let AttachmentState::Uploading { bytes, sent } = &mut self.state else {
            return;
        };
        if *sent != offset {
            return;
        }
        let end = (offset as usize + ATTACHMENT_CHUNK_BYTES).min(bytes.len());
        let msg = ClientMsg::UploadAttachment {
            id: self.id,
            name: self.name.clone(),
            size: bytes.len() as u64,
            offset,
            data: STANDARD.encode(&bytes[offset as usize..end]),
            session: Some(self.session.clone()),
        };
        if out.send(msg).is_ok() {
            *sent = end as u64;
        } else {
            self.state = AttachmentState::Failed(tr!("chat-cannot-send-attachments").into());
        }
    }
}

#[cfg(test)]
mod composer_draft_tests {
    use super::*;

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn composer_notices_replace_one_line_in_the_owning_thread() {
        use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, size};

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (window, view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(900.))))),
                ..gpui_kit::component::TitleBar::window_options()
            }, cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))).unwrap();
            let session = "thread-notice";
            let waiting = "受付への連絡: 相手を替えるには Claude のターンの完了を待つ";
            let stopped = "Codex: 止めた";
            let notice = |text: &str| ServerEvent::LocalNotice {
                session: session.into(), item: fuzukue_proto::COMPOSER_NOTICE_ITEM.into(),
                text: text.into(), localized: None,
            };
            window.update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.threads_loaded = true;
                    view.switch_session(session.into(), cx);
                    view.on_event(ServerEvent::TurnActivity { session: session.into(), running: true }, cx);
                });
                window.draw(cx).clear(cx);
                let viewport = window.shot_element_bounds("chat-thread-notice").unwrap();
                view.update(cx, |view, cx| {
                    for _ in 0..3 {
                        view.apply_event(notice(waiting), false, cx);
                    }
                    view.apply_event(notice(stopped), true, cx);
                    let chat = &view.chats[session];
                    assert_eq!(chat.composer_notice.as_deref(), Some(waiting));
                    assert!(chat.rows.is_empty());
                });
                window.draw(cx).clear(cx);
                assert_eq!(window.shot_element_bounds("chat-thread-notice").unwrap(), viewport);
                let label = window.shot_element_bounds("chat-composer-notice").unwrap();
                assert!(label.size.width > px(0.));
                assert!(viewport.bottom() <= label.top());
                assert!(label.bottom() <= window.shot_element_bounds("chat-input").unwrap().top());
                assert!(window.shot_element_bounds("chat-tail-activity-label").is_some());

                view.update(cx, |view, cx| {
                    view.switch_session("thread-other".into(), cx);
                    view.apply_event(notice(stopped), false, cx);
                    assert_eq!(view.chats[session].composer_notice.as_deref(), Some(stopped));
                    assert!(view.chats[session].rows.is_empty());
                    assert!(view.chats["thread-other"].composer_notice.is_none());
                });
                window.draw(cx).clear(cx);
                assert!(window.shot_element_bounds("chat-composer-notice").is_none());

                view.update(cx, |view, cx| {
                    view.switch_session(session.into(), cx);
                    view.on_event(ServerEvent::TurnEnded { session: session.into(), actor: Actor::Claude }, cx);
                    assert!(view.chats[session].composer_notice.is_none());
                });
                window.draw(cx).clear(cx);
                assert!(window.shot_element_bounds("chat-composer-notice").is_none());
            }).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn tool_disclosure_requests_full_output_and_drops_it_on_close() {
        use gpui_kit::AppContext as _;

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (_, view) = gpui_kit::open_window(gpui_kit::component::TitleBar::window_options(),
                cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Fixture"))))).unwrap();
            let (out, sent) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.out = out.into();
                view.protocol = fuzukue_proto::TOOL_DETAILS_PROTOCOL;
                view.transcripts.scoped = true;
                let session = "thread-test";
                let item = "read";
                let input = serde_json::json!({"path": "test.md"});
                view.on_event(ServerEvent::ToolCall {
                    session: session.into(), item: item.into(), actor: Actor::Claude, name: "Read".into(), input: input.clone(),
                }, cx);
                view.on_event(ServerEvent::ToolResult {
                    session: session.into(), item: item.into(), ok: true, output: "preview…".into(),
                }, cx);
                let (row, index) = view.chats[session].by_tool[item];
                assert!(sent.try_iter().all(|message| !matches!(message, ClientMsg::GetThreadToolDetails { .. })));

                view.toggle_tool_payload(session, row, Disclosure::Result(index), cx);
                let request = sent.try_iter().find_map(|message| match message {
                    ClientMsg::GetThreadToolDetails { item: id, request, .. } if id == item => Some(request),
                    _ => None,
                }).expect("opening a tool requests its detail");
                let reply = |request| ServerEvent::ThreadToolDetails {
                    session: session.into(), item: item.into(), request,
                    input: Some(input.clone()), output: Some("full output".into()), unavailable: false,
                };
                view.finish_tool_payload(reply(request + 1), cx);
                let Content::Tools(group) = &view.chats[session].rows[row].content else { panic!("missing tool group") };
                assert!(group.calls[index].detail.output.is_none());
                view.finish_tool_payload(reply(request), cx);
                let Content::Tools(group) = &view.chats[session].rows[row].content else { panic!("missing tool group") };
                assert_eq!(group.calls[index].detail.output.as_deref(), Some("full output"));
                view.toggle_tool_payload(session, row, Disclosure::Result(index), cx);
                let Content::Tools(group) = &view.chats[session].rows[row].content else { panic!("missing tool group") };
                assert!(group.calls[index].detail.output.is_none());
            });
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn cursor_reconnect_reconciles_stale_chat_activity() {
        use crate::conn::ConnEvent;
        use gpui_kit::AppContext as _;

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (window, view) = gpui_kit::open_window(gpui_kit::component::TitleBar::window_options(),
                cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))).unwrap();
            window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                let attach = |replay| ConnEvent::Event(fuzukue_proto::Envelope { workspace: None, seq: 0,
                    event: ServerEvent::WorkspaceSelected { id: "fixture".into(), replay },
                });
                view.on_conn(attach(true), window, cx);
                for (session, item) in [("stale", "delegate:stale"), ("live", "delegate:live")] {
                    view.on_event(ServerEvent::TurnActivity { session: session.into(), running: true }, cx);
                    view.on_event(ServerEvent::ToolCall {
                        session: session.into(), item: item.into(), actor: Actor::System, name: "delegate".into(),
                        input: serde_json::json!({"model": "fixture", "started_at_ms": 1000}),
                    }, cx);
                    assert!(view.chats[session].busy);
                    assert_eq!(view.chats[session].active_worker(), Some("fixture"));
                }

                view.on_conn(ConnEvent::Connected, window, cx);
                view.on_conn(attach(false), window, cx);
                view.on_conn(ConnEvent::Event(fuzukue_proto::Envelope {
                    workspace: Some("fixture".into()), seq: 0,
                    event: ServerEvent::ChatActivity {
                        turns: BTreeSet::from(["live".into()]),
                        workers: BTreeMap::from([("live".into(), BTreeSet::from(["delegate:live".into()]))]),
                    },
                }), window, cx);
                let stale = &view.chats["stale"];
                assert!(!stale.busy);
                assert!(stale.pending_jobs().next().is_none());
                assert!(matches!(&stale.rows[stale.by_item["delegate:stale"]].content,
                    Content::Delegate(block) if block.report.as_ref().is_some_and(|(ok, report)| !ok && report.interrupted)));
                assert!(view.chats["live"].busy);
                assert_eq!(view.chats["live"].active_worker(), Some("fixture"));
            })).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn transcript_fade_is_opaque_at_the_clip_and_clears_the_tail() {
        let [(first_y, first_alpha), _, (lower_y, lower_alpha), (opaque_y, opaque_alpha)] =
            CHAT_FADE_STOPS;
        assert_eq!((first_y, first_alpha), (px(0.), 0.));
        assert!(opaque_y <= CHAT_BOTTOM_FADE);
        assert_eq!(opaque_alpha, 1.);
        assert!(lower_y < opaque_y && 1. - lower_alpha < 0.35);
        for scale in [1., 1.25, 1.5, 2., 2.5] {
            let padding = chat_tail_padding(scale);
            assert!(padding >= CHAT_BOTTOM_FADE + px(4.));
        }
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn transcript_fade_meets_the_viewport_below_the_status_text_inset() {
        use gpui_kit::{size, AppContext as _, Bounds, WindowBounds, WindowOptions};

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let bounds = Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(900.)));
            let (window, view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..gpui_kit::component::TitleBar::window_options()
            }, cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))).unwrap();
            let session = "chat-fade-fixture".to_owned();
            view.update(cx, |view, cx| {
                view.threads_loaded = true;
                view.switch_session(session.clone(), cx);
            });

            window.update(cx, |_, window, cx| {
                window.draw(cx).clear(cx);
                let viewport_id = format!("chat-{session}");
                let idle = window.shot_element_bounds(&viewport_id).unwrap();
                view.update(cx, |view, cx| {
                    view.chat_waiting.goal_status.insert(session, GoalTail::Paused);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let viewport = window.shot_element_bounds(&viewport_id).unwrap();
                let fade = window.shot_element_bounds("chat-bottom-fade").unwrap();
                let label = window.shot_element_bounds("chat-tail-activity-label").unwrap();
                assert_eq!(viewport, idle);
                assert_eq!(fade.bottom(), viewport.bottom());
                assert_eq!(fade.size.height, CHAT_BOTTOM_FADE);
                let gap = label.top() - viewport.bottom();
                assert_eq!(gap, CHAT_STATUS_TEXT_INSET);
                assert!(gap <= px(2.5));
            }).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn chat_columns_share_centered_edges_and_footer_uses_right_edge() {
        use gpui_kit::{JustifyContent, Length};

        let mut column = chat_column();
        let column = column.style();
        assert_eq!(column.size.width, Some(relative(1.).into()));
        assert_eq!(column.max_size.width, Some(px(CHAT_WIDTH).into()));
        assert_eq!(column.margin.left, Some(Length::Auto));
        assert_eq!(column.margin.right, Some(Length::Auto));
        assert_eq!(column.padding.left, Some(CHAT_COLUMN_INSET.into()));
        assert_eq!(column.padding.right, Some(CHAT_COLUMN_INSET.into()));

        let mut footer = composer_footer();
        let footer = footer.style();
        assert_eq!(footer.size.width, Some(relative(1.).into()));
        assert_eq!(footer.justify_content, Some(JustifyContent::End));
        assert_eq!(footer.padding.right, None);
    }

    #[test]
    fn drafts_are_isolated_by_workspace_and_thread() {
        let mut drafts = DraftMap::default();
        assert!(drafts.set_text("lab", "first", "実験"));
        assert!(drafts.set_text("lab", "second", "比較"));
        assert!(drafts.set_text("other", "first", "別の下書き"));
        assert_eq!(drafts.get("other", "first").unwrap().text, "別の下書き");
        assert_eq!(drafts.get("lab", "second").unwrap().text, "比較");
        assert_eq!(drafts.get("lab", "first").unwrap().text, "実験");
        assert!(drafts.retain_threads("lab", |thread| thread == "second"));
        assert!(drafts.get("lab", "first").is_none());
        assert_eq!(drafts.get("other", "first").unwrap().text, "別の下書き");
        assert!(drafts.set_text("lab", "second", ""));
        assert!(drafts.get("lab", "second").is_none());
    }

    #[test]
    fn drafts_restore_from_settings_with_saved_attachments_and_unknown_keys() {
        let settings = r#"
future = true
[composer_drafts.lab."thread/1"]
text = "old"
future_per_draft = 42
"#;
        let mut settings: toml::Table = toml::from_str(settings).unwrap();
        let mut drafts = DraftMap::read(&settings).unwrap();
        drafts.set_text("lab", "thread/1", "日本語\n- 下書き");
        let attachment = SavedAttachment { name: "file.txt".into(), path: "inbox/file.txt".into() };
        drafts.edit("lab", "thread/1", |draft| {
            draft.attachments.push(attachment.clone());
            true
        });
        assert!(drafts.write(&mut settings).unwrap());
        assert!(!drafts.write(&mut settings).unwrap());
        let saved: toml::Table = toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        let restored = DraftMap::read(&saved).unwrap();
        let draft = restored.get("lab", "thread/1").unwrap();
        assert_eq!(draft.text, "日本語\n- 下書き");
        assert_eq!(draft.attachments, [attachment]);
        assert_eq!(draft.extra["future_per_draft"].as_integer(), Some(42));
        assert_eq!(saved["future"].as_bool(), Some(true));
    }

    #[test]
    fn drafts_cap_utf8_text_at_twenty_thousand_characters() {
        let mut drafts = DraftMap::default();
        let cap = "界".repeat(MAX_DRAFT_CHARS);
        let text = format!("{cap}🌿");
        assert!(drafts.set_text("lab", "thread", &text));
        assert_eq!(drafts.get("lab", "thread").unwrap().text, cap);
        assert!(!drafts.set_text("lab", "thread", &text));

        drafts.edit("lab", "thread", |draft| {
            draft.text = text;
            true
        });
        let mut settings = toml::Table::new();
        drafts.write(&mut settings).unwrap();
        let restored = DraftMap::read(&settings).unwrap();
        assert_eq!(restored.get("lab", "thread").unwrap().text, cap);
    }
}

#[cfg(test)]
mod one_voice_tests {
    use super::*;

    #[test]
    fn composer_action_accounts_for_content_activity_ime_and_drain() {
        use ComposerAction::*;

        for held in [false, true] {
            assert_eq!(composer_action(false, false, false, held), Empty);
            assert_eq!(composer_action(false, true, false, held), Stop);
            for running in [false, true] {
                for (has_content, ime) in [(true, false), (false, true), (true, true)] {
                    assert_eq!(
                        composer_action(has_content, running, ime, held),
                        Send { held },
                    );
                }
            }
        }
    }

    #[test]
    fn chat_markdown_recovers_cjk_emphasis_and_keeps_code_plain() {
        let text = "しかも**「監視を始めたスレッド」に結び付ける**。そのスレッドがアーカイブ（や削除）されていたら、次のイベントで**新しく立ち上げて**、結び付け直す";
        let source = format!("{text} 前**`code`**です\n\n```text\ncode block\n```");
        let blocks = parse_markdown(&source, math::extensions().into()).unwrap();
        let [BlockNode::Paragraph(paragraph), BlockNode::CodeBlock(code)] = blocks.as_slice() else {
            panic!("expected chat paragraph and code block");
        };
        let mut bold = Vec::new();
        let mut inline_code = Vec::new();
        for run in paragraph.children() {
            assert!(!run.text().contains("**"));
            for (range, mark) in run.mark_runs() {
                let value = &run.text()[range.clone()];
                if mark.bold {
                    bold.push(value);
                }
                if mark.code {
                    inline_code.push(value);
                }
            }
        }
        assert_eq!(bold, ["「監視を始めたスレッド」に結び付ける", "新しく立ち上げて", "code"]);
        assert_eq!(inline_code, ["code"]);
        assert_eq!(code.code().trim_end(), "code block");
    }

    #[test]
    fn context_usage_tooltip_uses_the_fullest_context() {
        let usage = |used, limit| ContextUsage { used: Some(used), limit };
        let first = usage(12_000, Some(200_000));
        assert_eq!(context_usage_tooltip([first]), (first.label(), 6.));

        let fullest = usage(8_000, Some(10_000));
        let unknown_limit = usage(90_000, None);
        let (label, percent) = context_usage_tooltip([first, fullest, unknown_limit]);
        assert_eq!((label, percent), (fullest.label(), 80.));
        let (label, percent) = context_usage_tooltip([unknown_limit]);
        assert!(label.contains("90000 / —") && !label.contains('%'), "{label}");
        assert_eq!(percent, 0.);
    }

    #[test]
    fn assistant_message_emptiness_follows_rendered_markdown() {
        let extensions = math::extensions();
        for text in ["", " \t\n", "<silent/>", "<silent />", "<b> </b>", "<!-- hidden -->"] {
            assert!(message_has_no_visible_text(text, &extensions), "{text:?}");
        }
        for text in ["普通の返事", "前<silent />後", "<b>本文</b>", "`<silent/>`", "![画像](image.png)"] {
            assert!(!message_has_no_visible_text(text, &extensions), "{text:?}");
        }
    }

    #[test]
    fn chat_heights_snap_to_device_pixels_at_fractional_scales() {
        for scale in [1.25, 1.5, 1.75, 2.5] {
            let height = px(17. * 1.5);
            let physical = f32::from(snap_chat_height(height, scale)) * scale;
            assert!((physical - (f32::from(height) * scale).round()).abs() < 0.0001);
        }
    }

    #[test]
    fn standalone_internal_notes_follow_the_worker_message_setting() {
        for (actor, item, text) in [
            (Actor::System, "thread-plan:current", "The plan is ready."),
            (Actor::System, "thread-plan:legacy", "Next I’ll work on …"),
            (Actor::System, "drain:1:notice", "Finish at a safe stopping point."),
            (Actor::Codex { thread_id: "desk".into(), parent_tool_use: None },
                "item:summary:0", "Check the next step."),
        ] {
            let kind = InternalNote::for_item(&actor, item).unwrap();
            let row = Row::new(Actor::System, Content::InternalNote { kind, text: text.into(), open: false });
            assert!(matches!(row.kind(false), RowKind::Hidden), "{item}");
            assert!(matches!(row.kind(true), RowKind::SpeakerBoundary), "{item}");
        }
        assert!(InternalNote::for_item(&Actor::System, "drain-1-checkpoint").is_none());
        assert!(InternalNote::for_item(&Actor::Claude, "reply").is_none());
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn older_message_actions_follow_hover_and_keyboard_focus() {
        use gpui_kit::{
            AppContext as _, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseExitEvent,
            MouseMoveEvent, PlatformInput, Render,
        };

        struct Fixture {
            focus: gpui_kit::FocusHandle,
            previous: gpui_kit::FocusHandle,
            covered: bool,
        }

        fn marker(latest: bool) -> gpui_kit::Hsla {
            gpui_kit::rgb(if latest { 0x185bea } else { 0xe43c92 }).into()
        }

        impl Render for Fixture {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let group = "fixture-older-message";
                let older = div().id("older-marker").w(px(80.)).h(px(24.)).bg(marker(false))
                    .child(div().id("older-control").size_full().track_focus(&self.focus));
                v_flex().size_full().tab_group()
                    .child(div().id("previous-control").h(px(40.)).track_focus(&self.previous))
                    .child(div().group(group).child(older_message_actions(
                        older, false, !self.covered && window.last_input_was_keyboard(),
                        (!self.covered).then(|| group.into()),
                    )))
                    .child(div().id("latest-marker").w(px(80.)).h(px(24.)).bg(marker(true)))
            }
        }

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            let (window, view) = gpui_kit::open_window(gpui_kit::component::TitleBar::window_options(),
                cx, |_, cx| cx.new(|cx| Fixture {
                    focus: cx.focus_handle().tab_index(1).tab_stop(true),
                    previous: cx.focus_handle().tab_index(0).tab_stop(true),
                    covered: false,
                })).unwrap();
            window.update(cx, |_, window, cx| {
                let draw = |window: &mut Window, cx: &mut gpui_kit::App| {
                    window.draw(cx).clear(cx);
                    let quads = window.painted_quads();
                    assert!(quads.iter().any(|quad| quad.background == marker(true).into()));
                    quads.iter().any(|quad| quad.background == marker(false).into())
                };
                let move_to = |position| PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() });

                assert!(!draw(window, cx));
                let older = window.shot_element_bounds("older-marker").unwrap().center();
                let latest = window.shot_element_bounds("latest-marker").unwrap().center();
                window.dispatch_event(move_to(older), cx);
                assert!(draw(window, cx));
                window.dispatch_event(PlatformInput::MouseExited(MouseExitEvent { position: older, ..Default::default() }), cx);
                assert!(!draw(window, cx));

                window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left, position: older, click_count: 1, ..Default::default()
                }), cx);
                let focus = view.read(cx).focus.clone();
                window.dispatch_event(move_to(latest), cx);
                assert!(focus.is_focused(window));
                assert!(!draw(window, cx));

                view.read(cx).previous.clone().focus(window, cx);
                window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke::parse("tab").unwrap(), is_held: false, prefer_character_input: false,
                }), cx);
                assert!(focus.is_focused(window));
                assert!(draw(window, cx));
                view.update(cx, |view, cx| { view.covered = true; cx.notify(); });
                assert!(!draw(window, cx));
            }).unwrap();
            cx.spawn(async move |cx| { let _ = cx.update(|cx| cx.quit()); }).detach();
        });
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn disclosure_rows_handle_full_width_clicks_drags_and_keyboard() {
        use gpui_kit::{
            AppContext as _, Bounds, KeyDownEvent, KeyUpEvent, Keystroke, MouseButton,
            MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Render, WindowBounds,
            WindowOptions,
        };

        struct Fixture {
            label: String,
            open: bool,
            press: Rc<Cell<Option<Point<Pixels>>>>,
            previous: gpui_kit::FocusHandle,
        }

        impl Render for Fixture {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let entity = cx.entity();
                v_flex().size_full().tab_group()
                    .child(div().id("previous").track_focus(&self.previous).tab_index(0))
                    .child(cancel_disclosure_drag(self.press.clone()))
                    .child(disclosure_row(
                        "disclosure".into(), self.open, &self.label, cx.theme().muted_foreground,
                        self.press.clone(), cx, move |cx| {
                            entity.update(cx, |this, cx| {
                                this.open = !this.open;
                                cx.notify();
                            });
                        },
                    ))
                    .children(self.open.then(|| div().child("Details")))
            }
        }

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            for label in [
                tr!("chat-tool-calls", calls = 2),
                tr!("chat-worker-tools", worker = tr!("chat-worker"), calls = 13, status = tr!("chat-done")),
                tr!("chat-earlier-worker-tools", calls = 1),
                tr!("chat-decided-approvals", count = 2),
                tr!("chat-worker-messages").into(),
            ] {
                let (window, view) = gpui_kit::open_window(WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(0.), px(0.)), gpui_kit::size(px(640.), px(240.)),
                    ))),
                    ..gpui_kit::component::TitleBar::window_options()
                }, cx, |_, cx| cx.new(|cx| Fixture {
                    label: label.clone(),
                    open: false,
                    press: Rc::default(),
                    previous: cx.focus_handle(),
                })).unwrap();
                window.update(cx, |_, window, cx| {
                    let move_to = |position, pressed_button| PlatformInput::MouseMove(MouseMoveEvent {
                        position, pressed_button, ..Default::default()
                    });
                    let down = |position| PlatformInput::MouseDown(MouseDownEvent {
                        position, click_count: 1, ..Default::default()
                    });
                    let up = |position| PlatformInput::MouseUp(MouseUpEvent {
                        position, click_count: 1, ..Default::default()
                    });
                    let draw = |window: &mut Window, cx: &mut gpui_kit::App| {
                        window.draw(cx).clear(cx);
                    };
                    draw(window, cx);
                    let row = window.shot_element_bounds("disclosure").unwrap();
                    let label_bounds = window.shot_element_bounds("disclosure-label").unwrap();
                    let chevron = window.shot_element_bounds("disclosure-chevron-right").unwrap();
                    assert!(row.size.height >= px(28.), "{label}");
                    assert!(chevron.right() <= label_bounds.left(), "{label}");
                    let label_center = label_bounds.center();
                    let empty = point(row.right() - px(4.), row.center().y);
                    assert!(empty.x > label_bounds.right(), "{label}");

                    for (position, open) in [(label_center, true), (empty, false)] {
                        for event in [move_to(position, None), down(position), up(position)] {
                            window.dispatch_event(event, cx);
                        }
                        draw(window, cx);
                        assert_eq!(view.read(cx).open, open, "{label}");
                        let direction = if open { "down" } else { "right" };
                        assert_eq!(window.shot_element_bounds(&format!("disclosure-chevron-{direction}")),
                            Some(chevron), "{label}");
                    }

                    // Returning the pointer to the press location still counts as a drag.
                    for event in [move_to(label_center, None), down(label_center),
                        move_to(point(empty.x, row.bottom() + px(12.)), Some(MouseButton::Left)),
                        move_to(label_center, Some(MouseButton::Left)), up(label_center)] {
                        window.dispatch_event(event, cx);
                    }
                    assert!(!view.read(cx).open, "{label}");

                    view.read(cx).previous.clone().focus(window, cx);
                    draw(window, cx);
                    for (key, open) in [("tab", false), ("enter", true), ("space", false)] {
                        let keystroke = Keystroke::parse(key).unwrap();
                        window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent {
                            keystroke: keystroke.clone(), is_held: false, prefer_character_input: false,
                        }), cx);
                        window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
                        draw(window, cx);
                        assert!(!view.read(cx).previous.is_focused(window), "{label} via {key}");
                        assert_eq!(view.read(cx).open, open, "{label} via {key}");
                    }
                    window.remove_window();
                }).unwrap();
            }
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn planner_note_disclosure_keeps_the_goal_pane_visible() {
        use gpui_kit::{point, size, AppContext as _, Bounds, WindowBounds, WindowOptions};

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (window, view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.), px(0.)), size(px(1440.), px(1000.))))),
                ..gpui_kit::component::TitleBar::window_options()
            }, cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))).unwrap();
            let session = "planner-note-fixture".to_owned();
            let plan = fuzukue_proto::Plan {
                goal: "原稿の引用を確認する".into(), done_condition: "引用と出典が一致する。".into(),
                status: fuzukue_proto::PlanStatus::Active, pace: None, done: vec![],
                next: vec![fuzukue_proto::PlanStep {
                    task: "Compare citations with the source".into(), reason: "Check the edition".into(),
                    agent: None, routing: Default::default(), tier_reason: None, blocker: None,
                }],
                rejected: vec![], question: None, verification: None, paused_by_user: false,
            };
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PLAN_PROTOCOL;
                view.appearance.settings.thread_info_open = true;
                view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                    id: session.clone(), title: "Citations".into(), updated: 0,
                    github: Default::default(), untitled: false, archived: false, pinned: false,
                    attached: vec![], auto_attached: vec![], suggestions: vec![], math_macros: vec![],
                    claude_session: None, artifacts: vec![], partner_settings: None,
                    worker_settings: None, agent_overrides: Default::default(),
                }] }, cx);
                view.on_event(ServerEvent::PlanChanged { session: session.clone(), plan: plan.clone(), progress: None, parallel_limits: Default::default() }, cx);
                view.on_event(ServerEvent::TextDelta {
                    session: session.clone(), item: "user:before-note".into(),
                    actor: Actor::User, text: "Check the citations.".into(),
                }, cx);
                view.on_event(ServerEvent::TextDelta {
                    session: session.clone(), item: format!("{}fixture", fuzukue_proto::PLAN_NOTE),
                    actor: Actor::System, text: "The plan is ready.".into(),
                }, cx);
                for (item, text) in [("user:after-note", "Check the edition."), ("user:next", "Compare the text.")] {
                    view.on_event(ServerEvent::TextDelta {
                        session: session.clone(), item: item.into(), actor: Actor::User, text: text.into(),
                    }, cx);
                }
                view.on_event(ServerEvent::TextDelta {
                    session: session.clone(), item: "empty-system-message".into(),
                    actor: Actor::System, text: " ".into(),
                }, cx);
                for (item, at_ms) in [("first-reply", 0), ("continued-reply", 60_000), ("later-reply", 360_000)] {
                    view.on_event(ServerEvent::TextDelta {
                        session: session.clone(), item: item.into(),
                        actor: Actor::Claude, text: item.into(),
                    }, cx);
                    view.chat(&session, cx).rows.last_mut().unwrap().at_ms = Some(at_ms);
                }
            });
            for (show, open) in [(false, false), (true, false), (true, true), (false, false)] {
                view.update(cx, |view, cx| {
                    if show != view.appearance.settings.show_worker_messages {
                        view.appearance.settings.show_worker_messages = show;
                        view.chat(&session, cx).reset_worker_disclosures();
                    }
                    if open {
                        view.chat(&session, cx).toggle(1, Disclosure::InternalNote, show, cx);
                    }
                    assert_eq!(view.thread_info.plans[&session].0, plan);
                    cx.notify();
                });
                window.update(cx, |_, window, cx| {
                    window.draw(cx).clear(cx);
                    assert_eq!(window.shot_element_bounds("chat-internal-note-1").is_some(), show);
                    assert_eq!(window.shot_element_bounds("chat-internal-note-text-1").is_some(), open);
                    assert!(window.shot_element_bounds("thread-plan").is_some());
                    assert!(window.shot_element_bounds("thread-goal-title").is_some());
                    assert!(window.shot_element_bounds("thread-goal-done-condition").is_some());
                    let chat = &view.read(cx).chats[&session];
                    let row = |ix| chat.scroller.bounds_for_item(ix).unwrap();
                    let bubble = |ix| window.shot_element_bounds(&format!("chat-message-bubble-{ix}")).unwrap();
                    assert_eq!(bubble(3).top() - bubble(2).bottom(), CHAT_GROUP_GAP);
                    assert_eq!(bubble(6).top() - bubble(5).bottom(), CHAT_GROUP_ROW_GAP);
                    assert!(window.shot_element_bounds("chat-message-actions-6").is_some());
                    assert_eq!(bubble(7).top() - bubble(6).bottom(), CHAT_GROUP_GAP);
                    assert_eq!(row(4).size.height, px(0.));
                    if !show {
                        assert_eq!(row(1).size.height, px(0.));
                        assert_eq!(bubble(2).top() - bubble(0).bottom(), CHAT_GROUP_GAP);
                    }
                }).unwrap();
            }
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn local_notices_stay_visible_between_assistant_groups() {
        let row = Row::new(Actor::System, Content::Notice { text: "New session".into(), divider: true });
        assert!(matches!(row.kind(false), RowKind::SpeakerBoundary));
        assert!(matches!(row.kind(true), RowKind::SpeakerBoundary));
        assert!(is_command_echo(&Actor::User, Some("slash-command:fixture")));
        assert!(!is_command_echo(&Actor::User, Some("user:fixture")));
    }

    #[test]
    fn plan_notice_updates_one_muted_row_and_hides_the_raw_error() {
        let mut notice = Row::new(Actor::System, Content::PlanNotice {
            count: 1, text: "Plan update timed out (will retry on the next update)".into(),
        });
        let counted = "Plan update timed out (will retry on the next update) (3 times)";
        assert!(merge_plan_notice(&mut notice.content, 3, counted.into()));
        assert!(!merge_plan_notice(&mut notice.content, 2, "Older replay".into()));
        assert!(matches!(&notice.content, Content::PlanNotice { count: 3, text } if text == counted));
        let item = "delegate:fixture:planner-error";
        assert_eq!(delegate_job(item, &Actor::System), Some("delegate:fixture"));
        assert!(is_worker_message(&Actor::System, Some(item)));
        assert!(matches!(notice.kind(false), RowKind::SpeakerBoundary));
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn slash_palette_and_notice_render_above_and_within_the_chat() {
        use gpui_kit::{point, size, AppContext as _, Bounds, WindowBounds, WindowOptions};
        use fuzukue_proto::LocalizedMessage;

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (window, view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(900.))))),
                ..gpui_kit::component::TitleBar::window_options()
            }, cx, |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))).unwrap();
            window.update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.protocol = fuzukue_proto::SLASH_COMMAND_PROTOCOL;
                    view.threads_loaded = true;
                    let session = "thread-slash-fixture".to_owned();
                    view.switch_session(session.clone(), cx);
                    view.on_event(ServerEvent::TextDelta {
                        session: session.clone(), item: "slash-command:fixture".into(), actor: Actor::User, text: "/clear".into(),
                    }, cx);
                    view.on_event(ServerEvent::LocalNotice {
                        session: session.clone(), item: "clear-fixture".into(), text: "New session".into(),
                        localized: Some(LocalizedMessage::new("command-cleared", [])),
                    }, cx);
                    let input = view.input.clone();
                    input.read(cx).focus_handle(cx).focus(window, cx);
                    input.update(cx, |input, cx| input.replace_range_atomically(0..0, "/c", window, cx));
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let palette = window.shot_element_bounds("composer-command-palette").unwrap();
                let input = window.shot_element_bounds("chat-input").unwrap();
                assert!(palette.bottom() <= input.top());
                assert!(window.shot_element_bounds("chat-command-echo-0").is_some());
                assert!(window.shot_element_bounds("chat-local-notice-1").is_some());
                for key in ["down", "tab"] {
                    assert!(window.dispatch_keystroke(gpui_kit::Keystroke::parse(key).unwrap(), cx));
                }
                assert_eq!(view.read(cx).input.read(cx).value().as_str(), "/clear");
                assert!(window.dispatch_keystroke(gpui_kit::Keystroke::parse("escape").unwrap(), cx));
                window.draw(cx).clear(cx);
                assert!(window.shot_element_bounds("composer-command-palette").is_none());
            }).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn chat_row_spacing_includes_actions_in_the_group_boundary() {
        assert_eq!(chat_row_spacing(false, false, px(0.)), (px(0.), px(8.)));
        for action_height in [px(0.), px(18.), px(24.)] {
            let (top, bottom) = chat_row_spacing(false, true, action_height);
            assert_eq!(top, px(0.));
            assert_eq!(action_height + bottom, px(24.));
        }
    }

    #[test]
    fn assistant_group_boundaries_use_speaker_and_adjacent_row_times() {
        use RowKind::{AssistantDetail, AssistantText, Hidden, SpeakerBoundary};

        let bounds = |rows| assistant_groups(rows).into_iter()
            .map(|group| (group.first, group.last, group.last_text))
            .collect::<Vec<_>>();
        assert_eq!(bounds(vec![(AssistantText, Some(0)), (AssistantText, Some(60_000))]),
            vec![(0, 1, Some(1))]);
        for gap in [300_000, 360_000] {
            assert_eq!(bounds(vec![(AssistantText, Some(0)), (AssistantText, Some(gap))]),
                vec![(0, 0, Some(0)), (1, 1, Some(1))]);
        }
        // Tool, job, and approval rows extend the run without moving the text action target.
        assert_eq!(bounds(vec![
            (AssistantText, Some(0)), (AssistantDetail, Some(60_000)), (AssistantDetail, Some(120_000)),
            (AssistantDetail, Some(240_000)), (AssistantText, Some(360_000)), (AssistantDetail, None),
        ]), vec![(0, 5, Some(4))]);
        assert_eq!(bounds(vec![(AssistantText, None), (SpeakerBoundary, None), (AssistantText, None)]),
            vec![(0, 0, Some(0)), (2, 2, Some(2))]);
        assert_eq!(bounds(vec![(Hidden, None), (Hidden, Some(600_000))]), vec![]);
        assert_eq!(bounds(vec![
            (Hidden, None), (AssistantText, Some(0)), (Hidden, Some(600_000)),
            (AssistantText, None), (AssistantDetail, Some(600_000)), (Hidden, None),
        ]), vec![(1, 4, Some(3))]);
    }

    #[test]
    fn active_chat_hides_only_the_latest_message_groups_actions() {
        use RowKind::{AssistantDetail, AssistantText, Hidden, SpeakerBoundary};

        let groups = assistant_groups([
            (SpeakerBoundary, None), (AssistantText, None), (AssistantDetail, None),
            (SpeakerBoundary, None), (Hidden, None),
        ]);
        assert_eq!(hidden_message_actions_row(false, Some(1), &groups), None);
        assert_eq!(hidden_message_actions_row(true, Some(1), &groups), Some(2));
        assert_eq!(hidden_message_actions_row(true, Some(3), &groups), Some(3));
        assert_eq!(hidden_message_actions_row(true, None, &groups), None);
    }

    #[test]
    fn single_activity_summary_uses_worker_count_and_timing() {
        let running = DelegateJob {
            agent: Some("Reviewer".into()), title: Some("Check proof".into()), model: String::new(),
            display: Default::default(), queued_at_ms: None,
            started_at_ms: Some(1000), selection: None, routing: None,
            touches: None, waiting: None, dismissed_from_panel: false,
        };
        let waiting = DelegateJob { title: None, started_at_ms: None, ..running.clone() };
        let second = DelegateJob { started_at_ms: Some(55_000), ..running.clone() };
        let thinking = TurnActivity {
            writing: false, phase_elapsed: Duration::from_secs(7),
            elapsed: Duration::from_secs(99), tool: None,
        };
        let writing = TurnActivity { writing: true, phase_elapsed: Duration::from_secs(8), tool: None, ..thinking };
        let wait = TimedWait { reason: ChatWaitReason::Worker, since: Instant::now() - Duration::from_secs(10) };
        let workers = tr!("chat-status-workers", count = 2);
        for (turn, key, elapsed) in [
            (Some(&thinking), "chat-thinking", "0:07"),
            (Some(&writing), "chat-writing", "0:08"),
        ] {
            assert_eq!(activity_summary(turn, &[&second, &running], Some(wait), 66_000),
                (format!("{}{STATUS_SEPARATOR}{workers}", tr!(key)), Some(elapsed.to_owned())));
        }
        assert_eq!(activity_summary(None, &[&second, &running], Some(wait), 66_000),
            (tr!("chat-status-working").to_owned(), Some(format!("{}{STATUS_SEPARATOR}1:05", tr!("chat-status-count", count = 2)))));
        let (label, elapsed) = activity_summary(None, &[], Some(wait), 66_000);
        assert_eq!(label, tr!("chat-status-waiting"));
        assert!(elapsed.is_some());
        assert_eq!(activity_summary(None, &[&waiting], None, 66_000),
            (tr!("chat-status-waiting").to_owned(),
                Some(tr!("chat-status-count", count = 1).to_owned())));
    }

    #[test]
    fn waiting_line_requires_a_reason_and_no_visible_turn_or_job() {
        let wait = Some(TimedWait { reason: ChatWaitReason::Worker, since: Instant::now() });
        assert!(visible_wait(None, false, false).is_none());
        assert!(visible_wait(wait, true, false).is_none());
        assert!(visible_wait(wait, false, true).is_none());
        assert_eq!(visible_wait(wait, false, false).unwrap().reason, ChatWaitReason::Worker);
    }

    #[test]
    fn waiting_clock_follows_reason_changes() {
        use ChatWaitReason::*;
        use fuzukue_proto::SelfUpdateStepKind as Step;

        let start = Instant::now();
        let later = start + Duration::from_secs(8);
        let mut state = ChatWaitingState::default();
        let snapshot = |stage, resume_at| ChatWaiting {
            workspace: Some(SelfUpdate { stage }),
            threads: BTreeMap::from([
                ("paused".to_owned(), Some(UsagePaused { resume_at })),
                ("working".to_owned(), None),
            ]),
            ..Default::default()
        };
        state.update(snapshot(Step::Build, Some(123)), start);
        state.update(snapshot(Step::Build, Some(456)), later);
        assert_eq!(state.reason("idle").unwrap().since, start);
        assert_eq!(state.reason("paused").unwrap().since, start);
        assert!(state.reason("working").is_none());

        state.update(snapshot(Step::Test, Some(456)), later);
        assert_eq!(state.reason("idle").unwrap().since, later);
        assert_eq!(state.reason("paused").unwrap().reason, UsagePaused { resume_at: Some(456) });
        state.update(ChatWaiting::default(), later);
        assert!(state.reason("idle").is_none());
        assert!(state.reason("paused").is_none());
    }

    #[test]
    fn phase_clock_resets_only_when_the_visible_phase_changes() {
        let old = Instant::now() - Duration::from_secs(10);
        let mut turn = TurnStatus {
            started: old, phase_started: old, writing: false, consulting: None,
            tools: Vec::new(), pending_approvals: Vec::new(),
        };
        turn.update_phase(|turn| turn.writing = true);
        assert!(turn.phase_started > old);
        let writing = turn.phase_started;
        turn.update_phase(|turn| turn.writing = true);
        assert_eq!(turn.phase_started, writing);

        turn.update_phase(|turn| turn.tools.push("bash".into()));
        let bash = turn.phase_started;
        turn.update_phase(|turn| turn.tools.push("read".into()));
        assert_eq!(turn.phase_started, bash);
        turn.phase_started = old;
        turn.update_phase(|turn| { turn.tools.remove(0); });
        assert!(turn.phase_started > old);
        turn.phase_started = old;
        turn.update_phase(|turn| turn.tools.clear());
        assert!(turn.phase_started > old);
    }

    #[test]
    fn activity_details_keep_tool_names_counts_and_wait_reasons() {
        let elapsed = Duration::from_secs(372);
        let tool = TurnActivity {
            writing: true,
            elapsed,
            phase_elapsed: Duration::from_secs(20),
            tool: Some(ToolActivity::Running { name: "Bash", elapsed: Duration::from_secs(299), additional: 1 }),
        };
        let tool_label = format!("{} +1", tr!("chat-tool-state-running", name = "Bash"));
        let worker = DelegateJob {
            agent: None, title: None, model: String::new(),
            display: Default::default(), queued_at_ms: None,
            started_at_ms: Some(1_000), selection: None, routing: None,
            touches: None, waiting: None, dismissed_from_panel: false,
        };
        assert_eq!(activity_summary(Some(&tool), &[&worker], None, 66_000),
            (format!("{tool_label}{STATUS_SEPARATOR}{}", tr!("chat-status-workers", count = 1)),
                Some("0:20".to_owned())));
        assert_eq!(turn_activity_detail(&tool), format!("{tool_label}{STATUS_SEPARATOR}4:59"));
        let approval = TurnActivity { tool: Some(ToolActivity::AwaitingApproval), ..tool };
        assert_eq!(activity_summary(Some(&approval), &[], None, 66_000),
            (tr!("chat-status-waiting").to_owned(), Some("0:20".to_owned())));
        assert_eq!(activity_summary(Some(&approval), &[&worker], None, 66_000),
            (tr!("chat-status-working").to_owned(), Some(format!("{}{STATUS_SEPARATOR}1:05", tr!("chat-status-count", count = 1)))));
        assert_eq!(turn_activity_detail(&approval), format!("{}{STATUS_SEPARATOR}6:12", tr!("chat-approval-pending")));
        let consulting = TurnActivity { tool: Some(ToolActivity::Consulting { elapsed: Duration::from_secs(7) }), ..approval };
        assert_eq!(activity_summary(Some(&consulting), &[], None, 66_000),
            (tr!("chat-consulting").to_owned(), Some("0:20".to_owned())));
        let finished = TurnActivity { tool: None, ..approval };
        assert_eq!(turn_activity_detail(&finished), format!("{}{STATUS_SEPARATOR}6:12", tr!("chat-writing")));

        let waiting = TimedWait {
            reason: ChatWaitReason::UsagePaused { resume_at: Some(126) },
            since: Instant::now() - Duration::from_secs(8),
        };
        assert_eq!(activity_summary(None, &[], Some(waiting), 66_000).0, tr!("chat-status-waiting"));
        assert!(waiting_label(waiting, 66_000).contains(&tr!("chat-wait-usage-resume", remaining = "1:00")));
    }

    #[test]
    fn conversation_job_heading_omits_configured_names_and_details_include_them() {
        let job = DelegateJob {
            agent: Some("custom-review-agent".into()),
            title: None,
            model: "test-configured-model".into(),
            display: Default::default(), queued_at_ms: None,
            started_at_ms: Some(10),
            selection: Some(AgentSelection {
                backend: AgentBackend::Codex, model: Some("test-configured-model".into()),
                effort: Some("high".into()),
            }),
            routing: Some(fuzukue_proto::TierDecision { tier: fuzukue_proto::Tier::Heavy, reason: "Proof".into() }),
            touches: None, waiting: None, dismissed_from_panel: false,
        };
        let details = delegate_details(&job, None).join("\n");
        let labels = [worker_heading(tr!("chat-working"), None, Some("0:01")), worker_heading(tr!("chat-done"), None, None)];
        for identity in ["test-configured-model", "custom-review-agent", "Codex", "Claude"] {
            assert!(labels.iter().all(|label| !label.contains(identity)), "{labels:?}");
        }
        assert!(details.contains("Codex") && details.contains("test-configured-model"));
        assert!(details.contains(tier_label(fuzukue_proto::Tier::Heavy)));
    }

    #[test]
    fn job_heading_shows_the_title_between_status_and_time() {
        let status = tr!("chat-working");
        let titled = worker_heading(status, Some(" 設定の読み込みを調べる "), Some("0:01"));
        assert!(titled.starts_with(&format!("{status}{STATUS_SEPARATOR}設定の読み込みを調べる{STATUS_SEPARATOR}")), "{titled}");
        assert_eq!(worker_heading(status, Some("  "), None), status);
        assert_eq!(worker_heading(status, None, None), status);
    }
}
