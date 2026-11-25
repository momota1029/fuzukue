use super::*;
use crate::test_support::TestDir;
use fuzukue_proto::{DocProseSpan, DocProseText};
use std::fs;

fn sections(deck: &Path) -> Vec<DocSection> {
    let event = document_for_deck(deck, None);
    // Round-trip the snapshot used by the desktop reader.
    let bytes = serde_json::to_vec(&event).unwrap();
    let ServerEvent::Document { sections, .. } = serde_json::from_slice(&bytes).unwrap() else {
        unreachable!()
    };
    sections
}

fn prose(deck: &Path, text: &DocProseText) -> Result<String> {
    match text {
        DocProseText::Tex(tex) => Ok(tex.clone()),
        DocProseText::Source(spans) => {
            let mut tex = String::new();
            for span in spans {
                if span.newline_before {
                    tex.push('\n');
                }
                tex.push_str(&browse::read_prose(deck, span)?);
            }
            Ok(tex)
        }
    }
}

fn card_id(item: &DocItem) -> &str {
    let DocItem::Card { id } = item else {
        panic!("expected a card")
    };
    id
}

#[tokio::test]
async fn source_paper_preserves_abstract_and_prose_order_without_loading_cards() -> Result<()> {
    use axum::{
        body::{to_bytes, Body},
        extract::ConnectInfo,
        http::{Request, StatusCode},
        Extension,
    };
    use tower::ServiceExt;

    let dir = TestDir::new();
    let deck = dir.path().join("paper");
    dir.write(
        "paper/deck.toml",
        "kind = 'paper'\n[build]\ntex_root = 'source/main.tex'\n",
    );
    dir.write(
        "paper/structure.yaml",
        "title: Paper\nsections: [{title: Introduction, items: [stable-one, thm-two]}]\n",
    );
    let original = r"\documentclass{amsart}
\newtheorem{theorem}{Theorem}
\begin{document}
\begin{abstract}
An abstract.
\end{abstract}
\maketitle
Before any section.
\section*{Introduction}
\input{intro}
\begin{theorem}
\input{../cards/group/first/statement}
\end{theorem}
\input{between}
\begin{theorem}\label{thm:two}
A theorem in the manuscript.
\end{theorem}
After the last theorem.
\end{document}";
    dir.write("paper/source/main.tex", original);
    dir.write("paper/source/intro.tex", "Introductory text.\n");
    dir.write("paper/source/between.tex", "Prose between theorems.");
    dir.write(
        "paper/cards/group/first/card.md",
        "---\nid: stable-one\nkind: theorem\n---\n",
    );
    dir.write(
        "paper/cards/group/first/statement.tex",
        "The statement kept in the card.\n",
    );
    dir.write(
        "paper/cards/second/card.md",
        "---\nid: thm-two\nkind: theorem\n---\n",
    );
    dir.write(
        "paper/cards/second/statement.tex",
        "A theorem in the manuscript.\n",
    );

    let views = sections(&deck);
    assert_eq!(views.len(), 2);
    let opening = &views[0];
    assert!(opening.title.is_empty() && opening.items.is_empty());
    let front = prose(&deck, &opening.prose[0].text)?;
    let markdown = fuzukue_view::read_card_tex(&front, None, &[]).markdown;
    assert!(markdown.find("An abstract").unwrap() < markdown.find("Before any section").unwrap());

    let intro = &views[1];
    assert_eq!(intro.title, "Introduction");
    assert_eq!(
        intro.items.iter().map(card_id).collect::<Vec<_>>(),
        ["stable-one", "thm-two"]
    );
    let expected = [
        "Introductory text.",
        "Prose between theorems.",
        "After the last theorem.",
    ];
    assert_eq!(intro.prose.len(), expected.len());
    for (index, (excerpt, expected)) in intro.prose.iter().zip(expected).enumerate() {
        assert_eq!(excerpt.before_item, index);
        assert_eq!(prose(&deck, &excerpt.text)?.trim(), expected);
    }
    let event = serde_json::to_string(&document_for_deck(&deck, None))?;
    assert!(
        !event.contains("Prose between theorems.")
            && !event.contains("The statement kept in the card.")
    );

    let workspace = Arc::new(crate::Workspace::new(
        dir.path().to_owned(),
        String::new(),
        dir.path().join("state"),
        Arc::new(crate::Hub::new()),
    ));
    let access = crate::remote::Access::new(dir.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(workspace), Arc::new(access))
        .layer(Extension(ConnectInfo(peer)));
    let DocProseText::Source(spans) = &intro.prose[1].text else {
        unreachable!()
    };
    let span = spans
        .iter()
        .find(|span| span.path.ends_with("between.tex"))
        .unwrap();
    let read = |span: &DocProseSpan| {
        Request::builder()
            .uri(format!(
                "/deck/prose?deck=paper&path={}&start={}&len={}&sha256={}",
                span.path, span.start, span.len, span.sha256
            ))
            .body(Body::empty())
            .unwrap()
    };
    let response = router.clone().oneshot(read(span)).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 100).await?,
        "Prose between theorems."
    );
    dir.write("paper/source/between.tex", "Prose with a new revision.");
    assert!(!router.oneshot(read(span)).await?.status().is_success());

    let original_bytes = fs::read(deck.join("source/main.tex"))?;
    let ServerEvent::Cards { cards, .. } = snapshot_for_deck(dir.path(), &deck) else {
        unreachable!()
    };
    let edit = cards
        .iter()
        .find(|card| card.id == "stable-one")
        .unwrap()
        .edit_body
        .as_ref()
        .unwrap();
    assert_eq!(edit.tex, Access::ManuscriptBuild);
    let (outcome, build) = save_card_with_build(
        dir.path(),
        "paper",
        "stable-one",
        CardEditPart::TexBody,
        &front,
        &edit.base_hash,
        None,
    )?;
    assert!(matches!(
        outcome,
        CardSaveOutcome::TexUnavailable {
            access: Access::ManuscriptBuild
        }
    ));
    assert!(build.is_none());
    assert_eq!(fs::read(deck.join("source/main.tex"))?, original_bytes);
    assert_eq!(
        fs::read_to_string(deck.join("cards/group/first/statement.tex"))?,
        edit.body
    );

    let draft = "An ordinary edit to the card statement.\n";
    let outcome = save_card(
        dir.path(),
        "paper",
        "stable-one",
        CardEditPart::Body,
        draft,
        &edit.base_hash,
        None,
    )?;
    assert!(matches!(outcome, CardSaveOutcome::Saved));
    assert_eq!(
        fs::read_to_string(deck.join("cards/group/first/statement.tex"))?,
        draft
    );
    assert_eq!(fs::read(deck.join("source/main.tex"))?, original_bytes);
    let after = sections(&deck);
    assert_eq!(prose(&deck, &after[0].prose[0].text)?, front);
    assert_eq!(
        prose(&deck, &after[1].prose[1].text)?.trim(),
        "Prose with a new revision."
    );
    Ok(())
}

