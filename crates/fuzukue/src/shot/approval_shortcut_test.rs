use fuzukue_proto::{Actor, ApprovalDecision, ApprovalGrantScope, Envelope};
use gpui_kit::component::WindowExt as _;

use super::*;
use crate::conn::ConnEvent;

/// What macOS types for Option+Y when the key is left alone.
const TYPED: &str = "\u{a5}";

fn alt_y() -> Keystroke {
    let mut key = Keystroke::parse_with_os("alt-y", gpui_kit::OperatingSystem::Linux).unwrap();
    key.key_char = Some(TYPED.into());
    key
}

fn down(runner: &Runner, cx: &mut AsyncApp) -> Result<()> {
    cx.update_window(runner.handle, |_, window, cx| {
        window.dispatch_keystroke(alt_y(), cx);
    })?;
    runner.frame(cx)
}

fn up(runner: &Runner, cx: &mut AsyncApp) -> Result<()> {
    cx.update_window(runner.handle, |_, window, cx| {
        window.dispatch_event(PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke: alt_y() }), cx);
    })?;
    runner.frame(cx)
}

fn tap(runner: &Runner, cx: &mut AsyncApp) -> Result<()> {
    down(runner, cx)?;
    up(runner, cx)
}

async fn settle(runner: &Runner, cx: &mut AsyncApp) -> Result<()> {
    runner.frame(cx)?;
    pause(cx, 600).await;
    runner.frame(cx)
}

