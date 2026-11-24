//! Counts and read-only navigation for the current deck's findings ledger.

use std::collections::BTreeSet;

use fuzukue_proto::{
    CardView, ReviewFinding, ReviewFindingAssessment, ReviewFindingChangeReference,
    ReviewFindingLocation, ReviewFindingSeverity, ReviewFindingSourceKind, ReviewFindingStatus,
    ReviewFindingsOutcome, ReviewFindingsSnapshot, REVIEW_FINDINGS_PROTOCOL,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{h_flex, scroll::ScrollableElement as _, v_flex, ActiveTheme as _};
use gpui_kit::{div, prelude::*, AnyElement, App, Context, SharedString, WeakEntity};

use crate::{
    icon_pill, pill, wire_deck, ConnectionStatus, RightPane, Symbol, Workbench, PANE_INSET,
};

#[derive(Default)]
pub(super) struct Findings {
    pub(super) cards_deck: Option<String>,
    outcome: Option<ReviewFindingsOutcome>,
    pane: Option<FindingsPane>,
}

struct FindingsPane {
    return_to: RightPane,
    card: Option<String>,
    status: Option<ReviewFindingStatus>,
    selected: Option<String>,
}

fn count_label(count: usize) -> String {
    tr!("findings-open-count", count = count as f64)
}

fn status_label(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => tr!("findings-open"),
        ReviewFindingStatus::Fixed => tr!("findings-fixed"),
        ReviewFindingStatus::Deferred => tr!("findings-deferred"),
    }
}

fn severity_label(severity: ReviewFindingSeverity) -> &'static str {
    match severity {
        ReviewFindingSeverity::Fatal => tr!("findings-fatal"),
        ReviewFindingSeverity::Major => tr!("findings-major"),
        ReviewFindingSeverity::Minor => tr!("findings-minor"),
        ReviewFindingSeverity::Form => tr!("findings-form"),
    }
}

fn finding_state_label(finding: &ReviewFinding) -> String {
    format!(
        "{} · {}",
        severity_label(finding.content.severity),
        status_label(finding.status)
    )
}

fn file_label(location: &ReviewFindingLocation) -> Option<String> {
    let mut label = location.file.clone()?;
    if let Some([start, end]) = location.lines {
        label.push_str(&format!(":{start}–{end}"));
    }
    if let Some(revision) = &location.revision {
        label.push_str(&format!(" ({revision})"));
    }
    Some(label)
}

impl Workbench {
    pub(super) fn on_review_findings(&mut self, deck: &str, outcome: ReviewFindingsOutcome) {
        if wire_deck(deck) == wire_deck(&self.deck.replace('\\', "/")) {
            self.findings.outcome = Some(outcome);
            self.read_list
                .remeasure_items(0..self.read_list.item_count());
            self.remeasure_finding_previews();
        }
    }

    pub(super) fn invalidate_review_findings(&mut self) {
        self.findings.outcome = None;
        self.read_list
            .remeasure_items(0..self.read_list.item_count());
        self.remeasure_finding_previews();
    }

    fn remeasure_finding_previews(&self) {
        if let Some(chat) = self.chats.get(&self.active) {
            chat.scroller.remeasure();
        }
    }

