use std::{collections::HashMap, sync::Arc};

use fuzukue_proto::{manuscript_comment_path, DocProseText};
use fuzukue_view::{
    comment_anchor::{resolve_passage_comment, Passage},
    TexMarkdown,
};
use gpui_kit::{
    base::{
        text::{RangeHighlight, TextViewState},
        TextSelection,
    },
    component::{text::TextView, ActiveTheme as _},
    div,
    prelude::*,
    px, App, Context, Div, Entity, MouseButton,
};

use crate::{pane_resize::PANE_INSET, ReadRow, RightPane, Workbench};

pub(super) enum ProseBody {
    Loading,
    Ready {
        tex: String,
        display: Arc<TexMarkdown>,
        view: Entity<TextViewState>,
    },
    Unavailable,
}

impl Workbench {
    pub(super) fn reconcile_prose(&mut self, rows: &[ReadRow], presentation_changed: bool) {
        let current: std::collections::HashSet<_> = rows
            .iter()
            .filter_map(|row| match row {
                ReadRow::Prose(text) => Some(text),
                _ => None,
            })
            .collect();
        self.read_prose.retain(|text, body| {
            current.contains(text)
                && !(presentation_changed && matches!(body, ProseBody::Ready { .. }))
        });
        let gone = |text: &DocProseText| !current.contains(text) || presentation_changed;
        if self.prose_selection.as_ref().is_some_and(gone) {
            self.prose_selection = None;
        }
        if self.prose_popup.as_ref().is_some_and(gone) {
            self.prose_popup = None;
            self.comment_popup = None;
        }
    }

