use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{
    base::StyledExt as _,
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        text::TextView,
        v_flex, ActiveTheme as _, WindowExt as _,
    },
    div,
    prelude::*,
    px, Context, Div, Entity, IntoElement, SharedString, Window,
};
use fuzukue_proto::{
    ClientMsg, ExtensionFileChange, ExtensionInfo, ExtensionNode, EXTENSIONS_PROTOCOL,
};

use super::{appearance::settings_switch, RightPane, Workbench};

#[derive(Default)]
pub(super) struct Extensions {
    pub items: Vec<ExtensionInfo>,
    seen_prompts: BTreeSet<String>,
    dialog_open: bool,
    pub active: Option<String>,
    request: u64,
    view: Option<Result<Vec<ExtensionNode>, String>>,
    inputs: BTreeMap<String, (Entity<InputState>, String)>,
    tabs: BTreeMap<String, usize>,
    pub error: Option<String>,
    pub update: Option<(String, Option<Vec<ExtensionFileChange>>)>,
}

fn element_id(prefix: &str, id: &str) -> SharedString {
    format!("extension-{prefix}-{id}").into()
}

fn permissions(item: &ExtensionInfo, cx: &gpui_kit::App) -> Div {
    let theme = cx.theme();
    let permissions = &item.permissions;
    let has_permission = !permissions.read.is_empty()
        || permissions.write.is_some()
        || !permissions.network.is_empty()
        || permissions.agent_tools
        || permissions.log;
    let mut rows = Vec::new();
    let private = format!(".fuzukue/extensions/{}/", item.name);
    let mut add = |label, scope: Option<&str>, broad| {
        rows.push(
            v_flex()
                .gap_0p5()
                .px_2()
                .py_1()
                .rounded_md()
                .when(broad, |row| {
                    row.bg(theme.muted).border_1().border_color(theme.warning)
                })
                .child(div().text_sm().child(label))
                .children(scope.map(|scope| div().text_xs().child(scope.to_owned())))
                .children(broad.then(|| {
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(tr!("extension-broad"))
                })),
        );
    };
    for scope in &permissions.read {
        let broad = scope
            .split('/')
            .next()
            .is_some_and(|head| head.contains(['*', '?', '[']));
        add(
            tr!("extension-read"),
            Some(scope),
            broad || scope.contains("**"),
        );
    }
    for scope in permissions.write.iter().flatten() {
        add(
            tr!("extension-write"),
            Some(scope),
            !scope.starts_with(&private),
        );
    }
    for host in &permissions.network {
        add(tr!("extension-network"), Some(host), true);
    }
    if permissions.agent_tools {
        add(tr!("extension-agent-tools"), None, true);
    }
    if permissions.log {
        add(tr!("extension-log"), None, false);
    }
    v_flex()
        .gap_1()
        .children(rows)
        .children((!has_permission).then(|| tr!("extension-no-permissions")))
}

impl Workbench {
    pub(super) fn extension_items(&mut self, items: Vec<ExtensionInfo>) {
        if self.extensions.active.as_ref().is_some_and(|id| {
            !items
                .iter()
                .any(|item| item.id == *id && (item.enabled || item.error.is_some()))
        }) {
            self.extensions.active = None;
            self.extensions.view = None;
            self.extensions.inputs.clear();
            if self.right == RightPane::Extension {
                self.right = RightPane::None;
            }
        }
        if let Some(error) = items
            .iter()
            .find(|item| Some(&item.id) == self.extensions.active.as_ref())
            .and_then(|item| item.error.clone())
        {
            self.extensions.view = Some(Err(error));
        }
        self.extensions.items = items;
    }

