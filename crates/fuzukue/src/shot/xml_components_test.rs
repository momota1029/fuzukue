use super::*;
use fuzukue_proto::{Actor, ReviewFindingsOutcome};

const MESSAGE: &str = include_str!("../../../../tests/shots/fixtures/xml-components.md");
const SESSION: &str = "xml-components-fixture";
const DECK: &str = "research/concentration";

#[test]
fn xml_components_render_only_after_the_stream_closes_the_fence() {
    let offset = |tag: &str| MESSAGE.find(&format!("~~~fuzukue-v1\n<{tag}")).unwrap();
    let callout = format!("xml-component-callout-{}", offset("callout"));
    let table = format!("xml-component-table-{}", offset("table"));
    let finding = format!("xml-component-finding-{}", offset("finding"));
    let preview = format!("xml-component-finding-preview-{}", offset("finding"));
    let split = MESSAGE.find("</callout>\n~~~").unwrap() + "</callout>\n~~~".len();
    let (prefix, suffix) = MESSAGE.split_at(split);
    let delta = |text: &str| ServerEvent::TextDelta {
        session: SESSION.into(),
        item: "reply".into(),
        actor: Actor::Claude,
        text: text.into(),
    };
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 1000.).unwrap();
            view.update(cx, |view, cx| {
                view.switch_session(SESSION.into(), cx);
                view.on_event(
                    ServerEvent::Workspace {
                        tabs: vec![],
                        deck: DECK.into(),
                    },
                    cx,
                );
                view.on_event(delta(prefix), cx);
            });

            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle,
                    view,
                    target: None,
                    output: PathBuf::new(),
                    read_only: false,
                    seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    runner
                        .step(
                            Step::WaitElement {
                                id: "chat-code-copy".into(),
                            },
                            cx,
                        )
                        .await?;
                    runner.update(cx, |_, window, _| {
                        assert!(window.shot_element_bounds(&callout).is_none());
                    })?;

                    runner.update(cx, |view, _, cx| view.on_event(delta(suffix), cx))?;
                    for id in [&callout, &table, &finding] {
                        runner
                            .step(Step::WaitElement { id: id.clone() }, cx)
                            .await?;
                    }
                    runner.update(cx, |_, window, _| {
                        assert!(window.shot_element_bounds(&preview).is_none());
                        assert!(window.shot_element_bounds("chat-code-copy").is_some());
                    })?;

                    let outcome: ReviewFindingsOutcome =
                        serde_json::from_value(serde_json::json!({
                            "kind": "loaded",
                            "snapshot": {
                                "open_count": 1, "card_open_counts": {},
                                "findings": [{
                                    "id": "cb38ac8b-d670-47be-91e3-9c4ecdb61d26",
                                    "summary": "Clarify the dependence on the number of factors",
                                    "detail": "A uniform constant is needed.",
                                    "severity": "major", "status": "open", "locations": [],
                                    "sources": [], "transitions": []
                                }]
                            }
                        }))?;
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::ReviewFindings {
                                deck: DECK.into(),
                                outcome,
                            },
                            cx,
                        );
                    })?;
                    runner
                        .step(
                            Step::WaitElement {
                                id: preview.clone(),
                            },
                            cx,
                        )
                        .await?;
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::Workspace {
                                tabs: vec![],
                                deck: "research/other".into(),
                            },
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    runner.update(cx, |_, window, _| {
                        assert!(window.shot_element_bounds(&preview).is_none());
                    })?;
                    Ok(())
                }
                .await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}
