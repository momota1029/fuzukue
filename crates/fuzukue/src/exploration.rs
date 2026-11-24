//! Read-only tree and status views for a research deck.

use std::collections::HashSet;

use fuzukue_proto::{ExplorationKind, ExplorationNode, ExplorationRecord, ExplorationState};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, Context, SharedString, Task};

use crate::{pill, RightPane, Symbol, Workbench};

#[derive(Default)]
pub(super) struct Pane {
    title: String,
    by_state: bool,
    collapsed: HashSet<String>,
    result: Option<Result<Vec<ExplorationNode>, String>>,
    _load: Option<Task<()>>,
}

#[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
impl Pane {
    pub(super) fn with_nodes(title: String, nodes: Vec<ExplorationNode>) -> Self {
        Self {
            title,
            result: Some(Ok(nodes)),
            ..Self::default()
        }
    }
}

const STATES: [ExplorationState; 5] = [
    ExplorationState::Queued,
    ExplorationState::Active,
    ExplorationState::Blocked,
    ExplorationState::Done,
    ExplorationState::Parked,
];

fn state_label(state: ExplorationState) -> &'static str {
    match state {
        ExplorationState::Queued => tr!("exploration-queued"),
        ExplorationState::Active => tr!("exploration-active"),
        ExplorationState::Blocked => tr!("exploration-blocked"),
        ExplorationState::Done => tr!("exploration-done"),
        ExplorationState::Parked => tr!("exploration-parked"),
    }
}

fn kind_label(kind: ExplorationKind) -> &'static str {
    match kind {
        ExplorationKind::Category => tr!("exploration-category"),
        ExplorationKind::Question => tr!("exploration-question"),
        ExplorationKind::Definition => tr!("exploration-definition"),
        ExplorationKind::Conjecture => tr!("exploration-conjecture"),
        ExplorationKind::Theorem => tr!("exploration-theorem"),
        ExplorationKind::Lemma => tr!("exploration-lemma"),
        ExplorationKind::Example => tr!("exploration-example"),
        ExplorationKind::Task => tr!("exploration-task"),
    }
}

