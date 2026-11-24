use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use fuzukue_proto::{CardEditPart, CardSaveOutcome, CardView, TranslationStatus};

use super::*;

fn cards(project: &Path, deck: &str) -> Vec<CardView> {
    let ServerEvent::Cards {
        cards, diagnostics, ..
    } = snapshot_for_deck(project, &project.join(deck))
    else {
        unreachable!()
    };
    assert!(!cards.is_empty(), "{diagnostics:?}");
    cards
}

#[test]
fn translation_stamp_tracks_source_and_translated_revisions() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    for deck in ["original", "translated"] {
        fs::create_dir_all(root.join(deck).join("cards"))?;
        fs::write(root.join(deck).join("harness.toml"), "")?;
        fs::write(root.join(deck).join("structure.yaml"), "sections: []\n")?;
    }
    let source = root.join("original/cards/item.md");
    fs::write(&source, "---\nid: item\nkind: lemma\n---\nOriginal\n")?;
    let translated = root.join("translated/cards/item.md");
    let front = "---\r\nid: local\r\ntitle: Translated # comment\r\nsource_id: 'original#item'\r\n";
    let stamp = "source_fingerprint: old-stamp  # remove drift after translating\r\n";
    let closing = "metadata: 'leave me alone'\r\n---\r\n";
    fs::write(
        &translated,
        format!("{front}{stamp}{closing}Translation\r\n"),
    )?;
    // The current translation has an independent source so it stays current during the edit below.
    let stable_source = "---\nid: stable\nkind: lemma\n---\nStable source\n";
    fs::write(root.join("original/cards/stable.md"), stable_source)?;
    let stable_hash = fuzukue_cards::source_fingerprint(stable_source)?;
    let current = format!(
        "---\nid: current\nsource_id: 'original#stable'\nsource_fingerprint: '{stable_hash}'\n---\n",
    );
    let peers = [
        ("current", Some(TranslationStatus::Current), current.as_str()),
        ("sibling", Some(TranslationStatus::Stale),
            "---\nid: sibling\nsource_id: 'original#item'\nsource_fingerprint: sibling-stamp\n---\n"),
        ("pending", Some(TranslationStatus::Unverified),
            "---\nid: pending\nsource_id: 'original#item'\nsource_fingerprint: PENDING\n---\n"),
        ("unverified", Some(TranslationStatus::Unverified),
            "---\nid: unverified\nsource_id: 'original#item'\n---\n"),
        ("missing", Some(TranslationStatus::SourceUnavailable),
            "---\nid: missing\nsource_id: 'original#gone'\nsource_fingerprint: old\n---\n"),
        ("legacy", None, "---\nid: legacy\nsource_fingerprint: old-stamp\n---\n"),
        ("plain", None, "---\nid: plain\n---\n"),
    ];
    let peer_path = |id| root.join(format!("translated/cards/{id}.md"));
    for (id, _, markdown) in peers {
        fs::write(peer_path(id), markdown)?;
    }

    let snapshot = || cards(root, "translated");
    let rows = snapshot();
    let local = rows.iter().find(|card| card.id == "local").unwrap();
    let translation = local.translation.as_ref().unwrap();
    assert_eq!(translation.status, TranslationStatus::Stale);
    let revision = translation.revision.as_ref().unwrap();
    let saved = |revision| record_source_fingerprint(root, "translated", "local", revision, None);
    let body = "Updated translation\r\n";
    let edit = local.edit_body.as_ref().unwrap();
    assert!(matches!(
        save_card(
            root,
            "translated",
            "local",
            CardEditPart::Body,
            body,
            &edit.base_hash,
            None
        )?,
        CardSaveOutcome::Saved
    ));
    let before_stamp = format!("{front}{stamp}{closing}{body}");
    assert_eq!(fs::read_to_string(&translated)?, before_stamp);
    assert!(matches!(
        saved(revision)?,
        CardSaveOutcome::TranslationConflict
    ));

    let check = || {
        snapshot()
            .into_iter()
            .find(|card| card.id == "local")
            .unwrap()
            .translation
            .unwrap()
    };
    let revision = check().revision.unwrap();
    fs::write(
        &source,
        "---\nid: item\nkind: lemma\n---\nOriginal edited\n",
    )?;
    assert!(matches!(
        saved(&revision)?,
        CardSaveOutcome::TranslationConflict
    ));
    assert_eq!(fs::read_to_string(&translated)?, before_stamp);

    let before = snapshot();
    let local = before.iter().find(|card| card.id == "local").unwrap();
    let translation = local.translation.as_ref().unwrap();
    assert_eq!(translation.status, TranslationStatus::Stale);
    let revision = translation.revision.as_ref().unwrap();
    assert!(matches!(saved(revision)?, CardSaveOutcome::Saved));
    let hash = &revision.source_fingerprint;
    assert_eq!(fs::read_to_string(&translated)?,
        format!("{front}source_fingerprint: '{hash}'  # remove drift after translating\r\n{closing}{body}"));

    let after = snapshot();
    assert_eq!(after.len(), before.len());
    let local = after.iter().find(|card| card.id == "local").unwrap();
    let translation = local.translation.as_ref().unwrap();
    assert_eq!(translation.status, TranslationStatus::Current);
    assert!(translation.revision.is_none());
    for (id, status, markdown) in peers {
        let previous = before.iter().find(|card| card.id == id).unwrap();
        let card = after.iter().find(|card| card.id == id).unwrap();
        assert_eq!(
            previous.translation.as_ref().map(|check| check.status),
            status,
            "{id}"
        );
        assert_eq!(
            serde_json::to_value(&card.translation)?,
            serde_json::to_value(&previous.translation)?,
            "{id} diagnostic"
        );
        assert_eq!(fs::read(peer_path(id))?, markdown.as_bytes(), "{id} file");
    }

    let unverified = snapshot()
        .into_iter()
        .find(|card| card.id == "unverified")
        .unwrap();
    assert!(matches!(
        record_source_fingerprint(
            root,
            "translated",
            "unverified",
            &unverified.translation.unwrap().revision.unwrap(),
            None
        )?,
        CardSaveOutcome::Saved
    ));
    assert_eq!(
        snapshot()
            .into_iter()
            .find(|card| card.id == "unverified")
            .unwrap()
            .translation
            .unwrap()
            .status,
        TranslationStatus::Current
    );

    fs::write(&source, "---\nid: [unreadable]\n---\n")?;
    assert_eq!(check().status, TranslationStatus::SourceUnavailable);
    fs::write(source, "---\nid: item\n---\nEdited again\n")?;
    let revision = check().revision.unwrap();
    fs::remove_file(&translated)?;
    assert!(matches!(
        saved(&revision)?,
        CardSaveOutcome::TranslationConflict
    ));
    assert!(!translated.exists());
    Ok(())
}

