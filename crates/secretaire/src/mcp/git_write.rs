//! Commit explicitly named files in an authenticated worker's isolated checkout.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::{Workspace, cancellation::Cancellation};

use super::workspace_read::git_read::{self, Git};
use super::workspace_read::{git_metadata, open_no_symlinks, open_regular_source};
use super::workspace_write::Scope;

pub(crate) mod integrate;

pub(crate) const NAME: &str = "git_commit_scoped";
const MAX_MESSAGE: usize = 16 * 1024;
const MAX_PATHS: usize = 100;
const MAX_PATH_BYTES: usize = 48 * 1024;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn definition() -> Value {
    json!({
        "name": NAME,
        "description": harness_text!(
            "Commit named files in the worker's isolated Git checkout. Each path must be within the job's touches. Supply the full commit message, including any trailers.",
            "作業役の分離チェックアウトで指定ファイルをコミットする。各パスはジョブの touches の範囲内に限る。トレーラーを含むコミットメッセージを指定する。"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {"type": "string", "minLength": 1, "maxLength": MAX_MESSAGE},
                "paths": {"type": "array", "minItems": 1, "maxItems": MAX_PATHS, "uniqueItems": true,
                    "items": {"type": "string", "minLength": 1, "maxLength": 4096}}
            },
            "required": ["message", "paths"],
            "additionalProperties": false
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    message: String,
    paths: Vec<String>,
}

impl Args {
    fn validate(&self, scope: &Scope) -> Result<()> {
        ensure!(
            !self.message.trim().is_empty()
                && !self.message.contains('\0')
                && self.message.len() <= MAX_MESSAGE,
            "Supply a nonblank commit message of at most 16 KiB, without NUL bytes"
        );
        ensure!(
            !self.paths.is_empty() && self.paths.len() <= MAX_PATHS,
            "Supply 1 to {MAX_PATHS} file paths"
        );
        let mut seen = BTreeSet::new();
        let mut bytes = 0;
        for path in &self.paths {
            ensure!(
                path.len() <= 4096 && seen.insert(path),
                "Commit paths must be distinct and at most 4096 bytes each"
            );
            bytes += path.len();
            scope.check_commit_path(Path::new(path))?;
        }
        ensure!(
            bytes <= MAX_PATH_BYTES,
            "Commit paths exceed the total size limit"
        );
        Ok(())
    }
}

pub(super) async fn call(app: &Workspace, session: &str, job: &str, args: &Value) -> Result<Value> {
    let args = super::parse_args(args)?;
    let (scope, cancel) = app.delegates.workspace_write_scope(session, job)?;
    let workspace = app.project_dir.canonicalize()?;
    let state = app.state_dir.clone();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _guard = scope.commit_guard(&cancel)?;
        runtime.block_on(commit(&workspace, &state, &scope, &cancel, args))
    })
    .await?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    name: String,
    email: String,
}

impl Identity {
    fn server_setting(state: &Path) -> Result<Option<Self>> {
        #[derive(Deserialize)]
        struct Config {
            delegate: Option<Delegate>,
        }

        #[derive(Deserialize)]
        struct Delegate {
            git_commit: Option<Identity>,
        }

        let file = state.join("fuzukue.toml");
        if matches!(file.symlink_metadata(), Err(err) if err.kind() == ErrorKind::NotFound) {
            return Ok(None);
        }
        let mut text = String::new();
        open_regular_source(&file)?
            .take(256 * 1024 + 1)
            .read_to_string(&mut text)?;
        ensure!(
            text.len() <= 256 * 1024,
            "Server configuration exceeds 256 KiB"
        );
        let config: Config = toml::from_str(&text).map_err(|_| {
            anyhow::anyhow!("Invalid server configuration for delegated Git commits")
        })?;
        Ok(config.delegate.and_then(|delegate| delegate.git_commit))
    }

