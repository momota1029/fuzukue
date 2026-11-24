use std::collections::BTreeSet;

use gpui_kit::component::{
    button::Button,
    h_flex,
    input::{Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, Context, Entity, Focusable as _, FontWeight, Window};
use fuzukue_proto::{
    ClientMsg, LedgerAction, LedgerEntry, LedgerKind, LedgerOutcome, LedgerSource, LedgerStatus,
    LedgerView, LEDGER_PROTOCOL,
};

use super::{pill, RightPane, Workbench};

const FILTERS: [Option<LedgerStatus>; 5] = [
    None,
    Some(LedgerStatus::Pending),
    Some(LedgerStatus::Acquired),
    Some(LedgerStatus::AskUser),
    Some(LedgerStatus::Abandoned),
];

#[derive(Clone, Copy)]
enum Source {
    Text,
    Card,
    Reference,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Text => tr!("ledger-source-text"),
            Self::Card => tr!("ledger-source-card"),
            Self::Reference => tr!("ledger-source-reference"),
        }
    }
}

#[derive(Default)]
pub(super) struct Pane {
    pub(super) open: bool,
    pub(super) request: u64,
    pending: bool,
    dirty: bool,
    error: bool,
    proposing: bool,
    no_proposals: bool,
    view: LedgerView,
    filter: Option<LedgerStatus>,
    expanded: BTreeSet<String>,
    proposal: Option<(Source, Entity<TextareaState>)>,
}

fn status_label(status: Option<LedgerStatus>) -> &'static str {
    match status {
        None => tr!("ledger-all"),
        Some(LedgerStatus::Pending) => tr!("ledger-pending"),
        Some(LedgerStatus::Acquired) => tr!("ledger-acquired"),
        Some(LedgerStatus::AskUser) => tr!("ledger-ask-user"),
        Some(LedgerStatus::Abandoned) => tr!("ledger-abandoned"),
    }
}

fn kind_label(kind: LedgerKind) -> &'static str {
    match kind {
        LedgerKind::Paper => tr!("ledger-paper"),
        LedgerKind::Supplement => tr!("ledger-supplement"),
        LedgerKind::Data => tr!("ledger-data"),
        LedgerKind::Code => tr!("ledger-code"),
        LedgerKind::WebPage => tr!("ledger-web-page"),
    }
}

fn outcome_label(outcome: LedgerOutcome) -> &'static str {
    match outcome {
        LedgerOutcome::Ok => tr!("ledger-attempt-ok"),
        LedgerOutcome::Forbidden => tr!("ledger-attempt-forbidden"),
        LedgerOutcome::BotWall => tr!("ledger-attempt-bot-wall"),
        LedgerOutcome::Login => tr!("ledger-attempt-login"),
        LedgerOutcome::NotFound => tr!("ledger-attempt-not-found"),
        LedgerOutcome::Timeout => tr!("ledger-attempt-timeout"),
        LedgerOutcome::Manual => tr!("ledger-attempt-manual"),
        LedgerOutcome::Error => tr!("ledger-attempt-error"),
        LedgerOutcome::Started => tr!("ledger-attempt-started"),
    }
}

fn reason_label(reason: &str) -> &str {
    if reason == "pasted_text" {
        Source::Text.label()
    } else {
        reason
    }
}

impl Pane {
    pub(super) fn disconnect(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.pending = false;
        self.dirty = false;
        self.error = true;
        self.proposing = false;
        self.view = LedgerView::default();
    }
}

impl Workbench {
    pub(crate) fn close_ledger(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.ledger_open() {
            return false;
        }
        self.library.ledger.open = false;
        self.reset_library_search(false, cx);
        true
    }

    pub(crate) fn open_ledger(&mut self, cx: &mut Context<Self>) {
        self.library.ledger.open = true;
        self.send_ledger(LedgerAction::List, cx);
    }

