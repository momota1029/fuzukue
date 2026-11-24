//! Configured document roots, excluding the CLI sandbox's toolchain directories.

use super::{Config, Scope};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, Mode, OFlags};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) fn configured_roots(project: &Path, scope: Option<&Scope>) -> Result<Vec<PathBuf>> {
    let path = fuzukue_proto::compat::workspace_config(project);
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let root = fs::open(project, flags | OFlags::DIRECTORY, Mode::empty())?;
    let name = path
        .file_name()
        .context("Workspace configuration has no file name")?;
    let file = match fs::openat(root, name, flags | OFlags::NONBLOCK, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(error) => return Err(error).context("Cannot open workspace read-root config"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "Workspace read-root config must be a regular file"
    );
    const CONFIG_BYTES: usize = 1024 * 1024;
    let mut text = String::new();
    file.take((CONFIG_BYTES + 1) as u64)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= CONFIG_BYTES,
        "Workspace read-root config exceeds 1 MiB"
    );
    // A TOML parse error can contain an excerpt from the file.
    let config = toml::from_str::<Config>(&text)
        .map_err(|_| anyhow::anyhow!("Invalid workspace read-root config"))?;
    #[cfg(test)]
    let home = scope.map(|scope| scope.scratch.join("test-home"));
    #[cfg(not(test))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    config
        .agents
        .read_roots
        .into_iter()
        .filter_map(|root| {
            let resolved = (|| {
                let path = if let Ok(relative) = root.strip_prefix("~") {
                    home.as_ref()
                        .context("HOME is required for ~/ in agents.read_roots")?
                        .join(relative)
                } else {
                    project.join(root)
                };
                path.canonicalize()
                    .with_context(|| format!("workspace read root {}", path.display()))
            })();
            match &resolved {
                Ok(path) if scope.is_some_and(|scope| !scope.read_roots.contains(path)) => None,
                _ => Some(resolved),
            }
        })
        .collect()
}
