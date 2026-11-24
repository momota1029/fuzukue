//! Markdown decoration ranges in the composer's editable source text.

use std::ops::Range;

use gpui_kit::base::input::{RopeExt as _, RoundedBackground, TextDecoration};
use gpui_kit::base::text::parse_markdown_ast;
use gpui_kit::component::{
    input::{InputEvent, TextareaState},
    ActiveTheme as _, Theme,
};
use gpui_kit::{
    px, Context, Entity, Focusable as _, Font, FontStyle, FontWeight, HighlightStyle, SharedString,
    StrikethroughStyle, UnderlineStyle, Window,
};
use markdown::mdast::Node;

use super::{appearance::Appearance, Workbench};

const MAX_CHARS: usize = 20_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Style {
    Bold,
    Italic,
    BoldItalic,
    Strike,
    Link,
    InlineCode,
    CodeBlock,
    InlineCodeBackground,
    CodeBlockBackground,
    Heading(u8),
    Quote,
    ListMarker,
    Math,
    MarkerDim,
    CodeMarkerDim,
    CodeInfo,
}

impl Style {
    fn is_marker(self) -> bool {
        self == Self::MarkerDim
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Span {
    range: Range<usize>,
    style: Style,
    code_span: Option<Range<usize>>,
}

pub(super) fn parse(source: &str) -> Option<Node> {
    // markdown 1.0.0 can panic while an open fence inside a list is being
    // typed (for example "- ```\n1."). Keep the editable source visible.
    std::panic::catch_unwind(|| {
        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        parse_markdown_ast(source, &options).ok()
    })
    .ok()
    .flatten()
}

/// Parse the original source. The caret line is zero-based and ignores visual wrapping.
fn spans(text: &str, caret_line: Option<usize>) -> Vec<Span> {
    if text.is_empty() || text.chars().take(MAX_CHARS + 1).count() > MAX_CHARS {
        return Vec::new();
    }

    let Some(root) = parse(text) else {
        return Vec::new();
    };

    let mut collector = Collector {
        text,
        spans: Vec::new(),
    };
    // Walk the owned nodes so deeply nested drafts don't require recursive traversal or drop.
    let mut nodes = vec![(root, None, 0)];
    while let Some((mut node, emphasis, quote_depth)) = nodes.pop() {
        let (emphasis, quote_depth) = collector.visit(&node, emphasis, quote_depth);
        if let Some(children) = node.children_mut() {
            nodes.extend(
                std::mem::take(children)
                    .into_iter()
                    .rev()
                    .map(|child| (child, emphasis, quote_depth)),
            );
        }
    }
    let Some(line) = caret_line.and_then(|line| {
        std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .nth(line)
    }) else {
        return collector.spans;
    };
    let end = text[line..]
        .find('\n')
        .map_or(text.len(), |offset| line + offset + 1);
    collector
        .spans
        .into_iter()
        .flat_map(|span| {
            visible_ranges(span.range.clone(), span.style.is_marker(), Some(&(line..end)))
                .map(move |range| Span { range, ..span.clone() })
        })
        .collect()
}

fn visible_ranges(
    range: Range<usize>,
    marker: bool,
    active_line: Option<&Range<usize>>,
) -> impl Iterator<Item = Range<usize>> {
    match active_line {
        Some(line) if marker && range.start < line.end && line.start < range.end => [
            (range.start < line.start).then_some(range.start..line.start),
            (line.end < range.end).then_some(line.end..range.end),
        ],
        _ => [Some(range), None],
    }
    .into_iter()
    .flatten()
}

fn source_range(node: &Node) -> Option<Range<usize>> {
    node.position().map(|pos| pos.start.offset..pos.end.offset)
}

struct Collector<'a> {
    text: &'a str,
    spans: Vec<Span>,
}

impl Collector<'_> {
    fn push(&mut self, range: Range<usize>, style: Style) {
        if !range.is_empty() {
            self.spans.push(Span {
                range,
                style,
                code_span: None,
            });
        }
    }

    fn marker(&mut self, range: Range<usize>) {
        self.push(range, Style::MarkerDim);
    }

    fn code_marker(&mut self, range: Range<usize>, code_span: Range<usize>) {
        self.spans.push(Span {
            range,
            style: Style::CodeMarkerDim,
            code_span: Some(code_span),
        });
    }

    fn delimited(&mut self, range: Range<usize>, width: usize, style: Style) {
        if width == 0 || range.len() < width * 2 {
            return;
        }
        self.push(range.start + width..range.end - width, style);
        let opening = range.start..range.start + width;
        let closing = range.end - width..range.end;
        if style == Style::InlineCode {
            self.code_marker(opening, range.clone());
            self.code_marker(closing, range);
        } else {
            self.marker(opening);
            self.marker(closing);
        }
    }

