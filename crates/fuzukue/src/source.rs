use std::{ops::RangeInclusive, path::Path, rc::Rc, time::Duration};

pub(super) mod card;
pub(super) mod editing;
mod lean;
mod lsp;
mod yulang;

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{
        EditorState, GoToDefinition, GutterDecorationCollection, InputEvent, RangeDecoration, RangeDecorationCollection,
        RangeDecorationStyle, RopeExt as _, ShowCompletions,
    },
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::base::Disableable as _;
use gpui_kit::{
    div, prelude::*, px, Context, Entity, Focusable as _, MouseButton, SharedString,
    Subscription, Task, Window,
};
use fuzukue_proto::{ClientMsg, LspStatus, SourceEdit, SourceRange, SourceSaveOutcome, EDITOR_PROTOCOL, LSP_PROTOCOL};
use fuzukue_view::source_language_with_text;
use sha2::{Digest, Sha256};

use super::{icons::Symbol, RightPane, Workbench};
use crate::source_outline::{self, OutlineEntry};

#[derive(Default)]
pub(super) struct SourcePane {
    tabs: Vec<SourceTab>,
    active: Option<u64>,
    next_id: u64,
    outline_open: bool,
    pub(super) centered: bool,
    lsp_requests: Rc<lsp::Requests>,
}

struct SourceTab {
    id: u64,
    path: String,
    _load: Task<()>,
    error: Option<String>,
    document: Option<SourceDocument>,
    closing: bool,
    return_to_library: bool,
    lsp_location: Option<lsp_types::Position>,
    reading: Option<lean::Reading>,
    reading_line: Option<usize>,
}

struct SourceDocument {
    editor: Entity<EditorState>,
    _subscription: Subscription,
    _observe: Subscription,
    base: String,
    language: &'static str,
    focus: Option<(SourceRange, String)>,
    outline: Vec<OutlineEntry>,
    outline_task: Option<Task<()>>,
    line_mark: Option<RangeDecorationCollection>,
    dirty: bool,
    edited: bool,
    pending: Option<u64>,
    conflict: Option<Conflict>,
    message: Option<String>,
    lsp: Rc<lsp::Remote>,
    lsp_status: LspStatus,
    lsp_diagnostics: Vec<serde_json::Value>,
    lsp_gutter: Option<GutterDecorationCollection>,
}

struct Conflict {
    latest: String,
    draft: String,
    diff: Option<String>,
}

impl SourcePane {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut SourceTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    fn document_mut(&mut self, id: u64) -> Option<&mut SourceDocument> {
        self.tab_mut(id)?.document.as_mut()
    }

