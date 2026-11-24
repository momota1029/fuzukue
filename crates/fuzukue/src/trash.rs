use gpui_kit::{
    component::{
        button::{Button, ButtonVariants},
        h_flex,
        scroll::ScrollableElement as _,
        v_flex, ActiveTheme, Sizable as _, WindowExt,
    },
    div,
    prelude::*,
    px, Context, IntoElement, Window,
};
use fuzukue_proto::{ClientMsg, TrashAction, TrashItem, TrashKind, TrashTarget, CLOSE_TRASH_PROTOCOL};

use crate::{icons::Symbol, RightPane, Workbench};

fn kind_label(kind: TrashKind) -> &'static str {
    match kind {
        TrashKind::Thread => tr!("trash-kind-thread"),
        TrashKind::Memo => tr!("library-memo"),
        TrashKind::Ledger => tr!("ledger-tab"),
        TrashKind::Submission => tr!("trash-kind-submission"),
        TrashKind::Report => tr!("trash-kind-report"),
        TrashKind::Comment => tr!("trash-kind-comment"),
        TrashKind::Extension => tr!("trash-kind-extension"),
    }
}

impl Workbench {
    pub(super) fn open_trash(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.trash_error = None;
        self.right = RightPane::Trash;
        self.refresh_trash();
        cx.notify();
    }

    pub(super) fn refresh_trash(&self) {
        if self.right == RightPane::Trash {
            let _ = self.out.send(ClientMsg::Trash {
                action: TrashAction::List,
            });
        }
    }

    pub(super) fn confirm_removal(
        &mut self,
        message: ClientMsg,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, question, confirm) = match &message {
            ClientMsg::Trash {
                action: TrashAction::Purge { .. },
            } => (
                tr!("trash-purge"),
                tr!("trash-confirm-purge", label = label),
                tr!("trash-purge"),
            ),
            ClientMsg::RemoveWorkspace { .. } => (
                tr!("trash-workspace-remove"),
                tr!("trash-confirm-workspace", label = label),
                tr!("trash-workspace-remove"),
            ),
            _ => (
                tr!("trash-delete"),
                tr!("trash-confirm-delete", label = label),
                tr!("trash-delete"),
            ),
        };
        let view = cx.weak_entity();
        let epoch = self.connection_epoch;
        window.open_dialog(cx, move |dialog, _, _| {
            let message = message.clone();
            let view = view.clone();
            dialog
                .title(title)
                .overlay_closable(false)
                .w(px(440.))
                .child(
                    v_flex()
                        .id("trash-confirmation")
                        .gap_4()
                        .child(question.clone())
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("trash-cancel")
                                        .label(tr!("trash-cancel"))
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("trash-confirm")
                                        .danger()
                                        .label(confirm)
                                        .on_click(move |_, window, cx| {
                                            if let Some(view) = view.upgrade() {
                                                let this = view.read(cx);
                                                if this.connection_epoch == epoch {
                                                    let _ = this.out.send(message.clone());
                                                }
                                            }
                                            window.close_dialog(cx);
                                        }),
                                ),
                        ),
                )
        });
    }

    pub(super) fn trash_button(
        &self,
        id: impl Into<gpui_kit::ElementId>,
        target: TrashTarget,
        label: String,
        cx: &Context<Self>,
    ) -> Option<Button> {
        (self.protocol >= CLOSE_TRASH_PROTOCOL).then(|| {
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(Symbol::Trash.icon())
                .tooltip(tr!("trash-delete"))
                .accessibility_label(tr!("trash-delete"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.confirm_removal(
                        ClientMsg::Trash {
                            action: TrashAction::Delete {
                                target: target.clone(),
                            },
                        },
                        label.clone(),
                        window,
                        cx,
                    );
                }))
        })
    }

    pub(super) fn render_trash(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let items = self.trash_items.as_deref();
        v_flex()
            .id("trash-view")
            .size_full()
            .min_w_0()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(860.))
                    .mx_auto()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_5()
                    .gap_3()
                    .child(
                        h_flex().justify_between()
                            .child(div().text_lg()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD).child(tr!("trash-title")))
                            .child(self.pane_close_button("trash-close", cx)),
                    )
                    .children(match items {
                        None => Some(tr!("trash-loading")),
                        Some([]) => Some(tr!("trash-empty")),
                        Some(_) => None,
                    })
                    .children(items.into_iter().flatten().map(|item: &TrashItem| {
                        let restore_id = item.id.clone();
                        let purge_id = item.id.clone();
                        let source_id = item.source_id.as_deref().unwrap_or(&item.label);
                        let title = item.title.as_deref().unwrap_or(source_id);
                        let label = title.to_owned();
                        let expiry = chrono::DateTime::from_timestamp(item.expires_at, 0)
                            .map(|date| {
                                date.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        h_flex()
                            .gap_3()
                            .min_w_0()
                            .border_b_1()
                            .border_color(theme.sidebar_border)
                            .py_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .id(format!("trash-item-title-{}", item.id))
                                            .truncate()
                                            .child(title.to_owned()),
                                    )
                                    .children((source_id != title).then(|| {
                                        div().text_sm().text_color(theme.muted_foreground)
                                            .truncate().child(source_id.to_owned())
                                    }))
                                    .child(
                                        div().text_sm().text_color(theme.muted_foreground).child(
                                            tr!(
                                                "trash-item-detail",
                                                kind = kind_label(item.kind),
                                                date = expiry
                                            ),
                                        ),
                                    ),
                            )
                            .child(
                                Button::new(format!("trash-restore-{}", item.id))
                                    .small()
                                    .label(tr!("trash-restore"))
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        let _ = this.out.send(ClientMsg::Trash {
                                            action: TrashAction::Restore {
                                                id: restore_id.clone(),
                                            },
                                        });
                                    })),
                            )
                            .child(
                                Button::new(format!("trash-purge-{}", item.id))
                                    .small()
                                    .ghost()
                                    .label(tr!("trash-purge"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_removal(
                                            ClientMsg::Trash {
                                                action: TrashAction::Purge {
                                                    id: purge_id.clone(),
                                                },
                                            },
                                            label.clone(),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                    })),
            )
    }
}
