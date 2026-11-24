use std::rc::Rc;

use fuzukue_i18n::{FluentArgs, Locale};

use gpui_kit::{
    base::{
        input::{
            language_config::{LanguageConfig, LineContinuation},
            AutoClosingPair, Rope, RopeExt as _, TabSize,
        },
        Scrollbar, ScrollbarMode,
    },
    component::{
        h_flex,
        input::{Editor, EditorState},
        scroll::ScrollableElement as _,
        v_flex, ActiveTheme as _, Sizable as _,
    },
    div,
    prelude::*,
    px, relative, App, Context, Entity, Focusable as _, KeyDownEvent, Window,
};

use crate::appearance::Appearance;

pub(crate) fn editor_state(
    language: &str,
    text: &str,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) -> EditorState {
    crate::source_outline::register_grammars();
    let mut state = EditorState::new(window, cx)
        .language(language)
        .editing_rules(editing_rules(language))
        .tab_size(tab_size(language))
        .soft_wrap(true)
        .wrap_indicator("↪")
        .searchable(true)
        .replaceable(true)
        .smart_indent(language != "markdown")
        .default_value(text.to_owned());
    state.set_scrollbar_renderer(Some(Rc::new(|context, _, _| {
        Scrollbar::new(&context)
            .axis(context.axis())
            .mode(ScrollbarMode::Hover)
            .into_any_element()
    })));
    if language == "yulang" {
        state.set_highlighter_factory(
            Rc::new(|language| {
                (language == "yulang")
                    .then(|| Box::new(super::yulang::Highlighter::default()) as Box<_>)
            }),
            cx,
        );
    }
    state
}

pub(super) fn editing_rules(language: &str) -> LanguageConfig {
    let rules = LanguageConfig::default()
        .auto_close_before("")
        .smart_home(true)
        .auto_closing_pairs(
            [("(", ")"), ("[", "]"), ("{", "}"), ("“", "”"), ("`", "`")]
                .map(|(open, close)| AutoClosingPair::new(open, close)),
        );
    if language == "markdown" {
        rules.continue_line(markdown_continuation)
    } else {
        rules
    }
}

pub(super) fn tab_size(language: &str) -> TabSize {
    TabSize {
        tab_size: match language {
            "markdown" | "tex" | "toml" | "yaml" | "lean" | "json" | "html" | "css"
            | "javascript" | "typescript" | "tsx" | "ruby" => 2,
            _ => 4,
        },
        hard_tabs: matches!(language, "make" | "go"),
    }
}

fn indentation(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn markdown_continuation(line: &str) -> Option<LineContinuation> {
    let indent = indentation(line);
    let content = &line[indent.len()..];
    let (marker, rest) = if let Some(rest) = content.strip_prefix("- ") {
        ("- ".to_owned(), rest)
    } else {
        let digits = content.bytes().take_while(u8::is_ascii_digit).count();
        let rest = content.get(digits..)?.strip_prefix(". ")?;
        let number = content[..digits].parse::<u64>().ok()?.checked_add(1)?;
        (format!("{number}. "), rest)
    };
    if rest.trim().is_empty() {
        Some(LineContinuation::RemoveSuffix(line.len() - indent.len()))
    } else {
        Some(LineContinuation::Prefix(format!("{indent}{marker}")))
    }
}

/// The shared editor frame. The outer border takes its color from the theme;
/// the input's stock focus ring is disabled by the Editor component.
pub(crate) fn surface(
    state: &Entity<EditorState>,
    appearance: &Appearance,
    language: &'static str,
    label: &'static str,
    readonly: bool,
) -> impl IntoElement {
    EditorSurface {
        state: state.clone(),
        font: appearance.code_font.clone(),
        size: px(appearance.settings.reading_font_size),
        language,
        label,
        readonly,
    }
}

#[derive(IntoElement)]
struct EditorSurface {
    state: Entity<EditorState>,
    font: gpui_kit::Font,
    size: gpui_kit::Pixels,
    language: &'static str,
    label: &'static str,
    readonly: bool,
}

impl RenderOnce for EditorSurface {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.state.read(cx).focus_handle(cx).is_focused(window);
        let input = self.state.clone();
        let language = self.language;
        div()
            .id("editor-frame")
            .size_full()
            .min_h_0()
            .min_w_0()
            .border_1()
            .border_color(if focused { theme.ring } else { theme.border })
            .capture_key_down(move |event, window, cx| {
                handle_edit_key(&input, language, event, window, cx);
            })
            .child(
                Editor::new(&self.state)
                    .aria_label(self.label)
                    .h(relative(1.))
                    .bordered(false)
                    .rounded_none()
                    .readonly(self.readonly)
                    .font(self.font)
                    .text_size(self.size)
                    .line_height(self.size * 1.5)
                    .small(),
            )
    }
}