#[tokio::test]
async fn translation_watcher_refreshes_the_open_deck_after_a_source_edit() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    for deck in ["original", "translated"] {
        fs::create_dir_all(root.join(deck).join("cards"))?;
        fs::write(
            root.join(deck).join("harness.toml"),
            "[build]\nauto = false\n",
        )?;
    }
    let source = root.join("original/cards/original.md");
    let original = "---\nid: original\n---\nOriginal\n";
    fs::write(&source, original)?;
    let hash = fuzukue_cards::source_fingerprint(original)?;
    fs::write(root.join("translated/cards/translation.md"), format!(
        "---\nid: translation\nsource_id: 'original#original'\nsource_fingerprint: '{hash}'\n---\nTranslation\n"
    ))?;

    let app = Arc::new(Workspace::new(
        root.canonicalize()?,
        "127.0.0.1:7878".into(),
        root.join(".fuzukue"),
        Arc::new(crate::hub::Hub::new()),
    ));
    *app.deck.write().unwrap() = "translated".into();
    let (_, mut snapshots) = app.hub.subscribe(0);
    watch(app.clone(), Default::default())?;
    assert_eq!(
        cards(root, "translated")[0]
            .translation
            .as_ref()
            .unwrap()
            .status,
        TranslationStatus::Current
    );
    fs::write(source, format!("{original}New paragraph\n"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::Cards { deck, cards, .. } = snapshots.recv().await?.event {
                if deck == "translated"
                    && cards[0]
                        .translation
                        .as_ref()
                        .is_some_and(|check| check.status == TranslationStatus::Stale)
                {
                    return anyhow::Ok(());
                }
            }
        }
    })
    .await??;
    Ok(())
}

