use super::*;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn findings(body: &str, config: &StyleConfig) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    inspect(
        &prose(body, false),
        config,
        &Phrases::compile(config),
        |_| true,
        |code, message, _, _| out.push((code, message)),
    );
    out
}

fn messages(body: &str, rule: &str) -> Vec<String> {
    findings(body, &StyleConfig::default())
        .into_iter()
        .filter_map(|(code, message)| (rule == code).then_some(message))
        .collect()
}

#[test]
fn ai_phrase_matches_literals_and_whole_english_words() {
    assert_eq!(messages("これは重要なのはと言えるでしょう。", AI).len(), 2);
    let english = messages("It is Worth Noting that we delve into this.", AI);
    assert_eq!(english.len(), 2);
    assert!(english[0].contains("delve") && english[1].contains("It is Worth Noting"));
    assert_eq!(messages("において重要な\n役割を果たす。", AI).len(), 1);
    assert!(messages(
        "He delved into crucially distinct results. 役割を担う。",
        AI
    )
    .is_empty());
    assert!(messages("Furthermore, both limits exist.", AI).is_empty());
    assert_eq!(
        messages(
            "Furthermore, both limits exist. Furthermore, they agree.",
            AI
        )
        .len(),
        1
    );
}

#[test]
fn long_sentence_counts_visible_japanese_chars_or_english_words() {
    let ja = format!("{}。", "あ".repeat(100));
    assert!(messages(&ja, LONG)[0].contains("101字"));
    assert!(messages(&format!("{}。", "あ".repeat(99)), LONG).is_empty());
    let en = format!("{} finite.", "word ".repeat(40));
    assert!(messages(&en, LONG)[0].contains("41語"));
    assert!(messages(&format!("{}finite.", "word ".repeat(39)), LONG).is_empty());
    let config = StyleConfig {
        max_ja_chars: Some(6),
        max_en_words: Some(3),
        ..Default::default()
    };
    let body = "空間を\n$\\text{無視する内容}$固定する。\n\nWe fix $x+y$ and proceed.";
    let out = findings(body, &config);
    let long: Vec<_> = out.iter().filter(|(code, _)| *code == LONG).collect();
    assert_eq!(long.len(), 2);
    assert!(long[0].1.contains("8字"));
    assert!(long[1].1.contains("4語"));
    assert!(long
        .iter()
        .all(|(_, m)| !m.contains("無視する内容") && !m.contains("x+y")));
    assert!(messages(
        "Apply Eq. 3.2, e.g. to the real line. All terms are finite.",
        LONG
    )
    .is_empty());
    assert_eq!(
        sentences("Apply Eq. 3.2, e.g. to the real line. All terms are finite.").count(),
        2
    );
    assert_eq!(
        sentences("The limit is x. The terms are finite.").count(),
        2
    );
}

#[test]
fn repeated_ending_needs_three_japanese_sentences_in_one_paragraph() {
    assert_eq!(
        messages(
            "これは定義である。\nそれは補題である．別の例である！再び同じである。",
            ENDING
        )
        .len(),
        1
    );
    assert_eq!(
        messages("値を固定する。集合を選択する。式を計算する。", ENDING).len(),
        1
    );
    for body in [
        "これは定義である。別の例である。",
        "これは定義である。例を示す。別の例である。",
        "これは定義である。\n\nこれは補題である。別の例である。",
        "これは定義である。 It is finite. 補題である。別の例である。",
        "It is finite. It remains finite. The limit is finite.",
        "- 定義である。\n- 補題である。\n- 別の例である。",
    ] {
        assert!(messages(body, ENDING).is_empty(), "{body}");
    }
}

#[test]
fn comma_density_counts_only_japanese_commas_in_one_sentence() {
    let quote = "定義は、集合、写像，距離、測度，順序を含む。";
    assert!(messages(quote, COMMA)[0].contains(quote));
    for body in [
        "集合、写像，距離、測度を使う。",
        "集合、写像、距離を使う。測度、順序、位相を使う。",
        "It contains a, b, c, d, e, f, and g.",
        "集合、写像，距離、測度$x、y，z$を使う。",
    ] {
        assert!(messages(body, COMMA).is_empty(), "{body}");
    }
}

