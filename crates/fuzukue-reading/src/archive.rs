use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{ensure, Context, Result};
use serde_json::json;
use walkdir::{DirEntry, WalkDir};

use crate::support::{command, id, sha, string, write};

const MAX_BYTES: u64 = 30 * 1024 * 1024;

fn keep(entry: &DirEntry, source: &Path) -> bool {
    let path = entry
        .path()
        .strip_prefix(source)
        .expect("entry is below the snapshot root");
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_type().is_dir() {
        return path != Path::new("related/code")
            && !matches!(
                entry.file_name().to_str(),
                Some("env" | ".temp" | "__pycache__" | "data" | ".venv" | "node_modules" | ".git")
            )
            && !entry
                .path()
                .parent()
                .is_some_and(|p| p.ends_with("results"));
    }
    true
}

fn copy(
    source: &Path,
    dest: &Path,
    relative: &Path,
    manifest: &mut BTreeMap<PathBuf, String>,
) -> Result<()> {
    let target = dest.join(relative);
    fs::create_dir_all(target.parent().context("snapshot path has no parent")?)?;
    fs::copy(source, &target).with_context(|| format!("copy {}", source.display()))?;
    // Windows preserves the last-write time in its native file copy.
    #[cfg(not(windows))]
    fs::File::open(&target)?
        .set_modified(fs::metadata(source)?.modified()?)
        .with_context(|| format!("preserve modification time of {}", target.display()))?;
    manifest.insert(relative.to_owned(), sha(fs::read(target)?));
    Ok(())
}

fn snapshot(
    source: &Path,
    dest: &Path,
    tag: &str,
    corpus: &Path,
    manifest: &mut BTreeMap<PathBuf, String>,
) -> Result<()> {
    ensure!(
        source.is_dir(),
        "source directory missing: {}",
        source.display()
    );
    for entry in WalkDir::new(source)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !e.path().starts_with(corpus) && keep(e, source))
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy();
        if name == ".DS_Store"
            || [".aux", ".log", ".synctex.gz"]
                .iter()
                .any(|ext| name.ends_with(ext))
        {
            continue;
        }
        let source_file = if entry.path_is_symlink() {
            let Ok(target) = entry.path().canonicalize() else {
                continue;
            };
            if !target.starts_with(source) || target.starts_with(corpus) {
                continue;
            }
            target
        } else if entry.file_type().is_file() {
            entry.path().to_owned()
        } else {
            continue;
        };
        let metadata = fs::metadata(&source_file)?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            continue;
        }
        let relative = Path::new(tag).join(entry.path().strip_prefix(source)?);
        copy(&source_file, dest, &relative, manifest)?;
    }
    Ok(())
}

pub fn archive(pair: &Path, workspace: &Path) -> Result<()> {
    let workspace = workspace.canonicalize()?;
    let corpus = workspace.join("writing/.corpus");
    let git_dir = corpus.join(".git");
    ensure!(
        corpus.canonicalize().ok().as_ref() == Some(&corpus)
            && git_dir.canonicalize().ok().as_ref() == Some(&git_dir)
            && git_dir.is_dir(),
        "refused: writing/.corpus is not an independent Git"
    );
    let git_root = command(
        Command::new("git")
            .arg("-C")
            .arg(&corpus)
            .args(["rev-parse", "--show-toplevel"]),
    )?;
    ensure!(
        Path::new(String::from_utf8(git_root)?.trim()).canonicalize()? == corpus,
        "refused: writing/.corpus is not an independent Git"
    );
    ensure!(
        command(Command::new("git").arg("-C").arg(&corpus).arg("remote"))?.is_empty(),
        "refused: the corpus has a remote (it must be local only)"
    );

    let pair = pair.canonicalize()?;
    ensure!(
        pair.starts_with(&workspace) && !pair.starts_with(&corpus),
        "PAIR.json must be inside the workspace and outside the corpus"
    );
    let lock = fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(git_dir.join("fuzukue-archive.lock"))?;
    lock.lock()?;
    let mut spec = crate::support::json(&pair)?;
    let slug = id(string(&spec, "slug"))?.to_owned();
    let date = chrono::Local::now().date_naive().to_string();
    let relative = Path::new("writing/.corpus/method-pairs")
        .join(&slug)
        .join(&date);
    let destination = fuzukue_cards::workspace_paths::resolve(
        &corpus,
        &corpus,
        relative.strip_prefix("writing/.corpus")?,
    )?;
    ensure!(
        destination == workspace.join(&relative),
        "archive destination contains a symlink"
    );
    let parent = destination.parent().context("archive has no parent")?;
    fs::create_dir_all(parent)?;
    let staged = tempfile::tempdir_in(parent)?;
    let base = pair.parent().context("pair has no parent")?;
    let file = |record, key| {
        let file = string(record, key);
        ensure!(!file.is_empty(), "pair needs {key}");
        let resolved = fuzukue_cards::workspace_paths::resolve(&workspace, base, Path::new(file))?;
        ensure!(
            !resolved.starts_with(&corpus),
            "pair source is inside the archive: {file}"
        );
        Ok::<_, anyhow::Error>(resolved)
    };
    let note_dir = file(&spec["note"], "source_dir")?;
    let companion_spec = file(&spec["companion"], "spec")?;
    let mut manifest = BTreeMap::new();
    copy(
        &file(&spec["note"], "pdf")?,
        staged.path(),
        Path::new("note.pdf"),
        &mut manifest,
    )?;
    copy(
        &file(&spec["companion"], "html")?,
        staged.path(),
        Path::new("companion.html"),
        &mut manifest,
    )?;
    snapshot(
        &note_dir,
        staged.path(),
        "source-note",
        &corpus,
        &mut manifest,
    )?;
    snapshot(
        companion_spec
            .parent()
            .context("companion spec has no parent")?,
        staged.path(),
        "source-companion",
        &corpus,
        &mut manifest,
    )?;
    write(
        &staged.path().join("MANIFEST.json"),
        serde_json::to_vec_pretty(
            &json!({"slug": slug, "paper": spec["paper"], "date": date, "files": manifest}),
        )?,
    )?;
    let backup = tempfile::tempdir_in(parent)?;
    let previous = backup.path().join("previous");
    if destination.exists() {
        fs::rename(&destination, &previous)?;
    }
    let transfer = (|| -> Result<()> {
        fs::rename(staged.path(), &destination)?;
        let corpus = crate::support::with(
            &spec["corpus"],
            [("dir", json!(relative.to_string_lossy().replace('\\', "/")))],
        );
        spec["corpus"] = corpus;
        write(&pair, serde_json::to_vec_pretty(&spec)?)
    })();
    if let Err(error) = transfer {
        if destination.exists() {
            fs::rename(&destination, staged.path())?;
        }
        if previous.exists() {
            fs::rename(&previous, &destination)?;
        }
        return Err(error);
    }
    println!(
        "copied {} files to {}; commit inside the corpus yourself (no remote, no push)",
        manifest.len(),
        relative.display()
    );
    Ok(())
}
