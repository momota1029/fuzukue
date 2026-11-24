use super::{DEADLINE, MAX_BYTES, ReadScope, TRUNCATED, paths, search};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    rev: String,
    path: Option<String>,
    repository: Option<PathBuf>,
}

pub(super) async fn show(mut scope: ReadScope, args: Args) -> Result<Value> {
    ensure!(
        !args.rev.starts_with('-')
            && !args.rev.is_empty()
            && args.rev.len() <= 256
            && args
                .rev
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_+.-@{}^~".contains(&b)),
        "Supply a commit revision such as HEAD, HEAD~1, a ref, or a commit hash"
    );
    let external = if let Some(repository) = &args.repository {
        crate::folder_grants::absolute(repository)?;
        let directory = scope.resolve_checked(repository, false)?;
        if directory == scope.cwd {
            false
        } else {
            scope.cwd = directory;
            true
        }
    } else {
        false
    };
    if let Some(path) = &args.path {
        check_path(&scope, path)?;
    }
    let snapshot;
    let metadata = if external {
        scope.audit_read(&scope.cwd)?;
        if let Some(path) = &args.path {
            scope.audit_read(&scope.cwd.join(path))?;
        }
        let repository = scope.cwd.clone();
        snapshot = tokio::task::spawn_blocking(move || snapshot_metadata(&repository)).await??;
        snapshot.path().to_path_buf()
    } else {
        metadata_for(&scope)?
    };
    let worktree = if external { &metadata } else { &scope.cwd };
    let revision = format!("{}^{{commit}}", args.rev);
    let id = git(
        worktree,
        &metadata,
        &["rev-parse", "--verify", "--end-of-options", &revision],
        128,
    )
    .await?;
    let id = object_id(std::str::from_utf8(&id)?.trim_end())?;
    let mut bytes = if let Some(path) = &args.path {
        let entry = git(
            worktree,
            &metadata,
            &["ls-tree", "--full-tree", "-z", id, "--", path],
            8192,
        )
        .await?;
        let (info, entry_path) = std::str::from_utf8(&entry)?
            .split_once('\t')
            .context("Path is absent at this revision")?;
        ensure!(
            entry_path.strip_suffix('\0') == Some(path),
            "Supply the exact repository-relative file path"
        );
        let mut fields = info.split_ascii_whitespace();
        ensure!(
            matches!(fields.next(), Some("100644" | "100755")) && fields.next() == Some("blob"),
            "Only regular files can be read from a revision"
        );
        let blob = object_id(fields.next().context("Git did not return a blob ID")?)?;
        git(worktree, &metadata, &["cat-file", "blob", blob], MAX_BYTES).await?
    } else {
        git(
            worktree,
            &metadata,
            &[
                "show",
                "--no-patch",
                "--no-notes",
                "--no-show-signature",
                "--format=commit %H%nAuthor: %an <%ae>%nDate: %aI%n%n%B",
                id,
                "--",
            ],
            MAX_BYTES,
        )
        .await?
    };
    let truncated = bytes.len() > MAX_BYTES;
    if truncated {
        bytes.truncate(MAX_BYTES);
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    let mut text = paths::text(&bytes)?.to_owned();
    if truncated {
        text.push_str(&format!("\n{TRUNCATED} Reached the 64 KiB output limit."));
    }
    Ok(json!({"rev": id, "path": args.path, "text": text, "truncated": truncated}))
}

fn check_path(scope: &ReadScope, name: &str) -> Result<()> {
    let path = Path::new(name);
    ensure!(
        !name.is_empty()
            && name.len() <= 4096
            && !name.contains('\0')
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Supply a repository-relative file path without parent-directory components"
    );
    let absolute = scope.cwd.join(path);
    scope.check(&absolute)?;
    paths::media(path)?;
    // A historical file may have been deleted. Resolve its nearest existing
    // ancestor and still enforce the same scope on any symlinks in the worktree.
    for ancestor in absolute.ancestors() {
        match ancestor.symlink_metadata() {
            Ok(_) => {
                let resolved = scope.resolve_checked(ancestor, false)?;
                paths::media(&resolved)?;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Git must never open links, configuration, or alternate object stores from an
/// external repository. Supply a bounded copy of only the refs and objects.
fn snapshot_metadata(repository: &Path) -> Result<tempfile::TempDir> {
    let dot_git = repository.join(".git");
    ensure!(paths::open(&dot_git)?.metadata()?.is_dir(),
        "External Git history requires a standalone repository with a .git directory");
    let snapshot = tempfile::tempdir()?;
    let mut budget = SnapshotBudget {
        files: 30_000,
        bytes: 256 * 1024 * 1024,
        deadline: std::time::Instant::now() + DEADLINE,
    };
    for name in ["HEAD", "packed-refs", "refs", "objects"] {
        let source = dot_git.join(name);
        if name == "packed-refs" && matches!(source.symlink_metadata(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
            continue;
        }
        budget.copy(&source, &snapshot.path().join(name), 0, name == "objects")?;
    }
    Ok(snapshot)
}

struct SnapshotBudget {
    files: usize,
    bytes: u64,
    deadline: std::time::Instant,
}

impl SnapshotBudget {
    fn copy(&mut self, source: &Path, dest: &Path, depth: usize, objects: bool) -> Result<()> {
        use rustix::fs::Dir;
        use std::os::unix::ffi::OsStrExt;

        ensure!(depth < 32 && self.files > 0 && std::time::Instant::now() < self.deadline,
            "External Git metadata exceeded the snapshot limits");
        self.files -= 1;
        let file = paths::open(source)?;
        if file.metadata()?.is_dir() {
            std::fs::create_dir(dest)?;
            for item in Dir::read_from(&file)? {
                let item = item?;
                let name = item.file_name().to_bytes();
                if matches!(name, b"." | b"..") || objects && depth == 0 && name == b"info" {
                    continue;
                }
                let name = std::ffi::OsStr::from_bytes(name);
                self.copy(&source.join(name), &dest.join(name), depth + 1, objects)?;
            }
        } else {
            let metadata = paths::regular(&file)?;
            ensure!(metadata.len() <= self.bytes, "External Git metadata exceeded 256 MiB");
            let mut output = std::fs::File::create(dest)?;
            let mut input = file.take(self.bytes + 1);
            let mut buffer = [0; 64 * 1024];
            loop {
                ensure!(std::time::Instant::now() < self.deadline, "External Git metadata snapshot timed out");
                let bytes = input.read(&mut buffer)?;
                if bytes == 0 {
                    break;
                }
                ensure!(bytes as u64 <= self.bytes, "External Git metadata exceeded 256 MiB");
                self.bytes -= bytes as u64;
                output.write_all(&buffer[..bytes])?;
            }
        }
        Ok(())
    }
}

fn metadata_for(scope: &ReadScope) -> Result<PathBuf> {
    let (primary, common) = read_metadata(&scope.workspace)?;
    check_objects(&common)?;
    if scope.cwd == scope.workspace {
        return Ok(primary);
    }
    let (metadata, worktree_common) = read_metadata(&scope.cwd)?;
    ensure!(
        worktree_common == common,
        "Git metadata does not belong to the assigned workspace"
    );
    Ok(metadata)
}

pub(super) fn check_objects(common: &Path) -> Result<()> {
    let objects = common.join("objects");
    ensure!(
        paths::open(&objects)?.metadata()?.is_dir(),
        "Expected a local Git object store"
    );
    for name in ["alternates", "http-alternates"] {
        match objects.join("info").join(name).symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => anyhow::bail!(
                "Git history with alternate object stores is outside the workspace scope"
            ),
        }
    }
    Ok(())
}

pub(in crate::mcp) fn read_metadata(root: &Path) -> Result<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let file = paths::open(&dot_git).context("The assigned directory has no local Git metadata")?;
    if file.metadata()?.is_dir() {
        ensure!(
            matches!(dot_git.join("commondir").symlink_metadata(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "A local Git metadata directory cannot redirect to another repository"
        );
        return Ok((dot_git.clone(), dot_git));
    }
    let pointer = read_small(&dot_git)?;
    let location = pointer
        .strip_prefix("gitdir: ")
        .context("Invalid worktree Git metadata")?;
    let metadata = root
        .join(location.trim_end_matches(['\n', '\r']))
        .canonicalize()?;
    let backlink = read_small(&metadata.join("gitdir"))?;
    ensure!(
        root.join(backlink.trim_end_matches(['\n', '\r']))
            .canonicalize()?
            == dot_git,
        "Git metadata does not belong to this worktree"
    );
    let link = read_small(&metadata.join("commondir"))?;
    let common = metadata
        .join(link.trim_end_matches(['\n', '\r']))
        .canonicalize()?;
    ensure!(
        metadata.parent() == Some(&common.join("worktrees"))
            && paths::open(&common)?.metadata()?.is_dir(),
        "Git metadata does not belong to a registered worktree"
    );
    Ok((metadata, common))
}

fn read_small(path: &Path) -> Result<String> {
    let file = paths::open(path)?;
    paths::regular(&file)?;
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    ensure!(text.len() <= 4096, "Git metadata path is too long");
    Ok(text)
}

fn object_id(id: &str) -> Result<&str> {
    ensure!(
        matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid Git object ID"
    );
    Ok(id)
}

async fn git(worktree: &Path, metadata: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs([
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ])
        .arg("--no-pager")
        .arg("--literal-pathspecs")
        .arg("--git-dir")
        .arg(metadata)
        .arg("--work-tree")
        .arg(worktree)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Could not start Git")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, bytes, error) = tokio::time::timeout(DEADLINE, async {
        tokio::try_join!(
            child.wait(),
            search::collect(stdout, cap + 1),
            search::collect(stderr, 4096)
        )
    })
    .await
    .context("Git history read timed out")??;
    ensure!(
        status.success(),
        "Git history read failed: {}",
        String::from_utf8_lossy(&error).trim()
    );
    Ok(bytes)
}
