use fuzukue_proto::{
    ClientMsg, WorkspaceRequestAction as Action, WorkspaceRequestDestination as Destination,
    WorkspaceRequestReply as Reply, WorkspaceRequestUpdateKind as Update,
    WorkspaceRequestsView as View,
};
use gpui_kit::{
    AnyElement, Context, Div, SharedString,
    base::{Disableable as _, StyledExt as _},
    component::{
        ActiveTheme as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        scroll::ScrollableElement as _,
        v_flex,
    },
    div,
    prelude::*,
    px,
};

use crate::Workbench;

#[derive(Default)]
pub(super) struct WorkspaceRequests {
    pub(super) available: bool,
    pub(super) view: Option<View>,
    selected_inbox: Option<String>,
    show_updates: bool,
}

impl Workbench {
    pub(super) fn render_workspace_request_chat(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let requests = &self.workspace_requests;
        let view = requests.view.as_ref()?;
        let updates: Vec<_> = view
            .updates
            .iter()
            .filter(|update| update.session == self.active)
            .collect();
        let draft = view
            .drafts
            .iter()
            .find(|draft| draft.session == self.active);
        if updates.is_empty() && draft.is_none() {
            return None;
        }

        let theme = cx.theme().clone();
        let mut section = crate::chat::chat_column().flex_none().min_w_0().gap_1();
        if let Some(latest) = updates.last() {
            let shown = if requests.show_updates {
                updates.as_slice()
            } else {
                std::slice::from_ref(latest)
            };
            let toggle = (updates.len() > 1).then(|| {
                Button::new("workspace-request-updates-toggle")
                    .ghost()
                    .xsmall()
                    .label(if requests.show_updates {
                        tr!("workspace-request-latest")
                    } else {
                        tr!("workspace-request-history")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_requests.show_updates =
                            !this.workspace_requests.show_updates;
                        cx.notify();
                    }))
            });
            section = section.child(
                v_flex()
                    .id("workspace-request-updates")
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .children(toggle)
                    .child(
                        v_flex()
                            .id("workspace-request-updates-scroll")
                            .max_h(px(112.))
                            .overflow_y_scrollbar()
                            .gap_2()
                            .children(shown.iter().enumerate().map(|(index, update)| {
                                let id = if requests.show_updates {
                                    gpui_kit::ElementId::from(("workspace-request-update", index))
                                } else {
                                    update_id(update.kind).into()
                                };
                                v_flex()
                                    .id(id)
                                    .min_w_0()
                                    .gap_1()
                                    .child(plain(format!(
                                        "{} · {}",
                                        update_label(update.kind),
                                        update.title
                                    )))
                                    .children(update.body.as_ref().map(|body| {
                                        let body = if update.external
                                            && !matches!(update.kind, Update::Request)
                                        {
                                            tr!(
                                                "workspace-request-external-reply",
                                                body = body.as_str()
                                            )
                                        } else {
                                            body.clone()
                                        };
                                        plain(body).text_color(theme.foreground)
                                    }))
                            })),
                    ),
            );
        }
        if let Some(draft) = draft {
            let Destination::Workspace { id, title } = &draft.destination;
            let revision = draft.revision.clone();
            let label = match draft.reply {
                None => tr!("workspace-request-preview"),
                Some(Reply::Question) => tr!("workspace-request-preview-question"),
                Some(Reply::Answer) => tr!("workspace-request-preview-answer"),
                Some(Reply::Result) => tr!("workspace-request-preview-result"),
            };
            section = section.child(
                v_flex()
                    .id("workspace-request-preview")
                    .flex_none()
                    .min_w_0()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .rounded_md()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(div().font_semibold().text_sm().child(label))
                    .child(
                        v_flex()
                            .id("workspace-request-preview-scroll")
                            .max_h(px(205.))
                            .overflow_y_scrollbar()
                            .min_w_0()
                            .gap_1()
                            .text_sm()
                            .child(
                                plain(tr!(
                                    "workspace-request-destination",
                                    title = title.as_str(),
                                    id = id.as_str()
                                ))
                                .id("workspace-request-to"),
                            )
                            .child(
                                plain(draft.title.clone())
                                    .id("workspace-request-title")
                                    .font_semibold(),
                            )
                            .child(plain(draft.body.clone()).id("workspace-request-body"))
                            .child(
                                div()
                                    .id("workspace-request-no-attachments")
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(tr!("workspace-request-no-attachments")),
                            ),
                    )
                    .child(crate::chat::approval_decision_buttons(
                        "workspace-request",
                        tr!("workspace-request-cancel"),
                        tr!("workspace-request-send"),
                        &cx.entity(),
                        move |send| ClientMsg::WorkspaceRequest {
                            action: Action::Decide {
                                revision: revision.clone(),
                                send,
                            },
                        },
                    )),
            );
        }
        Some(section.into_any_element())
    }

    pub(super) fn render_workspace_request_inbox(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let requests = &self.workspace_requests;
        if !requests.available {
            return None;
        }
        let theme = cx.theme().clone();
        let mut section = v_flex()
            .id("workspace-request-inbox")
            .min_w_0()
            .gap_2()
            .pb_3();
        let Some(view) = &requests.view else {
            return Some(
                section
                    .child(
                        plain(tr!("workspace-request-credentials"))
                            .text_sm()
                            .text_color(theme.muted_foreground),
                    )
                    .into_any_element(),
            );
        };
        let auto_start = view.auto_start;
        let out = self.out.clone();
        section = section
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("workspace-request-inbox-title")),
                    )
                    .child(
                        Button::new("workspace-request-auto-start")
                            .secondary()
                            .xsmall()
                            .label(if auto_start {
                                tr!("workspace-request-queue-mode")
                            } else {
                                tr!("workspace-request-desk-mode")
                            })
                            .on_click(move |_, _, _| {
                                let _ = out.send(ClientMsg::WorkspaceRequest {
                                    action: Action::SetAutoStart {
                                        enabled: !auto_start,
                                    },
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if auto_start {
                        tr!("workspace-request-policy-auto")
                    } else {
                        tr!("workspace-request-policy-queue")
                    }),
            );

        for (index, item) in view.inbox.iter().rev().enumerate() {
            let id = item.id.clone();
            let open = requests.selected_inbox.as_deref() == Some(&id);
            let status = if item.complete {
                tr!("workspace-request-complete")
            } else if item.starting {
                tr!("workspace-request-starting")
            } else if item.failed {
                tr!("workspace-request-start-failed")
            } else if item.session.is_some() {
                tr!("workspace-request-started")
            } else {
                tr!("workspace-request-queued")
            };
            let label = format!("{} · {status}", item.title);
            let mut row = v_flex().min_w_0().gap_2().child(
                Button::new(SharedString::from(format!(
                    "workspace-request-inbox-{index}"
                )))
                .ghost()
                .small()
                .max_w_full()
                .accessibility_label(label.clone())
                .child(div().min_w_0().truncate().child(label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let selection = &mut this.workspace_requests.selected_inbox;
                    *selection = (selection.as_deref() != Some(&id)).then(|| id.clone());
                    cx.notify();
                })),
            );
            if open {
                let id = item.id.clone();
                let out = self.out.clone();
                row = row
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(tr!("workspace-request-external")),
                    )
                    .child(
                        plain(item.body.clone())
                            .id("workspace-request-inbox-body")
                            .text_sm(),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children((item.session.is_none() || item.failed).then(|| {
                                Button::new("workspace-request-start")
                                    .primary()
                                    .xsmall()
                                    .disabled(item.starting)
                                    .label(if item.failed {
                                        tr!("workspace-request-retry")
                                    } else {
                                        tr!("workspace-request-start")
                                    })
                                    .on_click(move |_, _, _| {
                                        let _ = out.send(ClientMsg::WorkspaceRequest {
                                            action: Action::Start { id: id.clone() },
                                        });
                                    })
                            }))
                            .children(item.session.clone().map(|session| {
                                Button::new("workspace-request-open-thread")
                                    .secondary()
                                    .xsmall()
                                    .label(tr!("workspace-request-open-thread"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_thread(session.clone(), cx);
                                    }))
                            })),
                    );
            }
            section = section.child(row.p_2().border_1().border_color(theme.border).rounded_md());
        }
        Some(section.into_any_element())
    }
}

fn plain(text: impl Into<SharedString>) -> Div {
    div().min_w_0().whitespace_normal().child(text.into())
}

fn update_label(kind: Update) -> &'static str {
    match kind {
        Update::Request => tr!("workspace-request-external"),
        Update::Received => tr!("workspace-request-received"),
        Update::Started => tr!("workspace-request-started"),
        Update::StartFailed => tr!("workspace-request-start-failed"),
        Update::Question => tr!("workspace-request-question"),
        Update::Answer => tr!("workspace-request-answer"),
        Update::Result => tr!("workspace-request-result"),
    }
}

fn update_id(kind: Update) -> &'static str {
    match kind {
        Update::Request => "workspace-request-update-request",
        Update::Received => "workspace-request-update-received",
        Update::Started => "workspace-request-update-started",
        Update::StartFailed => "workspace-request-update-failed",
        Update::Question => "workspace-request-update-question",
        Update::Answer => "workspace-request-update-answer",
        Update::Result => "workspace-request-update-result",
    }
}
