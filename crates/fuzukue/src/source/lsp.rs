use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
};

use anyhow::Result;
use futures::channel::oneshot;
use gpui_kit::{
    component::{
        input::{
            CompletionProvider, DefinitionProvider, EditorState, GutterDecoration, HoverProvider,
            Rope, RopeExt as _,
        },
        ActiveTheme as _,
    },
    App, Context, Task, Window,
};
use fuzukue_proto::{ClientMsg, LspBuffer, LspOutcome, LspPosition, LspQuery, LspStatus, LspUpdate};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents,
    InsertTextFormat, LocationLink, MarkedString, MarkupContent, MarkupKind, Position,
    Range as LspRange, TextEdit,
};
use serde_json::Value;

use super::{jump_offset, SourceDocument, Workbench};
use crate::conn::Sender;

#[derive(Default)]
pub(super) struct Requests {
    next: Cell<u64>,
    pending: RefCell<HashMap<u64, oneshot::Sender<LspOutcome>>>,
}

impl Requests {
    pub(super) fn clear(&self) {
        self.pending.borrow_mut().clear();
    }
}

pub(super) struct Remote {
    document: u64,
    out: Sender,
    requests: Rc<Requests>,
    online: Cell<bool>,
    version: Cell<u64>,
    sent: RefCell<String>,
    projection: RefCell<(String, String)>,
    lean: bool,
}

impl Remote {
    pub(super) fn new(document: u64, language: &str, out: Sender, requests: Rc<Requests>) -> Self {
        Self {
            document,
            out,
            requests,
            online: Cell::new(false),
            version: Cell::new(0),
            sent: RefCell::default(),
            projection: RefCell::default(),
            lean: language == "lean",
        }
    }

    pub(super) fn open(&self, path: &str, text: String) {
        let version = self.advance();
        self.online.set(
            self.out
                .send(ClientMsg::LspBuffer {
                    document: self.document,
                    update: LspBuffer::Open {
                        path: path.into(),
                        version,
                        text: text.clone(),
                    },
                })
                .is_ok(),
        );
        *self.sent.borrow_mut() = text;
    }

    pub(super) fn offline(&self) {
        self.online.set(false);
    }

    pub(super) fn close(&self) {
        if self.online.replace(false) {
            let _ = self.out.send(ClientMsg::LspBuffer {
                document: self.document,
                update: LspBuffer::Close,
            });
        }
    }

    pub(super) fn project(&self, base: &str, focus: Option<Range<usize>>) {
        *self.projection.borrow_mut() = focus.map_or_else(
            || (String::new(), String::new()),
            |range| (base[..range.start].into(), base[range.end..].into()),
        );
    }

    pub(super) fn sync(&self, text: String, save: bool) -> bool {
        if !self.online.get() {
            return false;
        }
        let mut sent = self.sent.borrow_mut();
        let changed = *sent != text;
        if changed {
            self.advance();
            sent.clone_from(&text);
        } else if !save {
            return false;
        }
        let version = self.version.get();
        let update = if save {
            LspBuffer::Save { version, text }
        } else {
            LspBuffer::Change { version, text }
        };
        let _ = self.out.send(ClientMsg::LspBuffer {
            document: self.document,
            update,
        });
        changed
    }

    fn advance(&self) -> u64 {
        let next = self.version.get() + 1;
        self.version.set(next);
        next
    }

    fn snapshot(&self, rope: &Rope) -> Snapshot {
        let (prefix, suffix) = &*self.projection.borrow();
        let text = rope.to_string();
        let visible = prefix.len()..prefix.len() + text.len();
        let whole = format!("{prefix}{text}{suffix}");
        Snapshot { whole, visible }
    }

    pub(super) fn request(&self, query: LspQuery) -> Option<oneshot::Receiver<LspOutcome>> {
        if !self.online.get() {
            return None;
        }
        let request = self.requests.next.get() + 1;
        self.requests.next.set(request);
        self.out
            .send(ClientMsg::LspRequest {
                request,
                document: self.document,
                query,
            })
            .ok()?;
        let (tx, rx) = oneshot::channel();
        let mut pending = self.requests.pending.borrow_mut();
        pending.retain(|_, tx| !tx.is_canceled());
        pending.insert(request, tx);
        Some(rx)
    }

