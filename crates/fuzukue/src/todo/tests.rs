use super::*;
use crate::conn::ConnEvent;
use fuzukue_proto::{Envelope, ServerEvent, TodoAuthor};
use gpui_kit::{
    App, Entity, Keystroke, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Window,
};

const WORKSPACE: &str = "todo-fixture";
const THREAD: &str = "thread-a";

fn item(id: &str, state: TodoState, updated: i64) -> TodoItem {
    TodoItem {
        id: id.into(), text: format!("synthetic {id}"), state,
        wait: (state == TodoState::Waiting).then_some(TodoWait::Reply),
        note: None, by: TodoAuthor::Front, created: 0, updated,
    }
}

#[test]
fn the_list_reads_running_then_waiting_then_oldest_and_counts_waiting() {
    let list = List {
        items: vec![
            item("T10", TodoState::Todo, 0),
            item("T2", TodoState::Todo, 0),
            item("T3", TodoState::Waiting, 0),
            item("T4", TodoState::Doing, 0),
            item("T5", TodoState::Done, 1),
            item("T6", TodoState::Done, 2),
        ],
        unreadable: false,
    };
    let ids = |items: Vec<&TodoItem>| items.into_iter().map(|item| item.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(list.open()), ["T4", "T3", "T2", "T10"]);
    assert_eq!(ids(list.done()), ["T6", "T5"]);
    assert_eq!(list.label(), tr!("todo-badge-waiting", count = 4, waiting = 1));
}

fn draw(window: &mut Window, cx: &mut App) {
    window.draw(cx).clear(cx);
}

fn publish(view: &Entity<Workbench>, window: &mut Window, cx: &mut App, items: Vec<TodoItem>) {
    view.update(cx, |view, cx| view.on_conn(ConnEvent::Event(Envelope {
        workspace: Some(WORKSPACE.into()),
        seq: 0,
        event: ServerEvent::TodoChanged { session: THREAD.into(), revision: 1, items, unreadable: false },
    }), window, cx));
    draw(window, cx);
}

fn click(window: &mut Window, cx: &mut App, id: &str) {
    let position = window.shot_element_bounds(id).unwrap_or_else(|| panic!("missing {id}")).center();
    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
        position, click_count: 1, ..Default::default()
    }), cx);
    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
        position, click_count: 1, ..Default::default()
    }), cx);
    draw(window, cx);
}

#[test]
fn the_count_opens_a_list_that_marks_and_reopens_and_returns_focus() {
    gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        crate::math::init(cx);
        let (window, view) = gpui_kit::open_window(
            gpui_kit::component::TitleBar::window_options(),
            cx,
            |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Offline fixture")))),
        ).unwrap();
        window.update(cx, |_, window, cx| {
            let (out, sent) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.workspaces.active = Some(WORKSPACE.into());
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.threads_loaded = true;
                view.switch_session(THREAD.into(), cx);
                view.out = out.into();
                gpui_kit::Focusable::focus_handle(view.input.read(cx), cx).focus(window, cx);
            });
            draw(window, cx);
            let focus = gpui_kit::Focusable::focus_handle(view.read(cx).input.read(cx), cx);
            assert!(window.shot_element_bounds("todo-badge").is_none());

            publish(&view, window, cx, vec![
                item("T1", TodoState::Todo, 0), item("T2", TodoState::Done, 0),
            ]);
            assert!(focus.is_focused(window), "a TODO update blurred the composer");

            click(window, cx, "todo-badge");
            draw(window, cx);
            click(window, cx, "todo-mark-T1");
            click(window, cx, "todo-done-toggle");
            draw(window, cx);
            click(window, cx, "todo-mark-T2");
            let changes: Vec<_> = sent.try_iter().filter_map(|message| match message {
                ClientMsg::SetTodoState { session, item, done } => Some((session, item, done)),
                _ => None,
            }).collect();
            assert_eq!(changes, [
                (THREAD.to_owned(), "T1".to_owned(), true), (THREAD.to_owned(), "T2".to_owned(), false),
            ]);

            // The clicked rows may move when the server answers.
            publish(&view, window, cx, vec![
                item("T1", TodoState::Done, 1), item("T2", TodoState::Todo, 1),
            ]);
            window.dispatch_keystroke(Keystroke::parse("escape").unwrap(), cx);
            draw(window, cx);
            assert!(window.shot_element_bounds("todo-mark-T2").is_none(), "Escape left the list open");
            assert!(focus.is_focused(window), "closing the TODO list lost composer focus");
        }).unwrap();
        cx.spawn(async move |cx| {
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
}
