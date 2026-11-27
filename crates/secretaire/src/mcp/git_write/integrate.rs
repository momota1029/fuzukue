//! Integrate a checked worker patch into the front desk's checkout.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use fuzukue_proto::{Actor, ApprovalRisk, ApprovalUnattended};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::hub::approval_store::{Action, FullAuthorization, Registration};
use crate::mcp::{data_result, harness_candidates::sanitize, workspace_read::worktree_relative};
use crate::mutations::Mutation;

use super::{
    bail, check_ref_paths, current_commit, ensure, fs, git_output, git_read, input_file, json, oid,
    open_regular_source, run, symbolic_head, symlink, Context, ErrorKind, File, Git, Identity,
    IndexLock, Path, PathBuf, Read, Result, Value, Workspace, MAX_MESSAGE, MAX_PATHS, MAX_PATH_BYTES,
};

pub(crate) const NAME: &str = "git_integrate_patch";
const MAX_PATCH: usize = 768 * 1024;
const MAX_DIFF: usize = 2 * 1024 * 1024;
const MAX_INDEX: usize = 16 * 1024 * 1024;
const MAX_LOCATIONS: usize = 40;

pub(in crate::mcp) fn definition() -> Value {
    json!({
        "name": NAME,
        "description": harness_text!(
            "Front desk only: use this instead of git apply/commit/restore in Bash. Check and commit a workspace patch with a clean index, scan added lines for private data, and restore only matching worktree files. Push only when the user asks and approves.",
            "受付専用。Bash の git apply/commit/restore の代わりに使う。インデックスがクリーンなときにパッチを検査し、追加行の機密情報を検出してからコミットする。内容が一致する対象ファイルだけを作業ツリーで復元し、利用者が依頼して承認した場合に限りプッシュする。"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "patch": {"type": "string", "minLength": 1, "maxLength": 4096},
                "message": {"type": "string", "minLength": 1, "maxLength": MAX_MESSAGE},
                "co_author": {"type": "string", "maxLength": 500, "description": "Name <email>"},
                "push": {"type": "boolean", "default": false},
                "restore_worktree": {"type": "boolean", "default": true},
                "allow_paths": {"type": "array", "maxItems": MAX_PATHS,
                    "items": {"type": "string", "minLength": 1, "maxLength": 4096}},
                "override_privacy": {"type": "boolean", "default": false,
                    "description": "Proceed with privacy findings only after explicit user approval. When approval is needed the call returns status pending with an approval_id at once. The result arrives later as a new message. Continue other work meanwhile."}
            },
            "required": ["patch", "message"],
            "additionalProperties": false
        }
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::mcp) struct Args {
    patch: String,
    message: String,
    co_author: Option<String>,
    #[serde(default)]
    pub(in crate::mcp) push: bool,
    #[serde(default = "default_restore")]
    restore_worktree: bool,
    allow_paths: Option<Vec<String>>,
    #[serde(default)]
    pub(in crate::mcp) override_privacy: bool,
}

fn default_restore() -> bool {
    true
}

impl Args {
    fn allow_paths(&self) -> Result<Option<GlobSet>> {
        globs(self.allow_paths.as_deref())
    }
}

fn globs(patterns: Option<&[String]>) -> Result<Option<GlobSet>> {
    {
        let Some(patterns) = patterns else {
            return Ok(None);
        };
        ensure!(
            patterns.len() <= MAX_PATHS
                && patterns.iter().map(String::len).sum::<usize>() <= MAX_PATH_BYTES,
            "Too many allow_paths globs or their combined size is too large"
        );
        let mut set = GlobSetBuilder::new();
        for pattern in patterns {
            ensure!(
                pattern.len() <= 4096 && worktree_relative(Path::new(pattern)),
                "Use workspace-relative globs in allow_paths"
            );
            set.add(
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .map_err(|_| anyhow::anyhow!("Invalid allow_paths glob"))?,
            );
        }
        Ok(Some(
            set.build().context("Could not compile allow_paths globs")?,
        ))
    }
}

impl Args {
    fn commit_message(&self, state: &Path) -> Result<String> {
        let mut message = self.message.clone();
        ensure!(
            !message.trim().is_empty() && !message.contains('\0') && message.len() <= MAX_MESSAGE,
            "Supply a nonblank commit message of at most 16 KiB, without NUL bytes"
        );
        let author = match &self.co_author {
            Some(author) => Some(author.to_owned()),
            None => configured_co_author(state)?,
        };
        if let Some(author) = author {
            let valid = author
                .strip_suffix('>')
                .and_then(|value| value.rsplit_once(" <"))
                .is_some_and(|(name, email)| {
                    !name.is_empty()
                        && name.trim() == name
                        && !name.contains(['<', '>'])
                        && !email.contains(['<', '>'])
                        && !email.chars().any(char::is_whitespace)
                        && email.split_once('@').is_some_and(|(local, domain)| {
                            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
                        })
                });
            ensure!(
                valid && author.len() <= 500 && !author.chars().any(char::is_control),
                "Supply co_author as Name <email> on a single line"
            );
            let trailer = format!("Co-Authored-By: {author}");
            if !message
                .lines()
                .any(|line| line.eq_ignore_ascii_case(&trailer))
            {
                message = format!("{}\n\n{trailer}\n", message.trim_end_matches('\n'));
            }
        }
        ensure!(
            message.len() <= MAX_MESSAGE,
            "The commit message and trailer exceed 16 KiB"
        );
        Ok(message)
    }
}

fn configured_co_author(state: &Path) -> Result<Option<String>> {
    #[derive(serde::Deserialize)]
    struct Config {
        delegate: Option<Delegate>,
    }
    #[derive(serde::Deserialize)]
    struct Delegate {
        git_integrate_co_author: Option<String>,
    }

    let path = state.join("fuzukue.toml");
    if matches!(path.symlink_metadata(), Err(error) if error.kind() == ErrorKind::NotFound) {
        return Ok(None);
    }
    let bytes = read_capped(open_regular_source(&path)?, 256 * 1024)?;
    let config: Config = toml::from_str(std::str::from_utf8(&bytes)?)
        .map_err(|_| anyhow::anyhow!("Invalid Git integration server configuration"))?;
    Ok(config
        .delegate
        .and_then(|delegate| delegate.git_integrate_co_author))
}

fn read_capped(file: File, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "File exceeds the {limit}-byte limit");
    Ok(bytes)
}