    pub(super) fn install(
        self: &Rc<Self>,
        editor: &mut EditorState,
        workbench: gpui_kit::WeakEntity<Workbench>,
    ) {
        let lsp = editor.lsp_mut();
        lsp.completion_provider = Some(self.clone());
        lsp.definition_provider = Some(self.clone());
        lsp.hover_provider = Some(self.clone());
        lsp.show_document = Some(Rc::new(move |params, window, cx| {
            let Ok(url) = url::Url::parse(params.uri.as_str()) else {
                return false;
            };
            if url.scheme() != "fuzukue" {
                return false;
            }
            let Some((_, path)) = url.query_pairs().find(|(key, _)| key == "path") else {
                return true;
            };
            let (path, position) = (path.into_owned(), params.selection.map(|range| range.start));
            let workbench = workbench.clone();
            window.defer(cx, move |window, cx| {
                let _ = workbench.update(cx, |view, cx| {
                    view.open_lsp_location(path, position, window, cx)
                });
            });
            true
        }));
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.close();
    }
}

async fn value(reply: Option<oneshot::Receiver<LspOutcome>>) -> Result<Value> {
    let Some(reply) = reply else {
        return Ok(Value::Null);
    };
    match reply.await? {
        LspOutcome::Value { value } => Ok(value),
        LspOutcome::Error { .. } => Ok(Value::Null),
    }
}

struct Snapshot {
    whole: String,
    visible: Range<usize>,
}

impl Snapshot {
    fn position(&self, offset: usize) -> LspPosition {
        let before = &self.whole[..self.visible.start + offset];
        LspPosition {
            line: before.bytes().filter(|byte| *byte == b'\n').count() as u32,
            character: before
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .encode_utf16()
                .count() as u32,
        }
    }

    fn local_range(&self, range: LspRange, clip: bool) -> Option<LspRange> {
        let mut start = byte_offset(&self.whole, range.start)?;
        let mut end = byte_offset(&self.whole, range.end)?;
        if start > end || end < self.visible.start || start > self.visible.end {
            return None;
        }
        if clip {
            start = start.max(self.visible.start);
            end = end.min(self.visible.end);
        } else if start < self.visible.start || end > self.visible.end {
            return None;
        }
        let local = &self.whole[self.visible.clone()];
        let position = |offset| {
            let before = &local[..offset - self.visible.start];
            Position::new(
                before.bytes().filter(|byte| *byte == b'\n').count() as u32,
                before
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .count() as u32,
            )
        };
        Some(LspRange::new(position(start), position(end)))
    }
}

pub(super) fn byte_offset(text: &str, position: Position) -> Option<usize> {
    let mut lines = text.split_inclusive('\n');
    let mut offset = 0;
    for _ in 0..position.line {
        offset += lines.next()?.len();
    }
    let line = lines
        .next()
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n']);
    let mut column = 0;
    for ch in line.chars() {
        if column >= position.character {
            break;
        }
        column += ch.len_utf16() as u32;
        if column > position.character {
            break;
        }
        offset += ch.len_utf8();
    }
    Some(offset)
}

impl HoverProvider for Remote {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let snapshot = self.snapshot(text);
        let position = snapshot.position(offset);
        let hover = self.request(LspQuery::Hover { position });
        let goal = self
            .lean
            .then(|| self.request(LspQuery::LeanGoal { position }))
            .flatten();
        let goal_heading = tr!("editor-lsp-lean-goal");
        let term_heading = tr!("editor-lsp-lean-term-goal");
        cx.spawn(async move |_| {
            let (hover, goal) = futures::try_join!(value(hover), value(goal))?;
            let mut hover: Option<Hover> = serde_json::from_value(hover)?;
            if let Some(hover) = &mut hover {
                hover.range = hover
                    .range
                    .and_then(|range| snapshot.local_range(range, true));
            }
            let mut sections = Vec::new();
            if let Some(rendered) = goal["goal"]["rendered"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
            {
                sections.push(format!("**{goal_heading}**\n\n{rendered}"));
            }
            if let Some(term) = goal["term_goal"]["goal"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
            {
                sections.push(format!("**{term_heading}**\n\n```lean\n{term}\n```"));
            }
            if sections.is_empty() {
                return Ok(hover);
            }
            let range = hover.as_ref().and_then(|hover| hover.range);
            if let Some(hover) = hover {
                sections.insert(0, hover_markdown(hover.contents));
            }
            Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: sections.join("\n\n"),
                }),
                range,
            }))
        })
    }
}

