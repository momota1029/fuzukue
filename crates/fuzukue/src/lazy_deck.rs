use std::collections::HashMap;
use std::sync::Arc;

use fuzukue_proto::{CardContent, CardView, DeckCardPage, DeckDiagnosticPage, CARD_CONTENT_BYTES, DECK_PAGE_BYTES};
use gpui_kit::{component::{v_flex, ActiveTheme as _}, div, prelude::*, AnyElement, Context, MouseButton};

use crate::{card_diagnostic_text, conn::HttpTarget, pill, Workbench};

#[derive(Default)]
pub(super) struct LazyDeck {
    generation: Arc<()>,
    deck: Option<String>,
    revision: Option<u64>,
    summary_error: Option<String>,
    bodies: HashMap<String, Body>,
    diagnostics: HashMap<Option<String>, Diagnostics>,
}

enum Body {
    Loading,
    Loaded,
    Error(String),
}

#[derive(Default)]
struct Diagnostics {
    page: Option<DeckDiagnosticPage>,
    loading: bool,
    error: Option<String>,
}

fn summaries(http: &HttpTarget, deck: &str) -> Result<(u64, Vec<CardView>), String> {
    let mut cards = Vec::new();
    let mut revision = None;
    let mut offset = 0;
    loop {
        let page: DeckCardPage = http.read_deck("cards", deck, None, revision, offset, DECK_PAGE_BYTES)?;
        if revision.is_some_and(|revision| revision != page.revision) {
            return Err(tr!("workbench-deck-changed").into());
        }
        cards.extend(page.cards);
        match page.next {
            Some(next) if next > offset => {
                offset = next;
                revision = Some(page.revision);
            }
            Some(_) => return Err(tr!("workbench-deck-changed").into()),
            None => return Ok((page.revision, cards)),
        }
    }
}

impl Workbench {
    pub(super) fn deck_summaries_pending(&self, deck: &str) -> bool {
        self.lazy_deck.deck.as_deref() == Some(deck) && self.lazy_deck.revision.is_none()
    }