    fn visit(
        &mut self,
        node: &Node,
        mut emphasis: Option<Style>,
        mut quote_depth: usize,
    ) -> (Option<Style>, usize) {
        let Some(range) = source_range(node) else {
            return (emphasis, quote_depth);
        };
        let Some(raw) = self.text.get(range.clone()) else {
            return (emphasis, quote_depth);
        };
        match node {
            Node::Strong(_) | Node::Emphasis(_) => {
                let (width, style) = if matches!(node, Node::Strong(_)) {
                    (2, Style::Bold)
                } else {
                    (1, Style::Italic)
                };
                let combined = match emphasis {
                    Some(parent) if parent != style => Style::BoldItalic,
                    _ => style,
                };
                self.delimited(range, width, combined);
                emphasis = Some(combined);
            }
            Node::Delete(_) => self.delimited(range, prefix_len(raw, b'~'), Style::Strike),
            Node::InlineCode(_) => {
                self.push(range.clone(), Style::InlineCodeBackground);
                self.delimited(range, prefix_len(raw, b'`'), Style::InlineCode);
            }
            Node::InlineMath(_) => self.delimited(range, prefix_len(raw, b'$'), Style::Math),
            Node::Code(_) if matches!(raw.as_bytes().first(), Some(b'`' | b'~')) => {
                self.fenced(range, raw, quote_depth, Style::CodeBlock);
            }
            Node::Math(_) => self.fenced(range, raw, quote_depth, Style::Math),
            Node::Heading(heading) if raw.starts_with('#') && !raw.contains(['\r', '\n']) => {
                let opening_end = range.start + usize::from(heading.depth);
                self.marker(range.start..opening_end);
                let content_end = heading
                    .children
                    .last()
                    .and_then(source_range)
                    .map_or(opening_end, |p| p.end);
                if let Some(content_start) = heading
                    .children
                    .first()
                    .and_then(source_range)
                    .map(|p| p.start)
                {
                    self.push(content_start..content_end, Style::Heading(heading.depth));
                }
                let suffix = self.text[content_end..range.end].trim_matches([' ', '\t']);
                if suffix.starts_with('#') {
                    let end = range.end - (raw.len() - raw.trim_end_matches([' ', '\t']).len());
                    self.marker(end - suffix.len()..end);
                }
            }
            Node::Blockquote(_) => {
                self.quote(range, raw, quote_depth);
                quote_depth += 1;
            }
            Node::ListItem(item) => {
                let digits = raw.bytes().take_while(u8::is_ascii_digit).count();
                let marker_end = range.start + digits + 1;
                self.push(range.start..marker_end, Style::ListMarker);
                self.marker(range.start..marker_end);
                if item.checked.is_some() {
                    let rest = &raw[digits + 1..];
                    let start =
                        marker_end + rest.len() - rest.trim_start_matches([' ', '\t']).len();
                    if matches!(self.text.get(start..start + 3), Some("[ ]" | "[x]" | "[X]")) {
                        self.push(start..start + 3, Style::ListMarker);
                        self.marker(start..start + 3);
                    }
                }
            }
            Node::Link(link) if raw.starts_with('[') => {
                let label_end = link
                    .children
                    .last()
                    .and_then(source_range)
                    .map_or(range.start + 1, |p| p.end);
                if let Some(closing) = self.text[label_end..range.end].find("](") {
                    let closing = label_end + closing;
                    self.push(range.start + 1..closing, Style::Link);
                    self.marker(range.start..range.start + 1);
                    self.marker(closing..range.end);
                }
            }
            Node::Link(_) if raw.starts_with('<') => self.delimited(range, 1, Style::Link),
            Node::Link(_) => self.push(range, Style::Link),
            _ => {}
        }
        (emphasis, quote_depth)
    }

    fn quote(&mut self, range: Range<usize>, raw: &str, depth: usize) {
        let mut start = range.start;
        for line in raw.split_inclusive('\n') {
            let mut content = line;
            // The AST starts at this quote's marker on the first line.
            let depth = if start == range.start { 0 } else { depth };
            for _ in 0..depth {
                content = content
                    .trim_start_matches([' ', '\t'])
                    .strip_prefix('>')
                    .unwrap_or("");
            }
            let trimmed = content.trim_start_matches([' ', '\t']);
            let end = start + line.len();
            let body = if trimmed.starts_with('>') {
                let marker = end - trimmed.len();
                self.marker(marker..marker + 1);
                marker + 1 + usize::from(matches!(trimmed.as_bytes().get(1), Some(b' ' | b'\t')))
            } else {
                start
            };
            self.push(
                body..start + line.trim_end_matches(['\r', '\n']).len(),
                Style::Quote,
            );
            start = end;
        }
    }

