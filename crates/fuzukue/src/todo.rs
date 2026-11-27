//! The thread's TODO count above the composer, and the list it opens.

use std::{collections::HashMap, rc::Rc};

use fuzukue_proto::{ClientMsg, TodoItem, TodoState, TodoWait};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    popover::Popover,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, rems, Anchor, AnyElement, Context, SharedString, WeakEntity};

use crate::{appearance::UI_LINE_HEIGHT, icons::Symbol, Workbench};

#[derive(Default)]
pub(super) struct Todos {
    lists: HashMap<String, Rc<List>>,
    done_open: bool,
}

#[derive(Default)]
pub(super) struct List {
    items: Vec<TodoItem>,
    unreadable: bool,
}

impl List {
    /// Open items in reading order: in progress, waiting, then not started,
    /// each oldest first. IDs break ties between items added in one turn.
    fn open(&self) -> Vec<&TodoItem> {
        let mut items: Vec<_> = self.items.iter().filter(|item| item.state != TodoState::Done).collect();
        items.sort_by_key(|item| {
            let rank = match item.state {
                TodoState::Doing => 0,
                TodoState::Waiting => 1,
                TodoState::Todo | TodoState::Done => 2,
            };
            (rank, item.id.len(), item.id.as_str())
        });
        items
    }

    /// Done items, the latest first.
    fn done(&self) -> Vec<&TodoItem> {
        let mut items: Vec<_> = self.items.iter().filter(|item| item.state == TodoState::Done).collect();
        items.sort_by_key(|item| std::cmp::Reverse((item.updated, item.id.len(), item.id.as_str())));
        items
    }

    fn label(&self) -> String {
        if self.unreadable {
            return tr!("todo-unreadable-badge").into();
        }
        let open = self.open();
        let waiting = open.iter().filter(|item| item.state == TodoState::Waiting).count();
        match (open.len(), waiting) {
            (0, _) => tr!("todo-title").into(),
            (count, 0) => tr!("todo-badge", count = count),
            (count, waiting) => tr!("todo-badge-waiting", count = count, waiting = waiting),
        }
    }
}

impl Todos {
    pub(super) fn update(&mut self, session: String, items: Vec<TodoItem>, unreadable: bool) {
        self.lists.insert(session, Rc::new(List { items, unreadable }));
    }
}

fn state_label(item: &TodoItem) -> Option<&'static str> {
    match (item.state, item.wait) {
        (TodoState::Todo, _) => None,
        (TodoState::Doing, _) => Some(tr!("todo-doing")),
        (TodoState::Waiting, Some(TodoWait::Action)) => Some(tr!("todo-waiting-action")),
        (TodoState::Waiting, _) => Some(tr!("todo-waiting-reply")),
        (TodoState::Done, _) => Some(tr!("todo-done")),
    }
}

fn row(view: &WeakEntity<Workbench>, session: &str, item: &TodoItem, cx: &gpui_kit::App) -> AnyElement {
    let theme = cx.theme();
    let done = item.state == TodoState::Done;
    let symbol = match item.state {
        TodoState::Todo => Symbol::Circle,
        TodoState::Doing => Symbol::CircleDot,
        TodoState::Waiting => Symbol::Pause,
        TodoState::Done => Symbol::Check,
    };
    let action = if done { tr!("todo-reopen") } else { tr!("todo-mark-done") };
    let (view, session, id) = (view.clone(), session.to_owned(), item.id.clone());
    h_flex().w_full().items_start().gap_2()
        .child(Button::new(SharedString::from(format!("todo-mark-{}", item.id)))
            .ghost().xsmall().flex_none()
            .icon(symbol.icon())
            .accessibility_label(format!("{action}: {}", item.text))
            .tooltip(action)
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |this, _| {
                    let _ = this.out.send(ClientMsg::SetTodoState {
                        session: session.clone(), item: id.clone(), done: !done,
                    });
                });
            }))
        .child(v_flex().flex_1().min_w_0()
            .child(div().when(done, |text| text.text_color(theme.muted_foreground)).child(item.text.clone()))
            .children(item.note.clone().map(|note| {
                div().text_xs().text_color(theme.muted_foreground).child(note)
            })))
        .children(state_label(item).filter(|_| !done).map(|label| {
            div().flex_none().text_xs().text_color(theme.muted_foreground).child(label)
        }))
        .into_any_element()
}

fn panel(
    view: &WeakEntity<Workbench>, session: &str, list: &List, done_open: bool, cx: &gpui_kit::App,
) -> AnyElement {
    let theme = cx.theme();
    let open = list.open();
    let done = list.done();
    let toggle = view.clone();
    v_flex().id("todo-list").w(rems(24.)).max_h(px(320.)).overflow_y_scrollbar().gap_1()
        .text_sm().line_height(UI_LINE_HEIGHT)
        .when(list.unreadable, |panel| {
            panel.child(div().text_color(theme.danger).child(tr!("todo-unreadable")))
        })
        .when(!list.unreadable && open.is_empty(), |panel| {
            panel.child(div().text_color(theme.muted_foreground).child(tr!("todo-empty")))
        })
        .children(open.iter().map(|item| row(view, session, item, cx)))
        .when(!done.is_empty(), |panel| {
            panel.child(h_flex().child(Button::new("todo-done-toggle")
                .ghost().xsmall()
                .icon(if done_open { Symbol::ChevronDown } else { Symbol::ChevronRight }.icon())
                .label(tr!("todo-done-count", count = done.len()))
                .on_click(move |_, _, cx| {
                    let _ = toggle.update(cx, |this, cx| {
                        this.todos.done_open = !this.todos.done_open;
                        cx.notify();
                    });
                })))
                .when(done_open, |panel| panel.children(done.iter().map(|item| row(view, session, item, cx))))
        })
        .into_any_element()
}

impl Workbench {
    /// Nothing for a thread that never had a TODO.
    pub(super) fn render_todo_badge(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let list = self.todos.lists.get(&self.active)
            .filter(|list| list.unreadable || !list.items.is_empty())?
            .clone();
        let theme = cx.theme();
        let color = if list.unreadable {
            theme.danger
        } else if list.open().is_empty() {
            theme.muted_foreground
        } else {
            theme.foreground
        };
        let button = Button::new("todo-badge").ghost().xsmall()
            .line_height(UI_LINE_HEIGHT)
            .text_color(color)
            .label(list.label());
        let view = cx.weak_entity();
        let session = self.active.clone();
        let done_open = self.todos.done_open;
        Some(div().ml_auto().flex_none()
            .child(Popover::new(SharedString::from(format!("todo-popover-{session}")))
                .anchor(Anchor::BottomRight)
                .trigger(button)
                .content(move |_, _, cx| panel(&view, &session, &list, done_open, cx)))
            .into_any_element())
    }
}

#[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
mod tests;