fn protected(root: &Path, state: &Path, path: &Path) -> bool {
    crate::mcp::workspace_write::denied(path)
        || crate::worker_scope::protected_roots(state)
            .iter()
            .any(|protected| root.join(path).starts_with(protected))
}

fn read_patch(root: &Path, state: &Path, patch: &str) -> Result<Vec<u8>> {
    let path = Path::new(patch);
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .context("The patch must be inside the workspace")?
    } else {
        path
    };
    ensure!(
        patch.len() <= 4096 && worktree_relative(relative) && !protected(root, state, relative),
        "Use a workspace patch path without traversal or protected components"
    );
    let bytes = read_capped(
        open_regular_source(&root.join(relative)).context("Cannot open the workspace patch")?,
        MAX_PATCH,
    )?;
    ensure!(
        !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok(),
        "The patch must be UTF-8 text without NUL bytes"
    );
    Ok(bytes)
}

#[derive(PartialEq, Eq)]
struct Entry {
    mode: String,
    oid: String,
}

struct Change {
    path: String,
    before: Option<Entry>,
    after: Option<Entry>,
}

fn changes(bytes: &[u8]) -> Result<Vec<Change>> {
    let mut fields = std::str::from_utf8(bytes)?.split_terminator('\0');
    let mut paths = Vec::new();
    let mut path_bytes = 0;
    while let Some(header) = fields.next() {
        let modes = header
            .strip_prefix(':')
            .context("Invalid Git diff metadata")?;
        let fields_in_header = modes.split_ascii_whitespace().collect::<Vec<_>>();
        let &[old_mode, new_mode, old_oid, new_oid, "A" | "D" | "M" | "T"] =
            fields_in_header.as_slice()
        else {
            bail!("Unexpected Git diff metadata");
        };
        let path = fields.next().context("Missing changed path")?;
        path_bytes += path.len();
        ensure!(
            paths.len() < MAX_PATHS
                && path_bytes <= MAX_PATH_BYTES
                && path.len() <= 4096
                && worktree_relative(Path::new(path)),
            "The patch has too many changed paths or an invalid path"
        );
        let entry = |mode: &str, hash: &str| -> Result<Option<Entry>> {
            match mode {
                "000000" => Ok(None),
                "100644" | "100755" | "120000" => Ok(Some(Entry {
                    mode: mode.to_owned(),
                    oid: oid(hash.as_bytes())?,
                })),
                _ => bail!("Patches changing Git submodules or unsupported entries are refused"),
            }
        };
        paths.push(Change {
            path: path.to_owned(),
            before: entry(old_mode, old_oid)?,
            after: entry(new_mode, new_oid)?,
        });
    }
    ensure!(!paths.is_empty(), "The patch would not change HEAD");
    Ok(paths)
}

