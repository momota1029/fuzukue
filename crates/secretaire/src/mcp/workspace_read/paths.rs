use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self as unix, AtFlags, Dir, FileType, Mode, OFlags};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use crate::folder_grants::{self, Access, OutsideScope};
use crate::worker_scope::{self, Scope};

use super::ignores::Ignores;

pub(super) const SCAN_DEPTH: usize = 64;

#[derive(Clone)]
pub(crate) struct ReadScope {
    pub(super) workspace: PathBuf,
    pub(super) cwd: PathBuf,
    scratch: Option<PathBuf>,
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    sensitive_aliases: Vec<PathBuf>,
    access: Option<Access>,
}

impl ReadScope {
    pub(crate) fn new(workspace: &Path, state: &Path, worker: Option<&Scope>) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        let cwd = worker.map_or_else(|| workspace.clone(), |scope| scope.cwd.clone());
        let scratch = worker.map(|scope| scope.scratch.clone());
        let mut roots = worker_scope::workspace_read::configured_roots(&workspace, worker)?;
        roots.push(cwd.clone());
        roots.extend(scratch.iter().cloned());
        Ok(Self {
            workspace,
            cwd,
            scratch,
            roots,
            protected: worker_scope::protected_roots(state),
            sensitive_aliases: folder_grants::protected_aliases(),
            access: None,
        })
    }

    pub(crate) fn with_grants(mut self, access: Access) -> Self {
        self.access = Some(access);
        self
    }

    pub(super) fn in_scope(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    fn check_protected(&self, path: &Path) -> Result<()> {
        ensure!(
            !path
                .components()
                .any(|part| secret(part.as_os_str().as_bytes())),
            "Access denied to a secret-looking path"
        );
        ensure!(
            !path
                .components()
                .any(|part| part.as_os_str().as_bytes().eq_ignore_ascii_case(b".git")),
            "Access denied to Git metadata. Use git_read for status or refs, or git_show for files at a revision."
        );
        ensure!(
            !self.protected.iter().any(|root| path.starts_with(root))
                || self
                    .scratch
                    .as_ref()
                    .is_some_and(|scratch| path.starts_with(scratch)),
            "Access denied to Fuzukue configuration or state. Ask the front desk to use job_status or job_write_audit for job records."
        );
        ensure!(
            !folder_grants::sensitive(path) && !self.sensitive_aliases.iter().any(|root| path.starts_with(root)),
            "Access denied to a sensitive path"
        );
        Ok(())
    }

    fn granted(&self, path: &Path, audit: bool) -> Result<bool> {
        match &self.access {
            Some(access) => access.hub.folder_read(
                &access.session, path, access.worker_grants.as_deref(), audit,
            ),
            None => Ok(false),
        }
    }

    pub(super) fn check(&self, path: &Path) -> Result<()> {
        self.check_protected(path)?;
        ensure!(self.in_scope(path) || self.granted(path, false)?, OutsideScope(path.into()));
        Ok(())
    }

    /// Call immediately before traversing a directory or reading a file discovered by a scan.
    pub(super) fn audit_read(&self, path: &Path) -> Result<()> {
        self.check_protected(path)?;
        ensure!(self.in_scope(path) || self.granted(path, true)?, "Folder access expired or was revoked");
        Ok(())
    }

    pub(crate) fn resolve(&self, path: &Path) -> Result<PathBuf> {
        self.resolve_checked(path, true)
    }

    pub(super) fn resolve_checked(&self, path: &Path, audit: bool) -> Result<PathBuf> {
        ensure!(
            !path.components().any(|part| part == Component::ParentDir),
            "Parent-directory components (..) are outside the workspace path policy"
        );
        let absolute = self.cwd.join(path);
        self.check_protected(&absolute)?;
        if !self.in_scope(&absolute) && self.access.is_none() {
            return Err(OutsideScope(absolute).into());
        }
        let resolved = absolute.canonicalize().context("Could not resolve workspace path")?;
        self.check_protected(&resolved)?;
        if self.in_scope(&absolute) {
            ensure!(self.in_scope(&resolved), "Symlink target is outside the workspace read scope");
        } else {
            // Refuse aliases for externally granted reads. The descriptor walk also
            // refuses symlinks introduced after this check.
            ensure!(absolute == resolved, "Symlink is outside the workspace read scope for folder grants");
            if !self.granted(&resolved, audit)? {
                let folder = if resolved.is_dir() { resolved } else {
                    resolved.parent().context("No parent folder")?.to_path_buf()
                };
                return Err(OutsideScope(folder).into());
            }
        }
        Ok(resolved)
    }
}