#[test]
fn imported_book_keeps_front_matter_and_prose_in_existing_cards() -> Result<()> {
    let dir = TestDir::new();
    dir.write(
        "source/main.tex",
        r"\documentclass{book}
\newtheorem{theorem}{Theorem}
\begin{document}
\maketitle
Before the first chapter.
\chapter*{Preface}
\input{preface}
\chapter{Introduction}
\begin{theorem}\label{thm:first}First theorem.\end{theorem}
\input{between}
\begin{theorem}\label{thm:second}Second theorem.\end{theorem}
\end{document}",
    );
    dir.write("source/preface.tex", "A preface.");
    dir.write("source/between.tex", "Running prose.");
    let deck = dir.path().join("book");
    fuzukue_cards::latex_book::import(&dir.path().join("source/main.tex"), &deck, false)?;
    fs::remove_dir_all(dir.path().join("source"))?;

    let views = sections(&deck);
    assert_eq!(views.len(), 3);
    let original_ids: Vec<_> = views
        .iter()
        .flat_map(|view| &view.items)
        .map(card_id)
        .collect();
    let contents = |id: &str| fs::read_to_string(deck.join("cards").join(id).join("statement.tex"));
    assert!(contents(original_ids[0])?.contains("Before the first chapter."));
    assert_eq!(views[1].title, "Preface");
    assert!(contents(original_ids[1])?.contains("A preface."));
    assert_eq!(views[2].title, "Introduction");
    assert_eq!(original_ids[2], "thm-first");
    assert!(contents(original_ids[3])?.contains("Running prose."));
    assert_eq!(original_ids[4], "thm-second");

    // Existing imported outlines can also hold text directly in before_sections.
    let path = deck.join("structure.yaml");
    let mut old: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(&path)?)?;
    old["before_sections"] = serde_yaml::Value::String("Legacy opening prose.\n".into());
    fs::write(&path, serde_yaml::to_string(&old)?)?;
    let with_legacy = sections(&deck);
    assert_eq!(
        prose(&deck, &with_legacy[0].prose[0].text)?,
        "Legacy opening prose.\n"
    );
    assert_eq!(
        with_legacy
            .iter()
            .flat_map(|view| &view.items)
            .map(card_id)
            .collect::<Vec<_>>(),
        original_ids,
    );

    let cfg = DeckConfig::read(&deck)?;
    let (_, master) = fuzukue_cards::latex_book::render(&deck, &cfg)?;
    assert_eq!(master.matches("Legacy opening prose.").count(), 1);
    let mut rest = master.as_str();
    for id in original_ids {
        let needle = format!("cards/{id}/statement.tex");
        rest = rest
            .split_once(&needle)
            .expect("one statement in manuscript order")
            .1;
        assert_eq!(master.matches(&needle).count(), 1);
    }
    Ok(())
}

#[test]
fn large_prose_has_a_small_outline_and_bounded_utf8_excerpts() -> Result<()> {
    let dir = TestDir::new();
    let deck = dir.path().join("book");
    dir.write(
        "book/deck.toml",
        "kind = 'book'\n[build]\ntex_root = 'book.tex'\n",
    );
    let text = "地の文。".repeat(75_000);
    dir.write(
        "book/book.tex",
        &format!("\\begin{{document}}\n\\chapter*{{Preface}}\n{text}\n\\end{{document}}"),
    );
    let event = document_for_deck(&deck, None);
    assert!(serde_json::to_vec(&event)?.len() < 2048);
    let ServerEvent::Document { sections, .. } = event else {
        unreachable!()
    };
    assert_eq!(sections[0].title, "Preface");
    let excerpt = &sections[0].prose[0].text;
    let DocProseText::Source(spans) = excerpt else {
        unreachable!()
    };
    assert!(spans.len() > 1);
    assert!(spans
        .iter()
        .all(|span| span.len <= fuzukue_proto::DOC_PROSE_SPAN_BYTES));
    assert_eq!(prose(&deck, excerpt)?.trim(), text);
    Ok(())
}
