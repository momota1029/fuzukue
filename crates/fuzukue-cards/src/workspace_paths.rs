use std::path::{Component, Path, PathBuf};

use anyhow::{ensure, Context, Result};

/// Resolve a configured path, including an output that has not been created yet.
/// Check each existing component so a symlink cannot lead outside the workspace.
pub fn resolve(workspace: &Path, base: &Path, path: &Path) -> Result<PathBuf> {
    let root = workspace.canonicalize()?;
    let (mut full, tail) = if path.is_absolute() {
        let tail = path
            .strip_prefix(workspace)
            .or_else(|_| path.strip_prefix(&root))
            .with_context(|| format!("path escapes workspace: {}", path.display()))?;
        (root.clone(), tail)
    } else {
        (base.canonicalize()?, path)
    };
    ensure!(
        full.starts_with(&root),
        "path escapes workspace: {}",
        full.display()
    );

    for component in tail.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                ensure!(full != root, "path escapes workspace: {}", path.display());
                full.pop();
                continue;
            }
            Component::Normal(name) => full.push(name),
            _ => anyhow::bail!("invalid workspace path: {}", path.display()),
        }
        match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.is_symlink() => {
                full = full
                    .canonicalize()
                    .with_context(|| format!("cannot resolve symlink: {}", full.display()))?;
                ensure!(
                    full.starts_with(&root),
                    "path escapes workspace: {}",
                    path.display()
                );
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot resolve {}", full.display()))
            }
        }
    }
    Ok(full)
}

pub(crate) fn workspace_for(deck: &Path) -> &Path {
    deck.ancestors()
        .find(|p| fuzukue_proto::compat::workspace_config(p).is_file())
        .or_else(|| deck.ancestors().find(|p| p.join(".git").exists()))
        .or_else(|| deck.ancestors().find(|p| p.join("research").is_dir()))
        .unwrap_or(deck)
}

/// Express an absolute path relative to another directory in the same workspace.
pub fn relative(from: &Path, to: &Path) -> Result<PathBuf> {
    let common = from
        .ancestors()
        .find(|p| to.starts_with(p))
        .context("paths have no common root")?;
    let mut relative = PathBuf::new();
    for _ in from.strip_prefix(common)?.components() {
        relative.push("..");
    }
    Ok(relative.join(to.strip_prefix(common)?))
}
