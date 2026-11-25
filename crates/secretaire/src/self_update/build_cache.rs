use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{ensure, Context, Result};

const GIB: u64 = 1024 * 1024 * 1024;
const BUDGET: u64 = 16 * GIB;
const RESERVE: u64 = 8 * GIB;

pub(super) fn maintain(root: &Path, before_build: bool) -> Result<bool> {
    let root = root.canonicalize().context("open self-update storage")?;
    let target = root.join("target");
    fs::create_dir_all(&target)?;
    ensure!(target.canonicalize()? == target, "self-update target must be a real directory under the update root");
    let storage = fs::metadata(&target)?;
    let mut hard_links = HashSet::new();
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(&target).follow_links(false).same_file_system(true) {
        let metadata = entry?.metadata()?;
        ensure!(metadata.dev() == storage.dev(), "unexpected mount inside the self-update Cargo target");
        if metadata.nlink() > 1 && !hard_links.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        bytes = bytes.saturating_add(metadata.blocks().saturating_mul(512));
    }

    let free = |path: &Path| -> Result<u64> {
        let stat = rustix::fs::statvfs(path)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    };
    let clean = bytes > BUDGET || free(&target)? < RESERVE;
    if clean {
        // Removing a mounted cache could delete files on a separate volume.
        ensure!(storage.dev() == fs::metadata(&root)?.dev(), "cannot prune a mounted self-update Cargo target");
        fs::remove_dir_all(&target)?;
        fs::create_dir(&target)?;
    }
    if before_build {
        ensure!(free(&target)? >= RESERVE, "self-update requires 8 GiB free on the Cargo target volume");
    }
    Ok(clean)
}
