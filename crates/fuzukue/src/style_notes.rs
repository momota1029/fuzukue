use std::{collections::BTreeSet, ops::Range};

use fuzukue_proto::{CardStyleDiagnostic, CardView};
use fuzukue_view::{
    style_notes::{self, jump_target, rule_id},
    TexMarkdown,
};
use gpui_kit::{
    base::{
        input::{
            GutterDecoration, GutterDecorationCollection, TextDecoration, TextDecorationCollection,
        },
        text::TextViewState,
    },
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::EditorState,
        scroll::ScrollableElement as _,
        tooltip::Tooltip,
        v_flex, ActiveTheme as _, Sizable as _,
    },
    div,
    prelude::*,
    px, App, Context, Entity, HighlightStyle, Hsla, SharedString, UnderlineStyle, Window,
};

use crate::{
    appearance::{editor_style_note_warning, style_note_warning},
    i18n,
    icons::Symbol,
    Workbench,
};

#[derive(Clone, Copy)]
pub(super) enum StyleNoteView<'a> {
    Reading(&'a TexMarkdown, &'a Entity<TextViewState>),
    Editing(&'a Entity<EditorState>, &'a EditorStyleNotes),
}

enum Jump {
    Read {
        view: Entity<TextViewState>,
        source: Vec<Range<usize>>,
    },
    Edit(Entity<EditorState>, usize),
}

impl Jump {
    fn go(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Read { view, source } => view.update(cx, |view, cx| {
                let rendered = view.rendered_text();
                if let Some(range) = source
                    .iter()
                    .find_map(|range| rendered.range_for_source(range.clone()))
                {
                    let _ = view.reveal_range(range, cx);
                }
            }),
            Self::Edit(input, offset) => input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.move_caret_to(*offset, false, cx);
            }),
        }
    }
}

