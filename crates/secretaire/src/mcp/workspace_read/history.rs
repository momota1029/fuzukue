use super::{DEADLINE, MAX_BYTES, ReadScope, TRUNCATED, paths, search};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    rev: String,
    path: Option<String>,
}

pub(super) async fn show(scope: ReadScope, args: Args) -> Result<Value> {
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
    if let Some(path) = &args.path {
        check_path(&scope, path)?;
    }
    let metadata = metadata_for(&scope)?;
    let revision = format!("{}^{{commit}}", args.rev);
    let id = git(
        &scope,
        &metadata,
        &["rev-parse", "--verify", "--end-of-options", &revision],
        128,
    )
    .await?;
    let id = object_id(std::str::from_utf8(&id)?.trim_end())?;
    let mut bytes = if let Some(path) = &args.path {
        let entry = git(
            &scope,
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
        git(&scope, &metadata, &["cat-file", "blob", blob], MAX_BYTES).await?
    } else {
        git(
            &scope,
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
                let resolved = scope.resolve(ancestor)?;
                paths::media(&resolved)?;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn metadata_for(scope: &ReadScope) -> Result<PathBuf> {
    let (primary, common) = read_metadata(&scope.workspace)?;
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

fn read_metadata(root: &Path) -> Result<(PathBuf, PathBuf)> {
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

async fn git(scope: &ReadScope, metadata: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>> {
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
        .arg(&scope.cwd)
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