pub(in crate::mcp) fn secret(name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    matches!(
        name.as_str(),
        ".env" | ".envrc" | ".ssh" | "keystore.properties" | "remote-token"
    ) || name.starts_with(".env.")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
}

/// Validate a literal UTF-8 path shared by checkout-scoped tools.
pub(in crate::mcp) fn worktree_relative(path: &Path) -> bool {
    path.components().all(|part| matches!(part, Component::Normal(_)))
        && path.to_str().is_some_and(|path| {
            !path.chars().any(char::is_control) && !path.contains('\\')
                && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        })
}

/// Check the nearest existing ancestor of a path without following aliases.
pub(in crate::mcp) fn check_git_path(path: &Path, root: &Path) -> Result<()> {
    for ancestor in path.ancestors().take_while(|path| *path != root) {
        match open(ancestor) {
            Ok(_) => break,
            Err(error) if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.context("Git paths cannot traverse symlinks")),
        }
    }
    Ok(())
}

/// Open every component without following symlinks, including after a path check.
pub(in crate::mcp) fn open(path: &Path) -> Result<File> {
    let mut parts = path.components().peekable();
    ensure!(
        parts.next() == Some(Component::RootDir),
        "Expected an absolute workspace path"
    );
    let mut file = File::from(unix::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    while let Some(Component::Normal(name)) = parts.next() {
        let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        if parts.peek().is_some() {
            flags |= OFlags::DIRECTORY;
        }
        file = File::from(unix::openat(&file, name, flags, Mode::empty())?);
    }
    Ok(file)
}

pub(super) fn regular(file: &File) -> Result<fs::Metadata> {
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "Only regular files can be read");
    ensure!(meta.nlink() == 1, "Access denied to a hard-linked file");
    Ok(meta)
}

pub(super) struct Entry {
    pub path: PathBuf,
    pub kind: &'static str,
    pub size: u64,
    pub mtime_unix_secs: i64,
    pub mtime_nanos: u32,
}

pub(super) struct Scan<'a> {
    pub scope: &'a ReadScope,
    pub depth: usize,
    pub hidden: bool,
    pub ignored: bool,
    pub deadline: Instant,
    remaining: usize,
    search: bool,
    rules: Option<Ignores>,
}

impl<'a> Scan<'a> {
    pub fn new(
        scope: &'a ReadScope,
        depth: usize,
        hidden: bool,
        ignored: bool,
        deadline: Instant,
    ) -> Self {
        Self {
            scope,
            depth,
            hidden,
            ignored,
            deadline,
            remaining: 20_000,
            search: false,
            rules: None,
        }
    }

    pub fn for_search(scope: &'a ReadScope, hidden: bool, ignored: bool, deadline: Instant) -> Self {
        let mut scan = Self::new(scope, SCAN_DEPTH, hidden, ignored, deadline);
        scan.search = true;
        scan.rules = (!ignored).then(Ignores::default);
        scan
    }

    // The callback returns false when its result budget has been filled.
    pub fn visit(
        &mut self,
        path: &Path,
        file: &File,
        emit: &mut impl FnMut(Entry) -> bool,
    ) -> Result<bool> {
        if let Some(rules) = &mut self.rules {
            // Inherit rules above a requested subdirectory up to the nearest scope boundary.
            let ancestors: Vec<_> = path.ancestors().skip(1)
                .take(SCAN_DEPTH)
                .take_while(|parent| self.scope.check(parent).is_ok())
                .collect();
            for parent in ancestors.into_iter().rev() {
                if Instant::now() >= self.deadline || !open(parent).is_ok_and(|handle| {
                    rules.load(self.scope, parent, &handle, self.deadline)
                }) {
                    return Ok(false);
                }
            }
        }
        let mut complete = true;
        Ok(self.directory(path, file, 0, &mut complete, emit)? && complete)
    }

