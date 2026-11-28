//! Local state and controls for interactive reply blocks.

use std::sync::Arc;

use fuzukue_view::ui_kit::Interaction;
use gpui_kit::base::{GlobalState, StyledExt as _, text::{MarkdownExtensions, MarkdownNode}};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Theme};
use gpui_kit::{
    div, prelude::*, AnyElement, App, Div, MouseButton, Role, Stateful, Toggled, WeakEntity, Window,
};

use super::panel;
use crate::Workbench;

pub(in crate) const PLUGIN: &str = "xml-interaction";

pub(in crate) type Key = (String, usize);

#[derive(Clone)]
pub(in crate) struct Reply {
    pub session: String,
    pub item: String,
    pub row: u64,
}

pub(super) struct Block {
    pub index: usize,
    pub label: String,
    pub control: Interaction,
}

#[derive(Clone, Copy)]
pub(in crate) enum State {
    Selection(u16),
    Number(i32),
    Panel(Option<usize>),
}

#[derive(Clone, Copy)]
pub(in crate) enum Action {
    Select(usize),
    Step(i32),
    Show(usize),
    InsertPanel,
    Remove,
}

pub(in crate) enum DraftChange {
    Keep,
    Set(String),
    Remove,
}

impl State {
    pub(in crate) fn initial(control: &Interaction) -> Self {
        match control {
            Interaction::Select { .. } => Self::Selection(0),
            Interaction::Number { value, .. } => Self::Number(*value),
            Interaction::Panels { tabs, .. } => Self::Panel(tabs.then_some(0)),
        }
    }

    pub(in crate) fn apply(
        &mut self, label: &str, control: &Interaction, action: Action,
    ) -> DraftChange {
        let value = match (self, control, action) {
            (_, _, Action::Remove) => return DraftChange::Remove,
            (Self::Selection(selected), Interaction::Select { multiple, options }, Action::Select(index))
                if index < options.len() =>
            {
                if *multiple {
                    *selected ^= 1 << index;
                } else {
                    *selected = 1 << index;
                }
                if *selected == 0 {
                    tr!("xml-interaction-none").to_owned()
                } else {
                    options.iter().enumerate()
                        .filter(|(index, _)| *selected & (1 << index) != 0)
                        .map(|(_, option)| option.as_str())
                        .collect::<Vec<_>>().join(", ")
                }
            }
            (Self::Number(current), Interaction::Number { min, max, step, .. }, Action::Step(direction)) => {
                let next = (*current + direction * step).clamp(*min, *max);
                if next == *current {
                    return DraftChange::Keep;
                }
                *current = next;
                current.to_string()
            }
            (Self::Panel(current), Interaction::Panels { tabs, items }, Action::Show(index))
                if index < items.len() =>
            {
                *current = (*tabs || *current != Some(index)).then_some(index);
                return DraftChange::Keep;
            }
            (Self::Panel(Some(index)), Interaction::Panels { items, .. }, Action::InsertPanel) => {
                items[*index].label.clone()
            }
            _ => return DraftChange::Keep,
        };
        // The fixed prefix keeps model-provided labels outside slash syntax.
        DraftChange::Set(tr!("xml-interaction-response", label = label.to_owned(), value = value))
    }

    pub(in crate) fn visible_text(self, label: &str, control: &Interaction) -> String {
        let mut lines = vec![label.to_owned()];
        match (self, control) {
            (Self::Selection(selected), Interaction::Select { options, .. }) => {
                lines.extend(options.iter().enumerate().map(|(index, option)| {
                    let mark = if selected & (1 << index) == 0 { "○" } else { "✓" };
                    format!("{mark} {option}")
                }));
            }
            (Self::Number(current), Interaction::Number { min, max, step, .. }) => {
                lines.push(format!("{current} ({min}–{max}, Δ {step})"));
            }
            (Self::Panel(current), Interaction::Panels { items, .. }) => {
                for (index, item) in items.iter().enumerate() {
                    lines.push(item.label.clone());
                    if current == Some(index) {
                        lines.push(item.text.clone());
                    }
                }
            }
            _ => {}
        }
        lines.join("\n")
    }
}

pub(in crate) fn with_reply(
    extensions: MarkdownExtensions, workbench: WeakEntity<Workbench>, reply: Reply,
) -> MarkdownExtensions {
    extensions.block_renderer(PLUGIN, move |node, window, cx| render(&workbench, &reply, node, window, cx))
}

pub(super) fn fallback(node: &MarkdownNode) -> AnyElement {
    div().min_w_0().whitespace_normal()
        .children(node.as_text().lines().map(|line| div().child(line.to_owned())))
        .into_any_element()
}