    async fn load(state: &Path, git: &Git) -> Result<Self> {
        let identity = match Self::server_setting(state)? {
            Some(identity) => identity,
            None => {
                let mut name = None;
                let mut email = None;
                for (key, value) in git.config("^user\\.(name|email)$").await? {
                    match key.as_str() {
                        "user.name" => name = Some(value),
                        "user.email" => email = Some(value),
                        _ => {}
                    }
                }
                let (Some(name), Some(email)) = (name, email) else {
                    bail!(
                        "Configure the server's delegated commit identity or set both local Git user.name and user.email in this repository"
                    );
                };
                Self { name, email }
            }
        };
        for field in [&identity.name, &identity.email] {
            ensure!(
                !field.is_empty()
                    && field.len() <= 500
                    && field.trim() == field
                    && !field.contains(['<', '>'])
                    && !field.chars().any(char::is_control),
                "The configured Git commit identity is invalid"
            );
        }
        ensure!(
            identity
                .email
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@'))
                && !identity.email.chars().any(char::is_whitespace),
            "The configured Git email is invalid"
        );
        Ok(identity)
    }

    fn apply(&self, command: &mut Command) {
        command.envs([
            ("GIT_AUTHOR_NAME", &self.name),
            ("GIT_AUTHOR_EMAIL", &self.email),
            ("GIT_COMMITTER_NAME", &self.name),
            ("GIT_COMMITTER_EMAIL", &self.email),
        ]);
    }
}

async fn git_output(command: Command, action: &str) -> Result<Vec<u8>> {
    let result = git_read::output(command).await?;
    ensure!(
        result.exit_code == 0 && !result.truncated,
        "Git could not {action}"
    );
    Ok(result.bytes)
}

async fn run(git: &Git, args: &[&str], index: Option<&Path>, action: &str) -> Result<Vec<u8>> {
    let mut command = git.command();
    command.args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    git_output(command, action).await
}

fn oid(bytes: &[u8]) -> Result<String> {
    let hash = std::str::from_utf8(bytes)?.trim_end();
    ensure!(
        matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Git returned an invalid object ID"
    );
    Ok(hash.to_owned())
}

async fn symbolic_head(mut command: Command) -> Result<String> {
    command.args(["symbolic-ref", "--quiet", "HEAD"]);
    let result = git_read::output(command).await?;
    if result.exit_code == 1 && !result.truncated {
        return Ok("HEAD".into());
    }
    ensure!(
        result.exit_code == 0 && !result.truncated,
        "Git could not resolve the checkout's HEAD"
    );
    let reference = std::str::from_utf8(&result.bytes)?.trim_end();
    ensure!(
        reference.starts_with("refs/heads/"),
        "The checkout's HEAD must refer to a local branch or a commit"
    );
    Ok(reference.into())
}

async fn protect_shared_head(git: &Git, workspace: &Path, reference: &str) -> Result<()> {
    if reference == "HEAD" || git.metadata == git.config_root {
        return Ok(());
    }
    let (metadata, _) = git_metadata(workspace)?;
    let shared_head = symbolic_head(git.command_for(workspace, &metadata)).await?;
    ensure!(
        reference != shared_head,
        "The job's worktree points to the front desk's active branch"
    );
    Ok(())
}

fn check_ref_paths(git: &Git, reference: &str) -> Result<()> {
    if reference == "HEAD" {
        return Ok(());
    }
    ensure!(
        super::workspace_read::worktree_relative(Path::new(reference)),
        "Invalid branch reference path"
    );
    // Git's refs backend may follow directory symlinks while creating a lock.
    // Verify existing components of both the branch ref and its reflog.
    for prefix in [&git.config_root, &git.config_root.join("logs")] {
        super::workspace_read::check_git_path(&prefix.join(reference), &git.config_root)?;
    }
    Ok(())
}

async fn current_commit(git: &Git) -> Result<Option<String>> {
    let mut command = git.command();
    command.args(["rev-parse", "--quiet", "--verify", "HEAD^{commit}"]);
    let result = git_read::output(command).await?;
    ensure!(!result.truncated, "Git could not resolve HEAD");
    match result.exit_code {
        0 => Ok(Some(oid(&result.bytes)?)),
        1 if result.bytes.is_empty() => Ok(None),
        _ => bail!("Git could not resolve HEAD"),
    }
}

struct IndexLock {
    path: PathBuf,
    file: File,
    published: bool,
}

