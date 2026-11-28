use fuzukue_proto::{Actor, Plan, Question, QuestionOutcome, WorkspaceInfo};
use gpui_kit::{EntityInputHandler as _, Focusable as _};
use serde_json::json;

use super::*;

#[test]
fn desktop_navigation_shortcuts_preserve_drafts_and_find_waiting_threads() {
    let (done, result) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false,
                    seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, window, cx| -> Result<()> {
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.status = crate::ConnectionStatus::Connected;
                        view.out = out.into();
                        // The workspace snapshot arrives in most-recently-used order.
                        view.workspaces.entries = ["Beta", "Alpha"].map(|title| WorkspaceInfo {
                            id: title.to_ascii_lowercase(),
                            title: title.into(),
                            path: format!("/fixtures/{title}"),
                            last_opened: Some(0),
                            running: false,
                        }).into();
                        view.workspace_selected("alpha".into(), false, window, cx);
                        let threads = [
                            ("draft", false, false),
                            ("pinned", true, false),
                            ("question", false, false),
                            ("plan", false, false),
                            ("approval", false, true),
                        ].map(|(id, pinned, archived)| serde_json::from_value(json!({
                            "id": id, "title": id, "updated": 0, "pinned": pinned, "archived": archived,
                        })).unwrap());
                        view.on_event(ServerEvent::Threads { threads: threads.into() }, cx);
                        view.select_thread("draft".into(), cx);
                        let question: Question = serde_json::from_value(json!({
                            "id": "question", "question": "What next?", "allow_other": true,
                        }))?;
                        view.on_event(ServerEvent::QuestionAsked {
                            session: "question".into(), request_id: "reply".into(),
                            actor: Actor::Claude, questions: vec![question],
                        }, cx);
                        let plan: Plan = serde_json::from_value(json!({
                            "goal": "Check the draft", "done_condition": "Ready to share",
                            "status": "waiting_for_user", "done": [], "next": [], "rejected": [],
                            "question": "Which version should I check?",
                        }))?;
                        view.on_event(ServerEvent::PlanChanged {
                            session: "plan".into(), plan, progress: None, parallel_limits: Default::default(),
                        }, cx);
                        view.on_event(ServerEvent::ApprovalRequested {
                            session: "approval".into(), request_id: "approve".into(),
                            actor: Actor::Claude, summary: "Check the file".into(),
                            localized: None, shell_directory: None, shell_prefix: None,
                        }, cx);
                        Ok(())
                    })??;
                    runner.frame(cx)?;
                    let press = |cx: &mut AsyncApp, key: &str| -> Result<()> {
                        runner.key(cx, key)?;
                        runner.frame(cx)
                    };
                    let active = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| view.active.clone());
                    runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                    runner.type_text(cx, "unsent")?;
                    for key in ["up", "up", "down", "down", "shift-up"] {
                        press(cx, key)?;
                        assert_eq!(active(cx)?, "draft", "{key} left the draft");
                    }
                    runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                        input.replace_and_mark_text_in_range(None, "あ", Some(1..1), window, cx);
                    }))?;
                    runner.frame(cx)?;
                    for key in ["alt-up", "ctrl-shift-j", "ctrl-2"] {
                        press(cx, key)?;
                        assert_eq!(active(cx)?, "draft", "{key} interrupted preedit");
                        assert!(runner.update(cx, |view, window, cx| crate::composing(&view.input, window, cx))?);
                    }
                    assert!(!messages.try_iter().any(|msg| matches!(msg, ClientMsg::SwitchWorkspace { .. })));
                    runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                        input.unmark_text(window, cx);
                        input.set_value("unsent", window, cx);
                    }))?;

                    for (key, thread) in [
                        ("alt-up", "pinned"), ("alt-up", "pinned"), ("alt-down", "draft"),
                        ("ctrl-shift-j", "question"), ("ctrl-shift-j", "plan"),
                        ("ctrl-shift-j", "approval"), ("ctrl-shift-j", "question"),
                    ] {
                        press(cx, key)?;
                        assert_eq!(active(cx)?, thread, "wrong destination for {key}");
                    }
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::QuestionResolved {
                        session: "question".into(), request_id: "reply".into(),
                        outcome: QuestionOutcome::Cancelled, answers: Default::default(),
                    }, cx))?;
                    press(cx, "ctrl-shift-j")?;
                    assert_eq!(active(cx)?, "plan");
                    press(cx, "alt-up")?;
                    press(cx, "alt-up")?;
                    assert_eq!(active(cx)?, "draft");
                    assert_eq!(runner.update(cx, |view, _, cx| view.input.read(cx).value().to_string())?, "unsent");

                    press(cx, "ctrl-k")?;
                    runner.type_text(cx, "query")?;
                    for (key, range) in [("up", 0..0), ("down", 5..5), ("shift-up", 0..5)] {
                        press(cx, key)?;
                        assert_eq!(runner.update(cx, |view, window, cx| {
                            let input = view.palette.as_ref().unwrap().read(cx);
                            assert!(input.focus_handle(cx).is_focused(window));
                            input.selected_range()
                        })?, range, "wrong single-line selection for {key}");
                    }
                    for key in ["alt-down", "ctrl-2"] {
                        press(cx, key)?;
                        assert_eq!(active(cx)?, "draft");
                    }
                    assert!(!messages.try_iter().any(|msg| matches!(msg, ClientMsg::SwitchWorkspace { .. })));
                    press(cx, "escape")?;

                    for key in ["ctrl-1", "ctrl-9", "ctrl-alt-2", "ctrl-shift-2"] {
                        press(cx, key)?;
                    }
                    assert!(!messages.try_iter().any(|msg| matches!(msg, ClientMsg::SwitchWorkspace { .. })));
                    press(cx, "ctrl-2")?;
                    press(cx, "ctrl-2")?;
                    let switches: Vec<_> = messages.try_iter().filter_map(|msg| match msg {
                        ClientMsg::SwitchWorkspace { id } => Some(id),
                        _ => None,
                    }).collect();
                    assert_eq!(switches, ["beta"]);
                    Ok(())
                })();
                let _ = done.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
    result.recv().unwrap().unwrap();
}