pub(crate) fn is_save_key(event: &KeyDownEvent) -> bool {
    let mods = event.keystroke.modifiers;
    event.keystroke.key.eq_ignore_ascii_case("s")
        && (mods.control || mods.platform)
        && mods.number_of_modifiers() == 1
}

fn handle_edit_key(
    input: &Entity<EditorState>,
    language: &str,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let mods = event.keystroke.modifiers;
    let key = event.keystroke.key.as_str();
    if !mods.control || mods.number_of_modifiers() != 1 || !matches!(key, "/" | "d") {
        return;
    }
    if !input.read(cx).focus_handle(cx).is_focused(window)
        || !input.read(cx).is_editable()
        || super::super::composing(input, window, cx)
    {
        return;
    }
    input.update(cx, |state, cx| {
        if key == "d" {
            duplicate_line(state, window, cx);
        } else if let Some(syntax) = comment_syntax(language) {
            toggle_comment(state, syntax, window, cx);
        }
    });
    cx.stop_propagation();
}

fn line_content_end(text: &Rope, row: usize) -> usize {
    let end = text.line_end_offset(row);
    end - usize::from(
        end > 0 && text.get_byte(end - 1) == Some(b'\r') && text.get_byte(end) == Some(b'\n'),
    )
}

fn duplicate_line(state: &mut EditorState, window: &mut Window, cx: &mut Context<EditorState>) {
    let text = state.text();
    let offset = state.cursor();
    let row = text.offset_to_point(offset).row;
    let start = text.line_start_offset(row);
    let end = line_content_end(text, row);
    let next = text.line_start_offset(row + 1);
    let newline = newline_before(text, if end < next { next } else { start });
    let line = text.slice(start..end);
    let duplicate = format!("{newline}{line}");
    state.replace_range_atomically(end..end, &duplicate, window, cx);
    state.move_caret_to(end + newline.len() + offset.min(end) - start, false, cx);
}

#[derive(Clone, Copy)]
struct CommentSyntax {
    open: &'static str,
    close: &'static str,
}

fn comment_syntax(language: &str) -> Option<CommentSyntax> {
    let (open, close) = match language {
        "rust" | "c" | "cpp" | "go" | "java" | "javascript" | "typescript" | "tsx" | "zig"
        | "kotlin" | "swift" | "scala" | "yulang" => ("//", ""),
        "python" | "toml" | "yaml" | "bash" | "make" | "cmake" | "ruby" | "r" | "julia" => {
            ("#", "")
        }
        "lean" | "lua" | "sql" => ("--", ""),
        "tex" => ("%", ""),
        "markdown" | "html" => ("<!--", "-->"),
        "css" => ("/*", "*/"),
        _ => return None,
    };
    Some(CommentSyntax { open, close })
}