    fn send_ledger(&mut self, action: LedgerAction, cx: &mut Context<Self>) {
        let pane = &mut self.library.ledger;
        if self.protocol < LEDGER_PROTOCOL || pane.pending {
            return;
        }
        pane.request = pane.request.wrapping_add(1);
        pane.proposing = matches!(action, LedgerAction::Propose { .. });
        if pane.proposing {
            pane.no_proposals = false;
        }
        pane.pending = self
            .out
            .send(ClientMsg::Ledger {
                request: pane.request,
                action,
            })
            .is_ok();
        pane.error = !pane.pending;
        cx.notify();
    }

    pub(crate) fn on_ledger_result(
        &mut self,
        request: u64,
        view: Option<LedgerView>,
        cx: &mut Context<Self>,
    ) {
        let pane = &mut self.library.ledger;
        if pane.request != request {
            return;
        }
        pane.pending = false;
        pane.error = view.is_none();
        let proposing = std::mem::take(&mut pane.proposing);
        if let Some(view) = view {
            if proposing {
                pane.no_proposals = view.proposals == pane.view.proposals;
                if !pane.no_proposals {
                    pane.proposal = None;
                }
            }
            pane.view = view;
        }
        if std::mem::take(&mut pane.dirty) && !pane.error {
            self.ledger_changed(cx);
        }
    }

    pub(crate) fn ledger_changed(&mut self, cx: &mut Context<Self>) {
        if self.right == RightPane::Library && self.library.ledger.open {
            if self.library.ledger.pending {
                self.library.ledger.dirty = true;
            } else {
                self.send_ledger(LedgerAction::List, cx);
            }
        }
    }