fn hover_markdown(contents: HoverContents) -> String {
    let marked = |content| match content {
        MarkedString::String(value) => value,
        MarkedString::LanguageString(value) => {
            format!("```{}\n{}\n```", value.language, value.value)
        }
    };
    match contents {
        HoverContents::Scalar(value) => marked(value),
        HoverContents::Array(values) => values
            .into_iter()
            .map(marked)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(value) => value.value,
    }
}

impl DefinitionProvider for Remote {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let snapshot = self.snapshot(text);
        let reply = self.request(LspQuery::Definition {
            position: snapshot.position(offset),
        });
        cx.spawn(async move |_| {
            // The bridge rewrites file URIs to workspace-relative paths before sending them.
            let mut response = value(reply).await?;
            let targets = match &mut response {
                Value::Array(items) => items.iter_mut().collect::<Vec<_>>(),
                Value::Object(_) => vec![&mut response],
                _ => vec![],
            };
            for target in targets {
                for field in ["uri", "targetUri"] {
                    if let Some(path) = target[field].as_str() {
                        let mut url = url::Url::parse("fuzukue://workspace")?;
                        url.query_pairs_mut().append_pair("path", path);
                        target[field] = url.as_str().into();
                    }
                }
            }
            let response: Option<GotoDefinitionResponse> = serde_json::from_value(response)?;
            let from_location = |location: lsp_types::Location| LocationLink {
                origin_selection_range: None,
                target_uri: location.uri,
                target_range: location.range,
                target_selection_range: location.range,
            };
            Ok(match response {
                Some(GotoDefinitionResponse::Scalar(location)) => vec![from_location(location)],
                Some(GotoDefinitionResponse::Array(locations)) => {
                    locations.into_iter().map(from_location).collect()
                }
                Some(GotoDefinitionResponse::Link(mut links)) => {
                    for link in &mut links {
                        link.origin_selection_range = link
                            .origin_selection_range
                            .and_then(|range| snapshot.local_range(range, true));
                    }
                    links
                }
                None => vec![],
            })
        })
    }
}

impl CompletionProvider for Remote {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let snapshot = self.snapshot(text);
        // The editor calls providers before emitting its Change event.
        self.sync(snapshot.whole.clone(), false);
        let prefix = text
            .chars_at(offset)
            .reversed()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        let range = LspRange::new(
            text.offset_to_position(offset - prefix),
            text.offset_to_position(offset),
        );
        let reply = self.request(LspQuery::Completion {
            position: snapshot.position(offset),
            context: serde_json::to_value(CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            })
            .unwrap_or_default(),
        });
        cx.spawn(async move |_| {
            let items: Option<CompletionResponse> = serde_json::from_value(value(reply).await?)?;
            let items = match items {
                Some(CompletionResponse::Array(items)) => items,
                Some(CompletionResponse::List(list)) => list.items,
                None => Vec::new(),
            };
            Ok(CompletionResponse::Array(
                items
                    .into_iter()
                    .filter_map(|item| completion(item, &snapshot, range))
                    .collect(),
            ))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, text: &str, _cx: &mut App) -> bool {
        self.online.get()
            && text.chars().count() == 1
            && text
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | ':'))
    }
}

fn completion(
    mut item: CompletionItem,
    snapshot: &Snapshot,
    fallback: LspRange,
) -> Option<CompletionItem> {
    // The editor applies a single replacement. Keep entries whose edit can be applied in full.
    if item.insert_text_format == Some(InsertTextFormat::SNIPPET)
        || item
            .additional_text_edits
            .as_ref()
            .is_some_and(|edits| !edits.is_empty())
    {
        return None;
    }
    item.text_edit = Some(match item.text_edit.take() {
        Some(CompletionTextEdit::Edit(mut edit)) => {
            edit.range = snapshot.local_range(edit.range, false)?;
            CompletionTextEdit::Edit(edit)
        }
        Some(CompletionTextEdit::InsertAndReplace(mut edit)) => {
            edit.insert = snapshot.local_range(edit.insert, false)?;
            edit.replace = snapshot.local_range(edit.replace, false)?;
            CompletionTextEdit::InsertAndReplace(edit)
        }
        None => CompletionTextEdit::Edit(TextEdit::new(
            fallback,
            item.insert_text
                .take()
                .unwrap_or_else(|| item.label.clone()),
        )),
    });
    Some(item)
}

