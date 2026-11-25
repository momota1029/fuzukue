use super::*;
use fuzukue_proto::Actor;
use gpui_kit::{component::WindowExt as _, Focusable as _, ListOffset};

const THREAD: &str = "navigation-fixture";
const OTHER: &str = "navigation-other";
const JOB: &str = "delegate:navigation";

impl Runner {
    pub(super) async fn assert_thread_navigation_keys(&self, cx: &mut AsyncApp) -> Result<()> {
        ensure!(!self.read_only, "keyboard fixture requires a writable local scenario");
        let (links, mut received_links) = futures::channel::mpsc::unbounded();
        self.update(cx, |view, window, cx| {
            view.link_tx = links;
            view.protocol = 4; // Show sidebar threads and use the local transcript.
            view.transcripts.scoped = false;
            view.receiving_backlog = false;
            let threads = [THREAD, OTHER].map(|id| serde_json::from_value(serde_json::json!({
                "id": id, "title": id, "updated": 1,
            })).unwrap());
            view.on_event(ServerEvent::Threads { threads: threads.into() }, cx);
            view.select_thread(THREAD.into(), cx);
            for session in [THREAD, OTHER] {
                for row in 0..48 {
                    let text = if session == THREAD && row == 0 {
                        "[[navigation-link|Open the fixture link]]".into()
                    } else {
                        format!("Message {row} in {session}")
                    };
                    view.on_event(ServerEvent::TextDelta {
                        session: session.into(), item: format!("user:{row}"), actor: Actor::User,
                        text,
                    }, cx);
                }
            }
            view.on_event(ServerEvent::ToolCall {
                session: THREAD.into(), item: JOB.into(), actor: Actor::System, name: "delegate".into(),
                input: serde_json::json!({"title": "Check navigation", "model": "codex", "started_at_ms": 1}),
            }, cx);
            view.on_event(ServerEvent::TextDelta {
                session: THREAD.into(), item: "code".into(), actor: Actor::Claude, text: "```sh\necho hello\n```".into(),
            }, cx);
            view.chats[THREAD].scroll_to_end();
            window.blur(cx);
            cx.notify();
        })?;
        self.frame(cx)?;

        let list = |cx: &mut AsyncApp| self.update(cx, |view, _, _| view.chats[&view.active].scroller.clone());
        let press = |cx: &mut AsyncApp, key: &str| -> Result<()> {
            self.key(cx, key)?;
            self.frame(cx)
        };
        let click = |cx: &mut AsyncApp, id: &str| -> Result<()> {
            self.click(cx, ClickTarget::Element { id: id.into() })?;
            self.frame(cx)
        };
        let endpoints = |cx: &mut AsyncApp, context: &str| -> Result<()> {
            let list = list(cx)?;
            ensure!(list.logical_scroll_top().item_ix > 0, "{context}: fixture must start below the first row");
            press(cx, "home")?;
            ensure!(list.logical_scroll_top() == ListOffset::default(), "{context}: Home missed the first row");
            press(cx, "end")?;
            let distance_to_end = list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y;
            ensure!(list.is_following_tail() && distance_to_end.abs() <= px(1.),
                "{context}: End missed the latest row");
            Ok(())
        };
        let stays = |cx: &mut AsyncApp, key: &str, context: &str| -> Result<()> {
            let list = list(cx)?;
            let before = (list.logical_scroll_top(), list.is_following_tail());
            press(cx, key)?;
            ensure!((list.logical_scroll_top(), list.is_following_tail()) == before,
                "{context}: {key} moved the transcript");
            Ok(())
        };

        ensure!(self.update(cx, |_, window, cx| window.focused(cx).is_none())?, "window must start without focus");
        endpoints(cx, "no focus")?;
        click(cx, &format!("chat-{THREAD}"))?;
        ensure!(self.update(cx, |view, window, _| view.chat_focus.is_focused(window))?, "transcript must have focus");
        endpoints(cx, "transcript focus")?;

        click(cx, crate::menu_bar::SETTINGS_BUTTON)?;
        click(cx, "close-settings")?;
        click(cx, &format!("chat-{THREAD}"))?;
        endpoints(cx, "conversation after closing settings")?;

        for session in [OTHER, THREAD] {
            click(cx, &format!("thread-row-title-{session}"))?;
            ensure!(self.update(cx, |view, _, _| view.active == session)?, "sidebar did not switch to {session}");
            endpoints(cx, "sidebar thread switch")?;
        }
        press(cx, "home")?;
        click(cx, "jump-to-latest")?;
        endpoints(cx, "jump button")?;

        click(cx, &format!("delegate-block-{JOB}"))?;
        endpoints(cx, "job row")?;
        self.wait_for(cx, "fenced code", TIMEOUT_MS, |_, window| window.shot_element_bounds("codeblock-0").is_some()).await?;
        click(cx, "codeblock-0")?;
        endpoints(cx, "code block")?;

        press(cx, "home")?;
        let link = self.update(cx, |_, window, _| window.shot_element_bounds("chat-message-bubble-0"))?
            .context("fixture link is not visible")?;
        self.click(cx, ClickTarget::Coordinates { x: (link.left() + px(50.)).into(), y: link.center().y.into() })?;
        ensure!(matches!(received_links.try_recv(), Ok(crate::MarkdownLink::Card(id)) if id == "navigation-link"),
            "click did not activate the Markdown link in {link:?}");
        press(cx, "end")?;
        endpoints(cx, "Markdown link")?;

        click(cx, "chat-input")?;
        ensure!(self.update(cx, |view, window, cx| {
            let input = view.input.read(cx);
            input.focus_handle(cx).is_focused(window) && input.value().is_empty()
        })?, "empty composer must have focus");
        endpoints(cx, "empty composer")?;
        let list = list(cx)?;
        press(cx, "pageup")?;
        let page_up = list.logical_scroll_top();
        ensure!(!list.is_following_tail() && page_up.item_ix > 0, "PageUp should scroll about one viewport");
        press(cx, "pagedown")?;
        ensure!(list.logical_scroll_top().item_ix > page_up.item_ix, "PageDown should move toward the latest message");
        press(cx, "end")?;

        let composer = |cx: &mut AsyncApp, text: &str, caret: usize| -> Result<()> {
            self.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                input.set_value(text.to_owned(), window, cx);
                input.set_selected_range(caret..caret, cx);
            }))?;
            self.frame(cx)
        };
        let selection = |cx: &mut AsyncApp| self.update(cx, |view, _, cx| view.input.read(cx).selected_range());
        for (key, range) in [("home", 0..0), ("end", 5..5), ("shift-home", 0..2), ("shift-end", 2..5)] {
            composer(cx, "draft", 2)?;
            stays(cx, key, "single-line composer")?;
            ensure!(selection(cx)? == range, "single-line composer: wrong selection after {key}");
        }
        for key in ["pageup", "pagedown"] {
            stays(cx, key, "populated composer")?;
        }
        composer(cx, "draft", 2)?;
        press(cx, "ctrl-home")?;
        ensure!(list.logical_scroll_top() == ListOffset::default() && selection(cx)? == (2..2),
            "Ctrl+Home should navigate the thread and preserve the single-line caret");
        press(cx, "ctrl-end")?;
        ensure!(list.is_following_tail() && selection(cx)? == (2..2),
            "Ctrl+End should navigate the thread and preserve the single-line caret");

        for (key, range) in [
            ("home", 4..4), ("end", 7..7), ("shift-home", 4..5), ("shift-end", 5..7),
            ("ctrl-home", 0..0), ("ctrl-end", 7..7),
        ] {
            composer(cx, "abc\n123", 5)?;
            stays(cx, key, "multiline composer")?;
            ensure!(selection(cx)? == range, "multiline composer: wrong selection after {key}");
        }
        composer(cx, "", 0)?;
        list.scroll_to(ListOffset { item_ix: 24, offset_in_item: px(0.) });
        self.frame(cx)?;

        self.update(cx, |view, window, cx| {
            let path = "cards/navigation.md";
            let card = serde_json::from_value(serde_json::json!({
                "id": "navigation", "title": "Keyboard fixture", "kind": "design", "status": "",
                "path": path, "links": [], "backlinks": [], "files": [], "body": "abc\n123", "external": false,
            })).unwrap();
            view.on_event(ServerEvent::Cards { deck: view.deck.clone(), cards: vec![card], diagnostics: vec![] }, cx);
            view.select("navigation".into(), cx);
            view.install_card_file_editor(path.into(), "abc\n123".into(), window, cx);
        })?;
        self.frame(cx)?;
        let editor_caret = |cx: &mut AsyncApp| self.update(cx, |view, _, cx| {
            view.editing.as_ref().unwrap().input.read(cx).cursor()
        });
        for (key, cursor) in [("ctrl-home", 0), ("end", 3), ("ctrl-end", 7), ("home", 4)] {
            stays(cx, key, "card editor")?;
            ensure!(editor_caret(cx)? == cursor, "card editor: wrong caret after {key}");
        }
        for key in ["up", "up", "down", "down"] {
            stays(cx, key, "card editor")?;
            ensure!(self.update(cx, |view, window, cx| {
                view.editing.as_ref().unwrap().input.read(cx).focus_handle(cx).is_focused(window)
            })?, "card editor lost focus after {key}");
        }
        self.update(cx, |view, _, cx| {
            view.close_card_file_editor(cx);
            view.close_right_pane(cx);
        })?;
        self.frame(cx)?;
        click(cx, "chat-input")?;

        press(cx, "ctrl-k")?;
        self.type_text(cx, "missing fixture")?;
        for key in ["pageup", "pagedown", "ctrl-home", "ctrl-end", "home", "end"] {
            stays(cx, key, "search")?;
        }
        ensure!(self.update(cx, |view, _, cx| {
            view.palette.as_ref().is_some_and(|input| input.read(cx).cursor() == "missing fixture".len())
        })?, "search should keep its End binding");
        for key in ["up", "down"] {
            stays(cx, key, "search")?;
            ensure!(self.update(cx, |view, window, cx| {
                view.palette.as_ref().is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
            })?, "search lost focus after {key}");
        }
        press(cx, "escape")?;

        self.update(cx, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Keyboard fixture"));
        })?;
        self.frame(cx)?;
        for key in ["home", "end", "ctrl-home", "ctrl-end", "pageup", "pagedown"] {
            stays(cx, key, "dialog")?;
        }
        press(cx, "escape")?;

        click(cx, "chat-tail-activity-trigger")?;
        ensure!(self.update(cx, |_, window, _| window.shot_element_bounds("chat-tail-activity-details").is_some())?,
            "job list did not open");
        for key in ["home", "end", "ctrl-home", "ctrl-end", "pageup", "pagedown"] {
            stays(cx, key, "job list")?;
        }
        press(cx, "escape")?;
        endpoints(cx, "after closing the job list")?;
        press(cx, "home")?;
        Ok(())
    }
}