#[test]
#[ignore = "requires FUZUKUE_TRANSLATION_FIXTURE pointing at the migrated decks and PyYAML"]
fn external_translation_matches_python_and_finds_cards_outside_structure() -> Result<()> {
    let input = std::env::var_os("FUZUKUE_TRANSLATION_FIXTURE")
        .map(PathBuf::from)
        .context("set FUZUKUE_TRANSLATION_FIXTURE to the migrated fixture directory")?
        .canonicalize()?;
    let name = input.file_name().and_then(|name| name.to_str()).context("fixture directory needs a UTF-8 name")?;
    let paper = format!("writing/{name}");
    let english = format!("{paper}/harness-en");
    let result = Command::new("python3")
        .args([
            "-B",
            "-c",
            r#"
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location('sync', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print(json.dumps({str(p): module.body_hash(p) for p in module.cards(module.JA).values()}))
"#,
        ])
        .arg(input.join("harness-en/sync.py"))
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let hashes: BTreeMap<PathBuf, String> = serde_json::from_slice(&result.stdout)?;
    assert!(!hashes.is_empty());
    for (path, expected) in hashes {
        assert_eq!(
            fuzukue_cards::source_fingerprint(&fs::read_to_string(&path)?)?,
            expected,
            "{}",
            path.display()
        );
    }

    let project = tempfile::tempdir()?;
    let root = project.path();
    for deck in ["harness", "harness-en"] {
        let target = root.join(&paper).join(deck);
        fs::create_dir_all(&target)?;
        fs::write(target.join("harness.toml"), "")?;
        fs::copy(
            input.join(deck).join("structure.yaml"),
            target.join("structure.yaml"),
        )?;
        for entry in walkdir::WalkDir::new(input.join(deck).join("cards")) {
            let entry = entry?;
            if !entry.file_type().is_file()
                || entry.path().extension().is_none_or(|ext| ext != "md")
            {
                continue;
            }
            let output = target.join(entry.path().strip_prefix(input.join(deck))?);
            fs::create_dir_all(output.parent().unwrap())?;
            fs::copy(entry.path(), output)?;
        }
    }
    let (_, sections) = read_structure(&root.join(&english))?;
    let chapter_ids = sections
        .into_iter()
        .flat_map(|section| section.items)
        .filter_map(|item| match item {
            DocItem::Card { id } => Some(id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let expected = [
        "en-lem-spectral",
        "en-thm-energy",
        "en-thm-gap",
        "ob-lem-formula",
        "x-why-moments",
    ]
    .map(str::to_owned)
    .into_iter()
    .collect::<BTreeSet<_>>();
    let statuses = || {
        cards(root, &english)
            .into_iter()
            .map(|card| {
                let check = card
                    .translation
                    .with_context(|| format!("{} has no source link", card.id))?;
                Ok((card.id, check.status))
            })
            .collect::<Result<BTreeMap<_, _>>>()
    };
    let before = statuses()?;
    assert_eq!(before.len(), 131);
    let stale = before
        .iter()
        .filter(|&(_, status)| *status == TranslationStatus::Stale)
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(stale, expected);
    assert!(stale.is_disjoint(&chapter_ids));
    assert_eq!(
        before
            .values()
            .filter(|&&status| status == TranslationStatus::Current)
            .count(),
        126,
    );

    let probe_id = "core-def-normalized";
    let source = root
        .join(&paper)
        .join("harness/cards/def/core-def-normalized.md");
    let original = fs::read_to_string(&source)?;
    fs::write(
        &source,
        original.replacen("\n---\n", "\n---\n\nMigration regression probe.\n", 1),
    )?;
    let mut expected = before.clone();
    assert_eq!(
        expected.insert(probe_id.to_owned(), TranslationStatus::Stale),
        Some(TranslationStatus::Current)
    );
    assert_eq!(statuses()?, expected);

    let translated = root.join(&english).join("cards/def/core-def-normalized.md");
    let original = fs::read_to_string(&translated)?;
    fs::write(
        &translated,
        original.replacen("\n---\n", "\n---\n\nMigration regression probe.\n", 1),
    )?;
    let revision = cards(root, &english)
        .into_iter()
        .find(|card| card.id == probe_id)
        .and_then(|card| card.translation)
        .and_then(|check| check.revision)
        .context("missing revision for the drifted probe card")?;
    assert!(matches!(
        record_source_fingerprint(root, &english, probe_id, &revision, None)?,
        CardSaveOutcome::Saved
    ));
    assert_eq!(statuses()?, before);
    Ok(())
}