impl Workbench {
    pub(super) fn card_style_diagnostics<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Iterator<Item = &'a CardStyleDiagnostic> {
        self.card_diags
            .iter()
            .filter(move |d| d.path == path)
            .filter_map(|d| d.style.as_ref())
    }

    pub(super) fn render_card_style_notes(
        &self,
        card: &CardView,
        view: StyleNoteView<'_>,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let mut rows = style_notes::rows(&card.body, self.card_style_diagnostics(&card.path));
        let editor_ranges = match view {
            StyleNoteView::Editing(_, marks) => {
                let ranges = marks.tracked_ranges(cx);
                rows.retain(|row| {
                    row.ranges.iter().any(|range| {
                        ranges
                            .iter()
                            .any(|(original, tracked)| original == range && !tracked.is_empty())
                    })
                });
                ranges
            }
            StyleNoteView::Reading(..) => Vec::new(),
        };
        if rows.is_empty() {
            return None;
        }
        let workspace = self.workspaces.active.as_deref().unwrap_or_default();
        let expanded = self
            .style_note_disclosure
            .is_expanded(workspace, &card.path);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.muted;
        let warning = style_note_warning(theme.warning, theme.background);
        let count = rows.iter().map(|row| row.count).sum::<usize>();
        let label = tr!("style-notes-summary", count = count);
        let summary = Button::new("card-style-notes")
            .ghost()
            .xsmall()
            .accessibility_label(label.clone())
            .child(
                h_flex()
                    .gap_1p5()
                    .text_color(muted)
                    .child(div().size(px(5.)).rounded_full().bg(warning))
                    .child(label)
                    .child(
                        if expanded {
                            Symbol::ChevronDown
                        } else {
                            Symbol::ChevronRight
                        }
                        .icon()
                        .size(px(12.)),
                    ),
            )
            .on_click(cx.listener({
                let workspace = workspace.to_owned();
                let path = card.path.clone();
                move |this, _, _, cx| {
                    this.style_note_disclosure
                        .toggle(workspace.clone(), path.clone());
                    cx.notify();
                }
            }));
        let mut section = v_flex().w_full().items_start().child(summary);
        if expanded {
            let editor_text = match view {
                StyleNoteView::Editing(input, _) => Some(input.read(cx).text().to_string()),
                StyleNoteView::Reading(..) => None,
            };
            let locale = i18n::current();
            let mut list = v_flex().w_full();
            for (index, row) in rows.into_iter().enumerate() {
                let target = row
                    .ranges
                    .first()
                    .cloned()
                    .and_then(|range| jump_target(&card.body, range));
                let (jump, line) = match &view {
                    StyleNoteView::Reading(display, text) => {
                        let jump = target.as_ref().and_then(|target| {
                            let source: Vec<_> = [target.range.clone(), target.line_range.clone()]
                                .into_iter()
                                .filter_map(|range| display.markdown_range(range))
                                .collect();
                            (!source.is_empty()).then(|| Jump::Read {
                                view: (*text).clone(),
                                source,
                            })
                        });
                        (jump, target.map(|target| target.line))
                    }
                    StyleNoteView::Editing(input, _) => {
                        let target = row.ranges.iter().find_map(|original| {
                            let (_, tracked) =
                                editor_ranges.iter().find(|(key, _)| key == original)?;
                            jump_target(editor_text.as_deref()?, tracked.clone())
                        });
                        (
                            target
                                .as_ref()
                                .map(|target| Jump::Edit((*input).clone(), target.range.start)),
                            target.map(|target| target.line),
                        )
                    }
                };
                let tooltip = row
                    .findings
                    .iter()
                    .map(|style| rule_id(style))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let messages = row
                    .findings
                    .iter()
                    .map(|style| {
                        locale.style_note_message(
                            &style.code,
                            style.textlint_rule.as_deref(),
                            &style.detail,
                            style.note.as_ref().and_then(|note| note.limit),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                let label = if row.excerpt.is_empty() {
                    messages
                } else {
                    let quote = tr!("style-note-excerpt", excerpt = row.excerpt.as_str());
                    format!("{quote} {messages}")
                };
                let item = h_flex()
                    .id(SharedString::from(format!("card-style-note-{index}")))
                    .w_full()
                    .gap_2()
                    .items_start()
                    .rounded_sm()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(div().flex_1().min_w_0().child(label))
                    .children(line.map(|line| {
                        div()
                            .flex_none()
                            .text_color(muted)
                            .child(tr!("style-note-line", line = line))
                    }))
                    .when_some(jump, |item, jump| {
                        item.cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| jump.go(window, cx))
                    });
                list = list.child(item);
            }
            section = section.child(
                div()
                    .id("card-style-notes-list")
                    .w_full()
                    .max_h(px(180.))
                    .overflow_y_scrollbar()
                    .child(list),
            );
        }
        Some(section)
    }
}

/// The gutter collection also provides the tracked byte ranges for jumps
/// after edits have inserted or removed text before a finding.
#[derive(Default)]
pub(super) struct EditorStyleNotes {
    originals: Vec<Range<usize>>,
    gutter: Option<GutterDecorationCollection>,
    underlines: Option<TextDecorationCollection>,
}

impl EditorStyleNotes {
    pub(super) fn install<'a>(
        &mut self,
        card: &CardView,
        file: &str,
        notes: impl IntoIterator<Item = &'a CardStyleDiagnostic>,
        input: &Entity<EditorState>,
        cx: &mut App,
    ) {
        let Some((_, file_body)) = fuzukue_cards::split_frontmatter(file) else {
            self.clear(cx);
            return;
        };
        let ranges: BTreeSet<_> = style_notes::rows(&card.body, notes)
            .into_iter()
            .flat_map(|row| row.ranges)
            .map(|range| (range.start, range.end))
            .collect();
        let ranges: Vec<_> = ranges
            .into_iter()
            .filter_map(|(start, end)| {
                let original = start..end;
                let buffer =
                    style_notes::editor_range(&card.body, file, file_body, original.clone())?;
                Some((original, buffer))
            })
            .collect();
        let (originals, ranges): (Vec<_>, Vec<_>) = ranges.into_iter().unzip();
        if self.originals == originals {
            return;
        }
        self.clear(cx);
        if ranges.is_empty() || input.read(cx).text() != file {
            return;
        }
        let color = editor_style_note_warning(cx.theme());
        let (gutter, underlines) = decorations(ranges, color);
        self.originals = originals;
        self.gutter = Some(input.update(cx, |input, cx| {
            input.create_gutter_decorations_collection(gutter, cx)
        }));
        if let Some(collection) = &self.underlines {
            collection.set(underlines, cx);
        } else {
            self.underlines = Some(input.update(cx, |input, cx| {
                input.create_decorations_collection(underlines, cx)
            }));
        }
    }

    pub(super) fn clear(&mut self, cx: &mut App) {
        if let Some(gutter) = self.gutter.take() {
            gutter.dispose(cx);
        }
        if let Some(underlines) = &self.underlines {
            underlines.clear(cx);
        }
        self.originals.clear();
    }

    pub(super) fn update_color(&self, cx: &mut App) {
        if let (Some(gutter), Some(underlines)) = (&self.gutter, &self.underlines) {
            let color = editor_style_note_warning(cx.theme());
            let (dots, waves) = decorations(gutter.get_ranges(cx), color);
            gutter.set(dots, cx);
            underlines.set(waves, cx);
        }
    }

    fn tracked_ranges(&self, cx: &App) -> Vec<(Range<usize>, Range<usize>)> {
        self.gutter.as_ref().map_or_else(Vec::new, |gutter| {
            self.originals
                .iter()
                .cloned()
                .zip(gutter.get_ranges(cx))
                .collect()
        })
    }
}