impl Workbench {
    pub(crate) fn on_lsp_result(&self, request: u64, outcome: LspOutcome) {
        if let Some(reply) = self
            .source
            .lsp_requests
            .pending
            .borrow_mut()
            .remove(&request)
        {
            let _ = reply.send(outcome);
        }
    }

    pub(crate) fn on_lsp_document(
        &mut self,
        document: u64,
        update: LspUpdate,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.source.document_mut(document) else {
            return;
        };
        match update {
            LspUpdate::Status { status } => {
                doc.lsp_status = status;
                doc.lsp
                    .online
                    .set(!matches!(status, LspStatus::Unavailable));
            }
            LspUpdate::Diagnostics {
                version,
                diagnostics,
            } if doc.lsp.version.get() == version => {
                doc.lsp_diagnostics = diagnostics;
                doc.paint_lsp_diagnostics(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn source_lsp_connected(&mut self, cx: &mut Context<Self>) {
        if self.protocol < fuzukue_proto::LSP_PROTOCOL {
            return;
        }
        for tab in &self.source.tabs {
            if let Some(doc) = &tab.document {
                doc.lsp.open(&tab.path, doc.local_file(cx));
            }
        }
    }

    fn open_lsp_location(
        &mut self,
        path: String,
        position: Option<Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = position.map(|position| position.line as usize + 1);
        self.open_source_file(path, line.map(|line| line..=line), window, cx);
        let Some(tab) = self.source.active.and_then(|id| self.source.tab_mut(id)) else {
            return;
        };
        if let Some(doc) = &tab.document {
            if doc.focus.is_none() {
                jump_lsp(doc, position, window, cx);
            }
        } else {
            tab.lsp_location = position;
        }
    }
}

pub(super) fn jump_lsp(
    doc: &SourceDocument,
    position: Option<Position>,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    if let Some(offset) =
        position.and_then(|pos| byte_offset(&doc.editor.read(cx).text().to_string(), pos))
    {
        jump_offset(&doc.editor, offset, window, cx);
    }
}

impl SourceDocument {
    pub(super) fn paint_lsp_diagnostics(&mut self, cx: &mut Context<Workbench>) {
        let theme = cx.theme();
        let (error, warning, info) = (theme.danger, theme.warning, theme.link);
        let text = self.editor.read(cx).text().clone();
        let snapshot = self.lsp.snapshot(&text);
        let mut diagnostics = self
            .lsp_diagnostics
            .iter()
            .filter_map(|value| {
                let mut item: lsp_types::Diagnostic = serde_json::from_value(value.clone()).ok()?;
                item.range = snapshot.local_range(item.range, true)?;
                Some(item)
            })
            .collect::<Vec<_>>();
        diagnostics.sort_by_key(|item| (item.range.start.line, item.range.start.character));
        let marks = diagnostics
            .iter()
            .map(|item| {
                let color = match item.severity {
                    Some(DiagnosticSeverity::ERROR) => error,
                    Some(DiagnosticSeverity::WARNING) => warning,
                    _ => info,
                };
                let start = text.position_to_offset(&item.range.start);
                let end = text.position_to_offset(&item.range.end);
                let range = if start == end {
                    let row = item.range.start.line as usize;
                    text.line_start_offset(row)..text.line_end_offset(row)
                } else {
                    start..end
                };
                GutterDecoration::new(range).with_color(color)
            })
            .collect();
        self.editor.update(cx, |editor, cx| {
            if let Some(set) = editor.diagnostics_mut() {
                set.reset(&text);
                set.extend(diagnostics);
            }
            editor.clear_diagnostic_popover(cx);
            if self.lsp_gutter.is_none() {
                self.lsp_gutter = Some(editor.create_gutter_decorations_collection(Vec::new(), cx));
            }
            cx.notify();
        });
        if let Some(gutter) = &self.lsp_gutter {
            gutter.set(marks, cx);
        }
    }
}
