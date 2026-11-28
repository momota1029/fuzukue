//! Estimates and retained Markdown views for the chat transcript.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    rc::Rc,
};

use gpui_kit::base::text::{MarkdownExtensions, TextViewState};
use gpui_kit::component::text::TextView;
use gpui_kit::{
    px, App, AppContext as _, Entity, Font, IntoElement, ListOffset, Pixels, RenderOnce,
    SharedString, Window,
};

use super::{AssistantGroup, Content, RowKind, ToolRuns, CHAT_GROUP_GAP, CHAT_WIDTH};

pub(crate) struct TranscriptPosition {
    anchor: ListOffset,
    item: Option<String>,
}

impl super::Chat {
    pub(in crate) fn transcript_position(&self) -> Option<TranscriptPosition> {
        if self.scroller.is_following_tail() {
            return None;
        }
        let anchor = self.scroller.logical_scroll_top();
        if anchor.item_ix >= self.rows.len() {
            return None;
        }
        let item = self.by_item.iter()
            .filter_map(|(item, &row)| (row == anchor.item_ix).then_some(item))
            .chain(self.by_tool.iter()
                .filter_map(|(item, &(row, _))| (row == anchor.item_ix).then_some(item)))
            .min()
            .cloned();
        Some(TranscriptPosition { anchor, item })
    }

    pub(in crate) fn restore_transcript_position(&self, position: TranscriptPosition) {
        let Some(last) = self.rows.len().checked_sub(1) else { return };
        let mut anchor = position.anchor;
        anchor.item_ix = position.item.as_ref()
            .and_then(|item| {
                self.by_item.get(item).copied()
                    .or_else(|| self.by_tool.get(item).map(|&(row, _)| row))
            })
            .unwrap_or(anchor.item_ix.min(last));
        self.scroller.scroll_to(anchor);
    }
}

#[cfg(all(target_os = "linux", feature = "headless-shot"))]
impl super::Chat {
    pub(in crate) fn prepend_scroll_fixture(&mut self, cx: &mut gpui_kit::Context<crate::Workbench>) {
        // Exercise the transcript's normal insert-before-existing-rows path.
        let existing = self.rows.len();
        self.delegate_anchor = Some(0);
        for index in 0..4 {
            self.append_message(
                fuzukue_proto::Actor::Claude,
                Some(format!("fixture-earlier-{existing}-{index}")),
                format!("Earlier message {index}\n\n{}", "Extra wrapped text. ".repeat(index * 8)),
                true,
                cx,
            );
        }
        self.delegate_anchor = None;
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct TranscriptStyle {
    pub rem: Pixels,
    pub ui_font: Font,
    pub code_font: Font,
    pub code_scale: f32,
    pub math_scale: f32,
    pub dark: bool,
    pub show_worker_messages: bool,
    pub tail_padding: Pixels,
}

#[derive(Default)]
pub(super) struct TranscriptView {
    pub rows: RefCell<Option<Rc<TranscriptRows>>>,
    style: RefCell<Option<TranscriptStyle>>,
    revision: Cell<u64>,
    activation: Cell<u64>,
    markdown: RefCell<HashMap<(u64, MarkdownPart), MarkdownView>>,
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    parse_requests: Cell<usize>,
}

pub(super) struct TranscriptRows {
    pub kinds: Vec<RowKind>,
    pub estimates: Vec<RowEstimate>,
    pub tool_runs: ToolRuns,
    pub first_visible: Option<usize>,
    pub last_visible: Option<usize>,
    pub latest_message: Option<usize>,
    pub latest_reply: Option<usize>,
    pub groups: Vec<AssistantGroup>,
    pub show_worker_messages: bool,
}

#[derive(Hash, PartialEq, Eq)]
pub(super) enum MarkdownPart {
    Message,
    WorkerText(usize),
    WorkerReport,
    Code(String),
}

#[derive(Default)]
struct MarkdownView {
    fingerprint: Option<u64>,
    activation: u64,
    source: SharedString,
    state: CachedDocument,
}

type CachedDocument = Rc<RefCell<Option<(u64, Entity<TextViewState>)>>>;

#[derive(IntoElement)]
pub(super) struct TranscriptMarkdown<F: FnOnce(TextView, &App) -> TextView + 'static> {
    source: SharedString,
    fingerprint: u64,
    state: CachedDocument,
    extensions: MarkdownExtensions,
    configure: F,
}

impl<F: FnOnce(TextView, &App) -> TextView + 'static> RenderOnce for TranscriptMarkdown<F> {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut cached = self.state.borrow_mut();
        let (fingerprint, state) = cached.get_or_insert_with(|| {
            let state = cx.new(|cx| {
                TextViewState::markdown_for_immediate_layout(
                    &self.source,
                    self.extensions.clone(),
                    cx,
                )
            });
            (self.fingerprint, state)
        });
        if *fingerprint != self.fingerprint {
            *fingerprint = self.fingerprint;
            state.update(cx, |state, cx| state.set_text(&self.source, cx));
        }
        (self.configure)(
            TextView::new(state).markdown_extensions(self.extensions),
            cx,
        )
    }
}

