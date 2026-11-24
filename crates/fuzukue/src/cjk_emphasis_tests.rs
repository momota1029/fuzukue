use gpui_kit::base::text::cjk_emphasis::apply;
use markdown::{
    mdast::{Node, Text},
    unist::{Point, Position},
};

fn parse(source: &str) -> Node {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.constructs.frontmatter = true;
    markdown::to_mdast(source, &options).unwrap()
}

fn recovered(source: &str) -> Node {
    let mut root = parse(source);
    apply(&mut root, source);
    root
}

fn nodes(root: &Node) -> impl Iterator<Item = &Node> {
    let mut pending = vec![root];
    std::iter::from_fn(move || {
        let next = pending.pop()?;
        if let Some(children) = next.children() {
            pending.extend(children.iter().rev());
        }
        Some(next)
    })
}

fn raw<'a>(node: &Node, source: &'a str) -> &'a str {
    let position = node.position().unwrap();
    &source[position.start.offset..position.end.offset]
}

#[test]
fn recovers_the_reported_japanese_sentence() {
    let source = "しかも**「監視を始めたスレッド」に結び付ける**。そのスレッドがアーカイブ（や削除）されていたら、次のイベントで**新しく立ち上げて**、結び付け直す";
    let root = recovered(source);
    let bold = nodes(&root)
        .filter(|node| matches!(node, Node::Strong(_)))
        .map(|node| node.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        bold,
        ["「監視を始めたスレッド」に結び付ける", "新しく立ち上げて"]
    );
    assert!(!root.to_string().contains("**"));
}

#[test]
fn recovers_japanese_chinese_and_korean_delimiters() {
    for (source, content) in [
        ("**`code` を直す**です", "code を直す"),
        ("**（注）**です", "（注）"),
        ("**【重要】**確認", "【重要】"),
        ("**“引用”**是对的", "“引用”"),
        ("这是**（括号）**文字", "（括号）"),
        ("\"**（括号）**\"文字", "（括号）"),
        ("**\"인용\"**을 읽는다", "\"인용\""),
        ("**(괄호)**는", "(괄호)"),
        ("한**(注)**글", "(注)"),
        ("ㄅ**(注)**ㄆ", "(注)"),
        ("𠀀**(注)**𠀁", "(注)"),
        ("前**「注」** and more", "「注」"),
    ] {
        let root = recovered(source);
        let bold = nodes(&root)
            .find(|node| matches!(node, Node::Strong(_)))
            .unwrap();
        assert_eq!(bold.to_string(), content, "{source}");
        assert!(raw(bold, source).starts_with("**") && raw(bold, source).ends_with("**"));
    }
    for source in ["*「斜体」*です", "~~「消す」~~と"] {
        let root = recovered(source);
        assert!(nodes(&root).any(|node| matches!(node, Node::Emphasis(_) | Node::Delete(_))));
        assert!(!root.to_string().contains(['*', '~']));
    }
}

#[test]
fn recovers_in_inline_parents_without_pairing_across_them() {
    let source = "# 前**「見出し」**後\n\n- 前**「項目」**後\n\n| 前**「表」**後 |\n| - |\n\n> 前**「引用」**後\n\n[前**「リンク先」**後](/url)";
    let root = recovered(source);
    let bold = nodes(&root)
        .filter(|node| matches!(node, Node::Strong(_)))
        .map(|node| raw(node, source))
        .collect::<Vec<_>>();
    assert_eq!(
        bold,
        [
            "**「見出し」**",
            "**「項目」**",
            "**「表」**",
            "**「引用」**",
            "**「リンク先」**"
        ]
    );

    let source = "- 前**「別\n- 項目」**後\n\n|前**「別|セル」**後|\n|-|-|";
    assert_eq!(recovered(source), parse(source));
}

#[test]
fn pairs_across_inline_nodes_and_nests_emphasis() {
    for (source, is_inner) in [
        ("**外側 *内側* 外側**", "italic"),
        ("前**「外 *「内」*側」**後", "italic"),
        ("前**`code`**です", "code"),
        ("前**[リンク](/url)**です", "link"),
        ("前**![画像](/image)**です", "image"),
    ] {
        let root = recovered(source);
        let bold = nodes(&root)
            .find(|node| matches!(node, Node::Strong(_)))
            .unwrap();
        assert!(
            bold.children().unwrap().iter().any(|child| matches!(
                (is_inner, child),
                ("italic", Node::Emphasis(_))
                    | ("code", Node::InlineCode(_))
                    | ("link", Node::Link(_))
                    | ("image", Node::Image(_))
            )),
            "{source}: {bold:?}"
        );
    }
}