    fn fenced(&mut self, range: Range<usize>, raw: &str, quote_depth: usize, style: Style) {
        let byte = raw.as_bytes()[0];
        let opening_width = prefix_len(raw, byte);
        let first_break = raw.find('\n').unwrap_or(raw.len());
        let closing = raw.rfind('\n').and_then(|last_break| {
            let last_line = raw[last_break + 1..].trim_end_matches([' ', '\t', '\r']);
            let mut closing = last_line.trim_start_matches([' ', '\t']);
            for _ in 0..quote_depth {
                closing = closing.strip_prefix('>')?.trim_start_matches([' ', '\t']);
            }
            (closing.len() >= opening_width && closing.bytes().all(|ch| ch == byte)).then(|| {
                let start = range.start + last_break + 1 + last_line.len() - closing.len();
                let body_end = range.start + raw[..last_break].trim_end_matches('\r').len();
                (start..start + closing.len(), body_end)
            })
        });
        let code_span = range.start..closing.as_ref().map_or(range.end, |(marker, _)| marker.end);
        let opening = range.start..range.start + opening_width;
        match style {
            Style::CodeBlock => {
                self.push(range.clone(), Style::CodeBlockBackground);
                self.code_marker(opening, code_span.clone());
                let info_end = raw[..first_break].trim_end_matches([' ', '\t', '\r']).len();
                self.push(
                    range.start + opening_width..range.start + info_end,
                    Style::CodeInfo,
                );
            }
            // Math flow keeps its existing explicit-close behavior.
            _ if closing.is_some() => self.marker(opening),
            _ => return,
        }
        if first_break == raw.len() {
            return;
        }
        let body_end = if let Some((marker, body_end)) = closing {
            if style == Style::CodeBlock {
                self.code_marker(marker, code_span);
            } else {
                self.marker(marker);
            }
            body_end
        } else {
            range.end
        };
        self.push(range.start + first_break + 1..body_end, style);
    }
}

fn prefix_len(text: &str, byte: u8) -> usize {
    text.bytes().take_while(|&ch| ch == byte).count()
}