    pub(super) fn load_deck_summaries(&mut self, deck: String, cx: &mut Context<Self>) {
        if self.findings.cards_deck.as_deref() != Some(crate::wire_deck(&deck.replace('\\', "/"))) {
            self.cards.clear();
            self.rebuild_read(true, cx);
        }
        self.lazy_deck = LazyDeck { deck: Some(deck.clone()), ..LazyDeck::default() };
        self.card_diags.clear();
        cx.notify();
        let generation = self.lazy_deck.generation.clone();
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let task_deck = deck.clone();
            let result = cx.background_executor().spawn(async move { summaries(&http, &task_deck) }).await;
            let _ = this.update(cx, |this, cx| {
                if !Arc::ptr_eq(&this.lazy_deck.generation, &generation) {
                    return;
                }
                match result {
                    Ok((revision, cards)) => {
                        this.lazy_deck.revision = Some(revision);
                        this.apply_deck_cards(&deck, cards, Vec::new(), cx);
                        if this.document_deck.as_deref() == Some(&deck) {
                            this.finish_paper_open(cx);
                        }
                        if this.right == crate::RightPane::Card {
                            if let Some(card) = this.selected.clone() {
                                this.load_card_content(card, cx);
                            }
                        }
                    }
                    Err(error) => {
                        this.lazy_deck.summary_error = Some(error);
                        this.finish_overview_deck_request(&deck);
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    pub(super) fn load_card_content(&mut self, id: String, cx: &mut Context<Self>) {
        let (Some(deck), Some(revision)) = (self.lazy_deck.deck.clone(), self.lazy_deck.revision) else {
            return;
        };
        if !self.cards.iter().any(|card| card.id == id) || self.lazy_deck.bodies.contains_key(&id)
            || self.lazy_deck.bodies.values().filter(|body| matches!(body, Body::Loading)).count() >= 4
        {
            return;
        }
        self.lazy_deck.bodies.insert(id.clone(), Body::Loading);
        let http = self.http.clone();
        let generation = self.lazy_deck.generation.clone();
        cx.spawn(async move |this, cx| {
            let card_id = id.clone();
            let content = cx.background_executor().spawn(async move {
                http.read_deck::<CardContent>("card", &deck, Some(&card_id), Some(revision), 0, CARD_CONTENT_BYTES)
            }).await;
            let _ = this.update(cx, |this, cx| {
                if !Arc::ptr_eq(&this.lazy_deck.generation, &generation) {
                    return;
                }
                match content {
                    Ok(content) => this.receive_card_content(id, content, cx),
                    Err(error) => {
                        this.lazy_deck.bodies.insert(id, Body::Error(error));
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    pub(super) fn receive_card_content(&mut self, id: String, content: CardContent, cx: &mut Context<Self>) {
        let Some(card) = self.cards.iter_mut().find(|card| card.id == id) else { return };
        card.body = content.body;
        card.edit_body = content.edit_body;
        card.edit_proof = content.edit_proof;
        self.lazy_deck.bodies.insert(id.clone(), Body::Loaded);
        self.rebuild_read_card(&id, cx);
    }

    pub(super) fn card_body_status(&self, id: &str, entity: &gpui_kit::Entity<Self>, cx: &gpui_kit::App) -> Option<AnyElement> {
        let deck = self.lazy_deck.deck.as_ref()?;
        let theme = cx.theme();
        match self.lazy_deck.bodies.get(id) {
            Some(Body::Loaded) => None,
            Some(Body::Error(error)) => {
                let id = id.to_owned();
                let entity = entity.clone();
                Some(v_flex().items_start().gap_1()
                    .child(div().text_sm().text_color(theme.warning).child(error.clone()))
                    .child(pill(format!("retry-card-{id}"), tr!("workbench-retry-card"), false, cx)
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.lazy_deck.bodies.remove(&id);
                                this.load_card_content(id.clone(), cx);
                                cx.notify();
                            });
                        }))
                    .into_any_element())
            }
            _ if self.lazy_deck.summary_error.is_some() => {
                let deck = deck.clone();
                let entity = entity.clone();
                Some(pill("retry-deck-summary", tr!("workbench-retry-deck"), false, cx)
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        entity.update(cx, |this, cx| this.load_deck_summaries(deck.clone(), cx));
                    }).into_any_element())
            }
            _ => Some(div().min_h(gpui_kit::px(56.)).text_sm().text_color(theme.muted_foreground)
                .child(tr!("workbench-loading-card")).into_any_element()),
        }
    }

    pub(super) fn queue_visible_card(index: usize, cx: &mut gpui_kit::App, entity: gpui_kit::Entity<Self>) {
        let this = entity.read(cx);
        let Some(crate::ReadRow::Card(id)) = this.read_rows.get(index) else { return };
        if this.lazy_deck.revision.is_some()
            && !this.lazy_deck.bodies.contains_key(id)
            && this.cards.iter().any(|card| &card.id == id)
        {
            let id = id.clone();
            let generation = this.lazy_deck.generation.clone();
            cx.defer(move |cx| entity.update(cx, |this, cx| {
                if Arc::ptr_eq(&this.lazy_deck.generation, &generation) {
                    this.load_card_content(id, cx);
                }
            }));
        }
    }

    fn load_deck_diagnostics(&mut self, card: Option<String>, cx: &mut Context<Self>) {
        let (Some(deck), Some(revision)) = (self.lazy_deck.deck.clone(), self.lazy_deck.revision) else {
            return;
        };
        let state = self.lazy_deck.diagnostics.entry(card.clone()).or_default();
        if state.loading {
            return;
        }
        let offset = match &state.page {
            Some(page) => match page.next { Some(offset) => offset, None => return },
            None => 0,
        };
        state.loading = true;
        state.error = None;
        let http = self.http.clone();
        let generation = self.lazy_deck.generation.clone();
        cx.spawn(async move |this, cx| {
            let id = card.clone();
            let result = cx.background_executor().spawn(async move {
                http.read_deck::<DeckDiagnosticPage>("diagnostics", &deck, id.as_deref(), Some(revision), offset, DECK_PAGE_BYTES)
            }).await;
            let _ = this.update(cx, |this, cx| {
                if !Arc::ptr_eq(&this.lazy_deck.generation, &generation) {
                    return;
                }
                let state = this.lazy_deck.diagnostics.entry(card.clone()).or_default();
                state.loading = false;
                match result {
                    Ok(mut page) => {
                        if let Some(previous) = state.page.take() {
                            let mut diagnostics = previous.diagnostics;
                            diagnostics.append(&mut page.diagnostics);
                            page.diagnostics = diagnostics;
                        }
                        state.page = Some(page);
                    }
                    Err(error) => state.error = Some(error),
                }
                this.sync_card_file_style_notes(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    pub(super) fn loaded_card_diagnostics<'a>(&'a self, path: &str) -> &'a [fuzukue_proto::CardDiagnostic] {
        self.cards.iter().find(|card| card.path == path)
            .and_then(|card| self.lazy_deck.diagnostics.get(&Some(card.id.clone())))
            .and_then(|state| state.page.as_ref())
            .map_or(&[], |page| &page.diagnostics)
    }

    pub(super) fn deck_diagnostics_panel(&self, card: Option<&str>, cx: &Context<Self>) -> Option<AnyElement> {
        let deck = self.lazy_deck.deck.as_ref()?;
        let theme = cx.theme();
        if self.lazy_deck.revision.is_none() {
            return card.is_none().then(|| {
                match &self.lazy_deck.summary_error {
                    Some(error) => {
                        let deck = deck.clone();
                        v_flex().items_start().gap_1()
                            .child(div().text_sm().text_color(theme.warning).child(error.clone()))
                            .child(pill("reload-deck", tr!("workbench-retry-deck"), false, cx)
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                    this.load_deck_summaries(deck.clone(), cx);
                                    cx.notify();
                                })))
                            .into_any_element()
                    }
                    None => div().text_sm().text_color(theme.muted_foreground)
                        .child(tr!("workbench-loading-deck")).into_any_element(),
                }
            });
        }
        let card = card.map(str::to_owned);
        let state = self.lazy_deck.diagnostics.get(&card);
        let page = state.and_then(|state| state.page.as_ref());
        let id = format!("diagnostics-{}", card.as_deref().unwrap_or("deck"));
        let mut panel = v_flex().items_start().gap_1().text_xs();
        if let Some(page) = page {
            panel = panel
                .child(div().text_color(theme.muted_foreground).child(tr!("workbench-diagnostic-total",
                    shown = page.diagnostics.len(), total = page.total, errors = page.errors)))
                .children(page.diagnostics.iter().map(|diagnostic| {
                    div().w_full().text_color(if diagnostic.error { theme.danger } else { theme.warning })
                        .child(if card.is_none() { format!("{} · {}", diagnostic.path, card_diagnostic_text(diagnostic)) }
                            else { card_diagnostic_text(diagnostic) })
                }));
        }
        if let Some(error) = state.and_then(|state| state.error.as_ref()) {
            panel = panel.child(div().text_color(theme.warning).child(error.clone()));
        }
        if state.is_some_and(|state| state.loading) {
            panel = panel.child(div().text_color(theme.muted_foreground).child(tr!("workbench-loading-diagnostics")));
        } else if page.is_none_or(|page| page.next.is_some()) {
            let label = if page.is_some() { tr!("workbench-more-diagnostics") } else { tr!("workbench-show-diagnostics") };
            panel = panel.child(pill(id, label, false, cx).on_mouse_down(MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.load_deck_diagnostics(card.clone(), cx))));
        }
        Some(panel.into_any_element())
    }
}