fn safe_path(path: &str) -> String {
    sanitize::field("path", path, 4096, true).unwrap_or_else(|_| "<redacted-path>".into())
}

#[derive(Default)]
struct Findings {
    locations: Vec<String>,
    count: usize,
    /// Every flagged file, including those beyond the displayed locations.
    paths: BTreeSet<String>,
}

fn scan(diff: &[u8], changes: &[Change]) -> Result<Findings> {
    let diff = std::str::from_utf8(diff).context("The changed diff must be UTF-8 text")?;
    let mut findings = Findings::default();
    let mut files = changes.iter().flat_map(|change| {
        // Git prints a regular-file/link type change as a deletion and an addition.
        let type_changed = change
            .before
            .as_ref()
            .zip(change.after.as_ref())
            .is_some_and(|(before, after)| (before.mode == "120000") != (after.mode == "120000"));
        std::iter::repeat_n(change, 1 + usize::from(type_changed))
    });
    let mut file = None;
    let mut line_number = None;
    for line in diff.split_terminator('\n') {
        if line.starts_with("diff --git ") {
            file = files.next();
            ensure!(file.is_some(), "Unexpected file in Git's checked diff");
            line_number = None;
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            bail!("Binary patches are refused");
        } else if line.starts_with("@@ ") {
            let start = line
                .split_ascii_whitespace()
                .nth(2)
                .and_then(|range| range.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .context("Invalid Git hunk range")?;
            line_number = Some(start.parse::<usize>()?);
        } else if let Some(number) = &mut line_number {
            if let Some(text) = line.strip_prefix('+') {
                let text = text.strip_suffix('\r').unwrap_or(text);
                if sanitize::patch_line_has_private_data(text, MAX_DIFF) {
                    findings.count += 1;
                    let path = &file.context("Missing path for added line")?.path;
                    findings.paths.insert(path.clone());
                    if findings.locations.len() < MAX_LOCATIONS {
                        findings
                            .locations
                            .push(format!("{}:{number}", safe_path(path)));
                    }
                }
                *number += 1;
            }
            if line.starts_with(' ') {
                *number += 1;
            }
        }
    }
    ensure!(files.next().is_none(), "Missing file in Git's checked diff");
    Ok(findings)
}

struct Prepared {
    git: Git,
    reference: String,
    head: String,
    index_hash: [u8; 32],
    patch: Vec<u8>,
    patch_sha256: String,
    tree: String,
    changes: Vec<Change>,
    findings: Findings,
    message: String,
}

fn index_bytes(git: &Git) -> Result<Vec<u8>> {
    read_capped(
        open_regular_source(&git.metadata.join("index"))
            .context("Cannot open the workspace index")?,
        MAX_INDEX,
    )
}

fn check_repository(git: &Git) -> Result<()> {
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
    ] {
        ensure!(
            matches!(git.metadata.join(marker).symlink_metadata(), Err(e) if e.kind() == ErrorKind::NotFound),
            "Finish the in-progress Git operation before integrating a patch"
        );
    }
    Ok(())
}

fn patch_command(git: &Git, quarantine: Option<&Path>) -> Result<tokio::process::Command> {
    let mut command = git.command();
    if let Some(quarantine) = quarantine {
        let alternates = std::env::join_paths([git.config_root.join("objects")])
            .context("The Git objects path cannot be used for patch quarantine")?;
        command
            .env("GIT_OBJECT_DIRECTORY", quarantine)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates);
    }
    Ok(command)
}

