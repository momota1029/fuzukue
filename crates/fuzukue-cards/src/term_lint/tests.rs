use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::parse_card;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let index = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fuzukue-term-{}-{index}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn open(&self) -> Graph {
        Graph::open(&self.0).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn codes(graph: &Graph, path: &str) -> Vec<&'static str> {
    graph
        .diagnostics
        .iter()
        .filter(|d| d.path == Path::new(path))
        .map(|d| {
            assert_eq!(d.severity, Severity::Warning);
            d.code.unwrap()
        })
        .collect()
}

#[test]
fn definition_terms_come_from_plain_emphasis_in_the_statement() {
    let card = parse_card(
        Path::new("/"),
        Path::new("/def.md"),
        r#"---
id: def
kind: definition
---
## Statement
\emph{中央値} or \textbf { median }. \emph{\(X\)-median}
$\emph{math}$ % \emph{comment}
\label{\emph{label}} \emph{An aside.}
## Proof
\emph{proof}
## Notes
\emph{notes}
"#,
    )
    .unwrap();
    assert_eq!(card.defines, ["中央値", "median"]);
}

#[test]
fn explicit_defines_override_inference_even_when_empty() {
    let deck = Fixture::new();
    deck.write(
        "cards/def/card.md",
        "---\nid: def\nkind: definition\ndefines: []\n---\n",
    );
    deck.write("cards/def/statement.tex", r"\emph{inferred}");
    deck.write(
        "cards/manual.md",
        "---\nid: manual\nkind: definition\ndefines: [Lévy 族]\n---\n\\emph{inferred}",
    );
    deck.write(
        "cards/note.md",
        "---\nid: note\ndefines: [集中関数, concentration function]\n---\n",
    );
    let graph = deck.open();
    assert!(graph.cards["def"].defines.is_empty());
    assert_eq!(graph.cards["manual"].defines, ["Lévy 族"]);
    assert_eq!(
        graph.cards["note"].defines,
        ["集中関数", "concentration function"]
    );
}

#[test]
fn japanese_uses_the_longest_term_and_english_checks_word_boundaries() {
    let deck = Fixture::new();
    deck.write(
        "cards/a.md",
        "---\nid: a\ndefines: [族, Lévy 族, median]\n---\n",
    );
    deck.write(
        "cards/match.md",
        "---\nid: match\n---\nLÉVY  族ではMEDIANを扱う。\n",
    );
    deck.write(
        "cards/no-match.md",
        "---\nid: no-match\n---\nmedians, immedian, median_x, an émedian\n",
    );
    let graph = deck.open();
    assert_eq!(codes(&graph, "cards/match.md"), [DEPENDENCY, DEPENDENCY]);
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.message.contains("`Lévy 族`")));
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.message.contains("`median`")));
    assert!(codes(&graph, "cards/no-match.md").is_empty());
}

#[test]
fn math_comments_and_reference_arguments_are_excluded() {
    let deck = Fixture::new();
    deck.write("cards/a.md", "---\nid: a\ndefines: [median]\n---\n");
    deck.write(
        "cards/b.md",
        r#"---
id: b
---
$median$ $$median$$ \(median\) \[median\]
\begin{equation}median\end{equation}
\begin {align*} \begin{aligned}median\end{aligned}
 % \end{align*}
median \end {align*}
% median \(ignored delimiter
\label {median} \Cref*{median} \cite[median {with ]}][median]{median}
"#,
    );
    deck.write("cards/c/card.md", "---\nid: c\n---\n");
    deck.write("cards/c/proof.tex", r"\$5 and \% then median");
    let graph = deck.open();
    assert!(codes(&graph, "cards/b.md").is_empty());
    assert_eq!(codes(&graph, "cards/c/card.md"), [DEPENDENCY]);
}

#[test]
fn order_and_reachability_are_independent() {
    let deck = Fixture::new();
    deck.write(
        "structure.yaml",
        "sections:\n- items: [early, {subsection: More}, {proof_of: def}, linked]\n- items: [{card: def}]\nappendix: [late]\n",
    );
    deck.write("cards/def/card.md", "---\nid: def\nkind: definition\n---\n");
    deck.write("cards/def/statement.tex", r"\emph{median} is the median.");
    deck.write(
        "cards/early.md",
        "---\nid: early\n---\nA median, another median. [[def]]",
    );
    deck.write(
        "cards/linked.md",
        "---\nid: linked\nrequires: [bridge]\n---\nA median.",
    );
    deck.write(
        "cards/bridge.md",
        "---\nid: bridge\nlinks: [{to: def, rel: implements}]\n---\n",
    );
    deck.write("cards/late.md", "---\nid: late\n---\nA median.");
    deck.write("cards/unplaced.md", "---\nid: unplaced\n---\nA median.");
    let graph = deck.open();

    assert_eq!(graph.cards["def"].defines, ["median"]);
    assert_eq!(codes(&graph, "cards/early.md"), [DEPENDENCY, BEFORE]);
    assert_eq!(codes(&graph, "cards/linked.md"), [BEFORE]);
    assert_eq!(codes(&graph, "cards/late.md"), [DEPENDENCY]);
    assert_eq!(codes(&graph, "cards/unplaced.md"), [DEPENDENCY]);
    assert!(codes(&graph, "cards/def/card.md").is_empty());
    assert!(!graph.has_errors());
    let diagnostic = serde_json::to_value(&graph.diagnostics[0]).unwrap();
    assert_eq!(diagnostic["code"], DEPENDENCY);
    assert_eq!(diagnostic["severity"], "Warning");
}

