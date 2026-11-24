use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self as unix, AtFlags, Dir, FileType, Mode, OFlags};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use crate::worker_scope::{self, Scope};

#[derive(Clone)]
pub(crate) struct ReadScope {
    pub(super) workspace: PathBuf,
    pub(super) cwd: PathBuf,
    scratch: Option<PathBuf>,
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
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
        })
    }

    pub(super) fn check(&self, path: &Path) -> Result<()> {
        ensure!(
            self.roots.iter().any(|root| path.starts_with(root)),
            "Path is outside the workspace read scope"
        );
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
            "Access denied to Git metadata. Use git_show for history."
        );
        ensure!(
            !self.protected.iter().any(|root| path.starts_with(root))
                || self
                    .scratch
                    .as_ref()
                    .is_some_and(|scratch| path.starts_with(scratch)),
            "Access denied to Fuzukue configuration or state"
        );
        Ok(())
    }

    pub(super) fn resolve(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            !path.components().any(|part| part == Component::ParentDir),
            "Parent-directory components (..) are outside the workspace path policy"
        );
        let absolute = self.cwd.join(path);
        self.check(&absolute)?;
        let resolved = absolute
            .canonicalize()
            .context("Could not resolve workspace path")?;
        self.check(&resolved)?;
        Ok(resolved)
    }
}

fn secret(name: &[u8]) -> bool {
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

/// Open every component without following symlinks, including after a path check.
pub(super) fn open(path: &Path) -> Result<File> {
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
}

pub(super) struct Scan<'a> {
    pub scope: &'a ReadScope,
    pub depth: usize,
    pub hidden: bool,
    pub ignored: bool,
    pub deadline: Instant,
    remaining: usize,
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
        }
    }

    // The callback returns false when its result budget has been filled.
    pub fn visit(
        &mut self,
        path: &Path,
        file: &File,
        emit: &mut impl FnMut(Entry) -> bool,
    ) -> Result<bool> {
        self.directory(path, file, 0, emit)
    }

    fn directory(
        &mut self,
        path: &Path,
        file: &File,
        depth: usize,
        emit: &mut impl FnMut(Entry) -> bool,
    ) -> Result<bool> {
        let mut entries = Vec::new();
        let mut complete = true;
        for item in Dir::read_from(file)? {
            let item = item?;
            let name = item.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            if self.remaining == 0 || Instant::now() >= self.deadline {
                complete = false;
                break;
            }
            self.remaining -= 1;
            let git = name.eq_ignore_ascii_case(b".git");
            if git && !self.ignored
                || !self.ignored && matches!(name, b"target" | b"node_modules")
                || !self.hidden && name.starts_with(b".") && !git
                || secret(name)
            {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(name);
            let child = path.join(name);
            if !git && self.scope.check(&child).is_err() {
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
            entries.push((
                name.to_os_string(),
                Entry {
                    path: child,
                    kind,
                    size: stat.st_size.max(0) as u64,
                },
                git,
            ));
        }
        entries.sort_unstable_by(|(a, _, _), (b, _, _)| a.cmp(b));
        for (name, entry, git) in entries {
            let descend = entry.kind == "directory" && !git && depth + 1 < self.depth;
            let child = entry.path.clone();
            if !emit(entry) {
                return Ok(false);
            }
            if descend {
                let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
                match unix::openat(file, &name, flags, Mode::empty()) {
                    Ok(handle) => {
                        if !self.directory(&child, &File::from(handle), depth + 1, emit)? {
                            return Ok(false);
                        }
                    }
                    Err(_) => complete = false,
                }
            }
        }
        Ok(complete)
    }
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