fn decorations(
    ranges: Vec<Range<usize>>,
    color: Hsla,
) -> (Vec<GutterDecoration>, Vec<TextDecoration>) {
    let underline = HighlightStyle {
        underline: Some(UnderlineStyle {
            color: Some(color),
            thickness: px(1.),
            wavy: true,
        }),
        ..Default::default()
    };
    let gutter = ranges
        .iter()
        .cloned()
        .map(|range| GutterDecoration::new(range).with_color(color).with_dot())
        .collect();
    let underlines = ranges
        .into_iter()
        .map(|range| TextDecoration::new(range, underline))
        .collect();
    (gutter, underlines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::CardStyleNote;

    #[test]
    fn editor_style_notes_follow_buffer_edits_and_move_the_caret() {
        gpui_kit::platform::headless().run(|cx| {
            gpui_kit::init(cx);
            let window = cx
                .open_window(
                    gpui_kit::WindowOptions {
                        show: false,
                        ..Default::default()
                    },
                    |_, cx| cx.new(|_| EmptyWindow),
                )
                .unwrap();
            window
                .update(cx, |_, window, cx| {
                    let card: CardView = serde_json::from_value(serde_json::json!({
                        "id": "sample", "kind": "lemma", "title": "", "status": "",
                        "path": "cards/sample.md", "links": [], "backlinks": [], "files": [],
                        "body": "これは写像？\n", "external": false
                    }))
                    .unwrap();
                    let prefix = "---\r\nid: sample\r\n---\r\n";
                    let file = format!("{prefix}{}", card.body);
                    let start = card.body.find('？').unwrap();
                    let range = start..start + '？'.len_utf8();
                    let note = CardStyleDiagnostic {
                        code: "style/no-exclamation-question-mark".into(),
                        textlint_rule: None,
                        detail: String::new(),
                        note: Some(CardStyleNote {
                            range: range.clone(),
                            excerpt: 0..range.end,
                            limit: None,
                        }),
                    };
                    let input =
                        cx.new(|cx| EditorState::new(window, cx).default_value(file.clone()));
                    let mut marks = EditorStyleNotes::default();
                    marks.install(&card, &file, [&note], &input, cx);
                    let new_line = "前置き。\n";
                    input.update(cx, |state, cx| {
                        state.replace_range_atomically(
                            prefix.len()..prefix.len(),
                            new_line,
                            window,
                            cx,
                        );
                    });
                    let shifted = prefix.len() + new_line.len();
                    let marked = start + shifted..range.end + shifted;
                    assert_eq!(marks.tracked_ranges(cx), [(range, marked.clone())]);
                    assert_eq!(
                        marks.underlines.as_ref().unwrap().get_ranges(cx),
                        [marked.clone()]
                    );
                    Jump::Edit(input.clone(), marked.start).go(window, cx);
                    assert_eq!(input.read(cx).cursor(), marked.start);
                })
                .unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }

    struct EmptyWindow;

    impl Render for EmptyWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
}