impl Workbench {
    pub(super) fn open_exploration(&mut self, deck: String, title: String, cx: &mut Context<Self>) {
        let http = self.http.clone();
        let epoch = self.connection_epoch;
        let load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { http.fetch_exploration(&deck) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch {
                    return;
                }
                this.exploration.result = Some(result);
                cx.notify();
            });
        });
        self.exploration = Pane {
            title,
            _load: Some(load),
            ..Pane::default()
        };
        self.right = RightPane::Exploration;
        cx.notify();
    }

    pub(super) fn render_exploration(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let by_state = self.exploration.by_state;
        let mode = |id, label, grouped| {
            pill(id, label, by_state == grouped, cx).on_click(cx.listener(move |this, _, _, cx| {
                this.exploration.by_state = grouped;
                cx.notify();
            }))
        };
        let mut rows = v_flex().w_full().max_w(px(920.)).mx_auto().p_5();
        rows = match &self.exploration.result {
            None => rows.child(div().text_sm().child(tr!("workbench-loading"))),
            Some(Err(error)) => rows.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .whitespace_normal()
                    .child(tr!("exploration-read-error", detail = error.clone())),
            ),
            Some(Ok(nodes)) if nodes.is_empty() => {
                rows.child(div().text_sm().child(tr!("exploration-empty")))
            }
            Some(Ok(nodes)) if by_state => rows.children(STATES.into_iter().filter_map(|state| {
                let group: Vec<_> = nodes.iter().filter(|node| node.state == state).collect();
                if group.is_empty() {
                    return None;
                }
                Some(
                    v_flex()
                        .pb_4()
                        .child(div().pb_1().text_sm().font_semibold().child(tr!(
                            "exploration-state-count",
                            state = state_label(state),
                            count = group.len()
                        )))
                        .children(
                            group
                                .into_iter()
                                .map(|node| self.exploration_row(node, false, cx)),
                        ),
                )
            })),
            Some(Ok(nodes)) => {
                let mut hidden_below = None;
                rows.children(nodes.iter().enumerate().filter_map(|(index, node)| {
                    if hidden_below.is_some_and(|depth| node.depth > depth) {
                        return None;
                    }
                    let branch = nodes
                        .get(index + 1)
                        .is_some_and(|next| next.depth > node.depth);
                    hidden_below = (branch && self.exploration.collapsed.contains(&node.id))
                        .then_some(node.depth);
                    Some(self.exploration_row(node, branch, cx))
                }))
            }
        };

        v_flex()
            .id("exploration-view")
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .flex_none()
                    .flex_wrap()
                    .gap_3()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_lg().child(tr!("exploration-title")))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(self.exploration.title.clone()),
                            ),
                    )
                    .child(mode(
                        "exploration-mode-tree",
                        tr!("exploration-tree"),
                        false,
                    ))
                    .child(mode(
                        "exploration-mode-state",
                        tr!("exploration-by-state"),
                        true,
                    ))
                    .child(self.pane_close_button("exploration-close", cx)),
            )
            .child(
                div()
                    .id(if by_state {
                        "exploration-state-scroll"
                    } else {
                        "exploration-tree-scroll"
                    })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(rows),
            )
    }

    fn exploration_row(
        &self,
        node: &ExplorationNode,
        branch: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let grouped = self.exploration.by_state;
        let collapsed = self.exploration.collapsed.contains(&node.id);
        let branch_id = node.id.clone();
        let mut row = h_flex()
            .gap_2()
            .items_start()
            .py_1()
            .border_b_1()
            .border_color(theme.border);
        if !grouped {
            let branch = branch.then(|| {
                let label = if collapsed {
                    tr!("exploration-expand")
                } else {
                    tr!("exploration-collapse")
                };
                Button::new(format!("exploration-branch-{}", node.id))
                    .ghost()
                    .xsmall()
                    .icon(
                        if collapsed {
                            Symbol::ChevronRight
                        } else {
                            Symbol::ChevronDown
                        }
                        .icon(),
                    )
                    .tooltip(label)
                    .accessibility_label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.exploration.collapsed.remove(&branch_id) {
                            this.exploration.collapsed.insert(branch_id.clone());
                        }
                        cx.notify();
                    }))
            });
            row = row
                .pl(px(node.depth.min(12) as f32 * 18.))
                .child(div().w(px(24.)).flex_none().children(branch));
        }

        row.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .child(node.title.clone()),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(kind_label(node.kind))
                        .children((!grouped).then(|| state_label(node.state))),
                )
                .children(node.record.as_ref().map(|record| {
                    match record {
                        ExplorationRecord::Unavailable { path } => div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(tr!("exploration-record-unavailable", path = path.clone()))
                            .into_any_element(),
                        ExplorationRecord::Card { path, .. } | ExplorationRecord::File { path } => {
                            let record = record.clone();
                            let path = path.clone();
                            div()
                                .id(SharedString::from(format!(
                                    "exploration-record-{}",
                                    node.id
                                )))
                                .text_xs()
                                .text_color(theme.link)
                                .whitespace_normal()
                                .cursor_pointer()
                                .hover(|row| row.underline())
                                .child(path)
                                .on_click(cx.listener(move |this, _, _, cx| match &record {
                                    ExplorationRecord::Card { deck, card, .. } => {
                                        this.open_paper_from_chat(
                                            deck.clone(),
                                            Some(card.clone()),
                                            cx,
                                        )
                                    }
                                    ExplorationRecord::File { path } => {
                                        this.open_note(path.clone(), cx)
                                    }
                                    ExplorationRecord::Unavailable { .. } => {}
                                }))
                                .into_any_element()
                        }
                    }
                })),
        )
    }
}