impl IndexLock {
    fn new(metadata: &Path) -> Result<Self> {
        let path = metadata.join("index.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&path)
            .context("Cannot lock the assigned checkout's index")?;
        Ok(Self {
            path,
            file,
            published: false,
        })
    }

    fn still_owned(&self) -> bool {
        self.path
            .symlink_metadata()
            .ok()
            .zip(self.file.metadata().ok())
            .is_some_and(|(path, file)| (path.dev(), path.ino()) == (file.dev(), file.ino()))
    }

    fn prepare(&mut self, updated: &Path) -> Result<()> {
        std::io::copy(&mut File::open(updated)?, &mut self.file)?;
        self.file.sync_all()?;
        Ok(())
    }

    fn publish(&mut self, index: &Path) -> Result<()> {
        ensure!(self.still_owned(), "The checkout's index lock changed");
        fs::rename(&self.path, index)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.published && self.still_owned() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn input_file(bytes: &[u8]) -> Result<Stdio> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(Stdio::from(file))
}

struct Prepared {
    commit: String,
    tree: String,
    paths: Vec<String>,
}

async fn prepare_commit(
    git: &Git,
    identity: &Identity,
    head: Option<&str>,
    args: &Args,
    scope: &Scope,
    lock: &mut IndexLock,
) -> Result<Option<Prepared>> {
    let stage = git.view.path().join("commit-index");
    let read_tree = ["read-tree", head.unwrap_or("--empty")];
    run(git, &read_tree, Some(&stage), "initialize the commit index").await?;
    let base = oid(&run(git, &["write-tree"], Some(&stage), "read the base tree").await?)?;
    let mut add = git.command();
    add.env("GIT_INDEX_FILE", &stage)
        .args(["add", "-A", "--"])
        .args(&args.paths);
    git_output(add, "stage the named paths").await?;
    let tree = oid(&run(
        git,
        &["write-tree"],
        Some(&stage),
        "write the selected tree",
    )
    .await?)?;
    if tree == base {
        return Ok(None);
    }

    let changed = run(
        git,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-r",
            "-z",
            &base,
            &tree,
        ],
        None,
        "check the changed paths",
    )
    .await?;
    let paths: Vec<_> = std::str::from_utf8(&changed)?
        .split_terminator('\0')
        .map(str::to_owned)
        .collect();
    ensure!(
        !paths.is_empty() && paths.iter().all(|path| args.paths.contains(path)),
        "The staged tree contains an unrequested path"
    );
    for path in &args.paths {
        scope.check_commit_path(Path::new(path))?;
    }

    // Build an updated copy of the real index. Remove all stages for each named
    // path before replacing it with precisely the stage-zero entry in the commit.
    let mut list = git.command();
    list.env("GIT_INDEX_FILE", &stage)
        .args(["ls-files", "--stage", "-z", "--"])
        .args(&args.paths);
    let entries = git_output(list, "read the selected index entries").await?;
    let mut selected = BTreeMap::new();
    for entry in std::str::from_utf8(&entries)?.split_terminator('\0') {
        let (header, path) = entry
            .split_once('\t')
            .context("Malformed Git index entry")?;
        let (mode, hash) = header
            .strip_suffix(" 0")
            .and_then(|header| header.split_once(' '))
            .context("Expected a stage-zero Git index entry")?;
        ensure!(
            matches!(mode, "100644" | "100755")
                && args.paths.iter().any(|arg| arg == path)
                && oid(hash.as_bytes())?.len() == tree.len(),
            "The selection must contain regular files only"
        );
        ensure!(
            selected.insert(path, entry).is_none(),
            "Git returned a duplicate index entry"
        );
    }
    let index = git.metadata.join("index");
    let updated = git.view.path().join("updated-index");
    if git_read::check_entry(&index, false)? {
        std::io::copy(&mut open_no_symlinks(&index)?, &mut File::create(&updated)?)?;
    }
    let mut input = String::new();
    let zero = "0".repeat(tree.len());
    for path in &args.paths {
        input.push_str(&format!("0 {zero}\t{path}\0"));
        if let Some(entry) = selected.get(path.as_str()) {
            input.push_str(entry);
            input.push('\0');
        }
    }
    let mut update = git.command();
    update
        .env("GIT_INDEX_FILE", &updated)
        .args(["update-index", "-z", "--index-info"])
        .stdin(input_file(input.as_bytes())?);
    git_output(update, "prepare the updated index").await?;
    lock.prepare(&updated)?;

    let mut create = git.command();
    identity.apply(&mut create);
    create.args(["-c", "commit.gpgSign=false", "commit-tree", &tree]);
    if let Some(head) = head {
        create.args(["-p", head]);
    }
    create.stdin(input_file(args.message.as_bytes())?);
    let commit = oid(&git_output(create, "create the commit object").await?)?;
    Ok(Some(Prepared {
        commit,
        tree,
        paths,
    }))
}

async fn commit(
    workspace: &Path,
    state: &Path,
    scope: &Scope,
    cancel: &Cancellation,
    args: Args,
) -> Result<Value> {
    let root = scope.root();
    ensure!(
        root != workspace,
        "Scoped Git commits require a separate worker checkout"
    );
    args.validate(scope)?;
    let git = Git::open(workspace, root)?;
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
    ] {
        match git.metadata.join(marker).symlink_metadata() {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => bail!("Finish the in-progress Git operation before making a scoped commit"),
        }
    }
    let logs = git.config_root.join("logs");
    if git_read::check_entry(&logs, true)? {
        symlink(logs, git.view.path().join("logs"))?;
    }
    let mut lock = IndexLock::new(&git.metadata)?;
    let (identity, reference, head, prepared) = tokio::time::timeout(PREPARE_TIMEOUT, async {
        git.prepare_config().await?;
        let identity = Identity::load(state, &git).await?;
        let reference = symbolic_head(git.command()).await?;
        protect_shared_head(&git, workspace, &reference).await?;
        check_ref_paths(&git, &reference)?;
        let head = current_commit(&git).await?;
        let prepared =
            prepare_commit(&git, &identity, head.as_deref(), &args, scope, &mut lock).await?;
        Ok::<_, anyhow::Error>((identity, reference, head, prepared))
    })
    .await
    .context("Preparing a scoped Git commit timed out")??;
    let Some(Prepared {
        commit,
        tree,
        paths,
    }) = prepared
    else {
        return Ok(
            json!({"created": false, "message": "No changes to commit in the selected paths.", "paths": []}),
        );
    };