#[test]
fn rhetorical_negation_only_flags_contrast() {
    for body in [
        "距離ではなく位相を考える。",
        "距離だけでなく測度も考える。",
        "We use not a metric but an order.",
        "This holds not only here, but also in the limit.",
    ] {
        assert!(messages(body, NEGATION)[0].contains(body));
    }
    for body in [
        "距離ではなくなる場合を調べる。",
        "ある点は含まれない。",
        "We did not use a metric. But we did use an order.",
        "This number is not zero.",
        "Choose a small but nonzero value.",
    ] {
        assert!(messages(body, NEGATION).is_empty(), "{body}");
    }
}

#[test]
fn hedging_requires_stacked_qualifiers() {
    let findings = messages(
        "正しいかもしれない可能性がある。必要と考えられると思われる。 It might possibly hold.",
        HEDGING,
    );
    assert_eq!(findings.len(), 3);
    assert!(findings[2].contains("might possibly"));
    assert!(messages(
        "必要と思われる。 It might hold. The bound is possibly sharp.",
        HEDGING
    )
    .is_empty());
}

#[test]
fn common_non_prose_is_excluded_without_losing_visible_tex() {
    let hidden = "重要なのは、あ、い、う、え、お。定義である。補題である。例である。AではなくB。It might possibly be crucial.";
    let body = format!(
        r#"$ {hidden} $
$$ {hidden} $$
\( {hidden} \)
\[ {hidden} \]
\begin{{align*}}
{hidden}
\end{{align*}}
% {hidden}
<!-- {hidden}
{hidden} -->
\label{{{hidden}}} \Cref*{{{hidden}}} \cite[{hidden}][more]{{{hidden}}}
\verb|{hidden}|
\begin{{verbatim}}
{hidden}
\end{{verbatim}}
\begin{{comment}}
{hidden}
\end{{comment}}
`{hidden}`
``
{hidden}
``
```tex
{hidden}
```
~~~~text
{hidden}
~~~
{hidden}
~~~~
## Notes
> {hidden}
＞ {hidden}
ユーザー（面談）:「{hidden}」
「{hidden}」
2026-11-12 著者の決定：{hidden}
出典：{hidden}
旧Box版のLean宣言：{hidden}
"#
    );
    let config = StyleConfig {
        max_ja_chars: Some(1),
        max_en_words: Some(1),
        ..Default::default()
    };
    assert!(findings(&body, &config).is_empty());
    assert_eq!(
        messages(r"\emph{It is worth noting} that it holds.", AI).len(),
        1
    );
    assert_eq!(
        messages(r"\textbf{重要なのは}条件の範囲である。", AI).len(),
        1
    );
    assert!(messages("It is $x$ worth noting. It is `x` worth noting.", AI).is_empty());
    assert_eq!(
        messages("## Notes\nこの方法で重要なのは境界の扱いである。", AI).len(),
        1
    );
}

#[test]
fn reports_source_lines_after_multiline_code_and_tex() {
    let body = concat!(
        "``code\n",
        "continues``\n\n",
        "\\cite{First\n",
        "Second}\n\n",
        "において重要な\n",
        "役割を果たす。\n",
    );
    assert_eq!(
        mask_code_with_lines(body).text,
        crate::mask_code(body, '\0')
    );
    let config = StyleConfig::default();
    let mut spans = Vec::new();
    inspect(
        &prose(body, false),
        &config,
        &Phrases::compile(&config),
        |code| code == AI,
        |_, _, span, _| spans.push(span.start().line..=span.end().line),
    );
    assert_eq!(spans, [7..=8]);
}

struct Deck(PathBuf);