    pub(super) fn load_visible_prose(&mut self, cx: &mut Context<Self>) {
        if self.right != RightPane::Read {
            return;
        }
        const IN_FLIGHT: usize = 4;
        let loading = self
            .read_prose
            .values()
            .filter(|body| matches!(body, ProseBody::Loading))
            .count();
        // Also load the files that have comments, so that each comment can be
        // placed by its text rather than by an offset that an edit may move.
        let commented: Vec<_> = self
            .comments
            .iter()
            .filter_map(|comment| manuscript_comment_path(&comment.card))
            .collect();
        let anchored = self.read_rows.iter().filter(|row| {
            matches!(row, ReadRow::Prose(DocProseText::Source(spans))
                if spans.iter().any(|span| commented.contains(&span.path.as_str())))
        });
        let visible: Vec<_> = self
            .read_list
            .visible_item_range()
            .filter_map(|index| self.read_rows.get(index))
            .chain(anchored)
            .filter_map(|row| match row {
                ReadRow::Prose(text) if !self.read_prose.contains_key(text) => Some(text.clone()),
                _ => None,
            })
            .take(IN_FLIGHT.saturating_sub(loading))
            .collect();
        for text in visible {
            if self.read_prose.contains_key(&text) {
                continue;
            }
            match &text {
                DocProseText::Tex(tex) => self.install_prose(text.clone(), tex.clone(), cx),
                DocProseText::Source(spans) => {
                    self.read_prose.insert(text.clone(), ProseBody::Loading);
                    let spans = spans.clone();
                    let deck = self.deck.clone();
                    let http = self.http.clone();
                    cx.spawn(async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                let mut tex = String::new();
                                for span in &spans {
                                    if span.newline_before {
                                        tex.push('\n');
                                    }
                                    tex.push_str(&http.read_prose(&deck, span)?);
                                }
                                Ok::<_, String>(tex)
                            })
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            if !matches!(this.read_prose.get(&text), Some(ProseBody::Loading)) {
                                return;
                            }
                            match result {
                                Ok(tex) => this.install_prose(text, tex, cx),
                                Err(_) => {
                                    this.remeasure_prose(&text);
                                    this.read_prose.insert(text, ProseBody::Unavailable);
                                    cx.notify();
                                }
                            }
                        });
                    })
                    .detach();
                }
            }
        }
    }

    fn install_prose(&mut self, text: DocProseText, tex: String, cx: &mut Context<Self>) {
        let display = Arc::new(self.display_card_tex(&tex));
        let view = cx.new(|cx| TextViewState::markdown(&display.markdown, cx));
        let observed = text.clone();
        let measured = std::cell::Cell::new(false);
        cx.observe(&view, move |this, view, cx| {
            let Some(ProseBody::Ready {
                display,
                view: current,
                ..
            }) = this.read_prose.get(&observed)
            else {
                return;
            };
            if current == &view
                && view.read(cx).rendered_text().source() == display.markdown
                && !measured.replace(true)
            {
                this.highlight_passage_quotes(cx);
                this.remeasure_prose(&observed);
                cx.notify();
            }
        })
        .detach();
        self.remeasure_prose(&text);
        self.read_prose
            .insert(text, ProseBody::Ready { tex, display, view });
        // A newly loaded text may claim a comment shown on another passage.
        if self
            .comments
            .iter()
            .any(|comment| manuscript_comment_path(&comment.card).is_some())
        {
            self.remeasure_all_prose();
        }
        cx.notify();
    }

    fn remeasure_prose(&self, text: &DocProseText) {
        for (index, row) in self.read_rows.iter().enumerate() {
            if matches!(row, ReadRow::Prose(prose) if prose == text) {
                self.read_list.remeasure_items(index..index + 1);
            }
        }
    }

    fn remeasure_all_prose(&self) {
        for (index, row) in self.read_rows.iter().enumerate() {
            if matches!(row, ReadRow::Prose(_)) {
                self.read_list.remeasure_items(index..index + 1);
            }
        }
    }

    /// Whether a passage shows text. Blank passages take no comments.
    pub(super) fn passage_shown(&self, text: &DocProseText) -> bool {
        matches!(self.read_prose.get(text),
            Some(ProseBody::Ready { display, .. }) if !display.markdown.trim().is_empty())
    }

    /// Manuscript comments by the reader row that shows them.
    pub(super) fn passage_comments(&self) -> HashMap<usize, Vec<usize>> {
        let mut placed: HashMap<usize, Vec<usize>> = HashMap::new();
        if !self
            .comments
            .iter()
            .any(|comment| manuscript_comment_path(&comment.card).is_some())
        {
            return placed;
        }
        let (rows, passages): (Vec<_>, Vec<_>) = self
            .read_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| {
                let ReadRow::Prose(text) = item else {
                    return None;
                };
                let tex = match self.read_prose.get(text) {
                    Some(ProseBody::Ready { tex, display, .. }) => {
                        if display.markdown.trim().is_empty() {
                            return None;
                        }
                        Some(tex.as_str())
                    }
                    _ => None,
                };
                Some((row, Passage { text, tex }))
            })
            .unzip();
        for (index, comment) in self.comments.iter().enumerate() {
            if let Some(passage) = resolve_passage_comment(comment, &passages) {
                placed.entry(rows[passage]).or_default().push(index);
            }
        }
        placed
    }

    pub(super) fn highlight_passage_quotes(&self, cx: &mut Context<Self>) {
        let background = cx.theme().link.alpha(0.18);
        let placed = self.passage_comments();
        for (row, item) in self.read_rows.iter().enumerate() {
            let ReadRow::Prose(text) = item else {
                continue;
            };
            let Some(ProseBody::Ready { tex, display, view }) = self.read_prose.get(text) else {
                continue;
            };
            let rendered = view.read(cx).rendered_text();
            let highlights: Vec<_> = placed
                .get(&row)
                .into_iter()
                .flatten()
                .filter_map(|&index| {
                    let comment = &self.comments[index];
                    let quote = comment.quote.as_deref()?;
                    comment
                        .source
                        .as_ref()
                        .filter(|_| display.markdown == rendered.source())
                        .and_then(|source| crate::paper::unique_quote(tex, &source.text))
                        .and_then(|range| display.markdown_range(range))
                        .and_then(|range| rendered.range_for_source(range))
                        .or_else(|| crate::paper::unique_quote(rendered.as_str(), quote))
                        .map(|range| RangeHighlight::new(range, background))
                })
                .collect();
            view.update(cx, |state, cx| {
                let _ = state.set_range_highlights(highlights, cx);
            });
        }
    }

    pub(super) fn prose_ready(&self, text: &DocProseText, cx: &App) -> bool {
        match self.read_prose.get(text) {
            Some(ProseBody::Ready { display, view, .. }) => {
                view.read(cx).rendered_text().source() == display.markdown
            }
            Some(ProseBody::Unavailable) => true,
            _ => false,
        }
    }

    pub(super) fn render_prose(&self, text: &DocProseText, entity: &Entity<Self>, cx: &App) -> Div {
        let row = div().px(PANE_INSET).w_full().min_w_0();
        match self.read_prose.get(text) {
            Some(ProseBody::Ready { display, .. }) if display.markdown.trim().is_empty() => row,
            Some(ProseBody::Ready { view, .. }) => {
                let entity = entity.clone();
                let selected = text.clone();
                let popup = self
                    .comment_popup
                    .as_ref()
                    .filter(|_| self.prose_popup.as_ref() == Some(text))
                    .map(|(target, offset)| {
                        Self::comment_popup_pill(
                            "quote-passage".into(),
                            target.clone(),
                            *offset,
                            &entity,
                            cx,
                        )
                    });
                row.py_2().child(
                    self.appearance
                        .style_paper(div().w_full().min_w_0().relative())
                        .capture_any_mouse_down(move |event, window, cx| {
                            if event.button == MouseButton::Left
                                && TextSelection::is_selectable_at(event.position, window, cx)
                            {
                                entity.update(cx, |this, _| {
                                    this.comment_selection = None;
                                    this.prose_selection = Some(selected.clone());
                                });
                            }
                        })
                        .child(self.card_body_text(TextView::new(view), true, cx))
                        .children(popup),
                )
            }
            Some(ProseBody::Unavailable) => row
                .py_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tr!("workbench-deck-changed")),
            _ => row.min_h(px(56.)),
        }
    }
}