impl CommentSyntax {
    fn unwrap<'a>(self, line: &'a str) -> Option<&'a str> {
        let body = line.strip_prefix(self.open)?;
        let body = body.strip_prefix(' ').unwrap_or(body);
        if self.close.is_empty() {
            Some(body)
        } else {
            let body = body.trim_end().strip_suffix(self.close)?;
            Some(body.strip_suffix(' ').unwrap_or(body))
        }
    }

    fn toggle(self, text: &str) -> String {
        let remove = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| self.unwrap(line.trim_start()).is_some());
        let remove = remove && !text.trim().is_empty();
        let mut result = String::new();
        for line in text
            .split_inclusive('\n')
            .chain(text.is_empty().then_some(""))
        {
            let (body, newline) = line.strip_suffix("\r\n").map_or_else(
                || {
                    line.strip_suffix('\n')
                        .map_or((line, ""), |body| (body, "\n"))
                },
                |body| (body, "\r\n"),
            );
            let indent = indentation(body);
            let rest = &body[indent.len()..];
            result.push_str(indent);
            if rest.is_empty() && !newline.is_empty() {
                // Leave interior blank lines untouched.
            } else if remove {
                result.push_str(self.unwrap(rest).unwrap_or(rest));
            } else {
                result.push_str(self.open);
                result.push(' ');
                result.push_str(rest);
                if !self.close.is_empty() {
                    if !rest.is_empty() {
                        result.push(' ');
                    }
                    result.push_str(self.close);
                }
            }
            result.push_str(newline);
        }
        result
    }
}

fn toggle_comment(
    state: &mut EditorState,
    syntax: CommentSyntax,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let text = state.text();
    let selection = state.selected_range();
    let start_row = text.offset_to_point(selection.start).row;
    let mut end_row = text.offset_to_point(selection.end).row;
    if !selection.is_empty() && selection.end == text.line_start_offset(end_row) {
        end_row = end_row.saturating_sub(1);
    }
    let range = text.line_start_offset(start_row)..line_content_end(text, end_row);
    let lines = text.slice(range.clone()).to_string();
    let changed = syntax.toggle(&lines);
    if changed == lines {
        return;
    }
    let reversed = state.is_selection_reversed();
    let after = if selection.is_empty() {
        let column = selection.start - range.start;
        let indent = indentation(&lines).len();
        let delta = if changed.len() > lines.len() {
            (syntax.open.len() + 1) as isize
        } else {
            let has_space = lines[indent..][syntax.open.len()..].starts_with(' ');
            -((syntax.open.len() + usize::from(has_space)) as isize)
        };
        let column = if column < indent {
            column
        } else {
            column
                .saturating_add_signed(delta)
                .max(indent)
                .min(changed.len())
        };
        let cursor = range.start + changed.floor_char_boundary(column);
        cursor..cursor
    } else {
        range.start..range.start + changed.len()
    };
    state.replace_range_atomically(range, &changed, window, cx);
    state.set_selected_range_with_direction(after, reversed, cx);
}

pub(crate) fn is_dirty(base: &str, text: &Rope) -> bool {
    text != base
}

fn newline_before(text: &Rope, end: usize) -> &'static str {
    if end >= 2 && text.get_byte(end - 1) == Some(b'\n') && text.get_byte(end - 2) == Some(b'\r') {
        "\r\n"
    } else {
        "\n"
    }
}

fn line_ending(text: &Rope) -> &'static str {
    if newline_before(text, text.line_start_offset(1)) == "\r\n" {
        "CRLF"
    } else {
        "LF"
    }
}