    fn findings_snapshot(&self) -> Option<&ReviewFindingsSnapshot> {
        match &self.findings.outcome {
            Some(ReviewFindingsOutcome::Loaded { snapshot }) => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn finding_chat_preview(&self, deck: &str, id: &str, cx: &App) -> Option<(String, AnyElement)> {
        if wire_deck(deck) != wire_deck(&self.deck.replace('\\', "/")) {
            return None;
        }
        let finding = self.findings_snapshot()?.findings.iter().find(|finding| finding.id == id)?;
        let summary = &finding.content.summary;
        let status = finding_state_label(finding);
        Some((
            format!("{summary}\n{status}"),
            v_flex()
                .gap_1()
                .child(div().text_sm().font_semibold().child(summary.clone()))
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(status))
                .into_any_element(),
        ))
    }

    fn findings_status(&self) -> Option<String> {
        match &self.findings.outcome {
            Some(ReviewFindingsOutcome::Loaded { .. }) => None,
            Some(ReviewFindingsOutcome::Error { message }) => {
                Some(tr!("findings-load-error", message = message.clone()))
            }
            None if matches!(self.status, ConnectionStatus::Connected) => {
                Some(tr!("findings-loading").into())
            }
            None => Some(tr!("workbench-action-disconnected").into()),
        }
    }

    fn card_findings_snapshot(&self, card: &CardView) -> Option<&ReviewFindingsSnapshot> {
        if card.external
            || self.findings.cards_deck.as_deref()? != wire_deck(&self.deck.replace('\\', "/"))
        {
            return None;
        }
        self.findings_snapshot()
    }

    pub(super) fn card_findings_count(&self, card: &CardView) -> Option<usize> {
        self.card_findings_snapshot(card)?
            .card_open_counts
            .get(&card.id)
            .copied()
    }

    pub(super) fn card_findings_badge(&self, card: &CardView, cx: &App) -> Option<AnyElement> {
        let count = self.card_findings_count(card)?;
        Some(
            div()
                .id(SharedString::from(format!(
                    "review-findings-row-{}",
                    card.id
                )))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(count_label(count))
                .into_any_element(),
        )
    }

    pub(super) fn card_findings_button(
        &self,
        card: &CardView,
        id: impl Into<SharedString>,
        entity: WeakEntity<Self>,
        cx: &App,
    ) -> Option<AnyElement> {
        let snapshot = self.card_findings_snapshot(card)?;
        let card = card.id.clone();
        let (label, status) = if let Some(count) = snapshot.card_open_counts.get(&card) {
            (count_label(*count), Some(ReviewFindingStatus::Open))
        } else if snapshot
            .findings
            .iter()
            .any(|finding| has_card(finding, &card))
        {
            (tr!("findings-history").into(), None)
        } else {
            return None;
        };
        Some(
            pill(id, label, false, cx)
                .self_start()
                .on_click(move |_, _, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.open_findings(Some(card.clone()), status, cx)
                    });
                })
                .into_any_element(),
        )
    }

    pub(super) fn deck_findings_control(
        &self,
        id: &'static str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if self.protocol < REVIEW_FINDINGS_PROTOCOL {
            return None;
        }
        if let Some(snapshot) = self.findings_snapshot() {
            return Some(
                pill(id, count_label(snapshot.open_count), false, cx)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_findings(None, Some(ReviewFindingStatus::Open), cx)
                    }))
                    .into_any_element(),
            );
        }
        self.findings_status().map(|status| {
            div()
                .id(id)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(status)
                .into_any_element()
        })
    }

    fn open_findings(
        &mut self,
        card: Option<String>,
        status: Option<ReviewFindingStatus>,
        cx: &mut Context<Self>,
    ) {
        self.findings.pane = Some(FindingsPane {
            return_to: self.right,
            card,
            status,
            selected: None,
        });
        self.right = RightPane::Findings;
        cx.notify();
    }

    pub(super) fn close_findings(&mut self) {
        if self.right == RightPane::Findings {
            self.right = self
                .findings
                .pane
                .take()
                .map_or(RightPane::None, |pane| pane.return_to);
        }
    }

    fn finding_locations(
        &self,
        locations: &[ReviewFindingLocation],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex().gap_1().children(locations.iter().map(|location| {
            let card = location.card.clone();
            let file = location.file.clone();
            // A saved revision identifies historical lines. Show the range alongside
            // that revision and open the current file without jumping to those lines.
            let lines = location
                .lines
                .filter(|_| location.revision.is_none())
                .map(|[start, end]| start as usize..=end as usize);
            let current_deck = self.findings.cards_deck.as_deref()
                == Some(wire_deck(&self.deck.replace('\\', "/")));
            let card_link = card.clone().filter(|id| {
                current_deck
                    && self
                        .cards
                        .iter()
                        .any(|card| !card.external && &card.id == id)
            });
            let card = card.map(|id| div().text_xs().child(id));
            let card = match (card, card_link) {
                (Some(label), Some(id)) => Some(
                    label
                        .text_color(cx.theme().link)
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)),
                        ),
                ),
                (label, _) => label,
            };
            h_flex().gap_2().flex_wrap().children(card).children(
                file.zip(file_label(location)).map(|(path, label)| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().link)
                        .cursor_pointer()
                        .child(label)
                        .on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.open_source_file(path.clone(), lines.clone(), window, cx);
                            }),
                        )
                }),
            )
        }))
    }

    fn render_finding(&self, finding: &ReviewFinding, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = &finding.content;
        let muted = |label: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let assessment = content
            .assessment
            .as_ref()
            .map(|assessment| match assessment {
                ReviewFindingAssessment::Confirmed => tr!("findings-confirmed"),
                ReviewFindingAssessment::Suspected => tr!("findings-suspected"),
                ReviewFindingAssessment::Unchecked => tr!("findings-unchecked"),
            });
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(content.summary.clone()),
            )
            .child(muted(finding_state_label(finding)))
            .child(div().text_sm().child(content.detail.clone()))
            .child(self.finding_locations(&content.locations, cx))
            .children(assessment.map(|label| muted(label.into())))
            .children(
                content
                    .evidence
                    .clone()
                    .map(|evidence| div().text_sm().child(evidence)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("findings-sources")),
                    )
                    .children(content.sources.iter().map(|source| {
                        let kind = match source.kind {
                            ReviewFindingSourceKind::Human => tr!("findings-source-human"),
                            ReviewFindingSourceKind::Agent => tr!("findings-source-agent"),
                        };
                        let refs = [
                            &source.reviewer,
                            &source.perspective,
                            &source.thread,
                            &source.review,
                            &source.finding,
                        ];
                        muted(
                            std::iter::once(kind)
                                .chain(refs.into_iter().filter_map(|item| item.as_deref()))
                                .collect::<Vec<_>>()
                                .join(" · "),
                        )
                    })),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("findings-history")),
                    )
                    .children(finding.transitions.iter().map(|transition| {
                        let action = &transition.action;
                        let time = chrono::DateTime::from_timestamp_millis(transition.time_ms)
                            .map(|time| {
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_else(|| transition.time_ms.to_string());
                        v_flex()
                            .gap_1()
                            .child(muted(format!(
                                "{} · {} · {time}",
                                status_label(action.status),
                                action.actor
                            )))
                            .children(
                                action
                                    .reason
                                    .clone()
                                    .map(|reason| div().text_sm().child(reason)),
                            )
                            .children(action.changes.iter().map(|change| {
                                let reference = match &change.reference {
                                    ReviewFindingChangeReference::Commit { oid } => {
                                        muted(oid.clone())
                                    }
                                    ReviewFindingChangeReference::CardEdit { card, record } => {
                                        let (card, record) = (card.clone(), record.clone());
                                        muted(format!("{card} · {record}"))
                                            .text_color(theme.link)
                                            .cursor_pointer()
                                            .on_mouse_down(
                                                gpui_kit::MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    this.open_finding_card_edit(
                                                        card.clone(),
                                                        record.clone(),
                                                    );
                                                    cx.notify();
                                                }),
                                            )
                                    }
                                };
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child(change.summary.clone()))
                                    .child(reference)
                                    .child(self.finding_locations(&change.locations, cx))
                            }))
                    })),
            )
    }

    pub(super) fn render_findings(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut panel = v_flex().id("review-findings-ledger").size_full().min_w_0();
        let Some(pane) = &self.findings.pane else {
            return panel;
        };
        let header = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                icon_pill(
                    "review-findings-close",
                    Symbol::ArrowLeft,
                    tr!("findings-back"),
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_findings();
                    cx.notify();
                })),
            )
            .child(div().text_sm().child(tr!("findings-title")))
            .children(pane.card.clone().map(|card| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(card)
            }));
        panel = panel.child(
            self.pane_heading(header, cx)
                .px(PANE_INSET)
                .py_2()
                .border_b_1()
                .border_color(theme.sidebar_border),
        );
        let Some(snapshot) = self.findings_snapshot() else {
            return panel.children(
                self.findings_status()
                    .map(|status| div().px(PANE_INSET).py_2().text_sm().child(status)),
            );
        };
        let content = if let Some(finding) = pane
            .selected
            .as_ref()
            .and_then(|id| snapshot.findings.iter().find(|finding| &finding.id == id))
        {
            v_flex()
                .gap_3()
                .child(
                    h_flex().child(
                        icon_pill(
                            "review-findings-back-to-list",
                            Symbol::ArrowLeft,
                            tr!("findings-back-to-list"),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(pane) = &mut this.findings.pane {
                                pane.selected = None;
                            }
                            cx.notify();
                        })),
                    ),
                )
                .child(self.render_finding(finding, cx))
        } else {
            let filters = [
                (
                    "open",
                    Some(ReviewFindingStatus::Open),
                    tr!("findings-open"),
                ),
                (
                    "fixed",
                    Some(ReviewFindingStatus::Fixed),
                    tr!("findings-fixed"),
                ),
                (
                    "deferred",
                    Some(ReviewFindingStatus::Deferred),
                    tr!("findings-deferred"),
                ),
                ("all", None, tr!("findings-all")),
            ];
            let mut findings = snapshot
                .findings
                .iter()
                .filter(|finding| {
                    pane.status.is_none_or(|status| status == finding.status)
                        && pane
                            .card
                            .as_ref()
                            .is_none_or(|card| has_card(finding, card))
                })
                .peekable();
            let empty = findings.peek().is_none();
            v_flex()
                .gap_2()
                .child(h_flex().gap_1().flex_wrap().children(filters.map(
                    |(key, status, label)| {
                        pill(
                            format!("review-findings-filter-{key}"),
                            label,
                            pane.status == status,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(pane) = &mut this.findings.pane {
                                pane.status = status;
                            }
                            cx.notify();
                        }))
                    },
                )))
                .children(empty.then(|| {
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("findings-empty"))
                }))
                .children(findings.map(|finding| {
                    let id = finding.id.clone();
                    let mut seen = BTreeSet::new();
                    let locations = finding
                        .content
                        .locations
                        .iter()
                        .flat_map(|location| {
                            location
                                .card
                                .clone()
                                .into_iter()
                                .chain(file_label(location))
                        })
                        .filter(|location| seen.insert(location.clone()))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    v_flex()
                        .id(SharedString::from(format!("review-finding-{id}")))
                        .p_2()
                        .gap_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|row| row.bg(theme.secondary))
                        .child(div().text_sm().child(finding.content.summary.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{} · {} · {locations}",
                                    severity_label(finding.content.severity),
                                    status_label(finding.status),
                                )),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(pane) = &mut this.findings.pane {
                                pane.selected = Some(id.clone());
                            }
                            cx.notify();
                        }))
                }))
        };
        panel.child(
            div()
                .id("review-findings-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .px(PANE_INSET)
                .py_2()
                .child(content),
        )
    }
}

fn has_card(finding: &ReviewFinding, id: &str) -> bool {
    finding
        .content
        .locations
        .iter()
        .any(|location| location.card.as_deref() == Some(id))
}