    fn active_tab(&self) -> Option<&SourceTab> {
        self.tabs.iter().find(|tab| Some(tab.id) == self.active)
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn active_id(&self) -> Option<u64> {
        self.active
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn active_editor(&self) -> Option<&Entity<EditorState>> {
        Some(&self.active_tab()?.document.as_ref()?.editor)
    }

    #[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
    pub(super) fn caret_for_test(&self, cx: &gpui_kit::App) -> Option<(u32, gpui_kit::Pixels)> {
        let editor = self.active_tab()?.document.as_ref()?.editor.read(cx);
        Some((editor.cursor_position().line, editor.scroll_offset().x))
    }

    pub(super) fn has_unsaved(&self) -> bool {
        self.tabs
            .iter()
            .filter_map(|tab| tab.document.as_ref())
            .any(|doc| doc.dirty || doc.pending.is_some())
    }

    pub(super) fn has_open_path(&self, path: &str) -> bool {
        self.tabs.iter().any(|tab| tab.path == path)
    }

    fn stop_lsp(&self) {
        self.lsp_requests.clear();
        for doc in self.tabs.iter().filter_map(|tab| tab.document.as_ref()) {
            doc.lsp.offline();
        }
    }

    pub(super) fn clear(&mut self) {
        self.stop_lsp();
        self.tabs.clear();
        self.centered = false;
        self.active = None;
    }
}

impl SourceDocument {
    fn edit(&self, path: &str, cx: &gpui_kit::App) -> SourceEdit {
        let (base, range, text) = if let Some(conflict) = &self.conflict {
            (&conflict.latest, None, conflict.draft.clone())
        } else {
            (
                &self.base,
                self.focus.as_ref().map(|(range, _)| *range),
                self.editor.read(cx).text().to_string(),
            )
        };
        SourceEdit {
            path: path.into(),
            base_hash: format!("{:x}", Sha256::digest(base)),
            range,
            text,
        }
    }

    fn local_file(&self, cx: &gpui_kit::App) -> String {
        let text = self.editor.read(cx).text().to_string();
        let Some((range, _)) = &self.focus else {
            return text;
        };
        let mut draft = self.base.clone();
        draft.replace_range(range.start..range.end, &text);
        draft
    }

    fn visible_base(&self) -> &str {
        match &self.focus {
            Some((range, _)) => &self.base[range.start..range.end],
            None => &self.base,
        }
    }
}

impl Workbench {
    pub(super) fn open_memo_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_source_file(path, None, window, cx);
        if let Some(tab) = self.source.active.and_then(|id| self.source.tab_mut(id)) {
            tab.return_to_library = true;
        }
    }

    pub(super) fn open_source_file(
        &mut self,
        path: String,
        lines: Option<RangeInclusive<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.right = RightPane::Source;
        if let Some(tab) = self
            .source
            .tabs
            .iter()
            .find(|tab| tab.path == path && tab.error.is_none())
        {
            let id = tab.id;
            self.select_source(id, window, cx);
            if lines.is_some() {
                if let Some(tab) = self.source.tab_mut(id) {
                    tab.reading = None;
                    tab.reading_line = None;
                }
                self.leave_source_node(id, window, cx);
            }
            if let (Some(lines), Some(doc)) = (lines, self.source.document_mut(id)) {
                if doc.focus.is_none() {
                    if let Some(mark) = doc.line_mark.take() {
                        mark.dispose(cx);
                    }
                    doc.line_mark = Some(jump_lines(&doc.editor, lines, window, cx));
                }
            }
            return;
        }
        self.source.tabs.retain(|tab| tab.path != path);
        if self.source.tabs.is_empty() {
            self.source.outline_open = true;
        }
        let id = self.source.id();
        let http = self.http.clone();
        let request_path = path.clone();
        let current_protocol = self.protocol >= EDITOR_PROTOCOL;
        let load = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = if current_protocol {
                        http.fetch_editor(&request_path)
                    } else {
                        http.fetch(&request_path)
                    }?;
                    let text = String::from_utf8(bytes).map_err(|err| err.to_string())?;
                    if text.contains('\0') {
                        return Err(tr!("editor-a-utf-8-text-file-is-required").into());
                    }
                    let language = source_language_with_text(&request_path, &text);
                    let outline = source_outline::extract(language, &text);
                    Ok((text, language, outline))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((base, language, outline)) => {
                        this.install_source_document(
                            id, base, language, outline, lines, window, cx,
                        );
                    }
                    Err(error) => {
                        if let Some(tab) = this.source.tab_mut(id) {
                            tab.error = Some(error);
                        }
                    }
                }
                cx.notify();
            });
        });
        self.source.tabs.push(SourceTab {
            id,
            path,
            _load: load,
            error: None,
            document: None,
            closing: false,
            return_to_library: false,
            lsp_location: None,
            reading: None,
            reading_line: None,
        });
        self.source.active = Some(id);
        cx.notify();
    }

    fn install_source_document(
        &mut self,
        id: u64,
        base: String,
        language: &'static str,
        outline: Vec<OutlineEntry>,
        lines: Option<RangeInclusive<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.source.active == Some(id) && self.right == RightPane::Source;
        let remote = Rc::new(lsp::Remote::new(id, language, self.out.clone(), self.source.lsp_requests.clone()));
        let (editor, subscription, observe) = Self::make_source_editor(id, language, &base, &remote, window, cx);
        let mut line_mark = None;
        if let Some(lines) = lines {
            if active {
                line_mark = Some(jump_lines(&editor, lines, window, cx));
            }
        } else if active {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        tab.error = None;
        if self.protocol >= LSP_PROTOCOL {
            remote.open(&tab.path, base.clone());
        }
        let doc = SourceDocument {
            editor,
            _subscription: subscription,
            _observe: observe,
            language,
            base,
            outline,
            focus: None,
            outline_task: None,
            line_mark,
            dirty: false,
            edited: false,
            pending: None,
            conflict: None,
            message: None,
            lsp: remote,
            lsp_status: LspStatus::Unavailable,
            lsp_diagnostics: Vec::new(),
            lsp_gutter: None,
        };
        if active {
            lsp::jump_lsp(&doc, tab.lsp_location.take(), window, cx);
        }
        tab.document = Some(doc);
        if let Some(line) = tab.reading_line.take() {
            self.show_lean_reading(id, Some(line), cx);
        }
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn complete_source_load_for_shot(
        &mut self,
        id: u64,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        let path = tab.path.clone();
        // Replacing the pending task cancels the test's intentionally unserved HTTP request.
        tab._load = Task::ready(());
        let language = source_language_with_text(&path, text);
        let outline = source_outline::extract(language, text);
        self.install_source_document(id, text.to_owned(), language, outline, None, window, cx);
    }

    fn make_source_editor(
        id: u64,
        language: &'static str,
        text: &str,
        remote: &Rc<lsp::Remote>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<EditorState>, Subscription, Subscription) {
        let workbench = cx.entity().downgrade();
        let editor = cx.new(|cx| {
            let mut state = editing::editor_state(language, text, window, cx);
            remote.install(&mut state, workbench);
            state
        });
        let subscription = cx.subscribe_in(
            &editor,
            window,
            move |this, editor, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.source_changed(id, &editor, window, cx);
                }
            },
        );
        let observe = cx.observe(&editor, |_, _, cx| cx.notify());
        (editor, subscription, observe)
    }

    fn source_changed(
        &mut self,
        id: u64,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.source.document_mut(id) else {
            return;
        };
        if doc.editor != *editor {
            return;
        }
        let text = editor.read(cx).text().to_string();
        doc.dirty = editing::is_dirty(doc.visible_base(), editor.read(cx).text());
        doc.edited |= doc.dirty;
        if doc.lsp.sync(doc.local_file(cx), false) || doc.dirty {
            doc.lsp_diagnostics.clear();
            if let Some(gutter) = &doc.lsp_gutter { gutter.clear(cx); }
        }
        if doc.focus.is_none() {
            let language = doc.language;
            doc.outline_task = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let outline = cx
                    .background_executor()
                    .spawn(async move { source_outline::extract(language, &text) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(doc) = this.source.document_mut(id) {
                        if doc.focus.is_none() {
                            doc.outline = outline;
                            doc.outline_task = None;
                            cx.notify();
                        }
                    }
                });
            }));
        }
        cx.notify();
    }

    fn select_source(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.source.active = Some(id);
        if let Some(doc) = self.source.document_mut(id) {
            doc.editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
        cx.notify();
    }

    pub(super) fn close_source_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.source.tabs.iter().find(|tab| Some(tab.id) == self.source.active && tab.return_to_library) {
            self.close_source(tab.id, false, window, cx);
        } else {
            self.close_right_pane(cx);
        }
    }

    fn close_source(
        &mut self,
        id: u64,
        discard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.source.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let tab = &mut self.source.tabs[index];
        if let Some(doc) = &tab.document {
            if doc.pending.is_some() {
                return;
            }
            if doc.dirty && !discard {
                tab.closing = true;
                self.source.active = Some(id);
                cx.notify();
                return;
            }
            doc.lsp.close();
            if doc.edited && self.protocol >= EDITOR_PROTOCOL {
                let _ = self.out.send(ClientMsg::EditorClosed {
                    path: tab.path.clone(),
                });
            }
        }
        let return_to_library = self.source.tabs.remove(index).return_to_library;
        if self.source.active == Some(id) {
            if let Some(tab) = self
                .source
                .tabs
                .get(index)
                .or_else(|| self.source.tabs.last())
            {
                self.select_source(tab.id, window, cx);
            } else {
                self.source.active = None;
                self.close_right_pane(cx);
            }
            if return_to_library && matches!(self.right, RightPane::Source | RightPane::None) {
                self.right = RightPane::Library;
                self.reset_library_search(false, cx);
            }
        }
        cx.notify();
    }

    fn save_source(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        if self.protocol < EDITOR_PROTOCOL {
            return;
        }
        let Some(id) = self.source.active else { return };
        let request = self.source.id();
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        let Some(doc) = &mut tab.document else { return };
        if !doc.dirty || doc.pending.is_some() || doc.conflict.is_some() != overwrite {
            return;
        }
        let edit = doc.edit(&tab.path, cx);
        match self.out.send(ClientMsg::SaveSource { request, edit }) {
            Ok(()) => {
                doc.pending = Some(request);
                doc.message = None;
                doc.editor
                    .update(cx, |editor, cx| editor.set_readonly(true, cx));
            }
            Err(error) => doc.message = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(super) fn source_disconnected(&mut self, cx: &mut Context<Self>) {
        self.source.stop_lsp();
        for doc in self
            .source
            .tabs
            .iter_mut()
            .filter_map(|tab| tab.document.as_mut())
        {
            doc.lsp_status = LspStatus::Unavailable;
            doc.lsp_diagnostics.clear();
            if doc.lsp_gutter.is_some() { doc.paint_lsp_diagnostics(cx); }
            if doc.pending.take().is_some() {
                if doc.conflict.is_none() {
                    doc.editor
                        .update(cx, |editor, cx| editor.set_readonly(false, cx));
                }
                doc.message = Some(tr!("editor-connection-lost-before-the-save-result-arrived").into());
            }
        }
    }

    pub(super) fn on_source_saved(
        &mut self,
        request: u64,
        outcome: SourceSaveOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.source.tabs.iter_mut().find(|tab| {
            tab.document
                .as_ref()
                .is_some_and(|doc| doc.pending == Some(request))
        }) else {
            return;
        };
        let Some(doc) = tab.document.as_mut() else {
            return;
        };
        doc.pending = None;
        match outcome {
            SourceSaveOutcome::Saved { text, range } => {
                let focus = doc.focus.take().and_then(|(_, name)| {
                    range
                        .filter(|range| text.get(range.start..range.end).is_some())
                        .map(|range| (range, name))
                });
                Self::reset_source(doc, &tab.path, text, focus, window, cx);
                doc.lsp.sync(doc.base.clone(), true);
                doc.message = None;
                tab.closing = false;
                if tab.return_to_library {
                    let id = tab.id;
                    self.close_source(id, false, window, cx);
                }
            }
            SourceSaveOutcome::Conflict { latest } => {
                let draft = doc
                    .conflict
                    .take()
                    .map_or_else(|| doc.local_file(cx), |conflict| conflict.draft);
                doc.conflict = Some(Conflict { latest, draft, diff: None });
            }
            SourceSaveOutcome::Error { message } => {
                if doc.conflict.is_none() {
                    doc.editor
                        .update(cx, |editor, cx| editor.set_readonly(false, cx));
                }
                doc.message = Some(message);
            }
        }
        cx.notify();
    }

    fn reset_source(
        doc: &mut SourceDocument,
        path: &str,
        base: String,
        focus: Option<(SourceRange, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        doc.base = base;
        doc.focus = focus;
        doc.lsp.project(&doc.base, doc.focus.as_ref().map(|(range, _)| range.start..range.end));
        doc.conflict = None;
        doc.dirty = false;
        let language = source_language_with_text(path, &doc.base);
        doc.outline_task = None;
        doc.outline = if doc.focus.is_none() {
            source_outline::extract(language, &doc.base)
        } else {
            Vec::new()
        };
        doc.editor.update(cx, |editor, cx| {
            editor.set_readonly(false, cx);
            if doc.language != language {
                editor.set_highlighter(language, cx);
                editor.set_editing_rules(editing::editing_rules(language));
                editor.set_tab_size(editing::tab_size(language), cx);
            }
            if editor.text().to_string() != doc.visible_base() {
                let position = editor.cursor_position();
                let focused = editor.focus_handle(cx).is_focused(window);
                editor.set_value(doc.visible_base(), window, cx);
                if focused {
                    editor.set_cursor_position(position, window, cx);
                }
            }
        });
        doc.language = language;
    }

    fn reload_source_conflict(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.source.active else { return };
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        let Some(doc) = tab.document.as_mut() else {
            return;
        };
        if doc.pending.is_some() {
            return;
        }
        let Some(conflict) = doc.conflict.take() else {
            return;
        };
        Self::reset_source(doc, &tab.path, conflict.latest, None, window, cx);
        doc.lsp.sync(doc.base.clone(), false);
        doc.message = None;
        doc.editor.update(cx, |editor, cx| editor.focus(window, cx));
        cx.notify();
    }

    fn focus_source_node(
        &mut self,
        id: u64,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.source.document_mut(id) else {
            return;
        };
        if self.protocol < EDITOR_PROTOCOL
            || doc.focus.is_some()
            || doc.outline_task.is_some()
            || doc.dirty
            || doc.pending.is_some()
            || doc.conflict.is_some()
        {
            return;
        }
        let Some(entry) = doc.outline.get(index) else {
            return;
        };
        if !entry.editable {
            return;
        }
        let range = SourceRange {
            start: entry.bytes.start,
            end: entry.bytes.end,
        };
        let Some(text) = doc.base.get(range.start..range.end) else {
            return;
        };
        doc.lsp.project(&doc.base, Some(range.start..range.end));
        let (editor, subscription, observe) = Self::make_source_editor(id, doc.language, text, &doc.lsp, window, cx);
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        doc.editor = editor;
        doc._subscription = subscription;
        doc._observe = observe;
        doc.line_mark = None;
        doc.focus = Some((range, entry.name.clone()));
        doc.lsp_gutter = None;
        if !doc.lsp_diagnostics.is_empty() { doc.paint_lsp_diagnostics(cx); }
        doc.outline_task = None;
        cx.notify();
    }

    fn leave_source_node(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.source.document_mut(id) else {
            return;
        };
        if doc.dirty || doc.pending.is_some() || doc.conflict.is_some() {
            return;
        }
        let Some((range, _)) = doc.focus.take() else {
            return;
        };
        doc.lsp.project(&doc.base, None);
        let (editor, subscription, observe) =
            Self::make_source_editor(id, doc.language, &doc.base, &doc.lsp, window, cx);
        jump_offset(&editor, range.start, window, cx);
        doc.editor = editor;
        doc._subscription = subscription;
        doc._observe = observe;
        doc.line_mark = None;
        doc.outline = source_outline::extract(doc.language, &doc.base);
        doc.lsp_gutter = None;
        if !doc.lsp_diagnostics.is_empty() { doc.paint_lsp_diagnostics(cx); }
        cx.notify();
    }

    pub(super) fn render_source_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::pane_resize::PANE_INSET;
        let theme = cx.theme().clone();
        let editable = self.protocol >= EDITOR_PROTOCOL;
        let tabs = h_flex()
            .w_full()
            .min_w_0()
            .flex_1()
            .overflow_x_scrollbar()
            .id("source-tabs")
            .children(self.source.tabs.iter().enumerate().map(|(index, tab)| {
                let id = tab.id;
                let dirty = tab.document.as_ref().is_some_and(|doc| doc.dirty);
                let name = Path::new(&tab.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&tab.path);
                h_flex()
                    .flex_none()
                    .gap_1()
                    .when(index > 0, |tab| tab.pl_2())
                    .pr_1()
                    .py_1()
                    .when(Some(id) == self.source.active, |d| d.bg(theme.secondary))
                    .child(
                        source_button(format!("source-tab-{id}"), name, true, cx).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.select_source(id, window, cx)
                            }),
                        ),
                    )
                    .when(dirty, |d| {
                        d.child(editing::dirty_dot(cx))
                    })
                    .child(
                        Button::new(format!("source-close-{id}")).ghost().xsmall()
                            .icon(Symbol::X.icon())
                            .tooltip(tr!("editor-close-tab"))
                            .accessibility_label(tr!("editor-close-tab"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_source(id, false, window, cx)
                            })),
                    )
            }));
        let mut pane = v_flex()
            .id("source-pane")
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                h_flex().id("source-pane-header").h_8().flex_none().min_w_0().px(PANE_INSET)
                    .gap_2().border_b_1().border_color(theme.sidebar_border)
                    .child(tabs)
                    .children((self.pane_close_id() == Some("close-source"))
                        .then(|| self.pane_close_button("close-source", cx))),
            )
            .capture_key_down(
                cx.listener(|this, ev: &gpui_kit::KeyDownEvent, window, cx| {
                    let mods = ev.keystroke.modifiers;
                    if this.protocol >= LSP_PROTOCOL && ev.keystroke.key == "f12" && !mods.control && !mods.alt && !mods.shift && !mods.platform {
                        window.dispatch_action(Box::new(GoToDefinition), cx);
                        cx.stop_propagation();
                    } else if this.protocol >= LSP_PROTOCOL && ev.keystroke.key == "space"
                        && mods.control && !mods.alt && !mods.shift && !mods.platform
                    {
                        window.dispatch_action(Box::new(ShowCompletions), cx);
                        cx.stop_propagation();
                    } else if editing::is_save_key(ev) {
                        let composing = this
                            .source
                            .active_tab()
                            .and_then(|tab| tab.document.as_ref())
                            .is_some_and(|doc| super::focused_composition(&doc.editor, window, cx));
                        if !composing {
                            this.save_source(false, cx);
                        }

                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(cx.listener(|this, _: &gpui_kit::base::input::Escape, window, cx| {
                if this.source.active_tab().and_then(|tab| tab.document.as_ref())
                    .is_some_and(|doc| super::focused_composition(&doc.editor, window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &gpui_kit::base::input::Escape, window, cx| {
                if let Some(tab) = this.source.active_tab() {
                    let id = tab.id;
                    if tab.closing {
                        this.source.tab_mut(id).unwrap().closing = false;
                        cx.notify();
                    } else {
                        this.close_source(id, false, window, cx);
                    }
                    cx.stop_propagation();
                }
            }));
        let Some(tab) = self.source.active_tab() else {
            return pane;
        };
        let id = tab.id;
        let doc = tab.document.as_ref();
        let saving = doc.is_some_and(|d| d.pending.is_some());
        let header = h_flex()
            .px(PANE_INSET)
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .child(self.render_file_header(&tab.path, "source-file-breadcrumb", cx)),
            )
            .when_some(doc.filter(|doc| {
                doc.lsp_status != LspStatus::Unavailable
                    || (doc.language == "yulang" && self.protocol >= LSP_PROTOCOL)
            }), |header, doc| {
                let (label, color) = match doc.lsp_status {
                    LspStatus::Unavailable => (tr!("editor-lsp-yulang-unavailable"), theme.muted_foreground),
                    LspStatus::Starting => (tr!("editor-lsp-starting"), theme.muted_foreground),
                    LspStatus::Running => (tr!("editor-lsp-running"), theme.link),
                    LspStatus::Failed => (tr!("editor-lsp-failed"), theme.danger),
                    LspStatus::Idle => (tr!("editor-lsp-idle"), theme.muted_foreground),
                };
                header.child(div().id("source-lsp-status").whitespace_nowrap().text_xs().text_color(color).child(label))
            })
            .child(
                source_button(
                    "source-dock-toggle",
                    if self.source.centered { tr!("editor-move-to-right-pane") } else { tr!("editor-move-to-center") },
                    true,
                    cx,
                )
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.source.centered = !this.source.centered;
                    cx.notify();
                })),
            )
            .child(
                source_button(
                    "source-outline-toggle",
                    tr!("editor-outline"),
                    doc.is_some_and(|d| d.focus.is_none()),
                    cx,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.source.outline_open = !this.source.outline_open;
                        cx.notify();
                    }),
                ),
            )
            .children(doc.filter(|doc| doc.dirty).map(|_| div().id("source-dirty").child(editing::dirty_dot(cx))))
            .child(
                Button::new("source-save").ghost().xsmall()
                    .label(if saving { tr!("editor-saving") } else { tr!("editor-save") })
                    .tooltip(tr!("editor-save-shortcut"))
                    .disabled(!editable || !doc.is_some_and(|d| d.dirty && !saving && d.conflict.is_none()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.source.active_tab().and_then(|tab| tab.document.as_ref())
                            .is_some_and(|doc| super::focused_composition(&doc.editor, window, cx))
                        {
                            this.save_source(false, cx);
                        }
                    })),
            )
            .child(
                Button::new("source-stop-editing").ghost().xsmall()
                    .label(tr!("editor-stop-editing"))
                    .tooltip(tr!("editor-stop-editing-shortcut"))
                    .disabled(saving)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_source(id, false, window, cx);
                    })),
            );
        let header = header.children(self.cards.iter().find(|card| card.path == tab.path)
            .and_then(|card| self.card_editor_history_button(format!("source-history-{id}"), card, cx.weak_entity())));
        let header = header.when(
            self.protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL
                && doc.is_some_and(|d| d.language == "lean"),
            |header| {
                header.child(
                    source_button("literate-lean-toggle", if tab.reading.is_some() {
                        tr!("literate-lean-source")
                    } else { tr!("literate-lean-reading") }, true, cx)
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                            this.toggle_lean_reading(id, window, cx);
                        })),
                )
            },
        );
        pane = pane.child(header);
        let Some(doc) = doc else {
            return pane.child(
                div()
                    .p_4()
                    .text_sm()
                    .child(tab.error.clone().unwrap_or_else(|| tr!("editor-loading").into())),
            );
        };

        if tab.closing {
            pane = pane.child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .child(tr!("editor-unsaved-changes"))
                    .child(
                        source_button("source-discard", tr!("editor-discard-changes-and-close"), !saving, cx)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.close_source(id, true, window, cx)
                                }),
                            ),
                    )
                    .child(
                        source_button("source-keep", tr!("editor-cancel"), true, cx).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if let Some(tab) = this.source.tab_mut(id) {
                                    tab.closing = false;
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            );
        }

        if let Some(message) = &doc.message {
            pane = pane.child(div().px_2().py_1().text_xs().child(message.clone()));
        }
        if let Some(conflict) = &doc.conflict {
            pane = pane.child(editing::conflict_banner([
                Button::new("source-reload").ghost().xsmall().label(tr!("editor-reload"))
                    .disabled(saving)
                    .on_click(cx.listener(|this, _, window, cx| this.reload_source_conflict(window, cx))),
                Button::new("source-view-diff").ghost().xsmall()
                    .label(if conflict.diff.is_some() { tr!("editor-hide-diff") } else { tr!("editor-view-diff") })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(conflict) = this.source.document_mut(id).and_then(|doc| doc.conflict.as_mut()) {
                            conflict.diff = if conflict.diff.is_some() { None } else {
                                Some(editing::conflict_diff(&conflict.latest, &conflict.draft))
                            };
                            cx.notify();
                        }
                    })),
                Button::new("source-overwrite").ghost().xsmall().label(tr!("editor-overwrite"))
                    .disabled(saving)
                    .on_click(cx.listener(|this, _, _, cx| this.save_source(true, cx))),
            ], conflict.diff.clone(), cx));
        }
        if let Some(reading) = &tab.reading {
            return pane.child(self.render_lean_reading(tab, reading, cx));
        }
        if let Some((_, name)) = &doc.focus {
            pane = pane.child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        source_button("source-whole", tr!("editor-whole-file"), !doc.dirty && !saving, cx)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.leave_source_node(id, window, cx)
                                }),
                            ),
                    )
                    .child(div().text_sm().truncate().child(name.clone())),
            );
        } else if self.source.outline_open && !doc.outline.is_empty() {
            let ready = doc.outline_task.is_none();
            pane = pane.child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .max_h(px(156.))
                    .overflow_y_scrollbar()
                    .id("source-outline")
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .children(doc.outline.iter().enumerate().map(|(index, entry)| {
                        let start = entry.bytes.start;
                        h_flex()
                            .gap_2()
                            .pl(px(8. * entry.depth.min(8) as f32))
                            .child(
                                div().flex_1().min_w_0().child(
                                    source_button(
                                        format!("source-jump-{index}"),
                                        &entry.name,
                                        ready,
                                        cx,
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            if let Some(doc) = this
                                                .source
                                                .document_mut(id)
                                                .filter(|doc| doc.outline_task.is_none())
                                            {
                                                jump_offset(&doc.editor, start, window, cx);
                                            }
                                        }),
                                    ),
                                ),
                            )
                            .when(entry.editable && editable, |d| {
                                d.child(
                                    source_button(
                                        format!("source-focus-{index}"),
                                        tr!("editor-edit-range"),
                                        !doc.dirty && !saving,
                                        cx,
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.focus_source_node(id, index, window, cx)
                                        }),
                                    ),
                                )
                            })
                    })),
            );
        }

        let path = Path::new(&tab.path);
        let refs: Vec<_> = self
            .cards
            .iter()
            .filter(|card| {
                card.files
                    .iter()
                    .any(|file| !file.is_empty() && path.starts_with(Path::new(file)))
                    || card.tex.iter().any(|file| path == Path::new(file))
            })
            .collect();
        if !refs.is_empty() && doc.focus.is_none() {
            pane = pane.child(
                h_flex()
                    .id("source-refs")
                    .overflow_x_scrollbar()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(tr!("editor-referenced-card"))
                    .children(refs.into_iter().map(|card| {
                        let id = card.id.clone();
                        let label = if card.title.is_empty() {
                            &card.id
                        } else {
                            &card.title
                        };
                        source_button(format!("source-ref-{id}"), label, true, cx).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)),
                        )
                    })),
            );
        }
        pane.child(
            div()
                .id("source-editor")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .child(
                    editing::surface(
                        &doc.editor, &self.appearance, doc.language,
                        tr!("editor-source-editor"), !editable || saving || doc.conflict.is_some(),
                    ),
                ),
        ).child(editing::status(&doc.editor, doc.language, doc.dirty, saving, cx))
    }
}

