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
                        references: None,
                        sections: vec![DocSection {
                            title: "Reader".into(),
                            items: vec![DocItem::Card { id: CARD.into() }],
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