#[test]
fn external_definitions_can_be_reached_through_their_namespace() {
    let deck = Fixture::new();
    deck.write("deck.toml", "[external]\nns = \"external\"\n");
    deck.write(
        "structure.yaml",
        "sections:\n- items: [linked, ns:def, unlinked]\n",
    );
    deck.write(
        "external/cards/def.md",
        "---\nid: def\ndefines: [中央値]\n---\n",
    );
    deck.write(
        "external/cards/bridge.md",
        "---\nid: bridge\nlinks: [{to: def, rel: refines}]\n---\n中央値",
    );
    deck.write("cards/def.md", "---\nid: def\n---\n");
    deck.write(
        "cards/linked.md",
        "---\nid: linked\nrequires: ['ns:bridge']\n---\n中央値",
    );
    deck.write("cards/unlinked.md", "---\nid: unlinked\n---\n中央値");
    let graph = deck.open();
    assert_eq!(
        graph.context(&["linked".into()]),
        ["ns:def", "ns:bridge", "linked"]
    );
    assert!(codes(&graph, "cards/linked.md").is_empty());
    assert_eq!(codes(&graph, "cards/unlinked.md"), [DEPENDENCY]);
    assert_eq!(graph.diagnostics.len(), 1);
}

#[test]
fn ignore_lists_apply_at_deck_and_card_scope() {
    for config in ["deck.toml", "harness.toml"] {
        let deck = Fixture::new();
        deck.write(config, "[lint]\nignore_terms = ['集合']\n");
        deck.write(
            "structure.yaml",
            "sections:\n- items: [ignored, active, def]\n",
        );
        deck.write(
            "cards/def.md",
            "---\nid: def\ndefines: [集合, median]\n---\n",
        );
        deck.write(
            "cards/ignored.md",
            "---\nid: ignored\nlint_ignore: [MEDIAN]\n---\n集合と median",
        );
        deck.write("cards/active.md", "---\nid: active\n---\n集合と median");
        let graph = deck.open();
        assert!(codes(&graph, "cards/ignored.md").is_empty());
        assert_eq!(codes(&graph, "cards/active.md"), [DEPENDENCY, BEFORE]);
        assert!(graph
            .diagnostics
            .iter()
            .all(|d| d.message.contains("`median`")));
    }
}

#[test]
fn known_card_ids_do_not_trigger_matches_for_parts_of_the_id() {
    let deck = Fixture::new();
    deck.write("cards/def.md", "---\nid: def\ndefines: [profile]\n---\n");
    deck.write("cards/ob-cor-profile.md", "---\nid: ob-cor-profile\n---\n");
    deck.write(
        "cards/ref.md",
        "---\nid: ref\n---\nob-cor-profileを参照する。",
    );
    deck.write(
        "cards/use.md",
        "---\nid: use\n---\nA profile-based argument.",
    );
    let graph = deck.open();
    assert!(codes(&graph, "cards/ref.md").is_empty());
    assert_eq!(codes(&graph, "cards/use.md"), [DEPENDENCY]);
}

#[test]
fn prh_reads_validates_writes_and_lints_visible_prose() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    std::fs::create_dir(root.join("cards"))?;
    std::fs::write(
        root.join("prh.yml"),
        r#"version: 1
imports: extra.yml
rules:
  - expected: ベンダー
    pattern: /ベンダ(?!ー)/
    specs:
      - {from: ベンダとベンダー, to: ベンダーとベンダー}
    prh: 長音を付ける。
  - expected: JavaScript
    specs:
      - {from: "x ＪＡＶＡＳＣＲＩＰＴ", to: "x JavaScript"}
  - expected: Cookie
    options: {wordBoundary: true}
    specs:
      - {from: "x cookie and aCOOKIE", to: "x Cookie and aCOOKIE"}
"#,
    )?;
    std::fs::write(
        root.join("extra.yml"),
        r#"version: 1
rules:
  - expected: 第$1節
    patterns: ["/節([0-9]+)/i", "/section ([0-9]+)/i"]
    specs:
      - {from: "SECTION 2・節3", to: "第2節・第3節"}
  - expected: さらに
    pattern: /(変)?更に/
    regexpMustEmpty: $1
    specs:
      - {from: "変更に、更に", to: "変更に、さらに"}
"#,
    )?;
    let dictionary = prh::Dictionary::read(&root.join("prh.yml"))?;
    dictionary.write(&root.join("shared.yml"))?;
    assert!(std::fs::read_to_string(root.join("shared.yml"))?.contains("version: 1"));
    std::fs::write(
        root.join("deck.toml"),
        "[lint]\nglossary = 'shared.yml'\nignore_terms = ['JavaScript']\n",
    )?;
    std::fs::write(
        root.join("cards/check.md"),
        r#"---
id: check
---
ベンダーとベンダ。SECTION 2を参照する。javascriptを使う。
$\text{節3}$ `節9` \cite{節8} \label{節7} 変更について。
"#,
    )?;
    let graph = Graph::open(root)?;
    let issues: Vec<_> = graph
        .diagnostics
        .iter()
        .filter(|d| d.code == Some("term-consistency/prh"))
        .collect();
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert!(issues[0].message.contains("長音を付ける"));
    assert!(issues[1].message.contains("第2節"));

    std::fs::write(
        root.join("extra.yml"),
        "version: 1\nrules: [{expected: 語, pattern: 字, specs: [{from: 字, to: 誤}]}]",
    )?;
    assert!(format!("{:#}", Graph::open(root).err().unwrap()).contains("prh spec"));
    std::fs::write(root.join("extra.yml"), "version: 1\nimports: [prh.yml]")?;
    assert!(format!("{:#}", Graph::open(root).err().unwrap()).contains("Cyclic prh import"));
    Ok(())
}