impl Deck {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fuzukue-style-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, file: &str, contents: &str) {
        let path = self.0.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for Deck {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn workspace_exclusions_apply_to_nested_decks_and_edited_cards() {
    let workspace = Deck::new();
    workspace.write("fuzukue.toml", "[style_lint]\nexclude = ['papers/demo/cards/drafts/*.md']\n");
    workspace.write("papers/demo/cards.toml", "dirs = ['cards', 'memos', 'inbox', 'threads']\n");
    let paths = [
        ("cards/prose.md", "lemma", "prose"),
        ("cards/plan.md", "plan", "plan"),
        ("cards/log.md", "design-log", "log"),
        ("cards/memo.md", "memo", "memo-kind"),
        ("cards/design/internal.md", "lemma", "internal"),
        ("memos/local/card.md", "lemma", "memo-dir"),
        ("inbox/incoming.md", "lemma", "incoming"),
        ("threads/log.md", "lemma", "thread"),
        ("cards/drafts/excluded.md", "lemma", "excluded"),
        ("cards/drafts/nested/visible.md", "lemma", "visible"),
    ];
    for (path, kind, id) in paths {
        workspace.write(&format!("papers/demo/{path}"), &format!(
            "---\nid: {id}\nkind: {kind}\n---\n😀学校のの近く？\n"
        ));
    }
    let deck = workspace.0.join("papers/demo");
    let edits = paths.into_iter().map(|(_, _, id)| (id.to_owned(), vec![1..=1])).collect::<BTreeMap<_, _>>();
    let ids = edits.keys().cloned().collect();
    let checked_paths = |diagnostics: Vec<Diagnostic>| diagnostics.into_iter()
        .filter(|d| d.code == Some("style/no-doubled-joshi"))
        .map(|d| d.path)
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from(["cards/prose.md".into(), "cards/drafts/nested/visible.md".into()]);
    let graph = Graph::open(&deck).unwrap();
    assert_eq!(checked_paths(graph.diagnostics.clone()), expected);
    assert_eq!(checked_paths(Graph::lint_edited_cards_with_rules(
        &deck, &ids, &edits, &[], Some(&workspace.0),
    ).unwrap()), expected);

    let body = &graph.cards["prose"].body;
    let repeated = graph.diagnostics.iter().find(|d| {
        d.path == Path::new("cards/prose.md") && d.code == Some("style/no-doubled-joshi")
    }).unwrap();
    let punctuation = graph.diagnostics.iter().find(|d| {
        d.path == repeated.path && d.code == Some("style/no-exclamation-question-mark")
    }).unwrap();
    let note = repeated.style_note.as_ref().unwrap();
    assert_eq!(&body[note.range.clone()], "の");
    assert_eq!(&body[note.excerpt.clone()], "😀学校のの近く？");
    assert_eq!(note.excerpt, punctuation.style_note.as_ref().unwrap().excerpt);
    assert_eq!((repeated.line, repeated.column), (Some(1), Some(6)));

    workspace.write("fuzukue.toml", "[style_lint]\nenabled = false\n");
    assert!(Graph::open(&deck).unwrap().diagnostics.iter().all(|d| d.style_note.is_none()));
    assert!(Graph::lint_edited_cards_with_rules(&deck, &ids, &edits, &[], Some(&workspace.0))
        .unwrap().iter().all(|d| d.style_note.is_none()));
}

#[test]
fn deck_and_local_settings_extend_disable_and_override() {
    for name in ["deck.toml", "cards.toml", "harness.toml"] {
        let deck = Deck::new();
        deck.write(
            name,
            "[style]\nextra_phrases = ['空疎な表現']\ndisable = ['hedging']\nmax_en_words = 1\n",
        );
        let settings = "extra_phrases = ['local phrase']\ndisable = ['style/comma-density']\nmax_en_words = 20\n";
        let local = if name == "deck.toml" {
            format!("[style]\n{settings}")
        } else {
            settings.to_owned()
        };
        deck.write("style.toml", &local);
        deck.write("cards/active/card.md", "---\nid: active\n---\n");
        deck.write("cards/active/statement.tex", "空疎な表現は、集合、写像、距離、測度、順序である。\nThis crucial local phrase might possibly be true.");
        deck.write("cards/ignore.md", "---\nid: ignore\nlint_ignore: [style/rhetorical-negation, style/ai-phrase]\n---\n重要なのは距離ではなく位相である。");
        for graph in [
            Graph::open(&deck.0).unwrap(),
            Graph::load(&deck.0, &["cards".into()]).unwrap(),
        ] {
            assert_eq!(graph.diagnostics.len(), 3);
            for d in &graph.diagnostics {
                assert_eq!(d.code, Some(AI));
                assert_eq!(d.severity, Severity::Warning);
                assert_eq!(d.path, Path::new("cards/active/card.md"));
            }
            for phrase in ["空疎な表現", "crucial", "local phrase"] {
                assert!(graph.diagnostics.iter().any(|d| d.message.contains(phrase)));
            }
            let json = serde_json::to_value(&graph.diagnostics[0]).unwrap();
            assert_eq!(json["code"], AI);
        }
        deck.write("style.toml", "disable = ['ai-phrase']");
        assert!(Graph::open(&deck.0)
            .unwrap()
            .diagnostics
            .iter()
            .all(|d| d.code != Some(AI)));
    }
}

#[test]
fn legacy_ledger_can_coexist_with_deck_configuration() {
    let deck = Deck::new();
    deck.write(
        "harness.toml",
        "[style]\nextra_phrases = ['local phrase']\n",
    );
    deck.write("cards/local.md", "---\nid: local\n---\nA local phrase.");
    deck.write(
        "style.toml",
        "# Legacy writing ledger\n\n[[rule]]\nrule = \"Say \"the metric\".\"\n",
    );
    assert_eq!(Graph::open(&deck.0).unwrap().diagnostics[0].code, Some(AI));
    deck.write("style.toml", "[style]\nextra_phrases = 1\n");
    assert!(Graph::open(&deck.0)
        .unwrap_err()
        .to_string()
        .contains("style.toml"));
}

#[test]
fn japanese_preset_rules_flag_specific_prose_and_accept_revisions() {
    for (rule, bad, good) in [
        (
            "no-mix-dearu-desumasu",
            "これは定義である。次に例を示します。",
            "これは定義である。次に例を示す。",
        ),
        (
            "ja-no-mixed-period",
            "補題を示す．ここで定義する。",
            "補題を示す。ここで定義する。",
        ),
        (
            "max-kanji-continuous-len",
            "国際連合安全保障理事会を指す。",
            "理事会を指す。",
        ),
        (
            "no-doubled-joshi",
            "私は彼は好きだ。学校のの近くだ。",
            "私は彼が好きだ。学校の近くだ。",
        ),
        (
            "no-double-negative-ja",
            "決まらないこともない。",
            "決まる場合もある。",
        ),
        (
            "ja-no-weak-phrase",
            "成り立つかもしれない。",
            "追加の条件で成り立つ。",
        ),
        (
            "ja-no-redundant-expression",
            "条件を変更することができる。",
            "条件を変更できる。",
        ),
        (
            "max-comma",
            "集合, 写像, 距離, 測度, 位相を扱う。",
            "集合, 写像, 距離, 測度を扱う。",
        ),
        (
            "no-exclamation-question-mark",
            "証明できる？",
            "証明できるかを調べる。",
        ),
        ("no-hankaku-kana", "ﾃﾞｰﾀを読む。", "データを読む。"),
    ] {
        let code = format!("style/{rule}");
        assert!(!messages(bad, &code).is_empty(), "{rule}: {bad}");
        assert!(messages(good, &code).is_empty(), "{rule}: {good}");
        assert!(messages(
            &format!(r"$\text{{{bad}}}$ `{bad}` \cite{{{bad}}} \label{{{bad}}}"),
            &code
        )
        .is_empty());
    }
}

#[test]
fn textlint_aliases_disable_rules_in_deck_and_card_settings() {
    let deck = Deck::new();
    deck.write("style.toml", "disable = ['max-ten']\nmax_kanji_chars = 9\nperiod_mark = '．'\nprefer_register = 'polite'");
    deck.write("cards/check.md", "---\nid: check\nlint_ignore: [no-mix-dearu-desumasu]\n---\n国際連合安全保障は、あ、い、う、えである．\n");
    assert!(Graph::open(&deck.0).unwrap().diagnostics.is_empty());
    deck.write("style.toml", "max_ja_commas = 5\nmax_kanji_chars = 6");
    let graph = Graph::open(&deck.0).unwrap();
    assert_eq!(graph.diagnostics.len(), 2);
    assert!(graph
        .diagnostics
        .iter()
        .all(|d| d.textlint_rule.is_some() && d.fix_hint.is_some()));
}