fn source_button(
    id: impl Into<SharedString>,
    label: &str,
    enabled: bool,
    cx: &Context<Workbench>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    div()
        .id(id.into())
        .px_1()
        .rounded_sm()
        .text_xs()
        .truncate()
        .when(enabled, |d| {
            d.cursor_pointer().hover(|d| d.bg(theme.secondary))
        })
        .when(!enabled, |d| d.text_color(theme.muted_foreground))
        .child(label.to_owned())
}

fn jump_lines(
    editor: &Entity<EditorState>,
    lines: RangeInclusive<usize>,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> RangeDecorationCollection {
    editor.update(cx, |state, cx| {
        let last_row = state.text().lines_len().saturating_sub(1);
        let first = lines.start().saturating_sub(1).min(last_row);
        let last = lines.end().saturating_sub(1).min(last_row);
        let bytes = state.text().line_start_offset(first)..state.text().line_end_offset(last);
        let position = state.text().offset_to_position(bytes.start);
        let mark = state.create_range_decorations_collection(
            vec![RangeDecoration::new(bytes).with_style(RangeDecorationStyle::Fill)],
            cx,
        );
        state.unfold_at(position, cx);
        state.set_cursor_position(position, window, cx);
        // Cursor movement locates the row in the display map, including folds.
        mark
    })
}

fn jump_offset(
    editor: &Entity<EditorState>,
    offset: usize,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    editor.update(cx, |state, cx| {
        let position = state.text().offset_to_position(offset);
        state.unfold_at(position, cx);
        state.set_cursor_position(position, window, cx);
    });
}
