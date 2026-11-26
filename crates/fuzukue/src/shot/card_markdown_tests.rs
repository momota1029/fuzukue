use super::*;
use fuzukue_proto::{DocItem, DocSection};
use gpui_kit::base::text::editor_host::{parse_markdown, BlockNode};

const DECK: &str = "papers/reader";
const CARD: &str = "markdown-design";
const PATH: &str = "papers/reader/cards/markdown-design.md";
const FIXTURE: &str = include_str!(
    "../../../../tests/shots/fixtures/card-markdown/papers/reader/cards/markdown-design.md"
);

#[test]
fn lazy_card_body_renders_markdown_in_the_reader_and_detail() {
    let (_, body) = fuzukue_cards::split_frontmatter(FIXTURE).unwrap();
    let summary = serde_json::from_value(serde_json::json!({
        "id": CARD, "kind": "design", "title": "Markdown card", "status": "",
        "path": PATH, "body": "", "links": [], "backlinks": [], "files": [], "external": false
    }))
    .unwrap();
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1120., 900.).unwrap();
            view.update(cx, |view, cx| {
                view.switch_session("fixture".into(), cx);
                view.on_event(
                    ServerEvent::Workspace {
                        tabs: vec![],
                        deck: DECK.into(),
                    },
                    cx,
                );
                view.apply_deck_cards(DECK, vec![summary], vec![], cx);
                view.on_event(
                    ServerEvent::Document {
                        title: "Card Markdown fixture".into(),
                        macros: vec![],
                        math_macros: vec![],
                        references: None,
                        sections: vec![DocSection {
                            title: "Reader".into(),
                            items: vec![DocItem::Card { id: CARD.into() }],
                            prose: vec![],
                            unmatched: false,
                        }],
                    },
                    cx,
                );
                view.open_paper_deck(DECK.into(), cx);
                assert!(view.read_md.is_empty());
                view.receive_card_content(CARD.into(), fuzukue_proto::CardContent {
                    body: body.into(), edit_body: None, edit_proof: None,
                }, cx);
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
                        .step(Step::ScrollToCard { id: CARD.into() }, cx)
                        .await?;
                    runner.update(cx, |view, window, cx| {
                        let part = crate::ReadPart {
                            card: CARD.into(),
                            proof: false,
                        };
                        let read = &view.read_md[&part];
                        assert_markdown_blocks(&read.display.markdown);
                        let rendered = read.view.read(cx).rendered_text();
                        assert!(rendered.as_str().contains("Restore the previous version."));
                        assert!(!rendered.as_str().contains("###"));
                        assert!(!rendered.as_str().contains("| Stage |"));
                        assert!(window.shot_element_bounds("chat-code-copy").is_some());
                    })?;

                    runner
                        .step(
                            Step::DragFromElement {
                                id: "formula:x^2".into(),
                                dx: 170.,
                                dy: 0.,
                            },
                            cx,
                        )
                        .await?;
                    runner.frame(cx)?;
                    runner.update(cx, |view, _, _| {
                        let target = &view.comment_popup.as_ref().expect("selection comment").0;
                        assert_eq!(target.quote.as_deref(), Some("$x^2$ before rollout."));
                        let source = target.source.as_ref().expect("card source anchor");
                        assert_eq!(&body[source.range.clone()], "$x^2$ before rollout.");
                    })?;

                    runner.click(
                        cx,
                        ClickTarget::Element {
                            id: format!("read-head-{CARD}"),
                        },
                    )?;
                    runner
                        .wait_for(cx, "Markdown card detail", TIMEOUT_MS, |view, window| {
                            view.right == RightPane::Card
                                && view.selected.as_deref() == Some(CARD)
                                && window.shot_element_bounds("formula:x^2").is_some()
                                && window.shot_element_bounds("chat-code-copy").is_some()
                        })
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

fn assert_markdown_blocks(source: &str) {
    let blocks = parse_markdown(source, math::extensions().into()).unwrap();
    let [BlockNode::Heading { level: 2, .. }, BlockNode::Heading { level: 3, .. }, BlockNode::Paragraph(prose), BlockNode::List {
        ordered: false,
        children,
        ..
    }, BlockNode::Table(_), BlockNode::Blockquote { .. }, BlockNode::CodeBlock(code), BlockNode::Paragraph(links)] =
        blocks.as_slice()
    else {
        panic!("unexpected Markdown card blocks: {blocks:?}");
    };
    let [BlockNode::ListItem { children, .. }] = children.as_slice() else {
        panic!("expected one bullet item");
    };
    assert!(matches!(children.as_slice(), [
        BlockNode::Paragraph(_),
        BlockNode::List { ordered: true, children, .. },
    ] if children.len() == 2));
    assert!(prose
        .children()
        .iter()
        .any(|node| node.as_custom().is_some()));
    assert_eq!(code.lang().as_deref(), Some("sh"));
    assert!(code.code().contains("'[[not-a-link]]'"));
    let urls: Vec<_> = links
        .children()
        .iter()
        .flat_map(|node| {
            node.mark_runs()
                .iter()
                .filter_map(|(_, mark)| mark.link.as_ref().map(|link| link.url.as_ref()))
        })
        .collect();
    assert_eq!(
        urls,
        [
            "https://example.test/guide",
            "fuzukue-card:markdown-design",
            "fuzukue-file:papers/reader/deck.toml#L2",
        ]
    );
}

fn synthetic_card(id: &str, kind: &str) -> fuzukue_proto::CardView {
    serde_json::from_value(serde_json::json!({
        "id": id, "kind": kind, "title": "", "status": "lean",
        "path": format!("{DECK}/cards/{id}.md"), "body": "", "links": [],
        "backlinks": [], "files": [], "external": false
    })).unwrap()
}

fn paper_document(ids: &[String]) -> ServerEvent {
    ServerEvent::Document {
        title: "Synthetic deck".into(),
        macros: vec![],
        math_macros: vec![],
        references: None,
        sections: vec![DocSection {
            title: "Cards".into(),
            items: ids.iter().map(|id| DocItem::Card { id: id.clone() }).collect(),
            prose: vec![],
            unmatched: false,
        }],
    }
}

fn run_card_reader_test(
    test: impl AsyncFnOnce(&mut Runner, &mut AsyncApp) -> Result<()> + 'static,
) {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1120., 900.).unwrap();
            view.update(cx, |view, cx| {
                view.switch_session("synthetic".into(), cx);
                view.on_event(ServerEvent::Workspace { tabs: vec![], deck: DECK.into() }, cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                result.send(test(&mut runner, cx).await).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn manuscript_prose_renders_in_order_before_and_between_lazy_cards() {
    use fuzukue_proto::{DocProse, DocProseText};

    run_card_reader_test(async |runner, cx| {
        let ids = ["first".to_owned(), "second".to_owned(), "unplaced".to_owned()];
        let opening = DocProseText::Tex(r"\begin{abstract}A short abstract with $x^2$.\end{abstract}".into());
        let between = DocProseText::Tex("Prose between cards.".into());
        runner.update(cx, |view, _, cx| {
            let cards = ids.iter().map(|id| synthetic_card(id, "theorem")).collect();
            view.apply_deck_cards(DECK, cards, vec![], cx);
            let mut document = paper_document(&ids[..2]);
            let ServerEvent::Document { sections, .. } = &mut document else { unreachable!() };
            sections.push(DocSection {
                title: String::new(),
                items: vec![DocItem::Card { id: ids[2].clone() }],
                prose: vec![],
                unmatched: true,
            });
            sections.insert(0, DocSection {
                title: String::new(), items: vec![],
                prose: vec![DocProse { before_item: 0, text: opening.clone() }],
                unmatched: false,
            });
            sections[1].prose.push(DocProse { before_item: 1, text: between.clone() });
            view.on_event(document, cx);
            assert!(view.read_prose.is_empty() && view.read_md.is_empty());
            assert!(matches!(view.read_rows.as_slice(), [
                crate::ReadRow::Title(_), crate::ReadRow::Prose(front), crate::ReadRow::Section(_),
                crate::ReadRow::Card(first), crate::ReadRow::Prose(middle), crate::ReadRow::Card(second),
                crate::ReadRow::Unmatched, crate::ReadRow::Card(unplaced),
            ] if front == &opening && first == &ids[0] && middle == &between
                && second == &ids[1] && unplaced == &ids[2]));
            view.open_paper_deck(DECK.into(), cx);
        })?;
        runner.wait_for(cx, "manuscript prose", TIMEOUT_MS, |view, window| {
            view.read_prose.len() == 2 && window.shot_element_bounds("formula:x^2").is_some()
        }).await?;
        Ok(())
    });
}

#[test]
fn harness_card_typesets_argument_macros_and_names_unknown_macros() {
    let id = "sample-definition".to_string();
    run_card_reader_test(async move |runner, cx| {
        let known = r"\sampleWrap{x}=7";
        let unknown = r"a + \sampleMissing{b} = c";
        runner.update(cx, |view, _, cx| {
            let mut card = synthetic_card(&id, "definition");
            card.lean = ["Synthetic.Item", "Synthetic.value", "Synthetic.other"].map(str::to_owned).to_vec();
            card.body = format!("## Statement\nBefore $x$.\n\\[\n{known}\n\\]\n\\[\n{unknown}\n\\]\nAfter.\n");
            view.apply_deck_cards(DECK, vec![card], vec![], cx);
            let mut document = paper_document(std::slice::from_ref(&id));
            let ServerEvent::Document { math_macros, references, .. } = &mut document else { unreachable!() };
            math_macros.push(fuzukue_proto::MathMacro {
                name: "sampleWrap".into(), body: r"\left(#1\right)".into(), arguments: 1,
            });
            *references = Some(fuzukue_proto::DocReferences {
                labels: [(format!("card:{id}"), fuzukue_proto::DocReference {
                    kind: "Theorem".into(), number: "1.3".into(), page: "1".into(), title: String::new(), card: Some(id.clone()),
                })].into(),
                ..Default::default()
            });
            view.on_event(document, cx);
            let decls = ["Synthetic.Item", "Synthetic.value", "Synthetic.other"];
            view.on_event(ServerEvent::LeanStatus {
                decls: decls.map(|decl| (decl.to_owned(), "defined".into())).to_vec(),
                running: false, doc_findings: vec![],
            }, cx);
            let (label, complete) = crate::paper::lean_card_progress(&view.cards[0], &view.lean).unwrap();
            assert_eq!(label, tr!("paper-lean-checked", ok = "3", n = "3"));
            assert!(complete);
            view.open_paper_deck(DECK.into(), cx);
        })?;
        let formulas_rendered = |_: &Workbench, window: &Window| {
            window.shot_element_bounds(&format!("formula:{known}")).is_some()
                && window.shot_element_bounds(&format!("formula:{unknown}")).is_some()
                && window.shot_element_bounds(r"math-unknown:\sampleMissing").is_some()
        };
        runner.wait_for(cx, "typeset paper card", TIMEOUT_MS, formulas_rendered).await?;
        runner.update(cx, |view, _, cx| view.select(id, cx))?;
        runner.wait_for(cx, "typeset card detail", TIMEOUT_MS, formulas_rendered).await?;
        Ok(())
    });
}

#[test]
fn lazy_paper_reload_keeps_the_card_offset_through_late_math_layout() {
    run_card_reader_test(async |runner, cx| {
        let ids: Vec<_> = (0..18).map(|index| format!("sample-{index:02}")).collect();
        let anchor_id = &ids[7];
        let anchor_part = crate::ReadPart { card: anchor_id.clone(), proof: false };
        let body = "## Statement\n\nA synthetic paragraph with $x^2$.\n\n".to_string()
            + &"More synthetic prose.\n\n".repeat(5);
        let content = |body: String| fuzukue_proto::CardContent { body, edit_body: None, edit_proof: None };
        let (cached, state) = runner.update(cx, |view, _, cx| {
            view.begin_deck_reload(DECK, cx);
            view.install_deck_summaries(DECK, 1, ids.iter().map(|id| synthetic_card(id, "lemma")).collect(), cx);
            view.on_event(paper_document(&ids), cx);
            for id in &ids {
                view.receive_card_content(id.clone(), content(body.clone()), cx);
            }
            view.open_paper_deck(DECK.into(), cx);
            view.read_list.scroll_to(gpui_kit::ListOffset { item_ix: 9, offset_in_item: px(37.) });
            (view.read_md[&anchor_part].view.clone(), view.read_list.clone())
        })?;
        runner.wait_for(cx, "initial reader layout", TIMEOUT_MS, |_, _| state.bounds_for_item(9).is_some()).await?;
        assert_eq!(state.bounds_for_item(9).unwrap().top(), state.viewport_bounds().top() - px(37.));

        let inserted = "sample-inserted".to_string();
        let mut refreshed = ids.clone();
        refreshed.insert(7, inserted.clone());
        runner.update(cx, |view, _, cx| {
            view.begin_deck_reload(DECK, cx);
            view.on_event(paper_document(&refreshed), cx);
            view.install_deck_summaries(DECK, 2, refreshed.iter().map(|id| synthetic_card(id, "lemma")).collect(), cx);
            assert_eq!(view.read_md[&anchor_part].view, cached);
            assert_eq!(view.cards.iter().find(|card| &card.id == anchor_id).unwrap().body, body);
        })?;
        runner.frame(cx)?;
        assert_eq!(state.logical_scroll_top(), gpui_kit::ListOffset { item_ix: 10, offset_in_item: px(37.) });
        let old_max = state.max_offset_for_scrollbar().y;
        runner.update(cx, |view, _, cx| {
            view.receive_card_content(anchor_id.clone(), content(body.clone()), cx);
            assert_eq!(view.read_md[&anchor_part].view, cached);
            view.receive_card_content(inserted, content(format!(
                "## Statement\n\n{}",
                (r"\[".to_owned() + "\n" + r"\frac{\frac{a}{b}}{\frac{c}{d}}" + "\n" + r"\]" + "\n\n").repeat(8)
            )), cx);
        })?;
        runner.wait_for(cx, "late display math changes the height", TIMEOUT_MS, |_, _| {
            state.max_offset_for_scrollbar().y > old_max + px(100.)
        }).await?;
        assert_eq!(state.logical_scroll_top(), gpui_kit::ListOffset { item_ix: 10, offset_in_item: px(37.) });
        assert_eq!(state.bounds_for_item(10).unwrap().top(), state.viewport_bounds().top() - px(37.));
        Ok(())
    });
}

#[test]
fn lazy_reload_keeps_the_open_card_edit_draft_and_source_version() {
    run_card_reader_test(async |runner, cx| {
        let id = "sample-edit".to_string();
        let summary = || synthetic_card(&id, "lemma");
        let (out, messages) = std::sync::mpsc::channel();
        runner.update(cx, |view, _, cx| {
            view.out = out.into();
            view.protocol = fuzukue_proto::LAZY_DECK_PROTOCOL;
            view.begin_deck_reload(DECK, cx);
            view.install_deck_summaries(DECK, 1, vec![summary()], cx);
            assert!(view.cards[0].edit_body.is_none());
            view.on_event(paper_document(std::slice::from_ref(&id)), cx);
            view.receive_card_content(id.clone(), fuzukue_proto::CardContent {
                body: "Loaded source.".into(),
                edit_body: Some(fuzukue_proto::CardEditSource {
                    body: "Loaded source.".into(),
                    base_hash: "original-source-version".into(),
                    tex: Default::default(),
                }),
                edit_proof: None,
            }, cx);
            view.open_paper_deck(DECK.into(), cx);
        })?;
        let button = format!("edit-paper-statement-{id}");
        runner.wait_for(cx, "loaded card edit button", TIMEOUT_MS, |_, window| {
            window.shot_element_bounds(&button).is_some()
        }).await?;
        runner.click(cx, ClickTarget::Element { id: button })?;
        runner.step(Step::Key { key: "Ctrl+a".into() }, cx).await?;
        runner.step(Step::Type { text: "Typed synthetic draft.".into() }, cx).await?;
        runner.update(cx, |view, _, cx| {
            view.begin_deck_reload(DECK, cx);
            view.install_deck_summaries(DECK, 2, vec![summary()], cx);
            view.receive_card_content(id.clone(), fuzukue_proto::CardContent {
                body: "Source changed on disk.".into(),
                edit_body: Some(fuzukue_proto::CardEditSource {
                    body: "Source changed on disk.".into(),
                    base_hash: "updated-source-version".into(),
                    tex: Default::default(),
                }),
                edit_proof: None,
            }, cx);
        })?;
        runner.step(Step::Key { key: "Ctrl+Enter".into() }, cx).await?;
        let ClientMsg::SaveCard { card, body, base_hash, .. } = messages.try_recv()? else {
            bail!("the open editor did not submit its draft");
        };
        assert_eq!(card, id);
        assert_eq!(body, "Typed synthetic draft.");
        assert_eq!(base_hash, "original-source-version");
        Ok(())
    });
}
