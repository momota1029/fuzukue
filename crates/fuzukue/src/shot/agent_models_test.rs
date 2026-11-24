use super::*;
use crate::{agent_management::Tab, agent_models::ModelSettings};
use fuzukue_proto::{AgentInfo, AgentSelection, ChatSettings, DelegateSettings};

#[test]
fn fresh_model_setup_can_be_dismissed_and_reopened_from_the_roles_list() {
    let (report, result) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 1800.).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false,
                    seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    let directory = tempfile::tempdir()?;
                    let fresh = directory.path().join("fresh.toml");
                    let existing = directory.path().join("existing.toml");
                    std::fs::write(&existing, "future_key = true\n")?;
                    let (out, messages) = std::sync::mpsc::channel();
                    let seed = |path, front_desk_configured, cx: &mut AsyncApp| -> Result<()> {
                        runner.update(cx, |view, _, cx| {
                            view.agent_models = ModelSettings::load(Some(path));
                            view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                            view.on_event(ServerEvent::ChatCapabilities {
                                effort_levels: vec!["low".into(), "medium".into()],
                                mail_restricted: false, mcp_tools: vec![],
                            }, cx);
                            view.on_event(ServerEvent::PartnerDefaultChanged { settings: ChatSettings::default() }, cx);
                            view.on_event(ServerEvent::AgentModelCapabilities {
                                claude_available: true, codex_available: false, front_desk_configured,
                            }, cx);
                            cx.notify();
                        })?;
                        runner.frame(cx)
                    };
                    let visible = |cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    let click = |cx: &mut AsyncApp, id: &str| {
                        runner.click(cx, ClickTarget::Element { id: id.into() })?;
                        runner.frame(cx)
                    };

                    seed(existing.clone(), false, cx)?;
                    assert!(!visible(cx, "model-setup")?);
                    assert_eq!(std::fs::read_to_string(&existing)?, "future_key = true\n");
                    seed(directory.path().join("existing-workspace.toml"), true, cx)?;
                    assert!(!visible(cx, "model-setup")?);
                    let shown = directory.path().join("shown.toml");
                    seed(shown.clone(), false, cx)?;
                    assert!(visible(cx, "model-setup")?);
                    seed(shown, false, cx)?;
                    assert!(!visible(cx, "model-setup")?);
                    seed(fresh.clone(), false, cx)?;
                    assert!(visible(cx, "model-setup")?);
                    click(cx, "model-setup-later")?;
                    assert!(!visible(cx, "model-setup")?);
                    seed(fresh, false, cx)?;
                    assert!(!visible(cx, "model-setup")?);

                    runner.update(cx, |view, window, cx| {
                        view.out = out.into();
                        view.settings_open = true;
                        view.open_agent_management(Tab::Agents, window, cx);
                        view.on_event(ServerEvent::DelegateSettingsChanged { settings: DelegateSettings::default() }, cx);
                        let agents: Vec<AgentInfo> = ["planner", "lean", "literature", "review-style", "review-math"]
                            .into_iter().map(|name| serde_json::from_value(serde_json::json!({
                                "name": name, "description": name, "backend": "claude", "model": "claude-sonnet-5-5"
                            })).unwrap()).collect();
                        view.on_event(ServerEvent::Agents { agents }, cx);
                        view.sync_agent_hints(window, cx);
                    })?;
                    runner.frame(cx)?;
                    for id in [
                        "model-usage-settings", "model-front-desk-picker", "model-consult-picker",
                        "model-worker-picker", "model-tier-light-picker", "model-tier-standard-picker",
                        "model-tier-heavy-picker", "summary-model-picker", "model-thread-title-picker",
                        "model-worker-digest-picker", "agent-model-planner", "agent-model-lean",
                    ] {
                        assert!(visible(cx, id)?, "missing {id}");
                    }
                    click(cx, "model-setup-reopen")?;
                    assert!(visible(cx, "model-setup")?);
                    click(cx, "model-setup-apply")?;
                    let settings = AgentSelection::from_model(Some("claude-sonnet-5-5".into()), Some("low".into())).chat_settings();
                    assert!(messages.try_iter().any(|msg| matches!(msg, ClientMsg::SetPartnerDefault { settings: saved } if saved == settings)));
                    assert!(visible(cx, "model-setup")?);
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::PartnerDefaultChanged { settings }, cx);
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    assert!(!visible(cx, "model-setup")?);

                    runner.update(cx, |_, window, cx| {
                        let position = window.shot_element_bounds("summary-model-picker").unwrap().center();
                        window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                        window.dispatch_event(PlatformInput::Scroll(gpui_kit::ScrollEvent {
                            position, delta: gpui_kit::ScrollDelta::Pixels(point(px(0.), px(-900.))),
                            ..Default::default()
                        }), cx);
                    })?;
                    runner.frame(cx)?;
                    for id in ["agent-model-literature", "agent-model-review-style", "agent-model-review-math"] {
                        assert!(visible(cx, id)?, "missing {id}");
                    }
                    Ok(())
                })();
                report.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
    result.recv().unwrap().unwrap();
}