/// Return the one edit to apply for an Enter inside a Markdown list or quote.
/// All ranges are UTF-8 byte offsets, like the input's selection.
pub(super) fn continue_line(source: &str, selection: Range<usize>) -> Option<(Range<usize>, String)> {
    let start = source[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    let end = source[selection.start..]
        .find('\n')
        .map_or(source.len(), |i| selection.start + i);
    let line = &source[start..end];

    let mut after_quotes = line;
    let mut last_quote = None;
    while let Some(rest) = after_quotes.trim_start_matches([' ', '\t']).strip_prefix('>') {
        last_quote = Some(line.len() - after_quotes.len());
        after_quotes = rest.strip_prefix([' ', '\t']).unwrap_or(rest);
    }
    let item = after_quotes.trim_start_matches([' ', '\t']);
    let (prefix, body, next_marker, empty_start) = if let Some((body, marker)) = list_item(item) {
        (&line[..line.len() - item.len()], body, marker, line.len() - after_quotes.len())
    } else {
        let last_quote = last_quote?;
        let prefix = line.len() - line[last_quote..].trim_start_matches([' ', '\t']).len();
        (&line[..prefix], after_quotes, "> ".to_owned(), last_quote)
    };
    let body_start = end - body.len();
    if selection.start < body_start || selection.end > end || in_code_fence(source, selection.start) {
        return None;
    }
    if body.trim().is_empty() {
        return Some((start + empty_start..end, String::new()));
    }
    Some((selection, format!("\n{prefix}{next_marker}")))
}

fn list_item(item: &str) -> Option<(&str, String)> {
    let digits = item.bytes().take_while(u8::is_ascii_digit).count();
    let marker = item.get(digits..)?.chars().next()?;
    let rest = item.get(digits + marker.len_utf8()..)?;
    let body = if rest.is_empty() {
        rest
    } else {
        rest.strip_prefix([' ', '\t'])?
    };
    match (digits, marker) {
        (0, '-' | '*' | '+') => {
            let task_body = ["[ ]", "[x]", "[X]"].iter().find_map(|marker| {
                let rest = body.trim_start_matches([' ', '\t']).strip_prefix(marker)?;
                if rest.is_empty() { Some(rest) } else { rest.strip_prefix([' ', '\t']) }
            });
            Some(task_body.map_or_else(
                || (body, format!("{marker} ")),
                |body| (body, format!("{marker} [ ] ")),
            ))
        }
        (1.., '.' | ')') => {
            let number = item[..digits].parse::<u64>().ok()?.checked_add(1)?;
            Some((body, format!("{number}{marker} ")))
        }
        _ => None,
    }
}

fn in_code_fence(source: &str, offset: usize) -> bool {
    if !source.contains("```") && !source.contains("~~~") {
        return false;
    }
    // The parser understands fences nested under lists and quotes, including
    // an unfinished fence while typing. On a parse failure, keep Enter plain.
    let Some(root) = parse(source) else { return true };
    let mut nodes = vec![&root];
    while let Some(node) = nodes.pop() {
        if matches!(node, Node::Code(_)) && source_range(node).is_some_and(|range| {
            let start = &source[range.start..];
            range.start <= offset && offset <= range.end
                && (start.starts_with("```") || start.starts_with("~~~"))
        }) {
            return true;
        }
        if let Some(children) = node.children() {
            nodes.extend(children);
        }
    }
    false
}

pub(super) fn visual_edge_target(row: usize, rows: usize, down: bool, end: usize) -> Option<usize> {
    match (down, rows) {
        (_, 0) => None,
        (true, _) if row == rows - 1 => Some(end),
        (false, _) if row == 0 => Some(0),
        _ => None,
    }
}

use fuzukue_proto::{slash::{Spec, COMMANDS}, AgentBackend};

#[derive(Default)]
pub(super) struct CommandPalette {
    pub selected: usize,
    pub dismissed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommandKey {
    Move(usize),
    Complete(&'static str),
    Dismiss,
    Send,
}

pub(super) fn command_candidates(text: &str, backend: AgentBackend) -> Vec<Spec> {
    let Some(prefix) = text.strip_prefix('/') else { return Vec::new() };
    if prefix.starts_with('/') || prefix.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS.iter().copied().filter(|spec| spec.available(backend) && spec.name.starts_with(prefix)).collect()
}

impl CommandPalette {
    pub(super) fn key(&self, text: &str, backend: AgentBackend, key: &str, composing: bool) -> Option<CommandKey> {
        if composing || text.contains(['\r', '\n']) {
            return None;
        }
        let choices = if self.dismissed { Vec::new() } else { command_candidates(text, backend) };
        if let Some(selected) = choices.get(self.selected.min(choices.len().saturating_sub(1))) {
            return Some(match key {
                "up" => CommandKey::Move((self.selected + choices.len() - 1) % choices.len()),
                "down" => CommandKey::Move((self.selected + 1) % choices.len()),
                "escape" => CommandKey::Dismiss,
                "tab" => CommandKey::Complete(selected.name),
                "enter" if text.strip_prefix('/') == Some(selected.name) => CommandKey::Send,
                "enter" => CommandKey::Complete(selected.name),
                _ => return None,
            });
        }
        if key == "enter" && fuzukue_proto::slash::is_command(text) && text.len() > 1 {
            return Some(CommandKey::Send);
        }
        None
    }
}

#[derive(Default)]
pub(super) struct ComposerMarkdown {
    pub(super) drafts: Option<super::chat::ComposerDrafts>,
    source: Option<SharedString>,
    spans: Vec<Span>,
    code_font: Option<Font>,
    code_font_scale: f32,
    caret: Option<usize>,
    pub(super) composing: bool,
    pub(super) command_palette: CommandPalette,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Update {
    Text,
    Caret,
    Theme,
}

pub(super) fn observe(
    input: &Entity<TextareaState>,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let view = cx.weak_entity();
    let id = window.window_handle().window_id();
    cx.intercept_keystrokes(move |event, window, cx| {
        if window.window_handle().window_id() != id
            || event.keystroke.modifiers.modified()
            || !matches!(event.keystroke.key.as_str(), "enter" | "up" | "down" | "tab" | "escape")
        {
            return;
        }
        if view.update(cx, |this, cx| this.composer_command_key(&event.keystroke.key, window, cx)).unwrap_or(false) {
            cx.stop_propagation();
        }
    }).detach();
    cx.subscribe_in(input, window, |this, input, event, window, cx| {
        let reason = match event {
            InputEvent::Change => {
                this.composer_markdown.command_palette = CommandPalette::default();
                this.record_composer_text(cx);
                Update::Text
            }
            InputEvent::Focus | InputEvent::Blur => Update::Caret,
            _ => return,
        };
        this.composer_markdown
            .update(input, &this.appearance, reason, window, cx);
    })
    .detach();
    // Workspace and thread transitions already notify the Workbench after
    // replacing its conversation state. Restore the composer at that boundary.
    cx.observe_in(&cx.entity(), window, |this, _, window, cx| {
        if this.restore_composer_draft(window, cx) {
            this.composer_markdown.update(
                &this.input,
                &this.appearance,
                Update::Text,
                window,
                cx,
            );
            cx.notify();
        } else if this.composer_markdown.code_font.as_ref() != Some(&this.appearance.code_font)
            || this.composer_markdown.code_font_scale != this.appearance.settings.code_font_scale()
        {
            this.composer_markdown.update(
                &this.input,
                &this.appearance,
                Update::Theme,
                window,
                cx,
            );
        }
    })
    .detach();
    cx.on_app_quit(|this, _| {
        this.flush_composer_drafts();
        std::future::ready(())
    })
    .detach();
    cx.observe_in(input, window, |this, input, window, cx| {
        this.composer_markdown.update(
            &input,
            &this.appearance,
            Update::Caret,
            window,
            cx,
        );
    })
    .detach();
    cx.observe_global_in::<Theme>(window, |this, window, cx| {
        this.composer_markdown.update(
            &this.input,
            &this.appearance,
            Update::Theme,
            window,
            cx,
        );
    })
    .detach();
}

impl ComposerMarkdown {
    fn update(
        &mut self,
        input: &Entity<TextareaState>,
        appearance: &Appearance,
        reason: Update,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
        if super::composing(input, window, cx) {
            self.composing = true;
            return;
        }

        let finished_composing = std::mem::take(&mut self.composing);
        let state = input.read(cx);
        let mut source_changed = false;
        if reason == Update::Text || finished_composing {
            let source = (state.text().len_chars() <= MAX_CHARS).then(|| state.value());
            if source != self.source {
                self.spans = source
                    .as_deref()
                    .map_or_else(Vec::new, |text| spans(text, None));
                self.source = source;
                source_changed = true;
            }
        }
        let caret = state
            .focus_handle(cx)
            .is_focused(window)
            .then(|| state.cursor());
        if reason == Update::Caret && !finished_composing && caret == self.caret {
            return;
        }
        self.caret = caret;
        let active_line = caret.map(|offset| {
            let text = state.text();
            let line = text.offset_to_point(offset).row;
            text.line_start_offset(line)..text.line_start_offset(line + 1)
        });
        let theme = cx.theme();
        let mut backgrounds = Vec::new();
        let mut fonts = Vec::new();
        let update_base = source_changed || finished_composing || reason == Update::Theme;
        let mut decorations = Vec::new();
        for span in &self.spans {
            match span.style {
                Style::InlineCodeBackground => {
                    if update_base {
                        backgrounds.push(RoundedBackground::inline(
                            span.range.clone(),
                            theme.muted.blend(theme.foreground.opacity(0.07)),
                        ));
                    }
                }
                Style::CodeBlockBackground => {
                    if update_base {
                        backgrounds.push(RoundedBackground::block(
                            span.range.clone(),
                            theme.muted.blend(theme.foreground.opacity(0.04)),
                        ));
                    }
                }
                style => {
                    if update_base && matches!(style, Style::InlineCode | Style::CodeBlock) {
                        fonts.push((span.range.clone(), appearance.code_font.clone()));
                    }
                    let highlight = match style {
                        Style::CodeMarkerDim | Style::CodeInfo => HighlightStyle {
                            color: Some(theme.foreground),
                            fade_out: Some(
                                1.0 - code_marker_opacity(
                                    style,
                                    theme.mode.is_dark(),
                                    caret_in_code_span(caret, span.code_span.as_ref()),
                                ),
                            ),
                            ..Default::default()
                        },
                        _ => highlight(style, theme),
                    };
                    decorations.extend(
                        visible_ranges(span.range.clone(), style.is_marker(), active_line.as_ref())
                            .map(|range| TextDecoration::new(range, highlight)),
                    );
                }
            }
        }
        if update_base {
            self.code_font = Some(appearance.code_font.clone());
            let scale = appearance.settings.code_font_scale();
            self.code_font_scale = scale;
            input.update(cx, |state, cx| {
                state.set_scaled_base_decorations(Vec::new(), backgrounds, fonts, scale, cx)
            });
        }
        input.update(cx, |state, cx| state.set_decorations(decorations, cx));
    }
}

fn caret_in_code_span(caret: Option<usize>, code_span: Option<&Range<usize>>) -> bool {
    caret.zip(code_span)
        .is_some_and(|(offset, span)| (span.start..=span.end).contains(&offset))
}

fn code_marker_opacity(style: Style, dark: bool, active: bool) -> f32 {
    match style {
        Style::CodeInfo => 0.30,
        _ if active => 0.45,
        _ if dark => 0.14,
        _ => 0.20,
    }
}

fn highlight(style: Style, theme: &Theme) -> HighlightStyle {
    let mut highlight = HighlightStyle::default();
    match style {
        Style::Bold => highlight.font_weight = Some(FontWeight::BOLD),
        Style::Italic => highlight.font_style = Some(FontStyle::Italic),
        Style::BoldItalic => {
            highlight.font_weight = Some(FontWeight::BOLD);
            highlight.font_style = Some(FontStyle::Italic);
        }
        Style::Strike => {
            highlight.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            })
        }
        Style::Link => {
            highlight.color = Some(theme.link);
            highlight.underline = Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(theme.link),
                wavy: false,
            });
        }
        Style::InlineCode
        | Style::CodeBlock
        | Style::InlineCodeBackground
        | Style::CodeBlockBackground
        | Style::CodeMarkerDim
        | Style::CodeInfo => {}
        Style::Heading(_) => {
            highlight.font_weight = Some(FontWeight::BOLD);
            highlight.color = Some(theme.accent_foreground);
        }
        Style::Quote => {
            highlight.color = Some(theme.muted_foreground.blend(theme.foreground.opacity(0.18)));
        }
        Style::ListMarker => highlight.color = Some(theme.foreground),
        Style::Math => highlight.color = Some(theme.info.blend(theme.foreground.opacity(0.4))),
        Style::MarkerDim => highlight.fade_out = Some(0.42),
    }
    highlight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_continues_markdown_items() {
        for (initial, expected) in [
            ("- a|", "- a\n- "),
            ("* a|", "* a\n* "),
            ("+ a|", "+ a\n+ "),
            ("1. a|", "1. a\n2. "),
            ("9. a|", "9. a\n10. "),
            ("99) a|", "99) a\n100) "),
            ("- [ ] a|", "- [ ] a\n- [ ] "),
            ("- [x] a|", "- [x] a\n- [ ] "),
            ("  \t- a|", "  \t- a\n  \t- "),
            ("- 日本語|続き\n次の行", "- 日本語\n- 続き\n次の行"),
            ("- a\n- b\n- |", "- a\n- b\n"),
            ("  + |\n次の行", "\n次の行"),
            ("2. |", ""),
            ("- [ ] |", ""),
            ("> 引用|", "> 引用\n> "),
            ("> |", ""),
            ("> > 引用|", "> > 引用\n> > "),
            ("> - a|", "> - a\n> - "),
            ("> - |", "> "),
        ] {
            let caret = initial.find('|').unwrap();
            let mut result = initial.replacen('|', "", 1);
            let (range, replacement) = continue_line(&result, caret..caret)
                .unwrap_or_else(|| panic!("expected continuation for {initial:?}"));
            result.replace_range(range, &replacement);
            assert_eq!(result, expected, "{initial:?}");
        }
    }

    #[test]
    fn enter_stays_plain_in_code_fences_and_outside_markdown_items() {
        for source in [
            "plain|",
            "-| a",
            "-word|",
            "2)word|",
            "~~~md\n- a|\n~~~",
            "~~~md\n- a|",
            "> ~~~\n> - a|\n> ~~~",
            "- ~~~\n  - a|\n  ~~~",
        ] {
            let caret = source.find('|').unwrap();
            assert_eq!(continue_line(&source.replacen('|', "", 1), caret..caret), None, "{source:?}");
        }
        let backticks = "\u{60}".repeat(3);
        let source = format!("{backticks}\n- a\n{backticks}\n- b");
        let inside = source.find("a\n").unwrap() + 1;
        assert_eq!(continue_line(&source, inside..inside), None);
        assert_eq!(
            continue_line(&source, source.len()..source.len()),
            Some((source.len()..source.len(), "\n- ".to_owned())),
        );
    }

    #[test]
    fn arrows_use_the_first_and_last_visual_rows() {
        assert_eq!(visual_edge_target(0, 0, false, 0), None);
        assert_eq!(visual_edge_target(0, 1, false, 12), Some(0));
        assert_eq!(visual_edge_target(0, 1, true, 12), Some(12));
        // One logical line can span three visual rows.
        assert_eq!(visual_edge_target(0, 3, true, 12), None);
        assert_eq!(visual_edge_target(1, 3, false, 12), None);
        assert_eq!(visual_edge_target(1, 3, true, 12), None);
        assert_eq!(visual_edge_target(2, 3, true, 12), Some(12));
        assert_eq!(visual_edge_target(0, 3, false, 12), Some(0));
    }

    fn ranges(text: &str, line: Option<usize>, style: Style) -> Vec<Range<usize>> {
        spans(text, line)
            .into_iter()
            .filter(|span| span.style == style)
            .map(|span| span.range)
            .collect()
    }

    #[test]
    fn inline_styles_use_exact_source_ranges() {
        let text = "**b** *i* _i_ ***bi*** ~~s~~ `code`";
        assert_eq!(ranges(text, None, Style::Bold), [2..3]);
        assert_eq!(ranges(text, None, Style::Italic), [7..8, 11..12, 15..21]);
        assert_eq!(ranges(text, None, Style::BoldItalic), [17..19]);
        assert_eq!(ranges(text, None, Style::Strike), [25..26]);
        assert_eq!(ranges(text, None, Style::InlineCode), [30..34]);
        assert_eq!(
            ranges(text, None, Style::MarkerDim),
            [
                0..2,
                3..5,
                6..7,
                8..9,
                10..11,
                12..13,
                14..15,
                21..22,
                15..17,
                19..21,
                23..25,
                26..28,
            ]
        );
        assert_eq!(ranges(text, None, Style::CodeMarkerDim), [29..30, 34..35]);
    }

    #[test]
    fn blocks_and_task_markers_use_exact_source_ranges() {
        let text = "## title ##\n> quote\n\n- [x] task\n\n2. item\n\n```rs\ncode\n```";
        assert_eq!(ranges(text, None, Style::Heading(2)), [3..8]);
        assert_eq!(ranges(text, None, Style::Quote), [14..19]);
        assert_eq!(
            ranges(text, None, Style::ListMarker),
            [21..22, 23..26, 33..35]
        );
        assert_eq!(ranges(text, None, Style::CodeBlock), [48..52]);
        assert_eq!(
            ranges(text, None, Style::MarkerDim),
            [0..2, 9..11, 12..13, 21..22, 23..26, 33..35]
        );
        assert_eq!(ranges(text, None, Style::CodeMarkerDim), [42..45, 53..56]);
        assert_eq!(ranges(text, None, Style::CodeInfo), [45..47]);
        assert_eq!(ranges(text, None, Style::CodeBlockBackground), [42..56]);
        assert!(spans("#🙂\n--", None).is_empty());
    }

    #[test]
    fn quote_reply_decorates_prose_and_fenced_code_inside_one_quote() {
        use gpui_kit::base::text::editor_host::{parse_markdown, BlockNode};

        let (text, reply) =
            crate::chat_quote::quote_insertion("", 0, "Quoted text\n\n~~~\nlet x = 1;\n~~~").unwrap();
        assert_eq!(reply, text.len());
        let line = Some(text[..reply].matches('\n').count());
        assert!(ranges(&text, line, Style::Quote).contains(&(2..13)));
        assert_eq!(
            ranges(&text, line, Style::CodeMarkerDim)
                .into_iter().map(|range| &text[range]).collect::<Vec<_>>(),
            ["~~~", "~~~"]
        );
        let blocks = parse_markdown(&text, Default::default()).unwrap();
        assert!(matches!(blocks.as_slice(), [BlockNode::Blockquote { children, .. }]
            if matches!(children.as_slice(), [BlockNode::Paragraph(_), BlockNode::CodeBlock(code)]
                if code.code() == "let x = 1;")));
    }

    #[test]
    fn links_and_math_use_exact_source_ranges() {
        let text = "[x](https://a.test) https://b.test\n\n$x$ and $$y$$\n\n$$\nz+1\n$$";
        assert_eq!(ranges(text, None, Style::Link), [1..2, 20..34]);
        assert_eq!(ranges(text, None, Style::Math), [37..38, 46..47, 54..57]);
        assert_eq!(
            ranges(text, None, Style::MarkerDim),
            [0..1, 2..19, 36..37, 38..39, 44..46, 47..49, 51..53, 58..60,]
        );
        assert!(spans("[[card]]", None).is_empty());
    }

    #[test]
    fn caret_line_keeps_non_code_markers_at_normal_brightness() {
        let text = "**one**\n*two*\n`three`";
        assert_eq!(ranges(text, Some(1), Style::MarkerDim), [0..2, 5..7]);
        assert_eq!(
            ranges(text, Some(2), Style::CodeMarkerDim),
            [14..15, 20..21]
        );
        assert_eq!(ranges(text, Some(1), Style::Italic), [9..12]);

        let link = "[a](\nhttps://a.test\n)";
        assert_eq!(
            ranges(link, Some(1), Style::MarkerDim),
            [0..1, 2..5, 20..21]
        );
    }

    #[test]
    fn partial_markup_stays_plain() {
        for text in ["**unclosed", "`", "[link](page", "$$\nmath", "- ```\n1."] {
            assert!(spans(text, None).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn inline_code_spans_include_both_delimiters_and_keep_the_multiline_body() {
        let text = "pre ``あ\n` b`` post";
        let start = text.find("``").unwrap();
        let end = text.rfind("``").unwrap();
        for caret_line in [None, Some(0), Some(1)] {
            assert_eq!(
                ranges(text, caret_line, Style::CodeMarkerDim),
                [start..start + 2, end..end + 2]
            );
            assert_eq!(
                ranges(text, caret_line, Style::InlineCode),
                [start + 2..end]
            );
            assert_eq!(
                ranges(text, caret_line, Style::InlineCodeBackground),
                [start..end + 2]
            );
        }
    }

    #[test]
    fn code_delimiter_opacity_depends_on_theme_and_byte_position() {
        let text = "前 `あ` and `b`";
        let markers = spans(text, None)
            .into_iter()
            .filter(|span| span.style == Style::CodeMarkerDim)
            .collect::<Vec<_>>();
        assert_eq!(markers.len(), 4);
        for (caret, first, second) in [
            (None, false, false),
            (Some(3), false, false),
            (Some(4), true, false),
            (Some(5), true, false),
            (Some(9), true, false),
            (Some(10), false, false),
            (Some(text.len()), false, true),
        ] {
            for (marker, active) in markers.iter().zip([first, first, second, second]) {
                assert_eq!(caret_in_code_span(caret, marker.code_span.as_ref()), active);
                for (dark, quiet) in [(true, 0.14), (false, 0.20)] {
                    assert_eq!(
                        code_marker_opacity(marker.style, dark, active),
                        if active { 0.45 } else { quiet }
                    );
                    assert_eq!(code_marker_opacity(Style::CodeInfo, dark, active), 0.30);
                }
            }
        }

        let fenced = "x\n\n```rs\na\n```\n";
        let markers = spans(fenced, None)
            .into_iter()
            .filter(|span| span.style == Style::CodeMarkerDim)
            .collect::<Vec<_>>();
        assert_eq!(markers.len(), 2);
        let body = fenced.find('a').unwrap();
        for marker in markers {
            assert!(caret_in_code_span(Some(body), marker.code_span.as_ref()));
            assert!(!caret_in_code_span(Some(0), marker.code_span.as_ref()));
        }
    }

    #[test]
    fn open_fences_style_the_info_line_and_body_while_typing() {
        let text = "```rust example\nあ```";
        let body = text.find('\n').unwrap() + 1;
        assert_eq!(ranges(text, Some(0), Style::CodeMarkerDim), [0..3]);
        assert_eq!(ranges(text, Some(0), Style::CodeInfo), [3..body - 1]);
        assert_eq!(ranges(text, Some(1), Style::CodeBlock), [body..text.len()]);
        assert_eq!(
            ranges(text, Some(1), Style::CodeBlockBackground),
            [0..text.len()]
        );
        assert_eq!(ranges("```rust", Some(0), Style::CodeMarkerDim), [0..3]);
        assert_eq!(ranges("```rust", Some(0), Style::CodeInfo), [3..7]);
    }

    #[test]
    fn crlf_preserves_byte_offsets_and_logical_lines() {
        let text = "**a**\r\n_b_\r\n\r\n```\r\nc\r\n```";
        assert_eq!(ranges(text, Some(0), Style::MarkerDim), [7..8, 9..10]);
        assert_eq!(
            ranges(text, Some(3), Style::CodeMarkerDim),
            [14..17, 22..25]
        );
        assert_eq!(ranges(text, None, Style::Italic), [8..9]);
        assert_eq!(ranges(text, None, Style::CodeBlock), [19..20]);
    }

    #[test]
    fn japanese_and_emoji_preserve_byte_offsets() {
        let text = "日本語🙂 **太字** *絵🙂* `code`";
        assert_eq!(ranges(text, None, Style::Bold), [16..22]);
        assert_eq!(ranges(text, None, Style::Italic), [26..33]);
        assert_eq!(ranges(text, None, Style::InlineCode), [36..40]);
        assert_eq!(
            ranges(text, None, Style::MarkerDim),
            [14..16, 22..24, 25..26, 33..34]
        );
        assert_eq!(ranges(text, None, Style::CodeMarkerDim), [35..36, 40..41]);
        assert!(spans(text, None)
            .iter()
            .all(|span| text.get(span.range.clone()).is_some()));
    }

    #[test]
    fn recovered_cjk_emphasis_decorates_content_and_original_delimiters() {
        let text = "🙂前**「注」**です";
        assert_eq!(ranges(text, None, Style::Bold), [9..18]);
        assert_eq!(ranges(text, None, Style::MarkerDim), [7..9, 18..20]);
        assert!(ranges(text, Some(0), Style::MarkerDim).is_empty());
        assert_eq!(ranges(text, Some(0), Style::Bold), [9..18]);

        let text = "前**`コード`**です";
        assert_eq!(ranges(text, None, Style::Bold), [5..16]);
        assert_eq!(ranges(text, None, Style::InlineCode), [6..15]);
        assert_eq!(ranges(text, None, Style::MarkerDim), [3..5, 16..18]);
        assert_eq!(ranges(text, None, Style::CodeMarkerDim), [5..6, 15..16]);
        assert!(spans(r"\*\*「注」\*\*です", None).is_empty());
    }

    #[test]
    fn empty_and_long_text_bypass_parsing() {
        assert!(spans("", None).is_empty());
        let limit = format!("**b** {}", "日".repeat(MAX_CHARS - 6));
        assert_eq!(ranges(&limit, None, Style::Bold), [2..3]);
        assert!(spans(&(limit + "🙂"), None).is_empty());
    }
}

#[cfg(test)]
mod command_palette_tests {
    use super::*;

    #[test]
    fn filters_only_the_first_word_on_a_single_line() {
        let names = |text| command_candidates(text, AgentBackend::Claude).into_iter()
            .map(|command| command.name).collect::<Vec<_>>();
        assert_eq!(names("/c"), ["compact", "clear"]);
        assert_eq!(names("/pace"), ["pace"]);
        for text in [" /c", "//c", "/c ", "/c\ntext", "/c\rtext", "/c\ttext", "hello /c", "/unknown"] {
            assert!(names(text).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn navigates_completes_and_sends_without_handling_preedit() {
        let backend = AgentBackend::Codex;
        let mut palette = CommandPalette::default();
        assert_eq!(palette.key("/c", backend, "up", false), Some(CommandKey::Move(1)));
        assert_eq!(palette.key("/c", backend, "enter", false), Some(CommandKey::Complete("compact")));
        palette.selected = 1;
        assert_eq!(palette.key("/c", backend, "down", false), Some(CommandKey::Move(0)));
        assert_eq!(palette.key("/c", backend, "tab", false), Some(CommandKey::Complete("clear")));
        assert_eq!(palette.key("/clear", backend, "enter", false), Some(CommandKey::Send));
        assert_eq!(palette.key("/compact keep this", backend, "enter", false), Some(CommandKey::Send));
        assert_eq!(palette.key("/c", backend, "escape", false), Some(CommandKey::Dismiss));
        for key in ["up", "down", "tab", "enter", "escape"] {
            assert_eq!(palette.key("/clear", backend, key, true), None);
            assert_eq!(palette.key("/clear\npaste", backend, key, false), None);
        }
        palette.dismissed = true;
        assert_eq!(palette.key("/c", backend, "tab", false), None);
        assert_eq!(palette.key("/clear", backend, "enter", false), Some(CommandKey::Send));
    }
}