async fn apply(
    git: &Git,
    index: &Path,
    bytes: &[u8],
    option: Option<&str>,
    quarantine: Option<&Path>,
) -> Result<Vec<u8>> {
    let mut command = patch_command(git, quarantine)?;
    command
        .env("GIT_INDEX_FILE", index)
        .args(["apply", "--cached", "--whitespace=nowarn"])
        .args(option)
        .args((option == Some("--numstat")).then_some("-z"))
        .arg("-")
        .stdin(input_file(bytes)?);
    git_output(
        command,
        match option {
            Some("--check") => "apply the patch to the clean index (check failed)",
            Some("--numstat") => "inspect the patch",
            _ => "apply the checked patch",
        },
    )
    .await
}

async fn prepare(root: &Path, state: &Path, args: &Args) -> Result<Prepared> {
    let message = args.commit_message(state)?;
    let allow_paths = args.allow_paths()?;
    let patch = read_patch(root, state, &args.patch)?;
    prepare_bytes(root, state, patch, message, allow_paths.as_ref(), args.push).await
}

/// Check exact patch bytes against the current HEAD and a clean index.
async fn prepare_bytes(
    root: &Path, state: &Path, patch: Vec<u8>, message: String, allow_paths: Option<&GlobSet>, push: bool,
) -> Result<Prepared> {
    let patch_sha256 = format!("{:x}", Sha256::digest(&patch));
    let git = Git::open(root, root)?;
    git.prepare_config().await?;
    check_repository(&git)?;
    let reference = symbolic_head(git.command()).await?;
    ensure!(
        !push || reference != "HEAD",
        "Push requires an attached branch"
    );
    check_ref_paths(&git, &reference)?;
    let head = current_commit(&git)
        .await?
        .context("The workspace must have a HEAD commit")?;
    let stage = git.view.path().join("patch-index");
    let index = index_bytes(&git)?;
    let index_hash = Sha256::digest(&index).into();
    fs::write(&stage, index)?;

    let mut command = git.command();
    command.env("GIT_INDEX_FILE", &stage).args([
        "diff",
        "--cached",
        "--quiet",
        "--no-ext-diff",
        "--ignore-submodules=none",
        "--ita-visible-in-index",
        &head,
        "--",
    ]);
    let status = git_read::output(command).await?;
    ensure!(status.exit_code != 1, "The index has staged changes");
    ensure!(
        status.exit_code == 0 && !status.truncated,
        "Could not verify the index is clean"
    );

    let stats = apply(&git, &stage, &patch, Some("--numstat"), None).await?;
    ensure!(
        !stats
            .split(|&byte| byte == 0)
            .any(|line| line.starts_with(b"-\t-\t")),
        "Binary patches are refused"
    );
    apply(&git, &stage, &patch, Some("--check"), None).await?;

    // git apply --cached writes blobs even with a private index. Keep unreviewed
    // content out of the real object database as well as the real index.
    let quarantine = git.view.path().join("patch-objects");
    fs::create_dir(&quarantine)?;
    let quarantine = Some(quarantine.as_path());
    apply(&git, &stage, &patch, None, quarantine).await?;
    let mut write = patch_command(&git, quarantine)?;
    write.env("GIT_INDEX_FILE", &stage).arg("write-tree");
    let tree = oid(&git_output(write, "write the patch tree").await?)?;
    let mut raw = patch_command(&git, quarantine)?;
    raw.args([
        "diff-tree",
        "--raw",
        "--no-abbrev",
        "--no-commit-id",
        "--no-renames",
        "-r",
        "-z",
        &head,
        &tree,
    ]);
    let raw = git_output(raw, "read the changed paths").await?;
    let changes = changes(&raw)?;
    for change in &changes {
        let path = Path::new(&change.path);
        ensure!(
            !protected(root, state, path),
            "The patch changes a protected path"
        );
        ensure!(
            allow_paths.is_none_or(|globs| globs.is_match(path)),
            "Changed path is outside allow_paths: {}",
            safe_path(&change.path)
        );
    }
    let mut diff = patch_command(&git, quarantine)?;
    diff.args([
        "diff",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--no-indent-heuristic",
        "--unified=0",
        &head,
        &tree,
        "--",
    ]);
    let diff = git_read::output_limited(diff, MAX_DIFF).await?;
    ensure!(
        diff.exit_code == 0 && !diff.truncated,
        "Could not read the checked diff within the size limit"
    );
    let findings = scan(&diff.bytes, &changes)?;

    let logs = git.config_root.join("logs");
    if git_read::check_entry(&logs, true)? {
        symlink(logs, git.view.path().join("logs"))?;
    }
    Ok(Prepared {
        git,
        reference,
        head,
        index_hash,
        patch,
        patch_sha256,
        tree,
        changes,
        findings,
        message,
    })
}

