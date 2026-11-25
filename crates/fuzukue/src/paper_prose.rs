use fuzukue_proto::DocProseText;
use gpui_kit::{
    base::text::TextViewState,
    component::{text::TextView, ActiveTheme as _},
    div,
    prelude::*,
    px, App, Context, Div, Entity, MouseButton,
};

use crate::{pane_resize::PANE_INSET, ReadRow, RightPane, Workbench};

pub(super) enum ProseBody {
    Loading,
    Ready {
        markdown: String,
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
        let visible: Vec<_> = self
            .read_list
            .visible_item_range()
            .filter_map(|index| match self.read_rows.get(index) {
                Some(ReadRow::Prose(text)) if !self.read_prose.contains_key(text) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .take(IN_FLIGHT.saturating_sub(loading))
            .collect();
        for text in visible {
            if self.read_prose.contains_key(&text) {
                continue;
            }
            match &text {
                DocProseText::Tex(tex) => self.install_prose(text.clone(), tex, cx),
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
                                Ok(tex) => this.install_prose(text, &tex, cx),
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

    fn install_prose(&mut self, text: DocProseText, tex: &str, cx: &mut Context<Self>) {
        let markdown = self.display_card_tex(tex).markdown;
        let view = cx.new(|cx| TextViewState::markdown(&markdown, cx));
        let observed = text.clone();
        let measured = std::cell::Cell::new(false);
        cx.observe(&view, move |this, view, cx| {
            let Some(ProseBody::Ready {
                markdown,
                view: current,
            }) = this.read_prose.get(&observed)
            else {
                return;
            };
            if current == &view
                && view.read(cx).rendered_text().source() == markdown
                && !measured.replace(true)
            {
                this.remeasure_prose(&observed);
                cx.notify();
            }
        })
        .detach();
        self.remeasure_prose(&text);
        self.read_prose
            .insert(text, ProseBody::Ready { markdown, view });
        cx.notify();
    }

    fn remeasure_prose(&self, text: &DocProseText) {
        for (index, row) in self.read_rows.iter().enumerate() {
            if matches!(row, ReadRow::Prose(prose) if prose == text) {
                self.read_list.remeasure_items(index..index + 1);
            }
        }
    }

    pub(super) fn prose_ready(&self, text: &DocProseText, cx: &App) -> bool {
        match self.read_prose.get(text) {
            Some(ProseBody::Ready { markdown, view }) => {
                view.read(cx).rendered_text().source() == markdown
            }
            Some(ProseBody::Unavailable) => true,
            _ => false,
        }
    }

    pub(super) fn render_prose(&self, text: &DocProseText, entity: &Entity<Self>, cx: &App) -> Div {
        let row = div().px(PANE_INSET).w_full().min_w_0();
        match self.read_prose.get(text) {
            Some(ProseBody::Ready { markdown, .. }) if markdown.trim().is_empty() => row,
            Some(ProseBody::Ready { view, .. }) => {
                let entity = entity.clone();
                self.appearance
                    .style_paper(row.py_2())
                    .capture_any_mouse_down(move |event, _, cx| {
                        if event.button == MouseButton::Left {
                            entity.update(cx, |this, _| this.comment_selection = None);
                        }
                    })
                    .child(self.card_body_text(TextView::new(view), true, cx))
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