impl TranscriptView {
    pub(super) fn forget_rows(&self, removed: &HashSet<u64>) {
        self.rows.borrow_mut().take();
        self.markdown.borrow_mut().retain(|(row, _), _| !removed.contains(row));
    }

    pub fn style(&self, style: TranscriptStyle) -> u64 {
        let mut current = self.style.borrow_mut();
        if current.as_ref() != Some(&style) {
            *current = Some(style);
            self.revision.set(self.revision.get().wrapping_add(1));
        }
        self.revision.get()
    }

    pub fn current_style(&self) -> Option<TranscriptStyle> {
        self.style.borrow().clone()
    }

    pub fn invalidate_markdown(&self) {
        self.markdown.borrow_mut().clear();
    }

    pub fn activate(&self) {
        self.activation.set(self.activation.get().wrapping_add(1));
    }

    pub fn markdown<F: FnOnce(TextView, &App) -> TextView + 'static>(
        &self,
        row: u64,
        part: MarkdownPart,
        source: &str,
        extensions: MarkdownExtensions,
        configure: F,
    ) -> TranscriptMarkdown<F> {
        let code = matches!(part, MarkdownPart::Code(_));
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let mut cache = self.markdown.borrow_mut();
        let view = cache.entry((row, part)).or_default();
        let activation = self.activation.get();
        if view.fingerprint != Some(fingerprint) {
            if view.activation != activation {
                // A message edited while this thread was hidden needs an exact
                // first measurement. Active streaming can retain its old document.
                view.state = CachedDocument::default();
            }
            #[cfg(all(target_os = "linux", feature = "headless-shot"))]
            self.parse_requests.set(self.parse_requests.get() + 1);
            view.fingerprint = Some(fingerprint);
            view.source = if code {
                fuzukue_view::markdown_code::fenced_output(source).into()
            } else {
                fuzukue_view::xml_components::link_card_refs(source).into()
            };
        }
        view.activation = activation;
        TranscriptMarkdown {
            source: view.source.clone(),
            fingerprint,
            state: view.state.clone(),
            extensions,
            configure,
        }
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub fn parse_requests(&self) -> usize {
        self.parse_requests.get()
    }
}

#[derive(Clone, Copy)]
pub(super) enum RowEstimate {
    Text {
        bytes: usize,
        lines: usize,
        code: bool,
        padding: f32,
    },
    Disclosure(usize),
}

impl RowEstimate {
    pub fn for_content(actor: &fuzukue_proto::Actor, content: &Content) -> Self {
        let (text, padding) = match content {
            Content::Message(message) => (
                message.text.as_str(),
                if *actor == fuzukue_proto::Actor::User {
                    2.25
                } else {
                    1.0
                },
            ),
            Content::Notice { text, .. } | Content::PlanNotice { text, .. } => (text.as_str(), 1.0),
            Content::InternalNote {
                text, open: true, ..
            } => (text.as_str(), 2.0),
            Content::Tools(group) if group.open => {
                return Self::Disclosure(1 + group.calls.len() + group.approvals.len());
            }
            Content::Question { lines, .. } => return Self::Disclosure(*lines),
            _ => return Self::Disclosure(1),
        };
        Self::Text {
            bytes: text.len(),
            lines: text.lines().count().max(1),
            code: text.contains("```") || text.contains("~~~"),
            padding,
        }
    }

    pub fn height(self, width: Pixels, rem: Pixels, code_scale: f32) -> Pixels {
        let rem = f32::from(rem);
        match self {
            Self::Text {
                bytes,
                lines,
                code,
                padding,
            } => {
                let columns = ((f32::from(width).min(CHAT_WIDTH) - rem * 4.) / (rem * 0.65))
                    .max(10.) as usize;
                let lines = lines.max(bytes.div_ceil(columns));
                let line_height = if code { 1.5 * code_scale } else { 1.55 };
                px(rem * (lines as f32 * line_height + padding))
            }
            Self::Disclosure(lines) => px(rem * (1.6 * lines as f32 + 0.5)) + CHAT_GROUP_GAP,
        }
    }
}