    pub(super) fn extension_prompt(
        &mut self,
        item: ExtensionInfo,
        automatic: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(approval) = item.approval.clone() else {
            return;
        };
        if self.extensions.dialog_open
            || (automatic && self.extensions.seen_prompts.contains(&approval))
        {
            return;
        }
        self.extensions.seen_prompts.insert(approval.clone());
        self.extensions.dialog_open = true;
        self.extensions.error = None;
        let state = cx.entity().downgrade();
        let epoch = self.connection_epoch;
        window.open_dialog(cx, move |dialog, _, cx| {
            let state_close = state.clone();
            let state_approve = state.clone();
            let state_later = state.clone();
            let id = item.id.clone();
            let approval = approval.clone();
            dialog
                .title(tr!("extension-permission-title"))
                .overlay_closable(false)
                .w(px(496.))
                .on_close(move |_, _, cx| {
                    let _ = state_close.update(cx, |this, cx| {
                        this.extensions.dialog_open = false;
                        cx.notify();
                    });
                })
                .child(
                    v_flex()
                        .id("extension-permission-dialog")
                        .gap_3()
                        .child(
                            div()
                                .font_semibold()
                                .child(format!("{} · {}", item.title, item.version)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(item.source.clone()),
                        )
                        .child(tr!("extension-permission-summary"))
                        .child(
                            div()
                                .id("extension-permissions-scroll")
                                .max_h(px(260.))
                                .overflow_y_scrollbar()
                                .child(permissions(&item, cx)),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("extension-later")
                                        .label(tr!("extension-later"))
                                        .on_click(move |_, window, cx| {
                                            let _ = state_later.update(cx, |this, _| {
                                                this.extensions.dialog_open = false
                                            });
                                            window.close_dialog(cx);
                                        }),
                                )
                                .child(
                                    Button::new("extension-approve")
                                        .primary()
                                        .label(tr!("extension-approve"))
                                        .on_click(move |_, window, cx| {
                                            let _ = state_approve.update(cx, |this, _| {
                                                if this.connection_epoch == epoch {
                                                    let _ =
                                                        this.out.send(ClientMsg::GrantExtension {
                                                            id: id.clone(),
                                                            approval: approval.clone(),
                                                        });
                                                }
                                                this.extensions.dialog_open = false;
                                            });
                                            window.close_dialog(cx);
                                        }),
                                ),
                        ),
                )
        });
    }