    fn directory(
        &mut self,
        path: &Path,
        file: &File,
        depth: usize,
        complete: &mut bool,
        emit: &mut impl FnMut(Entry) -> bool,
    ) -> Result<bool> {
        let checkpoint = self.rules.as_ref().map_or(0, Ignores::checkpoint);
        if let Some(rules) = &mut self.rules {
            if !rules.load(self.scope, path, file, self.deadline) {
                rules.restore(checkpoint);
                *complete = false;
                return Ok(true);
            }
        }
        let mut entries = Vec::new();
        for item in Dir::read_from(file)? {
            let item = item?;
            let name = item.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            if Instant::now() >= self.deadline {
                return Ok(false);
            }
            if self.remaining == 0 {
                *complete = false;
                break;
            }
            self.remaining -= 1;
            let git = name.eq_ignore_ascii_case(b".git");
            if git && (self.search || !self.ignored)
                || !self.search && !self.ignored && matches!(name, b"target" | b"node_modules")
                || !self.hidden && name.starts_with(b".") && !git
                || secret(name)
            {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(name);
            let child = path.join(name);
            if (git && !self.scope.in_scope(path)) || (!git && self.scope.check(&child).is_err()) {
                continue;
            }
            let Ok(stat) = unix::statat(file, name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            let kind = match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => "directory",
                FileType::RegularFile if stat.st_nlink == 1 => "file",
                FileType::Symlink => "symlink",
                _ => continue,
            };
            if self.search && kind == "directory" && generated(name.as_bytes())
                || self.rules.as_mut().is_some_and(|rules| rules.excludes(&child, kind == "directory"))
            {
                continue;
            }
            entries.push((
                name.to_os_string(),
                Entry {
                    path: child,
                    kind,
                    size: stat.st_size.max(0) as u64,
                    mtime_unix_secs: stat.st_mtime as i64,
                    mtime_nanos: stat.st_mtime_nsec as u32,
                },
                git,
            ));
        }
        entries.sort_unstable_by(|(a, _, _), (b, _, _)| a.cmp(b));
        for (name, entry, git) in entries {
            if Instant::now() >= self.deadline {
                return Ok(false);
            }
            if self.depth == SCAN_DEPTH && entry.kind == "directory" && !git && depth + 1 == self.depth {
                *complete = false;
            }
            let descend = entry.kind == "directory" && !git && depth + 1 < self.depth;
            let child = entry.path.clone();
            if !emit(entry) {
                return Ok(false);
            }
            if descend {
                if self.remaining == 0 {
                    *complete = false;
                    continue;
                }
                self.scope.audit_read(&child)?;
                let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
                match unix::openat(file, &name, flags, Mode::empty()) {
                    Ok(handle) => {
                        if !self.directory(&child, &File::from(handle), depth + 1, complete, emit)? {
                            return Ok(false);
                        }
                    }
                    Err(_) => *complete = false,
                }
            }
        }
        if let Some(rules) = &mut self.rules {
            rules.restore(checkpoint);
        }
        Ok(true)
    }
}

fn generated(name: &[u8]) -> bool {
    matches!(name, b"target" | b"node_modules" | b"build" | b"dist" | b"out" | b"coverage"
        | b".next" | b".nuxt" | b".svelte-kit" | b".gradle" | b".venv" | b"__pycache__")
}

pub(super) fn text(bytes: &[u8]) -> Result<&str> {
    if bytes.starts_with(b"%PDF-")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP")
    {
        bail!("Image or PDF refused. Use the existing image viewer, open_file, or locate_pdf.");
    }
    ensure!(
        !bytes.contains(&0),
        "Binary file refused. Only UTF-8 text is supported."
    );
    std::str::from_utf8(bytes)
        .context("Binary or non-UTF-8 file refused. Only UTF-8 text is supported.")
}

pub(super) fn media(path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    ensure!(
        !matches!(
            extension.as_str(),
            "pdf"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "bmp"
                | "tif"
                | "tiff"
                | "ico"
                | "avif"
                | "heic"
                | "svg"
        ),
        "Image or PDF refused. Use the existing image viewer, open_file, or locate_pdf."
    );
    Ok(())
}
