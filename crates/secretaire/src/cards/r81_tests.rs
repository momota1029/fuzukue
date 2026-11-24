use super::*;
use crate::test_support::TestDir;
use fuzukue_proto::card_tex::{BuildOutcome, IssueKind};

const DECK: &str = "deck";
const STATEMENT: &str = "deck/cards/result/statement.tex";
const PROOF: &str = "deck/cards/result/proof.tex";
const META: &str = "deck/cards/result/card.md";

fn fixture() -> TestDir {
    let dir = TestDir::new();
    dir.write("deck/deck.toml", "kind = 'paper'\ntitle = 'Editing TeX'\n");
    dir.write(
        "deck/structure.yaml",
        "sections: [{title: Result, items: [result]}]\n",
    );
    dir.write(META, "---\nid: result\nkind: theorem\ntitle: Result\n---\n");
    dir.write(STATEMENT, "The original statement.\n");
    dir.write(PROOF, "The proof.\n");
    dir
}

fn sources(dir: &TestDir) -> (CardEditSource, CardEditSource) {
    let ServerEvent::Cards { mut cards, .. } =
        snapshot_for_deck(dir.path(), &dir.path().join(DECK))
    else {
        unreachable!()
    };
    let card = cards.pop().expect("fixture card");
    (card.edit_body.unwrap(), card.edit_proof.unwrap())
}

#[test]
fn r81_validates_and_saves_exact_bytes_through_the_shared_version_check() -> Result<()> {
    let dir = fixture();
    let (statement, proof) = sources(&dir);
    assert_eq!(statement.tex, Access::Editable);
    assert_eq!(proof.tex, Access::Editable);
    let meta = std::fs::read(dir.path().join(META))?;
    let save =
        |part, body, hash| save_card_with_build(dir.path(), DECK, "result", part, body, hash, None);
    let (rejected, build) = save(
        CardEditPart::TexBody,
        "line one\n{line two",
        &statement.base_hash,
    )?;
    assert!(matches!(rejected, CardSaveOutcome::TexInvalid { issue }
        if issue.line == 2 && issue.kind == IssueKind::UnbalancedGroup));
    assert!(build.is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(STATEMENT))?,
        statement.body
    );

    let draft = "% keep this comment\r\n\\begin{align}x &= y\\end{align}\r\n\r\n";
    std::fs::write(
        dir.path().join(STATEMENT),
        &statement.body.replace('\n', "\r\n"),
    )?;
    let (rejected, build) = save(CardEditPart::TexBody, draft, &statement.base_hash)?;
    let CardSaveOutcome::Conflict { latest } = rejected else {
        anyhow::bail!("the line-ending-only change was overwritten");
    };
    assert!(build.is_none());
    assert_eq!(latest.body, statement.body.replace('\n', "\r\n"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join(STATEMENT))?,
        latest.body
    );

    let (saved, build) = save(CardEditPart::TexBody, draft, &latest.base_hash)?;
    assert!(matches!(saved, CardSaveOutcome::Saved));
    assert!(build.unwrap().is_current());
    let (updated, untouched) = sources(&dir);
    assert_eq!(updated.body, draft);
    assert_eq!(std::fs::read(dir.path().join(STATEMENT))?, draft.as_bytes());
    assert_eq!(untouched.body, proof.body);
    assert_eq!(std::fs::read(dir.path().join(META))?, meta);

    let (rejected, build) = save(
        CardEditPart::Body,
        "A stale ordinary edit.\n",
        &statement.base_hash,
    )?;
    assert!(matches!(rejected, CardSaveOutcome::Conflict { latest } if latest.body == draft));
    assert!(build.is_none());
    let (saved, build) = save(CardEditPart::TexProof, "A new proof.\n", &proof.base_hash)?;
    assert!(matches!(saved, CardSaveOutcome::Saved) && build.unwrap().is_current());
    assert_eq!(std::fs::read(dir.path().join(STATEMENT))?, draft.as_bytes());
    assert_eq!(std::fs::read(dir.path().join(META))?, meta);
    Ok(())
}

#[test]
fn r81_refuses_imported_and_manuscript_build_decks_at_save_time() -> Result<()> {
    let dir = fixture();
    let (statement, _) = sources(&dir);
    for (extra, access) in [
        (
            "imported_latex = true\nmanuscript_main = 'missing.tex'\n",
            Access::ImportedManuscript,
        ),
        (
            "[build]\ntex_root = 'original.tex'\n",
            Access::ManuscriptBuild,
        ),
    ] {
        dir.write("deck/deck.toml", &format!("kind = 'paper'\n{extra}"));
        assert_eq!(sources(&dir).0.tex, access);
        let (outcome, build) = save_card_with_build(
            dir.path(),
            DECK,
            "result",
            CardEditPart::TexBody,
            "Edit the wrong copy.\n",
            &statement.base_hash,
            None,
        )?;
        assert!(
            matches!(outcome, CardSaveOutcome::TexUnavailable { access: actual } if actual == access)
        );
        assert!(build.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(STATEMENT))?,
            statement.body
        );
    }
    Ok(())
}

#[tokio::test]
async fn r81_builds_saved_tex_and_preserves_the_last_pdf_on_failure() -> Result<()> {
    if !crate::test_support::tex::ready("uplatex", &["latexmk", "dvipdfmx"])? {
        return Ok(());
    }
    let dir = fixture();
    let app = Arc::new(Workspace::new(
        dir.path().to_owned(),
        DECK.into(),
        dir.path().join(".fuzukue"),
        Arc::new(crate::hub::Hub::new()),
    ));
    let original = app.build_paper(DECK.into()).await??;
    assert!(original.ok, "{}", original.log);
    let read_pdf = || std::fs::read(dir.path().join("deck/build/paper.pdf"));
    let old_pdf = read_pdf()?;
    let save = |body: &str| -> Result<SavedCardTex> {
        let (statement, _) = sources(&dir);
        let (outcome, saved) = save_card_with_build(
            dir.path(),
            DECK,
            "result",
            CardEditPart::TexBody,
            body,
            &statement.base_hash,
            Some(&app),
        )?;
        anyhow::ensure!(matches!(outcome, CardSaveOutcome::Saved), "{outcome:?}");
        saved.context("saved revision")
    };

    let saved = save("The edited statement contains $x=2$.\n")?;
    let result = app.build_saved_card_tex(saved).await;
    assert!(
        matches!(result, BuildOutcome::Built { ref pdf } if pdf == "deck/build/paper.pdf"),
        "{result:?}"
    );
    let edited_pdf = read_pdf()?;
    assert_ne!(old_pdf, edited_pdf);
    let sync_path = dir.path().join("deck/build/paper.synctex.gz");
    let sync = std::fs::read(&sync_path)?;

    let invalid = "Line one.\n\\undefinedRogueFixtureCommand\n";
    let saved = save(invalid)?;
    let BuildOutcome::Failed { log, log_path } = app.build_saved_card_tex(saved).await else {
        anyhow::bail!("expected a failed build");
    };
    assert!(log.contains("statement.tex:2:"), "{log}");
    let full = std::fs::read_to_string(dir.path().join(log_path.context("build log")?))?;
    assert!(full.contains("Undefined control sequence"), "{full}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(STATEMENT))?,
        invalid
    );
    assert_eq!(read_pdf()?, edited_pdf);
    assert_eq!(std::fs::read(sync_path)?, sync);

    let saved = save("An older saved revision.\n")?;
    dir.write(STATEMENT, "A newer on-disk revision.\n");
    assert!(matches!(
        app.build_saved_card_tex(saved).await,
        BuildOutcome::Superseded
    ));
    Ok(())
}