    pub(super) fn clear_extensions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.extensions.dialog_open {
            window.close_dialog(cx);
        }
        let request = self.extensions.request.wrapping_add(1);
        self.extensions = Extensions {
            request,
            ..Extensions::default()
        };
        if self.right == RightPane::Extension {
            self.right = RightPane::None;
        }
    }

    pub(super) fn open_extension(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.right == RightPane::Extension
            && self.extensions.active.as_ref() == Some(&id)
            && !self.settings_open
        {
            self.close_right_pane(cx);
            return;
        }
        self.close_palette(window, cx);
        self.settings_open = false;
        self.pending_paper_open = None;
        self.right = RightPane::Extension;
        self.extensions.active = Some(id);
        self.extensions.view = None;
        self.extensions.inputs.clear();
        self.extensions.tabs.clear();
        self.request_extension_view(None);
        cx.notify();
    }

    pub(super) fn refresh_extension_locale(&mut self) {
        if self.protocol >= EXTENSIONS_PROTOCOL {
            let _ = self.out.send(ClientMsg::ListExtensions);
            if self.right == RightPane::Extension {
                self.request_extension_view(None);
            }
        }
    }

    pub(super) fn request_extension_view(&mut self, event: Option<(String, Option<String>)>) {
        let Some(id) = self.extensions.active.clone() else {
            return;
        };
        self.extensions.request = self.extensions.request.wrapping_add(1);
        let request = self.extensions.request;
        let message = match event {
            Some((element, value)) => ClientMsg::ExtensionUiEvent {
                id,
                request,
                element,
                value,
            },
            None => ClientMsg::RenderExtension { id, request },
        };
        if self.out.send(message).is_err() {
            self.extensions.view = Some(Err(tr!("extension-server-unavailable").into()));
        }
    }

    pub(super) fn extension_view(
        &mut self,
        id: String,
        request: u64,
        view: Result<Vec<ExtensionNode>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.extensions.request != request || self.extensions.active.as_ref() != Some(&id) {
            return;
        }
        let mut inputs = BTreeMap::new();
        if let Ok(nodes) = &view {
            Self::extension_inputs(nodes, &mut self.extensions.inputs, &mut inputs, window, cx);
        }
        self.extensions.inputs = inputs;
        self.extensions.view = Some(view);
    }

    fn extension_inputs(
        nodes: &[ExtensionNode],
        previous: &mut BTreeMap<String, (Entity<InputState>, String)>,
        inputs: &mut BTreeMap<String, (Entity<InputState>, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for node in nodes {
            match node {
                ExtensionNode::Input { id, value, .. } => {
                    let (state, _) = previous
                        .remove(id)
                        .filter(|(_, old)| old == value)
                        .unwrap_or_else(|| {
                            let state = cx
                                .new(|cx| InputState::new(window, cx).default_value(value.clone()));
                            let element = id.clone();
                            cx.subscribe_in(
                                &state,
                                window,
                                move |this, state, event: &InputEvent, _, cx| {
                                    if let InputEvent::PressEnter { .. } = event {
                                        let value = state.read(cx).value().to_string();
                                        this.request_extension_view(Some((
                                            element.clone(),
                                            Some(value),
                                        )));
                                    }
                                },
                            )
                            .detach();
                            (state, value.clone())
                        });
                    inputs.insert(id.clone(), (state, value.clone()));
                }
                ExtensionNode::Tabs { tabs, .. } => {
                    for tab in tabs {
                        Self::extension_inputs(&tab.children, previous, inputs, window, cx);
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn render_extension_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let section = v_flex()
            .id("extensions-settings")
            .gap_2()
            .child(div().font_semibold().child(tr!("extensions-title")))
            .children(self.extensions.error.as_ref().map(|error| {
                div()
                    .text_color(theme.danger)
                    .child(tr!("extension-error", detail = error.clone()))
            }))
            .children(
                self.extensions
                    .items
                    .is_empty()
                    .then(|| div().text_sm().child(tr!("extensions-empty"))),
            );
        let section = section.children(self.extensions.items.iter().map(|item| {
            let id = item.id.clone();
            let update_id = id.clone();
            let review = item.clone();
            v_flex()
                .p_2()
                .gap_1()
                .border_1()
                .rounded_md()
                .border_color(theme.border)
                .child(div().font_semibold().child(item.title.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} · {}", item.version, item.source)),
                )
                .children(
                    (!item.description.is_empty())
                        .then(|| div().text_sm().child(item.description.clone())),
                )
                .child(permissions(item, cx))
                .children(item.error.as_ref().map(|error| {
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(tr!("extension-error", detail = error.clone()))
                }))
                .children((item.approval.is_none() && item.error.is_none()).then(|| {
                    settings_switch(
                        element_id("toggle", &item.name), tr!("extension-enable"), item.enabled, false, cx,
                        cx.listener(move |this, checked, _, _| {
                            let _ = this.out.send(ClientMsg::SetExtensionEnabled {
                                id: id.clone(),
                                enabled: *checked,
                            });
                        }),
                    )
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(self.trash_button(
                            element_id("delete", &item.name),
                            fuzukue_proto::TrashTarget::Extension { id: item.id.clone() },
                            item.title.clone(), cx,
                        ))
                        .children(item.approval.as_ref().map(|_| {
                            Button::new(element_id("review", &item.name))
                                .label(tr!("extension-review"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.extension_prompt(review.clone(), false, window, cx)
                                }))
                        }))
                        .child(
                            Button::new(element_id("update", &item.name))
                                .label(tr!("extension-update"))
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.extensions.error = None;
                                    let _ = this.out.send(ClientMsg::UpdateExtension {
                                        id: update_id.clone(),
                                    });
                                })),
                        ),
                )
        }));
        section.children(self.extensions.update.as_ref().map(|(id, changes)| {
            let title = self
                .extensions
                .items
                .iter()
                .find(|item| &item.id == id)
                .map(|item| item.title.as_str())
                .unwrap_or_default();
            let summary = tr!("extension-updated", name = title);
            match changes {
                Some(files) if files.is_empty() => {
                    v_flex().child(summary).child(tr!("extension-no-changes"))
                }
                Some(files) => v_flex().child(summary).children(files.iter().map(|file| {
                    let status = match file.status.as_str() {
                        "added" => tr!("extension-file-added"),
                        "deleted" => tr!("extension-file-deleted"),
                        _ => tr!("extension-file-modified"),
                    };
                    div().text_sm().child(tr!(
                        "extension-file-change",
                        status = status,
                        path = file.path.clone()
                    ))
                })),
                None => v_flex()
                    .child(summary)
                    .child(tr!("extension-diff-unavailable")),
            }
        }))
    }

    pub(super) fn render_extension(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let item = self
            .extensions
            .items
            .iter()
            .find(|item| Some(&item.id) == self.extensions.active.as_ref());
        let panel = v_flex()
            .id("extension-view")
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                h_flex()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().flex_1().min_w_0()
                        .children(item.map(|item| div().font_semibold().child(item.title.clone()))))
                    .child(self.pane_close_button("extension-close-view", cx)),
            );
        let body = match &self.extensions.view {
            Some(Ok(nodes)) => self.render_extension_nodes(nodes, "extension", cx),
            Some(Err(error)) => div()
                .text_color(theme.danger)
                .child(tr!("extension-error", detail = error.clone())),
            None => div()
                .text_color(theme.muted_foreground)
                .child(tr!("extension-loading")),
        };
        panel.child(
            div()
                .id("extension-view-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(div().max_w(px(840.)).mx_auto().px_6().py_5().child(body)),
        )
    }

    fn render_extension_nodes(
        &self,
        nodes: &[ExtensionNode],
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let mut tree = v_flex().gap_4();
        for (index, node) in nodes.iter().enumerate() {
            let key = format!("{prefix}-{index}");
            tree =
                tree.child(match node {
                    ExtensionNode::Heading { text } => {
                        div().text_xl().font_semibold().child(text.clone())
                    }
                    ExtensionNode::Markdown { text } => div().child(
                        TextView::markdown(SharedString::from(key), text.clone())
                            .image_source(|_| {
                                gpui_kit::ImageSource::Custom(std::sync::Arc::new(|_, _| None))
                            })
                            .on_link_click(|_, _, _, _| {}),
                    ),
                    ExtensionNode::Table { columns, rows } => {
                        let row = |cells: &[String]| {
                            h_flex().items_start().children(cells.iter().map(|cell| {
                                div().flex_1().min_w_0().px_3().py_2().child(cell.clone())
                            }))
                        };
                        div().child(
                            v_flex()
                                .id(SharedString::from(format!("extension-table-{key}")))
                                .border_1()
                                .border_color(theme.border)
                                .rounded_md()
                                .child(row(columns).bg(theme.muted).font_semibold())
                                .children(rows.iter().map(|cells| {
                                    row(cells).border_t_1().border_color(theme.sidebar_border)
                                })),
                        )
                    }
                    ExtensionNode::List { items } => {
                        v_flex().gap_1().children(items.iter().map(|item| {
                            h_flex()
                                .items_start()
                                .gap_2()
                                .child("•")
                                .child(item.clone())
                        }))
                    }
                    ExtensionNode::Input { id, label, .. } => v_flex()
                        .gap_1()
                        .child(label.clone())
                        .children(self.extensions.inputs.get(id).map(|(state, _)| {
                            Input::new(state)
                                .id(element_id("input", id))
                                .aria_label(label.clone())
                        })),
                    ExtensionNode::Button { id, text } => {
                        let element = id.clone();
                        div().child(
                            Button::new(element_id("button", id))
                                .label(text.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.request_extension_view(Some((element.clone(), None)));
                                    cx.notify();
                                })),
                        )
                    }
                    ExtensionNode::Tabs { id, tabs } => {
                        let selected = self
                            .extensions
                            .tabs
                            .get(id)
                            .copied()
                            .filter(|index| *index < tabs.len())
                            .unwrap_or(0);
                        v_flex()
                            .gap_2()
                            .child(h_flex().gap_2().flex_wrap().children(
                                tabs.iter().enumerate().map(|(index, tab)| {
                                    let group = id.clone();
                                    Button::new(element_id("tab", &format!("{id}-{index}")))
                                        .label(tab.title.clone())
                                        .toggled(selected == index)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.extensions.tabs.insert(group.clone(), index);
                                            cx.notify();
                                        }))
                                }),
                            ))
                            .children(
                                tabs.get(selected).map(|tab| {
                                    self.render_extension_nodes(&tab.children, &key, cx)
                                }),
                            )
                    }
                });
        }
        tree
    }
}