#[test]
fn preserves_original_byte_and_line_positions_when_splitting() {
    let source = "> 前🙂**「x &amp;\r\n> 注」**後";
    let root = recovered(source);
    let quote = root.children().unwrap().first().unwrap();
    let paragraph = quote.children().unwrap().first().unwrap();
    let [before, bold, after] = paragraph.children().unwrap().as_slice() else {
        panic!("expected surrounding text and recovered emphasis: {paragraph:?}");
    };
    assert_eq!(raw(before, source), "前🙂");
    assert_eq!(raw(bold, source), "**「x &amp;\r\n> 注」**");
    assert_eq!(raw(after, source), "後");
    assert_eq!(bold.position().unwrap().start, Point::new(1, 10, 9));
    assert_eq!(bold.position().unwrap().end, Point::new(2, 11, 33));
    let content = &bold.children().unwrap()[0];
    assert_eq!(raw(content, source), "「x &amp;\r\n> 注」");
    assert!(content.to_string().contains('&'));
}

#[test]
fn only_literal_runs_are_eligible() {
    for source in [
        r"\*\*「x」\*\*",
        r"前\*\*「x」**後",
        r"前**「x」\*\*後",
        r"前\~~「x」~~後",
        "前&ast;&ast;「x」&#42;&#x2a;後",
        "前**「x」&ast;*後",
        "前&#42;*「x」**後",
        "前**「閉じていない",
        "前** 「空白」**後",
        "前**「空白」 **後",
        "前__「x」__後",
        "日本語\n\na**b**c 2**3 a**(b)**c snake_case\n\n**\n\n- x",
    ] {
        assert_eq!(recovered(source), parse(source), "{source}");
    }
    let source = r"前\\**「x」**後";
    let root = recovered(source);
    assert!(nodes(&root).any(|node| matches!(node, Node::Strong(_))));
    assert_eq!(root.to_string(), r"前\「x」後");

    for source in [r"前\***「x」**後", "前&ast;**「x」**後"] {
        let root = recovered(source);
        let strong = nodes(&root)
            .find(|node| matches!(node, Node::Strong(_)))
            .unwrap();
        assert_eq!(raw(strong, source), "**「x」**");
        assert_eq!(root.to_string(), "前*「x」後");
    }
}

#[test]
fn leaves_opaque_syntax_and_ascii_only_trees_unchanged() {
    for source in [
        "a**b**c, a**(b)**c, 2**3, snake_case.\n\n**\n\n- a\n- b",
        "`前**「コード」**後` と $前**「数式」**後$",
        "```text\n前**「コード」**後\n```\n\n$$\n前**「数式」**後\n$$",
        "<!-- 前**「HTML」**後 -->\n\n[前](https://example.test/前**「x」**後)",
        "<https://example.test/前**「x」**後>",
        "---\nlabel: 前**「YAML」**後\n---\n\n[^前**「脚注」**後]: 本文",
    ] {
        assert_eq!(recovered(source), parse(source), "{source}");
    }
}

#[test]
fn long_document_pass_is_fast() {
    // Feed literal runs directly, so the timing measures only this pass.
    let source = "前**「🙂」**後  ".repeat(10_000);
    let text = Node::Text(Text {
        value: source.clone(),
        position: Some(Position::new(1, 1, 0, 1, source.len() + 1, source.len())),
    });
    let mut root = Node::Paragraph(markdown::mdast::Paragraph {
        children: vec![text],
        position: None,
    });
    let start = std::time::Instant::now();
    apply(&mut root, &source);
    let elapsed = start.elapsed();
    assert_eq!(
        nodes(&root)
            .filter(|node| matches!(node, Node::Strong(_)))
            .count(),
        10_000
    );
    if !cfg!(debug_assertions) {
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "{elapsed:?}"
        );
    }
}
