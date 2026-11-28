//! The list of segmented threads, kept at the root of the server state
//! directory. While it is not empty, or cannot be read, the server refuses to
//! switch to a build that cannot read the segmented format.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::durable;

/// Builds that read segmented threads embed this text in their executable. The
/// `--segmented-threads-format` argument prints it, which keeps it there.
pub(crate) const FORMAT_MARKER: &str = "secretaire-segmented-threads-v1";

const FILE: &str = "segmented-threads.json";
const LOCK: &str = "segmented-threads.lock";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Listed {
    pub(crate) workspace: String,
    /// Unknown when the front matter of a segmented file cannot be read.
    pub(crate) thread: Option<String>,
    pub(crate) file: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    threads: BTreeSet<Listed>,
}

/// Root used by synthetic fixtures with a single workspace.
#[cfg(test)]
pub(crate) fn root(state_dir: &Path) -> std::path::PathBuf {
    state_dir.parent().unwrap_or(state_dir).to_owned()
}

/// An absent file is an empty list. Any other failure is an error, which callers
/// treat as "a segmented format reader is required".
pub(crate) fn listed(root: &Path) -> Result<BTreeSet<Listed>> {
    let path = root.join(FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let marker: Marker = serde_json::from_slice(&bytes).with_context(|| format!("read {}", path.display()))?;
    Ok(marker.threads)
}

/// Change the list under a lock shared by every workspace. An unreadable list is
/// left untouched.
fn update(root: &Path, edit: impl FnOnce(&mut BTreeSet<Listed>)) -> Result<()> {
    durable::create_dir_all(root)?;
    let lock = File::options().create(true).truncate(false).read(true).write(true).open(root.join(LOCK))?;
    lock.lock()?;
    let mut threads = listed(root)?;
    let before = threads.clone();
    edit(&mut threads);
    if threads == before {
        return Ok(());
    }
    let path = root.join(FILE);
    if threads.is_empty() {
        fs::remove_file(path)?;
        durable::sync_dir(root)?;
        return Ok(());
    }
    let encoded = serde_json::to_vec(&Marker { threads })?;
    durable::write(&path, |output| Ok(output.write_all(&encoded)?))
}

fn workspace(project: &Path) -> String {
    project.to_string_lossy().into_owned()
}

pub(crate) fn add(root: &Path, project: &Path, thread: Option<&str>, file: &str) -> Result<()> {
    let entry = Listed { workspace: workspace(project), thread: thread.map(str::to_owned), file: file.into() };
    update(root, |threads| { threads.insert(entry); })
}

/// Remember whether this attempt inserted the mark, even if the write reports
/// an error after replacing the marker file.
pub(super) fn mark_split(
    root: &Path, project: &Path, thread: &str, file: &str, segmented: bool, owned: &mut Option<Listed>,
) -> Result<()> {
    let entry = Listed { workspace: workspace(project), thread: Some(thread.into()), file: file.into() };
    update(root, |threads| {
        let already_marked = threads.iter().any(|saved| saved.workspace == entry.workspace
            && (saved.thread == entry.thread || saved.file == entry.file));
        if !already_marked {
            threads.insert(entry.clone());
            if !segmented {
                *owned = Some(entry);
            }
        }
    })
}

pub(super) fn release_split(root: &Path, entry: Listed, is_single: impl Fn() -> bool) -> Result<()> {
    let mut removed = false;
    let result = update(root, |threads| {
        if threads.contains(&entry) && is_single() {
            removed = threads.remove(&entry);
        }
    });
    // A replacement can race the marker write. Restore the guard if the live
    // file stopped being a confirmed single-file thread during cleanup.
    if removed && !is_single() {
        update(root, |threads| { threads.insert(entry); })?;
    }
    result
}

pub(crate) fn remove(root: &Path, project: &Path, thread: &str) -> Result<()> {
    let workspace = workspace(project);
    update(root, |threads| {
        threads.retain(|entry| entry.workspace != workspace || entry.thread.as_deref() != Some(thread));
    })
}

/// At startup, drop entries of this workspace that `single` confirms are now in
/// the single-file format.
pub(crate) fn reconcile(root: &Path, project: &Path, single: impl Fn(&Listed) -> bool) -> Result<()> {
    if listed(root)?.is_empty() {
        return Ok(());
    }
    let workspace = workspace(project);
    update(root, |threads| threads.retain(|entry| entry.workspace != workspace || !single(entry)))
}