    fn edit_ledger_proposal(
        &mut self,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 5)
                .placeholder(source.label())
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.library.ledger.proposal = Some((source, input));
        self.library.ledger.no_proposals = false;
        cx.notify();
    }

    fn submit_ledger_proposal(&mut self, cx: &mut Context<Self>) {
        let pane = &mut self.library.ledger;
        let Some((source, input)) = &pane.proposal else {
            return;
        };
        let value = input.read(cx).value().trim().to_owned();
        if value.is_empty() || pane.pending {
            return;
        }
        let input = match source {
            Source::Text => LedgerSource::Text { text: value },
            Source::Card => LedgerSource::Card { path: value },
            Source::Reference => LedgerSource::Reference { key: value },
        };
        self.send_ledger(LedgerAction::Propose { input }, cx);
    }

    pub(super) fn render_ledger(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.library.ledger;
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let proposal_menu = Button::new("ledger-new-proposal")
            .small()
            .outline()
            .label(tr!("ledger-propose"))
            .dropdown_menu(move |mut menu, _, _| {
                for source in [Source::Text, Source::Card, Source::Reference] {
                    let entity = entity.clone();
                    menu = menu.item(PopupMenuItem::new(source.label()).on_click(
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.edit_ledger_proposal(source, window, cx)
                            });
                        },
                    ));
                }
                menu
            });
        let status_filters =
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(FILTERS.into_iter().enumerate().map(|(i, status)| {
                    pill(
                        format!("ledger-filter-{i}"),
                        status_label(status),
                        pane.filter == status,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.library.ledger.filter = status;
                        cx.notify();
                    }))
                }));
        let mut body = v_flex()
            .gap_3()
            .w_full()
            .max_w(px(980.))
            .mx_auto()
            .px_5()
            .py_3();
        if !pane.view.proposals.is_empty() {
            let ids: Vec<_> = pane.view.proposals.iter().map(|p| p.id).collect();
            let mut proposals = v_flex()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .pb_3()
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr!("ledger-proposals")),
                        )
                        .child(
                            pill("ledger-accept-all", tr!("ledger-accept-all"), false, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_ledger(LedgerAction::Accept { ids: ids.clone() }, cx);
                                })),
                        ),
                );
            for proposal in &pane.view.proposals {
                let id = proposal.id;
                let entry = &proposal.entry;
                proposals =
                    proposals.child(
                        v_flex()
                            .id(format!("ledger-proposal-{id}"))
                            .gap_1()
                            .child(entry.title.clone())
                            .child(div().text_sm().text_color(theme.muted_foreground).child(
                                format!(
                                    "{} · {} · {}",
                                    kind_label(entry.kind),
                                    status_label(Some(entry.status)),
                                    reason_label(&entry.reason)
                                ),
                            ))
                            .children(
                                entry
                                    .locations
                                    .iter()
                                    .map(|loc| div().text_sm().child(loc.browser_url())),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        pill(
                                            format!("ledger-accept-{id}"),
                                            tr!("ledger-accept"),
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.send_ledger(
                                                    LedgerAction::Accept { ids: vec![id] },
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                    .child(
                                        pill(
                                            format!("ledger-dismiss-{id}"),
                                            tr!("ledger-dismiss"),
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.send_ledger(
                                                    LedgerAction::Dismiss { ids: vec![id] },
                                                    cx,
                                                );
                                            }),
                                        ),
                                    ),
                            ),
                    );
            }
            body = body.child(proposals);
        }
        if !pane.view.downloads.is_empty() {
            body = body.child(
                v_flex()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .pb_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr!("ledger-downloads")),
                    )
                    .children(pane.view.downloads.iter().map(|file| {
                        let id = file.id;
                        let keys = file
                            .keys
                            .iter()
                            .filter_map(|key| {
                                let entry =
                                    pane.view.entries.iter().find(|entry| &entry.key == key)?;
                                Some((key.clone(), entry.title.clone()))
                            })
                            .collect::<Vec<_>>();
                        let entity = cx.entity();
                        h_flex()
                            .id(format!("ledger-download-{id}"))
                            .gap_2()
                            .flex_wrap()
                            .child(div().text_sm().child(file.name.clone()))
                            .child(
                                Button::new(format!("ledger-link-{id}"))
                                    .small()
                                    .outline()
                                    .label(tr!("ledger-link"))
                                    .dropdown_menu(move |mut menu, _, _| {
                                        for (key, title) in &keys {
                                            let key = key.clone();
                                            let entity = entity.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(title.clone()).on_click(
                                                    move |_, _, cx| {
                                                        entity.update(cx, |this, cx| {
                                                            this.send_ledger(
                                                                LedgerAction::Link {
                                                                    download: id,
                                                                    key: key.clone(),
                                                                },
                                                                cx,
                                                            )
                                                        });
                                                    },
                                                ),
                                            );
                                        }
                                        menu
                                    }),
                            )
                    })),
            );
        }
        let rows = pane
            .view
            .entries
            .iter()
            .filter(|entry| pane.filter.is_none_or(|filter| filter == entry.status));
        if rows.clone().next().is_none() && !pane.pending && !pane.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("ledger-empty")),
            );
        }
        body = body.child(
            v_flex()
                .child(
                    h_flex()
                        .gap_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(div().flex_1().child(tr!("ledger-title")))
                        .child(div().w(px(104.)).child(tr!("ledger-status")))
                        .child(div().w(px(102.)).child(tr!("ledger-attempts"))),
                )
                .children(rows.map(|entry| self.ledger_row(entry, cx))),
        );
        v_flex()
            .id("ledger-pane")
            .size_full()
            .min_w_0()
            .child(
                v_flex()
                    .w_full()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        pill(
                                            "library-search-tab",
                                            tr!("ledger-search-tab"),
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.library.ledger.open = false;
                                                this.reset_library_search(false, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(tr!("ledger-tab")),
                                    ),
                            )
                            .child(self.pane_close_button("ledger-close", cx)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(status_filters)
                            .child(proposal_menu)
                            .child(
                                pill("ledger-refresh", tr!("ledger-refresh"), false, cx).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.send_ledger(LedgerAction::List, cx)
                                    }),
                                ),
                            ),
                    )
                    .children(pane.proposal.as_ref().map(|(source, input)| {
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("ledger-proposal-input")
                                    .child(Textarea::new(input).aria_label(source.label())),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        pill("ledger-submit", tr!("ledger-propose"), false, cx)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_ledger_proposal(cx)
                                            })),
                                    )
                                    .child(
                                        pill("ledger-cancel", tr!("ledger-cancel"), false, cx)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.library.ledger.proposal = None;
                                                this.library.ledger.no_proposals = false;
                                                cx.notify();
                                            })),
                                    ),
                            )
                    }))
                    .children(
                        pane.no_proposals
                            .then(|| div().text_sm().child(tr!("ledger-no-proposals"))),
                    )
                    .children(
                        pane.pending
                            .then(|| div().text_sm().child(tr!("ledger-working"))),
                    )
                    .children(
                        pane.error
                            .then(|| div().text_sm().child(tr!("ledger-error"))),
                    ),
            )
            .child(
                div()
                    .id("ledger-rows")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(body),
            )
    }

    fn ledger_row(&self, entry: &LedgerEntry, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let key = entry.key.clone();
        let fetch_key = key.clone();
        let abandon_key = key.clone();
        let expand_key = key.clone();
        let locations = entry
            .locations
            .iter()
            .map(|loc| loc.browser_url())
            .filter(|url| {
                (url.starts_with("https://") || url.starts_with("http://"))
                    && !url.chars().any(char::is_control)
            })
            .collect::<Vec<_>>();
        let can_change = matches!(entry.status, LedgerStatus::Pending | LedgerStatus::AskUser);
        v_flex()
            .id(format!("ledger-row-{key}"))
            .gap_2()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_3()
                    .children(self.trash_button(
                        format!("ledger-delete-{key}"), fuzukue_proto::TrashTarget::Ledger { key: key.clone() },
                        entry.title.clone(), cx,
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(entry.title.clone()),
                            )
                            .child(div().text_sm().text_color(theme.muted_foreground).child(
                                format!(
                                    "{} · {}",
                                    kind_label(entry.kind),
                                    reason_label(&entry.reason)
                                ),
                            )),
                    )
                    .child(
                        div()
                            .w(px(104.))
                            .text_sm()
                            .child(status_label(Some(entry.status))),
                    )
                    .child(
                        div().w(px(102.)).child(
                            pill(
                                format!("ledger-attempts-{key}"),
                                tr!("ledger-attempt-count", count = entry.attempts.len() as i64),
                                self.library.ledger.expanded.contains(&key),
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !this.library.ledger.expanded.remove(&expand_key) {
                                        this.library.ledger.expanded.insert(expand_key.clone());
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(can_change.then(|| {
                        pill(
                            format!("ledger-fetch-{key}"),
                            tr!("ledger-fetch"),
                            false,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_ledger(
                                LedgerAction::Fetch {
                                    key: fetch_key.clone(),
                                },
                                cx,
                            );
                        }))
                    }))
                    .children((!locations.is_empty()).then(|| {
                        Button::new(format!("ledger-browser-{key}"))
                            .small()
                            .outline()
                            .label(tr!("ledger-browser"))
                            .dropdown_menu(move |mut menu, _, _| {
                                for url in &locations {
                                    let url = url.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(url.clone())
                                            .on_click(move |_, _, cx| cx.open_url(&url)),
                                    );
                                }
                                menu
                            })
                    }))
                    .children(can_change.then(|| {
                        pill(
                            format!("ledger-abandon-{key}"),
                            tr!("ledger-abandon"),
                            false,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_ledger(
                                LedgerAction::Abandon {
                                    key: abandon_key.clone(),
                                },
                                cx,
                            );
                        }))
                    })),
            )
            .children(self.library.ledger.expanded.contains(&key).then(|| {
                v_flex()
                    .id(format!("ledger-history-{key}"))
                    .gap_1()
                    .p_2()
                    .bg(theme.muted)
                    .text_sm()
                    .children(entry.acquired.iter().map(|file| {
                        v_flex()
                            .child(file.file.clone())
                            .child(file.source.clone())
                            .child(file.sha256.clone())
                            .child(file.date.clone())
                            .children(file.license.clone())
                    }))
                    .children(
                        entry
                            .attempts
                            .is_empty()
                            .then(|| div().child(tr!("ledger-no-attempts"))),
                    )
                    .children(entry.attempts.iter().rev().map(|attempt| {
                        v_flex()
                            .child(format!(
                                "{} · {}",
                                local_time(&attempt.at),
                                outcome_label(attempt.outcome)
                            ))
                            .child(attempt.location.browser_url())
                            .children(attempt.note.clone())
                    }))
            }))
    }
}

/// Show an RFC 3339 timestamp in local time; fall back to the raw text.
fn local_time(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| at.to_owned())
}