impl Prepared {
    async fn publish(&self, state: &Path, begin_write: impl FnOnce(), progress: &Progress) -> Result<String> {
        let git = &self.git;
        let mut lock = IndexLock::new(&git.metadata)?;
        check_repository(git)?;
        ensure!(
            symbolic_head(git.command()).await? == self.reference
                && current_commit(git).await?.as_deref() == Some(&self.head),
            "HEAD changed after the patch was checked. Retry against the new HEAD."
        );
        check_ref_paths(git, &self.reference)?;
        let index = index_bytes(git)?;
        let index_hash: [u8; 32] = Sha256::digest(&index).into();
        ensure!(
            index_hash == self.index_hash,
            "The index changed after the patch was checked. Review the staged changes and retry."
        );

        let identity = Identity::load(state, git).await?;
        let stage = git.view.path().join("patch-index");
        fs::write(&stage, index)?;
        apply(git, &stage, &self.patch, None, None).await?;
        let tree = oid(&run(
            git,
            &["write-tree"],
            Some(&stage),
            "write the approved tree",
        )
        .await?)?;
        ensure!(
            tree == self.tree,
            "The approved patch would now produce a different tree"
        );
        let mut create = git.command();
        identity.apply(&mut create);
        create
            .args([
                "-c",
                "commit.gpgSign=false",
                "commit-tree",
                &self.tree,
                "-p",
                &self.head,
            ])
            .stdin(input_file(self.message.as_bytes())?);
        let commit = oid(&git_output(create, "create the commit object").await?)?;
        lock.prepare(&stage)?;
        ensure!(lock.still_owned(), "The workspace index lock changed");

        let update_ref = |new: &str, old: &str, label: &str| {
            let mut command = git.command();
            identity.apply(&mut command);
            command.args([
                "update-ref",
                "--no-deref",
                "-m",
                label,
                &self.reference,
                new,
                old,
            ]);
            command
        };
        progress.record(|landed| landed.commit_sha = Some(commit.clone()));
        begin_write();
        git_output(
            update_ref(&commit, &self.head, "fuzukue: integrate patch"),
            "publish the patch commit",
        )
        .await?;
        progress.record(|landed| landed.ref_updated = true);
        #[cfg(test)]
        tests::fault("after_ref_update").await;
        if let Err(error) = lock.publish(&git.metadata.join("index")) {
            git_output(
                update_ref(&self.head, &commit, "fuzukue: roll back patch integration"),
                "roll back the commit after an index publication failure",
            )
            .await
            .with_context(|| format!("Could not publish the index: {error}"))?;
            progress.record(|landed| landed.rolled_back = true);
            return Err(error).context("Could not publish the index");
        }
        progress.record(|landed| landed.index_published = true);
        Ok(commit)
    }
}

/// The confirmed steps of one commit attempt. It lives outside the cancellable
/// operation, so an interrupted attempt can still say what landed.
#[derive(Default)]
struct Progress(std::sync::Mutex<Landed>);

#[derive(Clone, Default, serde::Serialize)]
struct Landed {
    /// The commit object this attempt created. See `ref_updated` for the branch.
    commit_sha: Option<String>,
    ref_updated: bool,
    index_published: bool,
    rolled_back: bool,
    restored: Vec<String>,
}

impl Progress {
    fn record(&self, change: impl FnOnce(&mut Landed)) {
        change(&mut self.0.lock().unwrap());
    }

    /// Describe an attempt that stopped after writing began.
    async fn review(&self, prepared: &Prepared, error: anyhow::Error) -> Value {
        let landed = self.0.lock().unwrap().clone();
        let head_now = tokio::time::timeout(std::time::Duration::from_secs(10), current_commit(&prepared.git))
            .await.ok().and_then(Result::ok).flatten();
        let paths = prepared.changes.iter().map(|change| &change.path).collect::<Vec<_>>();
        json!({
            "needs_review": true,
            "message": format!(
                "{error:#}. Writing had begun, so changes may have landed. It was not retried. Compare HEAD, the index and the listed paths with this report before doing anything else."
            ),
            "head_before": prepared.head,
            "head_now": head_now,
            "paths": paths,
            "landed": landed,
        })
    }
}