pub(crate) fn status(
    input: &Entity<EditorState>,
    language: &str,
    dirty: bool,
    pending: bool,
    cx: &App,
) -> impl IntoElement {
    let state = input.read(cx);
    let cursor = state.cursor_position();
    let selected = state.selected_text().len_chars();
    let locale = crate::i18n::current();
    let (position, selection, saved) = status_labels(
        locale,
        cursor.line + 1,
        cursor.character + 1,
        selected,
        dirty,
        pending,
    );
    let language = match language {
        "markdown" => "Markdown",
        "yaml" => "YAML",
        "toml" => "TOML",
        "tex" => "LaTeX",
        "rust" => "Rust",
        "lean" => "Lean",
        "yulang" => "Yulang",
        "json" => "JSON",
        "javascript" => "JavaScript",
        "typescript" => "TypeScript",
        "tsx" => "TSX",
        "html" => "HTML",
        "css" => "CSS",
        "python" => "Python",
        "bash" => "Shell",
        "c" => "C",
        "cpp" => "C++",
        "go" => "Go",
        "java" => "Java",
        "lua" => "Lua",
        "zig" => "Zig",
        "kotlin" => "Kotlin",
        "swift" => "Swift",
        "scala" => "Scala",
        "ruby" => "Ruby",
        "r" => "R",
        "julia" => "Julia",
        "make" => "Make",
        "cmake" => "CMake",
        "sql" => "SQL",
        _ => tr!("editor-plain-text"),
    };
    let theme = cx.theme();
    let fields = [
        Some(position.as_str()),
        selection.as_deref(),
        Some(language),
        Some(line_ending(state.text())),
        Some("UTF-8"),
        Some(saved),
    ];

    h_flex()
        .id("editor-status")
        .flex_none()
        .px_2()
        .h(px(24.))
        .gap_1p5()
        .overflow_hidden()
        .border_t_1()
        .border_color(theme.border)
        .text_color(theme.muted_foreground)
        .text_xs()
        .whitespace_nowrap()
        .children(
            fields
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, field)| {
                    let label = if index == 0 {
                        field.to_owned()
                    } else {
                        format!("·  {field}")
                    };
                    div().flex_none().child(label)
                }),
        )
}

fn status_labels(
    locale: Locale,
    line: u32,
    column: u32,
    selected: usize,
    dirty: bool,
    pending: bool,
) -> (String, Option<String>, &'static str) {
    let mut args = FluentArgs::new();
    args.set("line", line);
    args.set("column", column);
    let position = locale.format("editor-position", &args);
    let selection = (selected > 0).then(|| {
        let mut args = FluentArgs::new();
        args.set("count", selected);
        locale.format("editor-selected-characters", &args)
    });
    let saved = locale.text(if pending {
        "editor-saving"
    } else if dirty {
        "editor-unsaved"
    } else {
        "editor-saved"
    });
    (position, selection, saved)
}

pub(crate) fn dirty_dot(cx: &App) -> impl IntoElement {
    div()
        .id("editor-dirty")
        .aria_label(tr!("editor-unsaved"))
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(cx.theme().link)
}

pub(crate) fn conflict_diff(latest: &str, draft: &str) -> String {
    let mut diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_millis(100))
        .diff_lines(latest, draft)
        .unified_diff()
        .header(tr!("editor-on-disk"), tr!("editor-edited"))
        .to_string();
    if diff.len() > 32_000 {
        diff.truncate(diff.floor_char_boundary(32_000));
        diff.push_str(tr!("editor-n-rest-of-diff-omitted"));
    }
    diff
}

pub(crate) fn conflict_banner(
    actions: impl IntoIterator<Item = impl IntoElement>,
    diff: Option<String>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .id("editor-conflict")
        .flex_none()
        .gap_1()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_xs()
        .child(
            div()
                .text_color(theme.warning)
                .child(tr!("editor-the-file-on-disk-changed")),
        )
        .child(h_flex().gap_1().flex_wrap().children(actions))
        .children(diff.map(|diff| diff_view("source-conflict-diff", diff, cx)))
}