fn render(
    workbench: &WeakEntity<Workbench>, reply: &Reply, node: &MarkdownNode,
    window: &mut Window, cx: &mut App,
) -> AnyElement {
    let Some(block) = node.data::<Arc<Block>>() else { return fallback(node) };
    let key = (reply.item.clone(), block.index);
    let Some(entity) = workbench.upgrade() else { return fallback(node) };
    let Some((state, preview)) = entity.read(cx).reply_control_state(reply, &key, &block.control) else {
        return fallback(node);
    };
    let preview = preview.map(str::to_owned);
    let theme = cx.theme().clone();
    let id = |suffix| format!("xml-interaction-{}-{suffix}", block.index);
    let mut button = |suffix: String, name: String, action, enabled, selected| {
        let id = id(suffix);
        let focus = window.use_keyed_state(format!("reply-focus-{}-{id}", reply.row), cx, |_, cx| cx.focus_handle())
            .read(cx).clone().tab_index(0).tab_stop(enabled);
        let (workbench, reply, key, block) = (workbench.clone(), reply.clone(), key.clone(), block.clone());
        control_button(id, name, enabled, selected, &theme)
            .when(enabled || focus.is_focused(window), |button| button.track_focus(&focus))
            .when(enabled, |button| {
                button
                    // A selection gesture would give focus to the surrounding Markdown.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        GlobalState::suppress_text_selection(cx);
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        let _ = workbench.update(cx, |view, cx| {
                            view.operate_reply_control(&reply, &key, &block.label, &block.control, action, window, cx);
                        });
                    })
            })
    };
    let mut container = panel(id("panel".to_owned()), &theme)
        .rounded_lg().p_3().gap_2().w_full().min_w_0()
        .child(div().font_semibold().whitespace_normal().child(block.label.clone()));

    match (state, &block.control) {
        (State::Selection(selected), Interaction::Select { multiple, options }) => {
            let choices = v_flex().id(id("options".into())).w_full().min_w_0().gap_1()
                .role(if *multiple { Role::Group } else { Role::RadioGroup })
                .aria_label(block.label.clone())
                .children(options.iter().enumerate().map(|(index, option)| {
                    let checked = selected & (1 << index) != 0;
                    button(format!("option-{index}"), option.clone(), Action::Select(index), true, checked)
                        .w_full()
                        .role(if *multiple { Role::CheckBox } else { Role::RadioButton })
                        .aria_toggled(if checked { Toggled::True } else { Toggled::False })
                        .child(h_flex().min_w_0().gap_2()
                            .child(div().flex_none().child(if checked { "✓" } else { "○" }))
                            .child(div().min_w_0().whitespace_normal().child(option.clone())))
                }));
            container = container.child(choices);
        }
        (State::Number(current), Interaction::Number { min, max, step, .. }) => {
            let value = format!("{current} ({min}–{max}, Δ {step})");
            container = container.child(h_flex().w_full().min_w_0().flex_wrap().gap_2()
                .child(button("decrease".into(), tr!("xml-interaction-decrease", label = block.label.clone()),
                    Action::Step(-1), current > *min, false).child("−"))
                .child(div().id(id("value".into())).role(Role::Status).whitespace_normal().child(value))
                .child(button("increase".into(), tr!("xml-interaction-increase", label = block.label.clone()),
                    Action::Step(1), current < *max, false).child("+")));
        }
        (State::Panel(current), Interaction::Panels { tabs, items }) => {
            let mut options = v_flex().id(id("panels".into())).w_full().min_w_0().gap_1();
            if *tabs {
                options = options.role(Role::TabList).aria_label(block.label.clone());
            }
            for (index, item) in items.iter().enumerate() {
                let open = current == Some(index);
                let toggle = button(format!("show-{index}"), item.label.clone(), Action::Show(index), true, open)
                    .w_full().child(div().min_w_0().whitespace_normal().child(item.label.clone()))
                    .when(*tabs, |button| button.role(Role::Tab).aria_selected(open))
                    .when(!*tabs, |button| button.aria_expanded(open));
                options = options.child(toggle);
                if open && !*tabs {
                    options = options.child(panel_content(&item.text));
                }
            }
            container = container.child(options);
            if let Some(index) = current {
                let item = &items[index];
                if *tabs {
                    container = container.child(panel_content(&item.text).role(Role::TabPanel).aria_label(item.label.clone()));
                }
                let insert = tr!("xml-interaction-insert");
                container = container.child(button("insert".into(), insert.to_owned(), Action::InsertPanel, true, false).child(insert));
            }
        }
        _ => return fallback(node),
    }
    if let Some(preview) = preview {
        let remove = tr!("xml-interaction-remove");
        container = container
            .child(div().id(id("preview".into())).role(Role::Status).min_w_0().whitespace_normal()
                .text_sm().text_color(theme.muted_foreground)
                .child(tr!("xml-interaction-preview", preview = preview)))
            .child(button("remove".into(), remove.to_owned(), Action::Remove, true, false).child(remove));
    }
    container.into_any_element()
}

fn panel_content(text: &str) -> Stateful<Div> {
    div().id("xml-interaction-panel-text").min_w_0().pl_2().whitespace_normal().child(text.to_owned())
}

fn control_button(
    id: String, name: String, enabled: bool, selected: bool, theme: &Theme,
) -> Stateful<Div> {
    let focus = theme.primary;
    let hover = theme.muted;
    div().id(id).role(Role::Button).aria_label(name).min_w_0()
        .px_2().py_1().border_1().rounded_md().border_color(theme.border)
        .when(selected, |button| button.border_color(focus).bg(theme.accent))
        .when(enabled, |button| button.cursor_pointer()
            .hover(move |style| style.bg(hover))
            .focus_visible(move |style| style.border_color(focus)))
        .when(!enabled, |button| button.text_color(theme.muted_foreground))
}
