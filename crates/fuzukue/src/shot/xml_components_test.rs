use super::*;
use fuzukue_proto::{Actor, ReviewFindingsOutcome};

const MESSAGE: &str = include_str!("../../../../tests/shots/fixtures/xml-components.md");
const UI_KIT: &str = include_str!("../../../../tests/shots/fixtures/ui-kit.md");
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

#[test]
fn bare_namespaced_xml_uses_commonmark_autolink_rules() {
    use gpui_kit::base::text::parse_markdown_ast;
    use markdown::mdast::Node;

    let parse = |source| {
        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        parse_markdown_ast(source, &options).unwrap()
    };
    let root = parse("<fuzukue:table>");
    let [Node::Paragraph(paragraph)] = root.children().unwrap().as_slice() else {
        panic!("{root:?}");
    };
    assert!(matches!(&paragraph.children[..], [Node::Link(link)] if link.url == "fuzukue:table"));
    for literal in ["</fuzukue:table>", r#"<fuzukue:table title="Example">"#] {
        let root = parse(literal);
        let [Node::Paragraph(paragraph)] = root.children().unwrap().as_slice() else {
            panic!("{root:?}");
        };
        assert!(matches!(&paragraph.children[..], [Node::Text(text)] if text.value == literal));
    }
    let source = "~~~fuzukue-v1\n<fuzukue:table>\n~~~\n";
    let root = parse(source);
    assert!(matches!(&root.children().unwrap()[..], [Node::Code(code)]
        if code.value == "<fuzukue:table>"));
}

#[test]
fn ui_kit_lays_out_nested_components_and_toggles_details() {
    let closed_details =
        "~~~fuzukue-v1\n<fuzukue:details title=\"Closed\">Notes</fuzukue:details>\n~~~\n";
    let open_details = "~~~fuzukue-v1\n<fuzukue:details title=\"Open\" open=\"true\">Notes</fuzukue:details>\n~~~\n";
    let reply = format!("{UI_KIT}\n{closed_details}\n{open_details}");
    let card_offset = reply.find("~~~fuzukue-v1").unwrap();
    let id = move |tag, path| format!("xml-component-{tag}-{card_offset}{path}");
    let details_offset = reply.find(closed_details).unwrap();
    let details_toggle = format!("xml-component-details-{details_offset}-toggle");
    let details_body = format!("xml-component-details-{details_offset}-body");
    let open_offset = reply.find(open_details).unwrap();
    let open_body = format!("xml-component-details-{open_offset}-body");
    let seed_reply = |view: &Entity<Workbench>, reply: &str, cx: &mut App| {
        view.update(cx, |view, cx| {
            view.switch_session(SESSION.into(), cx);
            view.on_event(
                ServerEvent::TextDelta {
                    session: SESSION.into(),
                    item: "ui-kit".into(),
                    actor: Actor::Claude,
                    text: reply.into(),
                },
                cx,
            );
        });
    };
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 1000.).unwrap();
            seed_reply(&view, &reply, cx);
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
                            Step::Theme {
                                theme: ShotTheme::Dark,
                            },
                            cx,
                        )
                        .await?;
                    let stat_a = id("stat", "-0-0");
                    let stat_b = id("stat", "-0-1");
                    let side_by_side = |_: &Workbench, window: &Window| {
                        window
                            .shot_element_bounds(&stat_a)
                            .zip(window.shot_element_bounds(&stat_b))
                            .is_some_and(|(a, b)| a.right() <= b.left() && a.top() == b.top())
                    };
                    runner
                        .wait_for(cx, "two stat columns", TIMEOUT_MS, side_by_side)
                        .await?;
                    runner.update(cx, |_, window, _| {
                        let bounds = |id: String| {
                            window
                                .shot_element_bounds(&id)
                                .with_context(|| format!("missing UI kit element {id}"))
                        };
                        let progress_id = id("progress", "-1");
                        let card = bounds(id("card", ""))?;
                        let progress = bounds(format!("{progress_id}-track"))?;
                        let fill = bounds(format!("{progress_id}-fill"))?;
                        let divider = bounds(id("divider", "-2"))?;
                        let list = bounds(id("list", "-4"))?;
                        assert!(card.top() < progress.top() && progress.bottom() <= divider.top());
                        assert!(divider.bottom() <= list.top() && list.bottom() <= card.bottom());
                        assert!(
                            (fill.size.width - progress.size.width * (10. / 13.)).abs() <= px(1.)
                        );
                        assert!(window.shot_element_bounds(&details_body).is_none());
                        assert!(window.shot_element_bounds(&open_body).is_some());
                        Ok::<_, anyhow::Error>(())
                    })??;
                    runner.click(cx, ClickTarget::Element { id: details_toggle })?;
                    runner
                        .step(
                            Step::WaitElement {
                                id: details_body.clone(),
                            },
                            cx,
                        )
                        .await?;
                    runner.key(cx, "space")?;
                    cx.update_window(runner.handle, |_, window, cx| {
                        let keystroke = Keystroke::parse("space").unwrap();
                        window.dispatch_event(
                            PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }),
                            cx,
                        );
                    })?;
                    runner
                        .wait_for(cx, "details close via keyboard", TIMEOUT_MS, |_, window| {
                            window.shot_element_bounds(&details_body).is_none()
                        })
                        .await?;
                    let (handle, view) = cx.update(|cx| {
                        let (handle, view) = open_workbench_at_size(cx, 480., 1000.)?;
                        seed_reply(&view, UI_KIT, cx);
                        Ok::<_, anyhow::Error>((handle, view))
                    })?;
                    runner.handle = handle;
                    runner.view = view;
                    runner
                        .step(
                            Step::Theme {
                                theme: ShotTheme::Light,
                            },
                            cx,
                        )
                        .await?;
                    runner
                        .wait_for(
                            cx,
                            "stat tiles stack in narrow chat",
                            TIMEOUT_MS,
                            |_, window| {
                                window
                                    .shot_element_bounds(&stat_a)
                                    .zip(window.shot_element_bounds(&stat_b))
                                    .is_some_and(|(a, b)| {
                                        a.bottom() <= b.top() && a.left() == b.left()
                                    })
                            },
                        )
                        .await?;
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