fn failure(code: &str, message: impl ToString) -> Value {
    data_result(
        json!({"error": {"code": code, "message": message.to_string()}}),
        true,
    )
}

/// The operation bound to a deferred approval. Execution never rereads the
/// workspace patch file or the server configuration.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    patch: String,
    patch_sha256: String,
    message: String,
    restore_worktree: bool,
    allow_paths: Option<Vec<String>>,
    override_privacy: bool,
    push_requested: bool,
    findings_count: usize,
    findings_locations: Vec<String>,
    findings_paths: BTreeSet<String>,
    paths: BTreeSet<String>,
    root: PathBuf,
    head: String,
}

impl Snapshot {
    fn new(root: &Path, args: &Args, prepared: &Prepared) -> Result<Self> {
        Ok(Self {
            patch: String::from_utf8(prepared.patch.clone()).context("The patch must be UTF-8 text")?,
            patch_sha256: prepared.patch_sha256.clone(),
            message: prepared.message.clone(),
            restore_worktree: args.restore_worktree,
            allow_paths: args.allow_paths.clone(),
            override_privacy: args.override_privacy,
            push_requested: args.push,
            findings_count: prepared.findings.count,
            findings_locations: prepared.findings.locations.clone(),
            findings_paths: prepared.findings.paths.clone(),
            paths: prepared.changes.iter().map(|change| change.path.clone()).collect(),
            root: root.into(),
            head: prepared.head.clone(),
        })
    }

