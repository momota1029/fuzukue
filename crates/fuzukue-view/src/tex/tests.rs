use super::{read_card_tex, BTreeMap, DocReference, DocReferences, TexMarkdown, ITEM_LABEL_PREFIX};
use ::markdown::{mdast::Node, Constructs, ParseOptions};

fn rendered(markdown: &str) -> Node {
    ::markdown::to_mdast(
        markdown,
        &ParseOptions {
            constructs: Constructs {
                math_flow: true,
                math_text: true,
                ..Constructs::gfm()
            },
            ..ParseOptions::default()
        },
    )
    .unwrap()
}

fn walk(node: &Node) -> Box<dyn Iterator<Item = &Node> + '_> {
    Box::new(std::iter::once(node).chain(node.children().into_iter().flatten().flat_map(walk)))
}

fn plain(node: &Node) -> String {
    walk(node)
        .filter_map(|node| match node {
            Node::Text(text) => Some(text.value.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn labelled_enumerate_keeps_math_in_its_items() {
    // The layout of (c), (d), and (e) in a nested list,
    // with the supported display delimiters represented in the same list.
    let tex = r"
    次が成り立つ．
    \begin{enumerate}
        \item[(c)] 評価 $a\le b$ が成り立つ．
        \[
            a\le b
        \]
        \item[(d)] \(b=c\) とおくと
        $$
            c=d
        $$
        続きの本文．
        \begin{equation*}
            a=d
        \end{equation*}
        \item[(e)] 最後に $d=e$ について
        \begin{align*}
            x&=y\\
            y&=z
        \end{align*}
    \end{enumerate}
    ";
    let md = read_card_tex(tex, None, &[]);
    let tree = rendered(&md.markdown);
    assert!(!walk(&tree).any(|node| matches!(node, Node::List(_))));
    let items: Vec<_> = walk(&tree)
        .filter(|node| matches!(node, Node::Blockquote(_)))
        .collect();
    assert_eq!(items.len(), 3);
    for (item, (label, displays)) in items.into_iter().zip([("(c)", 1), ("(d)", 2), ("(e)", 1)]) {
        let rendered_label = walk(item).find_map(|node| match node {
            Node::InlineCode(code) => code.value.strip_prefix(ITEM_LABEL_PREFIX),
            _ => None,
        });
        assert_eq!(rendered_label, Some(label));
        assert_eq!(
            walk(item)
                .filter(|node| matches!(node, Node::Math(_)))
                .count(),
            displays,
            "{}",
            md.markdown
        );
        assert_eq!(
            walk(item)
                .filter(|node| matches!(node, Node::InlineMath(_)))
                .count(),
            1
        );
    }
    assert!(!walk(&tree).any(|node| matches!(node, Node::Code(_))));
    assert!(md.markdown.contains(r"\begin{aligned}x&=y\\"));
}

#[test]
fn ordered_nested_description_code_and_quote() {
    let tex = r"
\begin{enumerate}[start=3]
  \item
    \begin{description}
      \item[Name] a value
      \begin{minted}{rust}
      if ok {
          println!(`x`);
      }
      \end{minted}
    \end{description}
  \item
    \begin{quote}A \textbf{bold} word.\end{quote}
\end{enumerate}
\begin{enumerate}[label=(\alph*),start=3]
    \item First.
    \item Second.
\end{enumerate}

\begin{lstlisting}[language=Python]
# a literal \emph{command}
    do_something()
\end{lstlisting}
\begin{verbatim}
a % b $ c ```
\end{verbatim}
\begin{center}Centered content\end{center}
";
    let md = read_card_tex(tex, None, &[]);
    let tree = rendered(&md.markdown);
    let mut nodes = walk(&tree);
    let first_list = nodes
        .find_map(|node| match node {
            Node::List(list) => Some(list),
            _ => None,
        })
        .unwrap();
    assert_eq!(first_list.start, Some(3));
    assert_eq!(first_list.children.len(), 2);
    assert!(walk(&first_list.children[0]).any(|node| matches!(node, Node::Blockquote(_))));
    assert!(walk(&first_list.children[1]).any(|node| matches!(node, Node::Blockquote(_))));
    let code = walk(&tree)
        .filter_map(|node| match node {
            Node::Code(code) => Some((code.lang.as_deref(), code.value.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        code,
        vec![
            (Some("rust"), "if ok {\n    println!(`x`);\n}"),
            (
                Some("python"),
                "# a literal \\emph{command}\n    do_something()"
            ),
            (None, "a % b $ c ```"),
        ],
        "{}",
        md.markdown
    );
    let plain = plain(&tree);
    for label in ["Name", "(c)", "(d)"] {
        assert!(walk(&tree).any(|node| matches!(node, Node::InlineCode(code)
            if code.value.strip_prefix(ITEM_LABEL_PREFIX) == Some(label))));
    }
    assert!(plain.contains("First."));
    assert!(plain.contains("Second."));
    assert!(plain.contains("Centered content"));
}

#[test]
fn tex_text_is_literal_and_unknown_commands_keep_their_content() {
    let tex = r"
        # literal *stars* _under_ ---
        1. no Markdown list
        \par
        A% \fake command
        B \% \& \_ \{ \} a~b\,c
        \emph{重点} \textit{斜体} \textbf{太字} \texttt{a_b} \mathrm{立体}
        \footnote{説明に $x$} \unknown{引数} \begin{other}内容\end{other}
    ";
    let md = read_card_tex(tex, None, &[]);
    let tree = rendered(&md.markdown);
    assert!(
        !walk(&tree).any(|node| matches!(node, Node::Code(_) | Node::Heading(_) | Node::List(_)))
    );
    let text = plain(&tree);
    for part in [
        "# literal *stars* _under_ — 1. no Markdown list",
        "AB % & _ { } a\u{a0}b\u{2009}c",
        "立体",
        "〔注 説明に ",
        "⟨unknown⟩引数",
        "⟨other⟩内容",
    ] {
        assert!(text.contains(part), "{part:?} absent from {text:?}");
    }
    assert!(!text.contains("fake"));
}

#[test]
fn card_sections_keep_literal_headings_in_code() {
    for literal in ["\\begin{verbatim}\n## Proof\n\\end{verbatim}", "~~~~sh\n## Proof\n~~~~"] {
        let card = format!("## Statement\n{literal}\n## Proof\nProof text.");
        let sections = crate::card_section_ranges(&card);
        assert_eq!(sections.len(), 2);
        let statement = read_card_tex(&card[sections[0].1.clone()], None, &[]);
        assert!(matches!(
            walk(&rendered(&statement.markdown)).find(|node| matches!(node, Node::Code(_))),
            Some(Node::Code(code)) if code.value == "## Proof"
        ));
        assert_eq!(&card[sections[1].1.clone()], "Proof text.");
    }
}

#[test]
fn protects_inline_code_before_parsing_tex_groups_and_math() {
    let input = "$5 \\textbf{``前} $HOME\n[[literal]]`` と $x^2$}";
    let display = read_card_tex(input, None, &[]);
    let literals = "``前} $HOME\n[[literal]]``";
    assert!(display.markdown.contains(literals));
    let start = display.markdown.find("$HOME").unwrap();
    let original = input.find("$HOME").unwrap();
    assert_eq!(display.tex_range(start..start + 5), Some(original..original + 5));
    let tree = rendered(&display.markdown);
    assert!(walk(&tree).any(|node| matches!(node, Node::InlineCode(code) if code.value == "前} $HOME\n[[literal]]")));
    let math: Vec<_> = walk(&tree).filter_map(|node| match node {
        Node::InlineMath(math) => Some(math.value.as_str()),
        _ => None,
    }).collect();
    assert_eq!(math, ["x^2"]);
}

#[test]
fn references_are_resolved_and_mapped_to_original_source() {
    let reference = |kind: &str, number: &str, card: Option<&str>| DocReference {
        kind: kind.into(),
        number: number.into(),
        card: card.map(str::to_owned),
        page: String::new(),
        title: String::new(),
    };
    let refs = DocReferences {
        labels: BTreeMap::from([
            ("custom".into(), reference("定理", "3.2", Some("x"))),
            ("eq:main".into(), reference("Equation", "4", None)),
        ]),
        citations: BTreeMap::from([("source".into(), "7".into())]),
    };
    let card = "## Statement\n日本語 日本語 \\Cref{custom} \\cref{custom} \\ref{custom} \\eqref{eq:main} \\cite[p.~3]{source}";
    let mut md = TexMarkdown::default();
    for (_, range) in crate::card_section_ranges(card) {
        md.append(
            read_card_tex(&card[range.clone()], Some(&refs), &[]),
            range.start,
        );
    }
    let tree = rendered(&md.markdown);
    assert!(plain(&tree).contains("定理 3.2 定理 3.2 3.2 (4) [7, p.\u{a0}3]"));
    assert_eq!(
        walk(&tree)
            .filter(|node| matches!(node, Node::Link(link) if link.url == "fuzukue-card:x"))
            .count(),
        3
    );
    let second = md.markdown.rfind("日本語").unwrap();
    let range = md.tex_range(second..second + "日本".len()).unwrap();
    assert_eq!(range.start, card.rfind("日本語").unwrap());
    assert_eq!(&card[range.clone()], "日本");
    assert_eq!(
        md.markdown_range(range).unwrap(),
        second..second + "日本".len()
    );
    let link = md.markdown.find("定理 3").unwrap();
    let range = md.tex_range(link..link + "定理".len()).unwrap();
    assert_eq!(&card[range], r"\Cref{custom}");
}

#[test]
#[ignore = "Run with FUZUKUE_TEX_TEST_CARDS pointing to the harness cards directory"]
fn compact_target_gds_cards_parse() {
    use std::{fs, path::Path};
    fn cards(path: &Path, count: &mut usize) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                cards(&path, count);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                *count += 1;
                let source = fs::read_to_string(&path).unwrap();
                let (_, body) = source
                    .trim_start_matches('\u{feff}')
                    .split_once('\n')
                    .unwrap()
                    .1
                    .split_once("\n---\n")
                    .unwrap();
                for (_, range) in crate::card_section_ranges(body) {
                    let md = read_card_tex(&body[range], None, &[]);
                    let parsed = rendered(&md.markdown);
                    for node in walk(&parsed) {
                        if let Node::Text(text) = node {
                            assert!(
                                !text.value.as_bytes().windows(2).any(
                                    |bytes| bytes[0] == b'\\' && bytes[1].is_ascii_alphabetic()
                                ),
                                "{}: literal command in {:?}",
                                path.display(),
                                text.value,
                            );
                        }
                        assert!(
                            !matches!(node, Node::Code(_)),
                            "{}: unexpected code block",
                            path.display()
                        );
                    }
                    if path.file_stem().is_some_and(|stem| stem == "fp-lem-law")
                        && md.markdown.contains(&format!("{ITEM_LABEL_PREFIX}(e)"))
                    {
                        let items = walk(&parsed)
                            .filter(|node| matches!(node, Node::Blockquote(quote)
                                if quote.children.first().is_some_and(|first| walk(first).any(|node|
                                    matches!(node, Node::InlineCode(code) if code.value.starts_with(ITEM_LABEL_PREFIX))))))
                            .collect::<Vec<_>>();
                        assert_eq!(items.len(), 5);
                        assert_eq!(
                            walk(items[2])
                                .filter(|node| matches!(node, Node::Math(_)))
                                .count(),
                            2
                        );
                        assert_eq!(
                            walk(items[3])
                                .filter(|node| matches!(node, Node::Math(_)))
                                .count(),
                            2
                        );
                        assert_eq!(
                            walk(items[4])
                                .filter(|node| matches!(node, Node::Math(_)))
                                .count(),
                            1
                        );
                    }
                }
            }
        }
    }
    let directory = fuzukue_proto::compat::env_os("FUZUKUE_TEX_TEST_CARDS").expect("FUZUKUE_TEX_TEST_CARDS");
    let mut count = 0;
    cards(Path::new(&directory), &mut count);
    assert!(count >= 100, "checked {count} cards");
    eprintln!("Checked {count} cards");
}