pub(crate) fn diff_view(id: &'static str, diff: String, cx: &App) -> impl IntoElement {
    div()
        .h_auto()
        .max_h(px(160.))
        .overflow_y_scrollbar()
        .id(id)
        .font(crate::appearance::code_font(cx))
        .children(diff.lines().map(|line| div().child(line.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};

    #[test]
    fn markdown_continues_indented_and_ordered_lists_and_terminates_empty_items() {
        use LineContinuation::{Prefix, RemoveSuffix};

        assert_eq!(indentation("\t  example"), "\t  ");
        assert_eq!(
            markdown_continuation("  - item"),
            Some(Prefix("  - ".into()))
        );
        assert_eq!(
            markdown_continuation("\t19. 項目"),
            Some(Prefix("\t20. ".into()))
        );
        assert_eq!(markdown_continuation("  - "), Some(RemoveSuffix(2)));
        assert_eq!(markdown_continuation("  10.  "), Some(RemoveSuffix(5)));
        assert_eq!(markdown_continuation("  2.text"), None);
        assert_eq!(markdown_continuation("   prose"), None);
    }

    #[test]
    fn toggling_comment_uses_a_language_delimiter_and_keeps_line_endings() {
        for (language, commented) in [
            ("rust", "  // 一\r\n  // 二"),
            ("yulang", "  // 一\r\n  // 二"),
            ("yaml", "  # 一\r\n  # 二"),
            ("toml", "  # 一\r\n  # 二"),
            ("tex", "  % 一\r\n  % 二"),
            ("lean", "  -- 一\r\n  -- 二"),
            ("markdown", "  <!-- 一 -->\r\n  <!-- 二 -->"),
            ("css", "  /* 一 */\r\n  /* 二 */"),
        ] {
            let original = "  一\r\n  二";
            let syntax = comment_syntax(language).unwrap();
            assert_eq!(syntax.toggle(original), commented, "{language}");
            assert_eq!(syntax.toggle(commented), original, "{language}");
        }
        assert!(comment_syntax("text").is_none());
        assert_eq!(
            comment_syntax("rust").unwrap().toggle("hi\n\nbye"),
            "// hi\n\n// bye"
        );
    }

    #[test]
    fn dirty_state_and_status_use_the_actual_buffer_and_locale() {
        let original = "本文\r\n";
        assert!(!is_dirty(original, &Rope::from(original)));
        assert!(is_dirty(original, &Rope::from("本文\n")));
        assert!(is_dirty(original, &Rope::from("本文!\r\n")));
        assert_eq!(line_ending(&Rope::from(original)), "CRLF");
        assert_eq!(line_ending(&Rope::from("本文\n")), "LF");
        assert_eq!(
            status_labels(Locale::JAPANESE, 12, 5, 34, true, false),
            ("行 12, 列 5".into(), Some("選択 34 文字".into()), "未保存"),
        );
        let english = Locale::supported("en").unwrap();
        assert_eq!(
            status_labels(english, 1, 2, 0, false, false),
            ("Line 1, Col 2".into(), None, "Saved"),
        );
        assert_eq!(
            status_labels(english, 1, 2, 1, true, true).1.as_deref(),
            Some("1 character selected")
        );
        assert_eq!(status_labels(english, 1, 2, 1, true, true).2, "Saving…");
    }

    #[test]
    fn markdown_and_front_matter_get_styles_in_light_and_dark() {
        crate::source_outline::register_grammars();
        let text = "---\nid: dogfood\ntitle: \"Prose\"\n---\n\n# Heading\n- `inline` and [link](https://example.org)\n";
        let rope = Rope::from(text);
        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(highlighter.update(None, &rope, None));
        for theme in [
            HighlightTheme::default_light(),
            HighlightTheme::default_dark(),
        ] {
            let styles = highlighter.styles(&(0..text.len()), &*theme);
            let color_at = |label| {
                let offset = text.find(label).unwrap();
                styles
                    .iter()
                    .find(|(range, _)| range.contains(&offset))
                    .and_then(|(_, style)| style.color)
            };
            for token in ["id:", "\"Prose\"", "Heading", "- `", "`inline", "https://"] {
                assert!(
                    color_at(token).is_some(),
                    "{token} has no color: {styles:?}"
                );
            }
            assert_ne!(color_at("Heading"), color_at("`inline"));
            assert_ne!(color_at("id:"), color_at("\"Prose\""));
        }
    }
}
