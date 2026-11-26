use fuzukue_proto::{ApprovalDecision, ApprovalGrantScope, Envelope};

use super::*;
use crate::conn::ConnEvent;

fn press(runner: &Runner, cx: &mut AsyncApp, key: &str) -> Result<()> {
    runner.key(cx, key)?;
    let keystroke = Keystroke::parse_with_os(key, gpui_kit::OperatingSystem::Linux)
        .map_err(|error| anyhow!("{error}"))?;
    cx.update_window(runner.handle, |_, window, cx| {
        window.dispatch_event(PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }), cx);
    })
}

#[test]
fn approval_dock_keeps_composer_focus_and_supports_keyboard_and_bulk_review() {
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
                view.status = crate::ConnectionStatus::Connected;
                view.workspaces.active = Some("approvals-test".into());
                view.out = out.into();
                view.switch_session("approvals".into(), cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };
                    let visible = |runner: &Runner, cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    runner.step(click("chat-input"), cx).await?;
                    let composer_focus = runner.update(cx, |_, window, cx| window.focused(cx).unwrap())?;
                    runner.step(Step::SeedApprovalDock, cx).await?;
                    runner.step(wait("approval-dock-scroll"), cx).await?;
                    runner.update(cx, |view, window, cx| {
                        assert!(composer_focus.is_focused(window));
                        assert_eq!(view.approval_count(&view.active), 12);
                        let scroll = window.shot_element_bounds("approval-dock-scroll").unwrap();
                        let dock = window.shot_element_bounds("approval-dock").unwrap();
                        let composer = window.shot_element_bounds("chat-input").unwrap();
                        assert!(scroll.size.height <= px(252.));
                        assert!(dock.bottom() <= composer.top());
                        assert!(window.focused(cx).is_some());
                    })?;

                    runner.step(click("approval-toggle"), cx).await?;
                    press(&runner, cx, "space")?;
                    runner.frame(cx)?;
                    press(&runner, cx, "tab")?;
                    runner.frame(cx)?;
                    press(&runner, cx, "space")?;
                    runner.step(wait("approval-request-details"), cx).await?;
                    press(&runner, cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "approval-request-details")?);
                    assert!(!visible(&runner, cx, "approval-select-fixture-2")?);
                    press(&runner, cx, "shift-tab")?;
                    runner.frame(cx)?;
                    press(&runner, cx, "tab")?;
                    runner.frame(cx)?;
                    press(&runner, cx, "enter")?;
                    runner.step(wait("approval-request-details"), cx).await?;
                    press(&runner, cx, "escape")?;
                    runner.frame(cx)?;

                    runner.step(Step::Wheel { pane: Pane::ApprovalDock, pixels: Some(-310.), lines: None, pages: None }, cx).await?;
                    runner.step(Step::AssertApprovalDockScrolled, cx).await?;
                    runner.step(Step::Wheel { pane: Pane::ApprovalDock, pixels: Some(1000.), lines: None, pages: None }, cx).await?;
                    runner.frame(cx)?;
                    runner.step(click("approval-select-fixture-0"), cx).await?;
                    runner.step(Step::Wheel { pane: Pane::ApprovalDock, pixels: Some(-160.), lines: None, pages: None }, cx).await?;
                    runner.frame(cx)?;
                    runner.step(click("approval-select-fixture-1"), cx).await?;
                    runner.step(click("approval-bulk-allow"), cx).await?;
                    runner.step(wait("approval-bulk-review"), cx).await?;
                    assert!(!messages.try_iter().any(|message| matches!(message, ClientMsg::DecideApproval { .. })));
                    press(&runner, cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "approval-bulk-review")?);
                    press(&runner, cx, "enter")?;
                    runner.step(wait("approval-bulk-review"), cx).await?;
                    runner.step(click("approval-confirm"), cx).await?;
                    let decisions: Vec<_> = messages.try_iter().filter_map(|message| match message {
                        ClientMsg::DecideApproval { request, approval_id, decision, .. } => Some((request, approval_id, decision)),
                        _ => None,
                    }).collect();
                    assert_eq!(decisions.len(), 2);
                    for ((_, id, decision), expected) in decisions.iter().zip(["fixture-0", "fixture-1"]) {
                        assert_eq!(id, expected);
                        assert_eq!(*decision, ApprovalDecision::Grant { scope: ApprovalGrantScope::Once });
                    }
                    assert_eq!(runner.update(cx, |view, _, _| view.approval_count(&view.active))?, 12);
                    runner.update(cx, |view, window, cx| {
                        view.on_conn(ConnEvent::Event(Envelope {
                            seq: 0, workspace: Some("approvals-test".into()),
                            event: ServerEvent::ApprovalDecisionResult {
                                request: decisions[0].0,
                                result: Err("approval request has already been decided or consumed".into()),
                            },
                        }), window, cx);
                        let snapshot = view.approval_fixture_result("fixture-1");
                        view.on_conn(ConnEvent::Event(Envelope {
                            seq: 0, workspace: Some("approvals-test".into()),
                            event: ServerEvent::ApprovalDecisionResult { request: decisions[1].0, result: Ok(snapshot) },
                        }), window, cx);
                        view.resolve_approval_dock_fixture(window, cx);
                    })?;
                    runner.frame(cx)?;
                    assert_eq!(runner.update(cx, |view, _, _| view.approval_count(&view.active))?, 0);
                    assert!(!visible(&runner, cx, "approval-row-fixture-0")?);
                    assert!(visible(&runner, cx, "approval-dismiss")?);
                    assert!(!visible(&runner, cx, "chat-local-notice-11")?);
                    Ok(())
                }.await;
                finished.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
    result.recv().unwrap().unwrap();
}