    /// The snapshot fields the card shows. A pending card is reused only while they match.
    const REVIEWED: [&'static str; 4] = ["paths", "findings_count", "findings_locations", "findings_paths"];

    /// The decision identity. A pending request with the same identity is reused.
    fn action(&self) -> Action {
        Action {
            tool: NAME.into(),
            args: json!({
                "patch_sha256": self.patch_sha256,
                "message": crate::full_approval::fingerprint(&json!(self.message)),
                "restore_worktree": self.restore_worktree,
                "allow_paths": self.allow_paths,
                "override_privacy": self.override_privacy,
                "push": self.push_requested,
            }),
            scope: json!({"root": self.root}),
        }
    }

    fn registration(&self, session: &str) -> Registration {
        let short = |value: &str| value.chars().take(12).collect::<String>();
        let reason = if self.override_privacy {
            format!(
                "Integrate Git patch {} into {} files with a privacy override ({} flagged lines). Approve only if the flagged lines are intended.",
                short(&self.patch_sha256), self.paths.len(), self.findings_count,
            )
        } else {
            format!("Integrate Git patch {} into {} files.", short(&self.patch_sha256), self.paths.len())
        };
        Registration {
            owner: crate::deferred_approvals::owner(session),
            thread_auto: false,
            tool_item: None,
            anchor_item: None,
            risk: if self.override_privacy { ApprovalRisk::High } else { ApprovalRisk::Mid },
            // Every deferred request is left to a person, even if full approval mode is enabled later.
            unattended: ApprovalUnattended::NeverUnattended,
            reason,
            expires_at: fuzukue_proto::APPROVAL_NO_EXPIRY,
            action: self.action(),
            display_args: json!({
                "paths": self.paths,
                "patch_sha256": self.patch_sha256,
                "head": self.head,
                "override_privacy": self.override_privacy,
                "privacy_findings": self.findings_count,
                "locations": self.findings_locations,
                "restore_worktree": self.restore_worktree,
                "push": self.push_requested,
            }),
            display_scope: json!({"workspace": self.root.file_name().and_then(|name| name.to_str()).unwrap_or_default()}),
        }
    }
}

/// The decision identity of a saved snapshot, used to check it against the approval record.
pub(crate) fn snapshot_action(payload: &Value) -> Result<Action> {
    let snapshot: Snapshot = serde_json::from_value(payload.clone())
        .context("The saved patch snapshot is unreadable")?;
    Ok(snapshot.action())
}

/// Describe a deferred request from the snapshot bound to its card.
fn deferred(registered: crate::deferred_approvals::Registered) -> Result<Value> {
    use crate::deferred_approvals::Registered;
    let (id, payload, waiting) = match registered {
        Registered::Pending { id, payload, reused, replaced } => (id, payload, Some((reused, replaced))),
        Registered::Decided { id, payload } => (id, payload, None),
    };
    let snapshot: Snapshot = serde_json::from_value(payload)?;
    let mut data = json!({
        "status": if waiting.is_some() { "pending" } else { "decided" },
        "approval_id": id,
        "patch_sha256": snapshot.patch_sha256,
        "head": snapshot.head,
        "paths": snapshot.paths,
        "privacy_findings": snapshot.findings_count,
        "locations": snapshot.findings_locations,
        "push": if snapshot.push_requested {
            "not run after a deferred approval; request a push separately after the result"
        } else {
            "disabled"
        }
    });
    data["what_to_do_next"] = match waiting {
        Some((reused, replaced)) => {
            data["reused"] = json!(reused);
            if let Some(replaced) = replaced {
                data["replaced_approval_id"] = json!(replaced);
            }
            json!({
                "continue": "The approval card waits for the user without a deadline. Nothing has been committed yet. Tell the user briefly, then continue independent work or end the turn.",
                "result": "When the user decides, the result arrives as a new message in this thread. Do not call again to poll. The same request returns the same approval ID while its card shows the current findings.",
                "withdraw": "Call withdraw_approval with this approval_id if the user asks to cancel it or it is no longer needed."
            })
        }
        None => json!({
            "result": "The user has already decided this request, so no card is waiting. Its result arrives as a new message in this thread. Do not request it again before that message."
        }),
    };
    Ok(data_result(data, false))
}

/// How an approved snapshot ended once execution started. An error means nothing was written.
pub(crate) enum Execution {
    Completed(Value),
    /// Writing began and completion was not confirmed.
    NeedsReview(Value),
}

/// Run an approved snapshot once against the current HEAD and a clean index.
pub(crate) async fn execute_snapshot(
    app: &Arc<Workspace>, mutation: &Mutation<'_>, payload: &Value,
) -> Result<Execution> {
    let snapshot: Snapshot = serde_json::from_value(payload.clone())
        .context("The saved patch snapshot is unreadable. Nothing was changed")?;
    let root = app.project_dir.canonicalize()?;
    ensure!(snapshot.root == root, "The workspace moved after the approval was requested. Nothing was changed.");
    ensure!(
        format!("{:x}", Sha256::digest(snapshot.patch.as_bytes())) == snapshot.patch_sha256,
        "The saved patch does not match its approved hash. Nothing was changed."
    );
    let allow_paths = globs(snapshot.allow_paths.as_deref())?;
    let prepared = mutation.prepare(async {
        let _guard = app.git_lock.lock().await;
        prepare_bytes(&root, &app.state_dir, snapshot.patch.clone().into_bytes(), snapshot.message.clone(),
            allow_paths.as_ref(), false).await
    }).await.context("The approved patch cannot be applied to the current HEAD and index. Nothing was changed")?;
    let paths: BTreeSet<_> = prepared.changes.iter().map(|change| change.path.clone()).collect();
    ensure!(
        paths == snapshot.paths,
        "Against the current HEAD the patch would change other paths than approved. Nothing was changed."
    );
    let findings = &prepared.findings;
    ensure!(
        findings.count == 0 || snapshot.override_privacy
            && findings.count <= snapshot.findings_count
            && findings.paths.is_subset(&snapshot.findings_paths),
        "Against the current HEAD the patch has privacy findings that were not shown for approval. Nothing was changed."
    );
    let progress = Progress::default();
    let mut result = match commit(app, mutation, &root, &prepared, snapshot.restore_worktree, &progress).await {
        Ok(result) => result,
        Err(error) if mutation.wrote() => {
            let mut review = progress.review(&prepared, error).await;
            review["head_at_request"] = json!(snapshot.head);
            return Ok(Execution::NeedsReview(review));
        }
        Err(error) => return Err(error.context("Nothing was changed")),
    };
    if snapshot.push_requested {
        result["push"] = json!({"status": "not_run",
            "message": "Push is not run after a deferred approval. Request it separately if the user still wants it."});
    }
    result["head_at_request"] = json!(snapshot.head);
    Ok(Execution::Completed(result))
}

async fn commit(
    app: &Workspace, mutation: &Mutation<'_>, root: &Path, prepared: &Prepared, restore_worktree: bool,
    progress: &Progress,
) -> Result<Value> {
    let local = mutation
        .run(async {
            let _guard = app.git_lock.lock().await;
            let commit = prepared
                .publish(&app.state_dir, || mutation.begin_write(), progress)
                .await?;
            let restore = restore::report(root, &prepared.git, &prepared.changes, restore_worktree, progress).await;
            Ok((commit, restore))
        })
        .await;
    app.git_changed();
    let (commit, restore) = local?;
    let paths = prepared.changes.iter().map(|change| &change.path).collect::<Vec<_>>();
    Ok(json!({
        "commit_sha": commit, "paths": paths, "restore": restore,
        "push": {"status": "disabled"}
    }))
}

pub(in crate::mcp) async fn call(
    app: &Arc<Workspace>,
    session: &str,
    args: Args,
    local_approval: Option<Actor>,
    push_actor: Option<Actor>,
    trusted: bool,
    recheck: impl Future<Output = Result<()>>,
) -> Result<Value> {
    let mutation = Mutation::new(app, session, "Git patch integration".into())?;
    let root = app.project_dir.canonicalize()?;
    let prepared = match mutation
        .prepare(async {
            let _guard = app.git_lock.lock().await;
            prepare(&root, &app.state_dir, &args).await
        })
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => return Ok(failure("patch_check_failed", error)),
    };