#[test]
fn approval_shortcut_allows_only_a_visible_settled_first_card() {
    let (finished, result) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1120., 900.).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.workspaces.active = Some("approvals-test".into());
                view.out = out.into();
                view.switch_session("approvals".into(), cx);
            });
            cx.spawn(async move |cx| {
                let runner = &mut Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let visible = |runner: &Runner, cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    // Queue decisions and live resolutions sent since the last call.
                    let sent = || messages.try_iter().filter_map(|message| match message {
                        ClientMsg::DecideApproval { request, approval_id, decision, .. } => {
                            assert_eq!(decision, ApprovalDecision::Grant { scope: ApprovalGrantScope::Once });
                            Some((request, approval_id))
                        }
                        ClientMsg::Resolve { request_id, decision } => {
                            assert_eq!(decision, "accept");
                            Some((0, request_id))
                        }
                        ClientMsg::RememberShellApproval { .. } | ClientMsg::DecideFolderAccess { .. } => {
                            panic!("the shortcut must only allow once")
                        }
                        _ => None,
                    }).collect::<Vec<_>>();
                    let ids = |sent: &[(u64, String)]| sent.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>();
                    let draft = |runner: &Runner, cx: &mut AsyncApp| {
                        runner.update(cx, |view, _, cx| view.input.read(cx).value().to_string())
                    };
                    let answer = |runner: &Runner, cx: &mut AsyncApp, request, id: &str| -> Result<()> {
                        runner.update(cx, |view, window, cx| {
                            let snapshot = view.approval_fixture_result(id);
                            view.on_conn(ConnEvent::Event(Envelope {
                                seq: 0, workspace: Some("approvals-test".into()),
                                event: ServerEvent::ApprovalDecisionResult { request, result: Ok(snapshot) },
                            }), window, cx);
                        })?;
                        runner.frame(cx)
                    };

                    // The fixture's connection reports "Not connected" once, from its own
                    // thread. The shortcut depends on the status, so set it after that.
                    runner.wait_for(cx, "the fixture connection status", TIMEOUT_MS, |view, _| {
                        matches!(view.status, crate::ConnectionStatus::Detail(_))
                    }).await?;
                    runner.update(cx, |view, _, cx| {
                        view.status = crate::ConnectionStatus::Connected;
                        cx.notify();
                    })?;
                    runner.step(click("chat-input"), cx).await?;
                    runner.type_text(cx, "draft")?;
                    let composer = runner.update(cx, |_, window, cx| window.focused(cx).unwrap())?;
                    runner.step(Step::SeedApprovalDock, cx).await?;
                    runner.step(Step::WaitElement { id: "approval-dock-scroll".into() }, cx).await?;

                    // A card that has only just appeared is not allowed, and the key types nothing.
                    tap(runner, cx)?;
                    assert!(sent().is_empty());

                    settle(runner, cx).await?;
                    down(runner, cx)?;
                    let first = sent();
                    assert_eq!(ids(&first), ["fixture-0"]);
                    // Repeats of a held key do nothing, even after the next card has settled.
                    answer(runner, cx, first[0].0, "fixture-0")?;
                    settle(runner, cx).await?;
                    down(runner, cx)?;
                    cx.update_window(runner.handle, |_, window, cx| {
                        window.dispatch_event(PlatformInput::KeyDown(gpui_kit::KeyDownEvent {
                            keystroke: alt_y(), is_held: true, prefer_character_input: false,
                        }), cx);
                    })?;
                    assert!(sent().is_empty());
                    up(runner, cx)?;
                    tap(runner, cx)?;
                    let second = sent();
                    assert_eq!(ids(&second), ["fixture-1"]);
                    // Nothing more goes out while a decision awaits its result.
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    answer(runner, cx, second[0].0, "fixture-1")?;

                    // High risk needs a click however long it has waited, and the dock says why.
                    assert!(!visible(runner, cx, "approval-shortcut-notice")?);
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    assert!(visible(runner, cx, "approval-shortcut-notice")?);
                    settle(runner, cx).await?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    assert!(visible(runner, cx, "approval-shortcut-notice")?);
                    runner.step(click("approval-allow-fixture-2"), cx).await?;
                    let clicked = sent();
                    assert_eq!(ids(&clicked), ["fixture-2"]);
                    answer(runner, cx, clicked[0].0, "fixture-2")?;
                    assert!(!visible(runner, cx, "approval-shortcut-notice")?);
                    // The card that moved up waits.
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    runner.step(click("chat-input"), cx).await?;

                    // Scrolled away: the first press brings the card back.
                    settle(runner, cx).await?;
                    runner.step(Step::Wheel { pane: Pane::ApprovalDock, pixels: Some(-600.), lines: None, pages: None }, cx).await?;
                    settle(runner, cx).await?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    runner.update(cx, |_, window, _| {
                        let scroll = window.shot_element_bounds("approval-dock-scroll").unwrap();
                        let allow = window.shot_element_bounds("approval-allow-fixture-3").unwrap();
                        assert!(allow.top() >= scroll.top() && allow.bottom() <= scroll.bottom());
                    })?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty(), "a revealed card waits too");

                    // Collapsed: the first press expands.
                    runner.step(click("approval-toggle"), cx).await?;
                    runner.step(click("chat-input"), cx).await?;
                    settle(runner, cx).await?;
                    assert!(!visible(runner, cx, "approval-row-fixture-3")?);
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    assert!(visible(runner, cx, "approval-row-fixture-3")?);
                    tap(runner, cx)?;
                    assert!(sent().is_empty());

                    // The composer kept its focus and its draft through all of this.
                    runner.update(cx, |_, window, _| assert!(composer.is_focused(window)))?;
                    assert_eq!(draft(runner, cx)?, "draft");

                    // A dialog or a lost connection leaves the key alone.
                    settle(runner, cx).await?;
                    runner.update(cx, |_, window, cx| window.open_dialog(cx, |dialog, _, _| dialog))?;
                    runner.frame(cx)?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    runner.update(cx, |_, window, cx| window.close_dialog(cx))?;
                    runner.update(cx, |view, _, cx| {
                        view.status = crate::ConnectionStatus::Connecting;
                        cx.notify();
                    })?;
                    // The closing dialog can still cover the composer, so wait before clicking it.
                    settle(runner, cx).await?;
                    runner.step(click("chat-input"), cx).await?;
                    runner.frame(cx)?;
                    let before = draft(runner, cx)?;
                    runner.update(cx, |_, window, _| assert!(composer.is_focused(window)))?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    assert_eq!(draft(runner, cx)?.matches(TYPED).count(), before.matches(TYPED).count() + 1);
                    runner.update(cx, |view, _, cx| {
                        view.status = crate::ConnectionStatus::Connected;
                        cx.notify();
                    })?;

                    // With nothing to approve the key types as usual.
                    runner.update(cx, |view, window, cx| view.resolve_approval_dock_fixture(window, cx))?;
                    runner.frame(cx)?;
                    let before = draft(runner, cx)?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    assert_eq!(draft(runner, cx)?.matches(TYPED).count(), before.matches(TYPED).count() + 1);

                    // Live approvals: an ordinary one is allowed once, folder access and Git need a click.
                    for (id, allowed) in [("claude-live", true), ("folder-access-live", false), ("git:live", false)] {
                        runner.update(cx, |view, _, cx| {
                            view.on_event(ServerEvent::ApprovalRequested {
                                session: view.active.clone(), request_id: id.into(), actor: Actor::Claude,
                                summary: "Bash: ls".into(), localized: None, shell_directory: None, shell_prefix: None,
                            }, cx);
                            cx.notify();
                        })?;
                        settle(runner, cx).await?;
                        let before = draft(runner, cx)?;
                        tap(runner, cx)?;
                        let expected: &[&str] = if allowed { &[id] } else { &[] };
                        assert_eq!(ids(&sent()), expected);
                        assert_eq!(visible(runner, cx, "approval-shortcut-notice")?, !allowed);
                        // The resolution has not arrived, and the same request is not sent twice.
                        settle(runner, cx).await?;
                        tap(runner, cx)?;
                        assert!(sent().is_empty());
                        assert_eq!(draft(runner, cx)?, before);
                        runner.update(cx, |view, _, cx| {
                            view.on_event(ServerEvent::ApprovalResolved {
                                request_id: id.into(), decision: "accept".into(), automatic: false,
                            }, cx);
                            cx.notify();
                        })?;
                    }

                    // A question at the top of the dock takes no shortcut, nor does what follows it.
                    runner.update(cx, |view, _, cx| {
                        view.on_event(serde_json::from_value(serde_json::json!({
                            "type": "question_asked", "session": "approvals", "request_id": "question",
                            "actor": Actor::Claude, "questions": [{
                                "id": "0", "header": "Choice", "question": "Which one?",
                                "options": [{"id": "0", "label": "First", "description": ""}],
                                "allow_other": false, "multi_select": false, "secret": false,
                            }],
                        })).unwrap(), cx);
                    })?;
                    runner.step(Step::SeedApprovalDock, cx).await?;
                    settle(runner, cx).await?;
                    tap(runner, cx)?;
                    assert!(sent().is_empty());
                    Ok(())
                }.await;
                finished.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
    result.recv().unwrap().unwrap();
}
