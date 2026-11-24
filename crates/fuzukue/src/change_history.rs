//! History controls shared by cards, chat turns, and the information panel.

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, AnyElement, Context, SharedString, WeakEntity};
use fuzukue_proto::{
    ChangedTurns, ClientMsg, HistoryOutcome, HistoryRecord, HistoryRequest, HistoryScope, CHANGE_HISTORY_PROTOCOL,
};

use crate::{thread_info::disclosure, Workbench};

#[derive(Default)]
pub(super) struct History {
    next: u64,
    recent_open: bool,
    recent_request: Option<u64>,
    recent: Option<Result<Vec<HistoryRecord>, String>>,
    panel: Option<Panel>,
    changed_turns: ChangedTurns,
}

struct Panel {
    scope: HistoryScope,
    focus_record: Option<String>,
    list_request: Option<u64>,
    visible_records: usize,
    records: Option<Vec<HistoryRecord>>,
    detail: Option<Detail>,
    error: Option<String>,
}

struct Detail {
    record: HistoryRecord,
    request: Option<u64>,
    diff: Option<(String, bool)>,
    conflict: Option<(Vec<String>, Option<String>)>,
}

impl History {
    pub(super) fn close(&mut self) -> bool {
        self.panel.take().is_some()
    }

    pub(super) fn reset(&mut self) {
        let next = self.next;
        *self = Self {
            next,
            ..Default::default()
        };
    }
}

fn actor(actor: &str) -> SharedString {
    if actor == "human" {
        return tr!("history-human").into();
    }
    if actor == "external" {
        return tr!("history-external").into();
    }
    let Some((kind, name)) = actor.split_once(':') else {
        return actor.to_owned().into();
    };
    match kind {
        "agent" => tr!("history-agent", name = name),
        "reviewer" => tr!("history-reviewer", name = name),
        "extension" => tr!("history-extension", name = name),
        _ => actor.to_owned(),
    }
    .into()
}

fn title(record: &HistoryRecord) -> SharedString {
    if record.reverts.is_some() {
        return if record.summary == "restore" {
            tr!("history-restored")
        } else {
            tr!("history-reverted")
        }
        .into();
    }
    if let Some(branch) = &record.branch {
        return if branch.discarded {
            tr!("branch-review-history-discarded", branch = branch.name.clone())
        } else {
            tr!("branch-review-history-merged", branch = branch.name.clone())
        }
        .into();
    }
    if !record.summary.is_empty() {
        return record.summary.clone().into();
    }
    record.files.first().cloned().unwrap_or_default().into()
}

