use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use fuzukue_cards::{latex_book, DeckConfig, Graph};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/latex-book/main.tex")
}

#[test]
fn imports_structure_labels_proofs_sources_and_assets() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let deck = tmp.path().join("book");
    let result = latex_book::import(&fixture(), &deck, false)?;
    assert!(result.report.is_file());
    let cfg = DeckConfig::read(&deck)?;
    assert_eq!(cfg.meta.kind, "book");
    assert!(cfg.meta.imported_latex);
    let graph = Graph::open_local(&deck)?;
    let theorem = &graph.cards["thm-sample"];
    assert_eq!(theorem.kind, "theorem");
    assert_eq!(
        theorem.source.as_deref(),
        Some("chapters/sections/details.tex:6-11")
    );
    assert!(theorem.body.contains(r"\Cref{def:sample}"));
    assert!(theorem.body.contains(r"\label{eq:sample}"));
    assert!(theorem.proof.as_ref().unwrap().text.contains("両辺"));
    assert_eq!(graph.cards["def-sample"].kind, "definition");
    assert!(graph
        .cards
        .keys()
        .any(|id| id.starts_with("ch2-0-0-theorem")));
    assert!(graph.cards.contains_key("fig-sample"));
    assert!(deck.join("figures/marker.eps").is_file());
    assert!(fs::read_to_string(deck.join("preamble.tex"))?.contains(r"\newcommand{\sample}"));

    let (_, master) = latex_book::render(&deck, &cfg)?;
    let ordered = [
        r"\part{基礎}",
        r"\chapter[用語]{最初の用語}\label{ch:first}",
        r"\section{定義と定理}\label{sec:sample}",
        r"\subsection*{小さな例}",
        "thm-sample/statement.tex",
        "thm-sample/proof.tex",
        r"\input{cards/fig-sample/statement.tex}",
        r"\chapter{振り返り}",
    ];
    let mut rest = master.as_str();
    for item in ordered {
        rest = rest
            .split_once(item)
            .unwrap_or_else(|| panic!("missing {item} in {rest}"))
            .1;
    }
    assert!(latex_book::import(&fixture(), &deck, false).is_err());
    Ok(())
}

#[test]
#[ignore = "needs latexmk, upLaTeX, dvipdfmx and pdftotext"]
fn latex_book_pdfs_match() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let deck = tmp.path().join("book");
    let result = latex_book::import(&fixture(), &deck, true)?;
    let report = fs::read_to_string(result.report)?;
    println!("{report}");
    assert_eq!(result.verification, "一致", "{report}");
    assert!(deck.join("build/paper.pdf").is_file());
    Ok(())
}

#[test]
fn prose_splits_at_paragraphs_and_ignores_verbatim_structure() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let main = tmp.path().join("main.tex");
    let deck = tmp.path().join("book");
    let paragraph = "Some text in a paragraph. ".repeat(28);
    let paragraph = paragraph.trim_end();
    fs::write(
        &main,
        format!(
            "\\documentclass{{book}}\n\\begin{{document}}\n\\chapter{{One}}\n\
             % \\chapter{{Comment}}\n\\verb|\\chapter{{Literal}}|\n\
             {paragraph}\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n\n\
             \\begin{{verbatim}}\n\\chapter{{Example}}\n\\end{{verbatim}}\n\\end{{document}}\n"
        ),
    )?;
    let result = latex_book::import(&main, &deck, false)?;
    assert_eq!(result.cards, 2);
    let cfg = DeckConfig::read(&deck)?;
    let (_, master) = latex_book::render(&deck, &cfg)?;
    assert_eq!(master.matches(r"\chapter").count(), 1);
    let graph = Graph::open_local(&deck)?;
    for card in graph.cards.values() {
        assert_eq!(card.kind, "expl");
        let text = fs::read_to_string(deck.join(&card.tex[0]))?;
        assert!((1000..=3000).contains(&text.len()), "{}", text.len());
    }
    let cycle = tmp.path().join("cycle.tex");
    fs::write(&cycle, "\\input{cycle}\n")?;
    let failed = tmp.path().join("failed");
    let error = latex_book::import(&cycle, &failed, false).unwrap_err();
    assert!(error.to_string().contains("循環"));
    assert!(!failed.exists());
    assert!(!failed.with_extension("import-report.md").exists());
    Ok(())
}
