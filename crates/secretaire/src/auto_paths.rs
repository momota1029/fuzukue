//! Paths eligible for automatic PDF builds and reviews.
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, ensure};

const DECK_CONFIGS: &[&str] = &["deck.toml", "harness.toml", "cards.toml"];

pub(crate) fn excluded(project: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project) else {
        return true;
    };
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.starts_with('.')
            || matches!(name.as_ref(), "target" | "node_modules" | "_build")
            || name.starts_with("labo-worker-stage-")
            || name.starts_with("compact-target-review-")
    })
}

/// Use one check-ignore process for the whole batch. --no-index also applies
/// patterns to tracked staging copies. With no repository, use an empty Git
/// directory so Git still evaluates nested .gitignore files.
pub(crate) fn eligible(
    project: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<HashSet<PathBuf>> {
    let mut paths: HashSet<_> = paths
        .into_iter()
        .filter(|path| {
            !excluded(project, path)
                && !path
                    .ancestors()
                    .take_while(|p| *p != project)
                    .any(|p| p.is_symlink())
        })
        .collect();
    if paths.iter().all(|path| path == project) {
        return Ok(paths);
    }
    let mut command = git(project);
    let temporary_repo;
    if !project.ancestors().any(|root| {
        crate::git::metadata_dir(root).is_some_and(|metadata| metadata.join("HEAD").is_file())
    }) {
        temporary_repo = tempfile::tempdir()?;
        let result = git(temporary_repo.path())
            .args(["init", "--bare", "--quiet", "--template="])
            .output()?;
        ensure!(
            result.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        command
            .arg("--git-dir")
            .arg(temporary_repo.path())
            .arg("--work-tree")
            .arg(project);
    }
    // A file-backed stdin lets Git stream an arbitrarily large stdin and stdout.
    let mut input = tempfile::tempfile()?;
    for path in &paths {
        let relative = path.strip_prefix(project)?;
        if !relative.as_os_str().is_empty() {
            // Prevent a leading colon in a filename from becoming pathspec magic.
            input.write_all(b"./")?;
            input.write_all(relative.as_os_str().as_encoded_bytes())?;
            input.write_all(b"\0")?;
        }
    }
    input.rewind()?;
    let output = command
        .args(["check-ignore", "--no-index", "--stdin", "-z"])
        .stdin(Stdio::from(input))
        .output()?;
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "git check-ignore: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ignored: HashSet<_> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|path| path.strip_prefix(b"./"))
        .collect();
    paths.retain(|path| {
        !ignored.contains(
            path.strip_prefix(project)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes(),
        )
    });
    Ok(paths)
}

fn git(project: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(project);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        command.env_remove(key);
    }
    command
}

pub(crate) fn is_deck(path: &Path) -> bool {
    DECK_CONFIGS.iter().any(|name| path.join(name).is_file())
}

pub(crate) fn deck_eligible(project: &Path, path: &Path) -> bool {
    is_deck(path) && path_eligible(project, path)
}

pub(crate) fn path_eligible(project: &Path, path: &Path) -> bool {
    match eligible(project, [path.to_path_buf()]) {
        Ok(paths) => !paths.is_empty(),
        Err(error) => {
            tracing::warn!("automatic paper jobs: {error:#}");
            false
        }
    }
}

pub(crate) fn deck_roots(project: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    walkdir::WalkDir::new(project)
        .into_iter()
        .filter_entry(|entry| !excluded(project, entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && DECK_CONFIGS
                    .iter()
                    .any(|name| entry.file_name() == OsStr::new(name))
        })
        .filter_map(|entry| entry.path().parent().map(Path::to_path_buf))
}
