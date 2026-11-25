//! Actions and feedback for the deck overview.

use std::time::Duration;

use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, AnyElement, Context, MouseButton, SharedString, Task};

use crate::{
    card_diagnostic_text, wire_deck, ClientMsg, ConnectionStatus, RightPane, Symbol, Workbench,
    PANE_INSET,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct DeckOverview {
    pending_deck: Option<(String, Task<()>)>,
    deck_error: Option<&'static str>,
    refreshing_issues: Option<Task<()>>,
    issue_notice: Option<&'static str>,
}

impl DeckOverview {
    pub(super) fn clear_deck_request(&mut self) {
        self.pending_deck = None;
        self.deck_error = None;
    }

    pub(super) fn disconnected(&mut self) {
        if self.pending_deck.take().is_some() {
            self.deck_error = Some(tr!("workbench-action-disconnected"));
        }
        if self.refreshing_issues.take().is_some() {
            self.issue_notice = Some(tr!("workbench-action-disconnected"));
        }
    }

    pub(super) fn issues_received(&mut self, success: bool) {
        let refreshed = self.refreshing_issues.take().is_some() && success;
        self.issue_notice = refreshed.then(|| tr!("workbench-issues-refreshed"));
    }

    pub(super) fn deck_status(&self) -> Option<&'static str> {
        if self.pending_deck.is_some() {
            Some(tr!("workbench-loading"))
        } else {
            self.deck_error
        }
    }

    pub(super) fn issue_status(&self) -> Option<&'static str> {
        if self.refreshing_issues.is_some() {
            Some(tr!("workbench-refreshing-issues"))
        } else {
            self.issue_notice
        }
    }
}

impl Workbench {
    pub(super) fn open_overview_cards(&mut self, deck: &str, cx: &mut Context<Self>) {
        if self
            .deck_overview
            .pending_deck
            .as_ref()
            .is_some_and(|(target, _)| target == deck)
        {
            return;
        }
        self.deck_overview.clear_deck_request();
        if self.deck == deck {
            self.right = RightPane::DeckCards;
        } else if !matches!(self.status, ConnectionStatus::Connected)
            || self
                .out
                .send(ClientMsg::SelectDeck {
                    deck: deck.to_owned(),
                })
                .is_err()
        {
            self.deck_overview.deck_error = Some(tr!("workbench-action-disconnected"));
        } else {
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REQUEST_TIMEOUT).await;
                let _ = this.update(cx, |this, cx| {
                    this.deck_overview.pending_deck = None;
                    this.deck_overview.deck_error = Some(tr!("workbench-deck-load-timeout"));
                    cx.notify();
                });
            });
            self.deck_overview.pending_deck = Some((deck.to_owned(), task));
        }
        cx.notify();
    }

    pub(super) fn finish_overview_deck_request(&mut self, deck: &str) {
        let Some((target, _)) = &self.deck_overview.pending_deck else {
            return;
        };
        if target != wire_deck(&deck.replace('\\', "/")) {
            return;
        }
        if self.right == RightPane::Overview
            && !self.settings_open
            && self
                .active_tab
                .as_ref()
                .and_then(|id| self.tabs.iter().find(|tab| &tab.id == id))
                .and_then(|tab| tab.deck.as_ref())
                == Some(target)
        {
            self.right = RightPane::DeckCards;
        }
        self.deck_overview.clear_deck_request();
    }

    fn refresh_overview_issues(&mut self, cx: &mut Context<Self>) {
        if self.deck_overview.refreshing_issues.is_some() {
            return;
        }
        self.deck_overview.issue_notice = None;
        if !matches!(self.status, ConnectionStatus::Connected)
            || self.out.send(ClientMsg::RefreshIssues).is_err()
        {
            self.deck_overview.issue_notice = Some(tr!("workbench-action-disconnected"));
        } else {
            self.deck_overview.refreshing_issues = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REQUEST_TIMEOUT).await;
                let _ = this.update(cx, |this, cx| {
                    this.deck_overview.refreshing_issues = None;
                    this.deck_overview.issue_notice = Some(tr!("workbench-issue-refresh-timeout"));
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    pub(super) fn overview_issue_refresh_button(&self, cx: &Context<Self>) -> Button {
        Button::new("refresh-issues")
            .ghost()
            .xsmall()
            .flex_none()
            .label(tr!("workbench-refresh"))
            .tooltip(tr!("workbench-refresh-issues"))
            .accessibility_label(tr!("workbench-refresh-issues"))
            .disabled(self.deck_overview.refreshing_issues.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.refresh_overview_issues(cx)))
    }

    pub(super) fn render_deck_cards(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.deck_card_rows(cx);
        let empty = rows.is_empty();
        v_flex()
            .id("deck-cards")
            .size_full()
            .min_w_0()
            .child(
                self.pane_heading(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new("deck-cards-back")
                                .ghost()
                                .xsmall()
                                .icon(Symbol::ArrowLeft.icon())
                                .tooltip(tr!("workbench-back-to-overview"))
                                .accessibility_label(tr!("workbench-back-to-overview"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.right = RightPane::Overview;
                                    cx.notify();
                                })),
                        )
                        .child(div().text_sm().child(tr!("workbench-cards")))
                        .children(self.deck_findings_control("deck-review-findings", cx)),
                    cx,
                )
                .px(PANE_INSET)
                .py_3()
                .border_b_1()
                .border_color(theme.sidebar_border),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .pb_3()
                    .overflow_y_scrollbar()
                    .id("deck-card-list")
                    .child(v_flex().gap_0p5().children(rows).children(empty.then(|| {
                        div()
                            .px_2()
                            .py_3()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(
                                self.card_diags
                                    .iter()
                                    .find(|diagnostic| diagnostic.error)
                                    .map_or_else(
                                        || tr!("workbench-no-deck-cards").to_owned(),
                                        card_diagnostic_text,
                                    ),
                            )
                    })).children(self.deck_diagnostics_panel(None, cx))),
            )
    }

    pub(super) fn deck_card_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let mut rows = Vec::new();
        let mut last_kind = None;
        for card in self.cards.iter().filter(|card| !card.external) {
            if last_kind != Some(card.kind.as_str()) {
                rows.push(
                    div()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(card.kind.clone())
                        .into_any_element(),
                );
                last_kind = Some(card.kind.as_str());
            }
            let id = card.id.clone();
            let selected = self.selected.as_deref() == Some(id.as_str());
            let label = if card.title.is_empty() {
                id.clone()
            } else {
                card.title.clone()
            };
            rows.push(
                div()
                    .id(SharedString::from(format!("card-{id}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |row| {
                        row.bg(theme.accent).text_color(theme.accent_foreground)
                    })
                    .hover(|row| row.bg(theme.secondary))
                    .child(label)
                    .children(crate::translation::diagnostic(card).map(|message| {
                        div().id(SharedString::from(format!("translation-diagnostic-{id}")))
                            .text_xs().text_color(theme.warning).child(message)
                    }))
                    .children(self.card_findings_badge(card, cx))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)),
                    )
                    .into_any_element(),
            );
        }
        rows
    }
}
