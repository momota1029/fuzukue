use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use fuzukue_cards::DeckConfig;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Workspace;

pub(crate) const DECK: &str = "memos";
const CONFIG: &str = "kind = \"memos\"\ntitle = \"メモ\"\ndirs = [\".\"]\n";
const TITLE_LIMIT: usize = 80;

pub(crate) fn prompt() -> &'static str {
    harness_text!(
        "\nUse add_memo when the user asks to jot something down or remember it. Pass the memo text and an optional source.",
        "\n利用者が「メモしておいて」「覚えておいて」と頼んだら、本文と必要に応じて出どころを add_memo に渡す。",
    )
}

#[derive(Serialize)]
struct Front<'a> {
    id: &'a str,
    kind: &'static str,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct Added {
    pub id: String,
    pub path: String,
}

fn base_id(date: chrono::NaiveDate, word: &str) -> String {
    let slug = if word.is_ascii() {
        word.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .to_ascii_lowercase()
    } else {
        String::new()
    };
    let slug = slug[..slug.len().min(48)].trim_end_matches('-');
    if slug.is_empty() {
        let hash = format!("{:x}", Sha256::digest(word));
        format!("{date}-{}", &hash[..8])
    } else {
        format!("{date}-{slug}")
    }
}

fn ensure_deck(project: &Path) -> Result<PathBuf> {
    let deck = project.join(DECK);
    match fs::create_dir(&deck) {
        Ok(()) => (),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    ensure!(
        fs::symlink_metadata(&deck)?.is_dir(),
        harness_text!(
            "memos must be a project directory.",
            "memos はプロジェクト内のディレクトリにする"
        )
    );
    let config = deck.join("deck.toml");
    if !config.try_exists()? {
        let mut staged = tempfile::NamedTempFile::new_in(&deck)?;
        staged.write_all(CONFIG.as_bytes())?;
        match staged.persist_noclobber(&config) {
            Ok(_) => (),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        fs::symlink_metadata(&config)?.is_file() && DeckConfig::read(&deck)?.meta.kind == DECK,
        harness_text!("memos must be a memo deck.", "memos はメモのデッキにする")
    );
    Ok(deck)
}

#[cfg(test)]
fn add_at(
    project: &Path,
    date: chrono::NaiveDate,
    text: &str,
    from: Option<&str>,
) -> Result<Added> {
    add_with_image_at(project, date, text, from, None)
}

fn add_with_image_at(
    project: &Path,
    date: chrono::NaiveDate,
    text: &str,
    from: Option<&str>,
    image: Option<&[u8]>,
) -> Result<Added> {
    let word = text
        .split_whitespace()
        .next()
        .context(harness_text!("Enter some memo text.", "メモの本文を書く"))?;
    ensure!(
        !text.contains('\0'),
        harness_text!(
            "Memo text cannot contain NUL characters.",
            "メモの本文に NUL 文字は使えない"
        )
    );
    let deck = ensure_deck(project)?;
    let base = base_id(date, word);
    let (id, dir) = (1..)
        .find_map(|n| {
            let id = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let dir = deck.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => Some(Ok((id, dir))),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .unwrap()?;
    let write = || -> Result<()> {
        let title = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(TITLE_LIMIT)
            .collect();
        let front = serde_yaml::to_string(&Front {
            id: &id,
            kind: "memo",
            title,
            from,
        })?;
        let mut staged = tempfile::NamedTempFile::new_in(&dir)?;
        write!(staged, "---\n{front}---\n{text}")?;
        if let Some(png) = image {
            fs::write(dir.join("screenshot.png"), png)?;
            write!(staged, "\n\n![screenshot](screenshot.png)\n")?;
        }
        staged.persist_noclobber(dir.join("card.md"))?;
        Ok(())
    };
    if let Err(error) = write() {
        let _ = fs::remove_file(dir.join("screenshot.png"));
        let _ = fs::remove_dir(&dir);
        return Err(error);
    }
    let path = format!("{DECK}/{id}/card.md");
    Ok(Added { id, path })
}

pub(crate) fn memo_folder(project: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')),
        harness_text!("Invalid memo ID.", "メモの ID が不正")
    );
    let memos = project.join(DECK);
    let source = memos.join(id);
    ensure!(
        fs::symlink_metadata(&memos)?.is_dir()
            && fs::symlink_metadata(&source)?.is_dir()
            && fs::symlink_metadata(source.join("card.md"))?.is_file(),
        harness_text!("Memo folder not found.", "メモのフォルダが見つからない")
    );
    Ok(source)
}

fn move_folder(project: &Path, id: &str, deck: &str) -> Result<()> {
    let source = memo_folder(project, id)?;
    let memos = project.join(DECK);
    let relative = if deck == "." && project.join("cards").is_dir() {
        ".".into()
    } else {
        crate::workspace::deck_path(project, deck)?
    };
    let target = project.join(relative);
    let cfg = DeckConfig::read(&target)?;
    ensure!(
        !target.starts_with(&memos) && cfg.meta.kind != DECK,
        harness_text!(
            "Choose another deck for the memo.",
            "メモの移し先に別のデッキを選ぶ"
        )
    );
    let dir = target.join(&cfg.dirs[0]);
    // Validate the missing suffix before creating it.
    ensure!(
        !dir.components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        harness_text!(
            "Invalid destination card directory.",
            "移し先のカード用ディレクトリが不正"
        )
    );
    let mut parent = dir.as_path();
    while !parent.try_exists()? {
        parent = parent.parent().context("card directory has no parent")?;
    }
    let parent = parent.canonicalize()?;
    ensure!(
        parent.starts_with(project) && !parent.starts_with(&memos),
        harness_text!(
            "The destination card directory must be inside the project.",
            "移し先のカード用ディレクトリはプロジェクト内に置く"
        )
    );
    fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    ensure!(
        dir.starts_with(project) && !dir.starts_with(&memos),
        harness_text!(
            "The destination card directory must be inside the project.",
            "移し先のカード用ディレクトリはプロジェクト内に置く"
        )
    );
    let destination = dir.join(id);
    ensure!(
        matches!(fs::symlink_metadata(&destination), Err(error) if error.kind() == ErrorKind::NotFound),
        harness_text!(
            "The destination already contains this memo ID.",
            "移し先には同じ ID のフォルダがある"
        )
    );
    fs::rename(source, destination)?;
    Ok(())
}

pub(crate) async fn add(app: Arc<Workspace>, text: String, from: Option<String>) -> Result<Added> {
    add_with_image(app, text, from, None).await
}

pub(crate) async fn add_with_image(
    app: Arc<Workspace>, text: String, from: Option<String>, image: Option<Vec<u8>>,
) -> Result<Added> {
    let guard = app.file_save.clone().lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let added = add_with_image_at(
            &app.project_dir.canonicalize()?,
            chrono::Local::now().date_naive(),
            &text,
            from.as_deref(),
            image.as_deref(),
        )?;
        app.graph.invalidate();
        drop(_guard);
        crate::extensions::memo_added(&app, Path::new(&added.path));
        Ok(added)
    })
    .await?
}

pub(crate) async fn move_to(app: Arc<Workspace>, id: String, deck: String) -> Result<()> {
    let guard = app.file_save.clone().lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let _write = crate::cards::CARD_WRITE_LOCK.lock().unwrap();
        move_folder(&app.project_dir.canonicalize()?, &id, &deck)?;
        app.graph.invalidate();
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn date() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap()
    }

    #[test]
    fn first_word_is_slugged_or_hashed() {
        assert_eq!(
            base_id(date(), "(Sphere_Concentration!)"),
            "2026-11-20-sphere-concentration"
        );
        let hashed = base_id(date(), "球面");
        assert_eq!(hashed, base_id(date(), "球面"));
        assert_ne!(hashed, base_id(date(), "集中"));
        assert_eq!(hashed.len(), 19);
        assert!(hashed[11..].bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn creates_deck_and_deduplicates_while_preserving_text() -> Result<()> {
        let project = TestDir::new();
        assert!(add_at(project.path(), date(), " \t\n", None).is_err());
        assert!(!project.path().join(DECK).exists());
        let text = format!(
            " Sphere: {}\r\n\r\n  ---\nlast line ",
            "球".repeat(TITLE_LIMIT)
        );
        let from = "thread:alpha#user:3\nquoted: source";
        let first = add_at(project.path(), date(), &text, Some(from))?;
        let second = add_at(project.path(), date(), "Sphere another", None)?;
        let third = add_at(project.path(), date(), &text, None)?;
        assert_eq!(first.id, "2026-11-20-sphere");
        assert_eq!(second.id, "2026-11-20-sphere-2");
        assert_eq!(third.id, "2026-11-20-sphere-3");
        let root = project.path().join(DECK);
        let config = DeckConfig::read(&root)?;
        assert_eq!(config.meta.kind, DECK);
        assert_eq!(config.meta.title, "メモ");
        assert_eq!(config.dirs, [PathBuf::from(".")]);
        let path = project.path().join(first.path);
        let card = fuzukue_cards::read_card(&root, &path)?.unwrap();
        assert_eq!(card.kind, "memo");
        assert_eq!(
            card.title,
            text.lines()
                .next()
                .unwrap()
                .chars()
                .take(TITLE_LIMIT)
                .collect::<String>()
        );
        assert_eq!(card.from.as_deref(), Some(from));
        let raw = fs::read_to_string(path)?;
        assert_eq!(fuzukue_cards::split_frontmatter(&raw).unwrap().1, text);
        assert!(
            fuzukue_cards::read_card(&root, &project.path().join(second.path))?
                .unwrap()
                .from
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn moves_the_whole_folder_and_preserves_both_sides_on_collision() -> Result<()> {
        let project = TestDir::new();
        let root = project.path().canonicalize()?;
        let memo = add_at(&root, date(), "Sphere", None)?;
        project.write(
            "papers/target/deck.toml",
            "kind = 'paper'\ndirs = ['draft/cards']\n",
        );
        let dest = format!("papers/target/draft/cards/{}/card.md", memo.id);
        project.write(&dest, "existing");
        project.write(&format!("memos/{}/notes.txt", memo.id), "attachment");
        assert!(move_folder(&root, &memo.id, "memos").is_err());
        assert!(move_folder(&root, "../target", "papers/target").is_err());
        assert!(move_folder(&root, &memo.id, "papers/target").is_err());
        assert!(root.join(&memo.path).is_file());
        assert_eq!(fs::read_to_string(root.join(&dest))?, "existing");
        fs::remove_dir_all(root.join(&dest).parent().unwrap())?;
        move_folder(&root, &memo.id, "papers/target")?;
        assert!(!root.join(&memo.path).parent().unwrap().exists());
        let moved = root.join(dest);
        assert_eq!(
            fuzukue_cards::read_card(&root.join("papers/target"), &moved)?
                .unwrap()
                .kind,
            "memo"
        );
        assert_eq!(
            fs::read_to_string(moved.with_file_name("notes.txt"))?,
            "attachment"
        );
        Ok(())
    }
}