    ensure!(
        symbolic_head(git.command()).await? == reference,
        "The checkout's HEAD changed"
    );
    protect_shared_head(&git, workspace, &reference).await?;
    check_ref_paths(&git, &reference)?;
    scope.check_running(cancel)?;
    ensure!(lock.still_owned(), "The checkout's index lock changed");
    let mut audit = json!({
        "tool": NAME, "paths": paths, "commit_sha": commit, "tree_sha": tree,
        "at_ms": chrono::Utc::now().timestamp_millis(), "approved": false, "outcome": "pending"
    });
    let audit_id = scope.record_commit_audit(None, &audit)?;

    // Compare-and-swap the ref while still holding the real index lock. If the
    // final index rename fails, restore the old ref only if it is still ours.
    let zero = "0".repeat(commit.len());
    let old = head.as_deref().unwrap_or(&zero);
    let mut publish = git.command();
    identity.apply(&mut publish);
    publish.args([
        "update-ref",
        "--no-deref",
        "-m",
        "fuzukue: scoped commit",
        &reference,
        &commit,
        old,
    ]);
    let result = async {
        git_output(
            publish,
            "publish the commit because HEAD or its ref may have changed",
        )
        .await?;
        if let Err(error) = lock.publish(&git.metadata.join("index")) {
            let mut rollback = git.command();
            identity.apply(&mut rollback);
            rollback.args([
                "update-ref",
                "--no-deref",
                "-m",
                "fuzukue: restore after scoped commit failure",
                &reference,
                old,
                &commit,
            ]);
            git_output(rollback, "restore the ref after an index failure")
                .await
                .with_context(|| format!("Index publication failed: {error:#}"))?;
            return Err(error);
        }
        Ok(())
    }
    .await;
    audit["outcome"] = json!(if result.is_ok() { "written" } else { "failed" });
    let recorded = scope.record_commit_audit(Some(&audit_id), &audit);
    result?;
    recorded.context("The commit succeeded; its audit record still shows pending")?;
    Ok(
        json!({"created": true, "commit_sha": commit, "tree_sha": tree, "paths": paths, "audit_id": audit_id}),
    )
}

#[cfg(test)]
mod tests;