    drop(mutation);
    let findings = &prepared.findings;
    if findings.count != 0 && !args.override_privacy {
        return Ok(data_result(
            json!({"error": {
                "code": "privacy_refused",
                "message": "Possible private data in added lines. Review these locations before requesting an explicit override.",
                "locations": findings.locations,
                "count": findings.count,
                "truncated": findings.count > findings.locations.len()
            }}),
            true,
        ));
    }
    let full = (trusted && !args.override_privacy)
            .then(|| crate::full_approval::desk_registration(
                session,
                "mcp",
                NAME,
                &crate::mail::session_key(),
                &json!({
                    "patch": prepared.patch_sha256,
                    "head": prepared.head,
                    "index": prepared.index_hash,
                    "message": crate::full_approval::fingerprint(&json!(prepared.message)),
                }),
            ));
    if args.override_privacy || local_approval.is_some() {
        local_approval.context("Privacy override requires front desk approval")?;
        let automatic = match full {
            Some(full) => app.hub.authorize_full_approval(full)?,
            None => FullAuthorization::Human,
        };
        match automatic {
            FullAuthorization::Granted => recheck.await?,
            FullAuthorization::AlreadyConsumed => return Ok(failure(
                "approval_failed",
                "This full approval was already consumed. Check the original operation before retrying.",
            )),
            FullAuthorization::Human => {
                // Bind the checked bytes and return at once. The user decides without a deadline.
                let snapshot = Snapshot::new(&root, &args, &prepared)?;
                let request = crate::deferred_approvals::Request {
                    registration: snapshot.registration(session),
                    payload: serde_json::to_value(&snapshot)?,
                    reviewed: &Snapshot::REVIEWED,
                };
                let registered = crate::deferred_approvals::register(app, session, request).await?;
                return deferred(registered);
            }
        }
    } else if let Some(full) = full {
        crate::full_approval::authorize(&app.hub, full)?;
    }

    let mutation = Mutation::new(app, session, "Git patch integration".into())?;
    let committed = commit(app, &mutation, &root, &prepared, args.restore_worktree, &Progress::default()).await;
    drop(mutation);
    let mut result = match committed {
        Ok(result) => result,
        Err(error) => return Ok(failure("commit_failed", error)),
    };
    if args.push {
        let actor = push_actor.context("Push requires front desk approval")?;
        let commit = result["commit_sha"].as_str().unwrap_or_default().to_owned();
        result["push"] = match app
            .git_remote_expected(
                session,
                actor,
                crate::git::RemoteOperation::Push,
                Some((&prepared.reference, &commit)),
            )
            .await
        {
            Ok(message) => json!({"status": "pushed", "message": message}),
            Err(_) => json!({"status": "failed",
                "message": "The guarded push did not complete. The local commit is available."}),
        };
    }
    let push_failed = result["push"]["status"] == "failed";
    Ok(data_result(result, push_failed))
}

#[path = "integrate_restore.rs"]
mod restore;

#[cfg(test)]
#[path = "integrate_tests.rs"]
mod tests;