fn time(record: &HistoryRecord) -> String {
    chrono::DateTime::from_timestamp_millis(record.time_ms)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

fn record_button(id: impl Into<SharedString>, record: &HistoryRecord) -> Button {
    Button::new(id.into()).ghost().small().label(format!(
        "{} · {} · {}",
        actor(&record.actor),
        time(record),
        title(record)
    ))
}

impl Workbench {
    fn send_history(&mut self, request: HistoryRequest) -> Option<u64> {
        self.history.next += 1;
        let id = self.history.next;
        self.out.send(ClientMsg::History { id, request }).ok()?;
        Some(id)
    }

    pub(super) fn open_history(&mut self, scope: HistoryScope) {
        let list_request = self.send_history(HistoryRequest::List {
            scope: scope.clone(),
        });
        self.history.panel = Some(Panel {
            scope,
            focus_record: None,
            list_request,
            visible_records: 30,
            records: None,
            detail: None,
            error: None,
        });
    }

    pub(super) fn open_finding_card_edit(&mut self, card: String, record: String) {
        self.open_history(HistoryScope::Card { deck: self.deck.clone(), card });
        if let Some(panel) = &mut self.history.panel {
            panel.focus_record = Some(record);
        }
    }

    pub(super) fn toggle_history(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        if self.history.panel.as_ref().is_some_and(|panel| panel.scope == scope) {
            self.history.close();
        } else {
            self.open_history(scope);
        }
        cx.notify();
    }

    fn open_history_record(&mut self, record: HistoryRecord) {
        let request = self.send_history(HistoryRequest::Diff {
            record: record.id.clone(),
        });
        if let Some(panel) = &mut self.history.panel {
            panel.error = None;
            panel.detail = Some(Detail {
                record,
                request,
                diff: None,
                conflict: None,
            });
        }
    }

    fn refresh_recent_history(&mut self) {
        if self.protocol >= CHANGE_HISTORY_PROTOCOL && self.history.recent_open {
            self.history.recent_request = self.send_history(HistoryRequest::List {
                scope: HistoryScope::Global,
            });
        }
    }

    pub(super) fn refresh_history(&mut self) {
        self.refresh_recent_history();
        if let Some(scope) = self.history.panel.as_ref().map(|panel| panel.scope.clone()) {
            let request = self.send_history(HistoryRequest::List { scope });
            self.history.panel.as_mut().unwrap().list_request = request;
        }
    }

    pub(super) fn history_result(&mut self, id: u64, outcome: HistoryOutcome) {
        if self.history.recent_request == Some(id) {
            self.history.recent_request = None;
            self.history.recent = match outcome {
                HistoryOutcome::Records { records } => Some(Ok(records)),
                HistoryOutcome::Error { message } => Some(Err(message)),
                _ => None,
            };
            return;
        }
        let Some(panel) = &mut self.history.panel else {
            return;
        };
        if panel.list_request == Some(id) {
            panel.list_request = None;
            match outcome {
                HistoryOutcome::Records { records } => {
                    let first = if let Some(id) = panel.focus_record.take() {
                        let record = records.iter().find(|record| record.id == id).cloned();
                        if record.is_none() {
                            panel.error = Some(tr!("findings-change-unavailable").into());
                        }
                        record
                    } else {
                        (panel.detail.is_none() && matches!(panel.scope, HistoryScope::Turn { .. }))
                            .then(|| records.first().cloned())
                            .flatten()
                    };
                    panel.records = Some(records);
                    if let Some(record) = first {
                        self.open_history_record(record);
                    }
                }
                HistoryOutcome::Error { message } => panel.error = Some(message),
                _ => {}
            }
        } else if let Some(detail) = panel
            .detail
            .as_mut()
            .filter(|detail| detail.request == Some(id))
        {
            detail.request = None;
            match outcome {
                HistoryOutcome::Diff { text, truncated } => detail.diff = Some((text, truncated)),
                HistoryOutcome::Conflict { hunks, clean_only } => {
                    detail.conflict = Some((hunks, clean_only))
                }
                HistoryOutcome::Done => {
                    panel.detail = None;
                    self.refresh_history();
                }
                HistoryOutcome::Error { message } => panel.error = Some(message),
                _ => {}
            }
        }
    }

    fn history_operation(&mut self, request: HistoryRequest) {
        let request = self.send_history(request);
        if let Some(panel) = &mut self.history.panel {
            panel.error = None;
            if let Some(detail) = panel.detail.as_mut() {
                detail.request = request;
                detail.conflict = None;
            }
        }
    }

    pub(super) fn history_changed(&mut self, changed_turns: Option<ChangedTurns>, cx: &mut Context<Self>) {
        if let Some(changed_turns) = changed_turns {
            let old = std::mem::replace(&mut self.history.changed_turns, changed_turns);
            let empty = Default::default();
            for (session, chat) in &self.chats {
                let before = old.get(session).unwrap_or(&empty);
                let after = self.history.changed_turns.get(session).unwrap_or(&empty);
                chat.remeasure_history(before.symmetric_difference(after).map(String::as_str));
            }
            cx.notify();
        }
        self.refresh_history();
    }

    pub(super) fn has_turn_changes(&self, session: &str, turn: &str) -> bool {
        self.history.changed_turns.get(session).is_some_and(|turns| turns.contains(turn))
    }

    pub(super) fn history_button(
        &self,
        id: impl Into<SharedString>,
        scope: HistoryScope,
        entity: WeakEntity<Self>,
    ) -> Option<Button> {
        (self.protocol >= CHANGE_HISTORY_PROTOCOL).then(|| {
            let button = Button::new(id.into()).small().ghost();
            let button = if matches!(scope, HistoryScope::Turn { .. }) {
                let label = tr!("history-turn");
                button.icon(crate::icons::Symbol::FileDiff.icon()).accessibility_label(label).tooltip(label)
            } else {
                button.label(tr!("history-title"))
            };
            button.on_click(move |_, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.toggle_history(scope.clone(), cx);
                });
            })
        })
    }

    pub(super) fn card_history_button(
        &self,
        id: String,
        card: &str,
        entity: WeakEntity<Self>,
    ) -> Option<Button> {
        self.history_button(
            id,
            HistoryScope::Card {
                deck: self.deck.clone(),
                card: card.into(),
            },
            entity,
        )
    }

    pub(super) fn render_recent_history(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.protocol < CHANGE_HISTORY_PROTOCOL {
            return None;
        }
        let mut section = v_flex().gap_1().items_start().child(
            disclosure(
                "recent-history-toggle",
                tr!("history-recent").into(),
                self.history.recent_open,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.history.recent_open = !this.history.recent_open;
                this.refresh_recent_history();
                cx.notify();
            })),
        );
        if self.history.recent_open {
            section = match &self.history.recent {
                Some(Ok(records)) if !records.is_empty() => section
                    .children(records.iter().take(5).enumerate().map(|(index, record)| {
                        let value = record.clone();
                        record_button(format!("recent-history-{index}"), record).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.open_history(HistoryScope::Global);
                                this.open_history_record(value.clone());
                                cx.notify();
                            }),
                        )
                    }))
                    .child(
                        self.history_button(
                            "recent-history-all",
                            HistoryScope::Global,
                            cx.entity().downgrade(),
                        )
                        .unwrap(),
                    ),
                Some(Ok(_)) => section.child(tr!("history-empty")),
                Some(Err(message)) => {
                    section.child(tr!("history-error", message = message.clone()))
                }
                None => section.child(tr!("history-loading")),
            };
        }
        Some(section.into_any_element())
    }

    pub(super) fn render_history(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let panel = self.history.panel.as_ref()?;
        let theme = cx.theme();
        let heading = match panel.scope {
            HistoryScope::Global => tr!("history-recent"),
            HistoryScope::Card { .. } => tr!("history-title"),
            HistoryScope::Turn { .. } => tr!("history-turn"),
        };
        let header = h_flex()
            .id("history-pane-header")
            .justify_between()
            .gap_2()
            .child(div().id("history-pane-title").font_bold().child(heading))
            .child(self.pane_close_button("history-close", cx));
        let mut list = v_flex().gap_1().items_start();
        match &panel.records {
            None if panel.error.is_none() => list = list.child(tr!("history-loading")),
            None => {}
            Some(records) if records.is_empty() => list = list.child(tr!("history-empty")),
            Some(records) => {
                list = list.children(records.iter().take(panel.visible_records).enumerate().map(
                    |(index, record)| {
                        let value = record.clone();
                        let selected = panel
                            .detail
                            .as_ref()
                            .is_some_and(|detail| detail.record.id == record.id);
                        record_button(format!("history-record-{index}"), record)
                            .disabled(
                                selected
                                    || panel
                                        .detail
                                        .as_ref()
                                        .is_some_and(|detail| detail.request.is_some()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_history_record(value.clone());
                                cx.notify();
                            }))
                    },
                ));
                if records.len() > panel.visible_records {
                    list = list.child(
                        Button::new("history-more")
                            .small()
                            .ghost()
                            .label(tr!("history-more"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(panel) = &mut this.history.panel {
                                    panel.visible_records += 30;
                                }
                                cx.notify();
                            })),
                    );
                }
            }
        }
        let mut body = v_flex().gap_3();
        if let Some(message) = &panel.error {
            body = body.child(
                div()
                    .text_color(theme.danger)
                    .child(tr!("history-error", message = message.clone())),
            );
        }
        if let Some(detail) = &panel.detail {
            let busy = detail.request.is_some();
            let id = detail.record.id.clone();
            let request = HistoryRequest::Revert {
                record: id.clone(),
                clean_only: None,
            };
            let mut actions = h_flex().gap_2().flex_wrap().when(!detail.record.files.is_empty(), |row| row.child(
                Button::new("history-revert")
                    .small()
                    .outline()
                    .label(tr!("history-revert"))
                    .disabled(busy || detail.conflict.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.history_operation(request.clone());
                        cx.notify();
                    })),
            ));
            if let HistoryScope::Card { deck, card } = &panel.scope {
                let request = HistoryRequest::RestoreCard {
                    record: id.clone(),
                    deck: deck.clone(),
                    card: card.clone(),
                };
                actions = actions.child(
                    Button::new("history-restore")
                        .small()
                        .outline()
                        .label(tr!("history-restore"))
                        .disabled(busy || detail.conflict.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.history_operation(request.clone());
                            cx.notify();
                        })),
                );
            }
            body = body.child(actions).child(
                v_flex().text_xs().children(
                    detail
                        .record
                        .files
                        .iter()
                        .map(|path| div().child(path.clone())),
                ),
            );
            if let Some(branch) = detail.record.branch.as_ref().filter(|branch| branch.discarded) {
                body = body.child(div().text_xs().whitespace_normal().child(
                    tr!("branch-review-restore-tip", name = format!("'{}'", branch.name.replace('\'', "'\\''")), tip = branch.tip.clone()),
                ));
            }
            if let Some((hunks, token)) = &detail.conflict {
                body = body
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(tr!("history-conflict")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .children(token.as_ref().map(|token| {
                                let request = HistoryRequest::Revert {
                                    record: id,
                                    clean_only: Some(token.clone()),
                                };
                                Button::new("history-clean-only")
                                    .small()
                                    .outline()
                                    .label(tr!("history-clean-only"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.history_operation(request.clone());
                                        cx.notify();
                                    }))
                            }))
                            .child(
                                Button::new("history-cancel")
                                    .small()
                                    .ghost()
                                    .label(tr!("history-cancel"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(detail) = this
                                            .history
                                            .panel
                                            .as_mut()
                                            .and_then(|p| p.detail.as_mut())
                                        {
                                            detail.conflict = None;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .children(hunks.iter().map(|hunk| crate::unified_diff::render(hunk, cx)));
            } else if let Some((text, truncated)) = &detail.diff {
                body = body.child(
                    div()
                        .id("history-diff")
                        .w_full()
                        .overflow_x_scroll()
                        .child(crate::unified_diff::render(text, cx)),
                );
                if *truncated {
                    body = body.child(tr!("history-truncated"));
                }
            }
        }
        Some(
            v_flex()
                .id("history-panel")
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w_full()
                .max_w(px(600.))
                .min_h_0()
                .pl(crate::pane_resize::PANE_INSET - px(1.))
                .pr(crate::pane_resize::PANE_INSET)
                .py_4()
                .gap_3()
                .bg(theme.background)
                .border_l_1()
                .border_color(theme.sidebar_border)
                .shadow_lg()
                .text_sm()
                .occlude()
                .child(header)
                .child(
                    div()
                        .id("history-list")
                        .max_h(px(185.))
                        .overflow_y_scrollbar()
                        .child(list),
                )
                .child(
                    div()
                        .id("history-detail")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(body),
                )
                .into_any_element(),
        )
    }
}
