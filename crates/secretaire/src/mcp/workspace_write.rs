//! Workspace text mutations for an authenticated delegated job.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self as unix, AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Workspace;
use crate::cancellation::Cancellation;
use crate::delegate::touches::WriteSet;
use crate::folder_grants;
use crate::worker_scope;

use super::workspace_read::{open_no_symlinks, range::IntegerRange, worktree_relative};

const FILE_LIMIT: usize = 256 * 1024;
const PATCH_LIMIT: usize = 768 * 1024;
const RECOVERY: &str = "delegate-recovery";
pub(super) const AUDIT_LIMIT: IntegerRange = IntegerRange::new(1, 100);
static COMMIT: Mutex<()> = Mutex::new(());

pub(crate) const GUIDANCE: &str = "\n\nUse fuzukue fs_write, fs_edit, or fs_apply_patch for worktree edits. Avoid shell redirection, heredocs, and Python for writing files. Changes outside the job's touches require approval.";

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Write,
    Edit,
    ApplyPatch,
}

impl Tool {
    pub(crate) const ALL: [Self; 3] = [Self::Write, Self::Edit, Self::ApplyPatch];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Write => "fs_write",
            Self::Edit => "fs_edit",
            Self::ApplyPatch => "fs_apply_patch",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(super) fn definition(self) -> Value {
        let path = json!({"type":"string","description": harness_text!(
            "Worktree-relative file path with an existing parent directory. Symlinks and parent traversal are refused.",
            "作業ツリー相対のファイルパス。親ディレクトリは既存のものを指定する。シンボリックリンクと親への遡及は禁止。"
        )});
        let text = json!({"type":"string","maxLength": FILE_LIMIT});
        let (description, properties, required): (_, _, &[&str]) = match self {
            Self::Write => (
                harness_text!(
                    "Create or overwrite one UTF-8 file (256 KiB). Returns its diff and SHA-256. Changes outside touches require approval.",
                    "UTF-8 ファイル一件（256 KiB まで）を新規作成または上書きする。差分と SHA-256 を返す。touches の範囲外は承認が必要。"
                ),
                json!({"path": path, "content": text}),
                &["path", "content"],
            ),
            Self::Edit => (
                harness_text!(
                    "Replace one exact, unique string in a UTF-8 file (256 KiB). A unique replacement with no old match outside it is already applied. Changes outside touches require approval.",
                    "UTF-8 ファイル（256 KiB まで）内で一意に一致する文字列を置換する。置換後の文字列が一意で、その外側に置換前の一致がなければ適用済みとみなす。touches の範囲外は承認が必要。"
                ),
                json!({"path": path, "old": {"type":"string","minLength":1,"maxLength":FILE_LIMIT}, "new": text}),
                &["path", "old", "new"],
            ),
            Self::ApplyPatch => (
                harness_text!(
                    "Apply a unified text diff (768 KiB) to one UTF-8 file (256 KiB), with --- and +++ headers for path. Supports creation via /dev/null. Changes outside touches require approval.",
                    "単一の UTF-8 ファイル（256 KiB まで）に unified diff（768 KiB まで）を適用する。--- と +++ のヘッダーにパスを指定する。/dev/null からの新規作成にも対応。touches の範囲外は承認が必要。"
                ),
                json!({"path": path, "patch": {"type":"string","maxLength":PATCH_LIMIT}}),
                &["path", "patch"],
            ),
        };
        json!({"name": self.name(), "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required, "additionalProperties": false
        }})
    }

    pub(super) async fn call(
        self,
        app: &Workspace,
        session: &str,
        job: &str,
        args: &Value,
    ) -> Result<Value> {
        let (scope, cancel) = app.delegates.workspace_write_scope(session, job)?;
        let (path, operation) = match self {
            Self::Write => {
                let args: WriteArgs = super::parse_args(args)?;
                (args.path, Operation::Write(args.content))
            }
            Self::Edit => {
                let args: EditArgs = super::parse_args(args)?;
                (
                    args.path,
                    Operation::Edit {
                        old: args.old,
                        new: args.new,
                    },
                )
            }
            Self::ApplyPatch => {
                let args: PatchArgs = super::parse_args(args)?;
                (args.path, Operation::Patch(args.patch))
            }
        };
        let staged = {
            let scope = scope.clone();
            tokio::task::spawn_blocking(move || scope.prepare(path, operation)).await??
        };
        let needs_approval = staged.changed() && !scope.writes.contains(&staged.path);
        if needs_approval {
            let mut preview: String = staged.diff.chars().take(1500).collect();
            if preview.len() < staged.diff.len() {
                preview.push_str("\n…");
            }
            let summary = fuzukue_proto::LocalizedMessage::new(
                "approval-worker-write",
                [
                    ("tool", self.name().into()),
                    ("path", format!("{:?}", staged.path)),
                    ("hash", digest(staged.after.as_bytes())),
                    ("diff", preview),
                ],
            );
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(crate::cancellation::Cancelled.into()),
                _ = scope.closed.cancelled() => bail!("The worker job has ended"),
                decision = crate::delegate::request_workspace_write_approval(app, session, job, summary) => decision?,
            };
            ensure!(decision == "accept", "The out-of-scope write was declined");
        }
        tokio::task::spawn_blocking(move || scope.commit(self, staged, &cancel, needs_approval))
            .await?
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: PathBuf,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: PathBuf,
    old: String,
    new: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    path: PathBuf,
    patch: String,
}

enum Operation {
    Write(String),
    Edit { old: String, new: String },
    Patch(String),
}

pub(crate) struct Scope {
    root: PathBuf,
    writes: Arc<WriteSet>,
    protected: Vec<PathBuf>,
    recovery: PathBuf,
    closed: Cancellation,
}

impl Scope {
    pub(crate) fn new(
        root: PathBuf,
        writes: Arc<WriteSet>,
        state: &Path,
        job: &str,
    ) -> Result<Self> {
        ensure!(
            root == writes.root(),
            "The assigned worktree has changed since the job was queued"
        );
        let mut protected = worker_scope::protected_roots(state);
        protected.extend(folder_grants::protected_aliases());
        Ok(Self {
            root,
            writes,
            protected,
            recovery: recovery_dir(state, job)?,
            closed: Cancellation::default(),
        })
    }

    pub(crate) fn close(&self) {
        self.closed.cancel();
        // Wait for any write that passed its final cancellation check.
        drop(COMMIT.lock().unwrap());
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn check_running(&self, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        self.closed.check()?;
        Ok(())
    }

    pub(super) fn commit_guard(&self, cancel: &Cancellation) -> Result<MutexGuard<'static, ()>> {
        let guard = COMMIT.lock().unwrap();
        self.check_running(cancel)?;
        Ok(guard)
    }

    pub(super) fn check_commit_path(&self, relative: &Path) -> Result<()> {
        let (parent, name) = self.destination(relative)?;
        ensure!(self.writes.contains(relative),
            "Commit path {relative:?} is outside the job's touches. Ask the front desk to authorize the expanded scope.");
        match unix::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => ensure!(unix::FileType::from_raw_mode(stat.st_mode) == unix::FileType::RegularFile
                && stat.st_nlink == 1, "Commit paths must be regular files without links"),
            Err(rustix::io::Errno::NOENT) => {} // Git verifies a deleted path was tracked.
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub(super) fn record_commit_audit(&self, id: Option<&str>, record: &Value) -> Result<String> {
        let at_ms = record["at_ms"].as_i64().context("Missing commit audit timestamp")?;
        write_audit_record(&self.recovery, id, record, at_ms)
    }

    fn destination(&self, relative: &Path) -> Result<(File, PathBuf)> {
        ensure!(
            worktree_relative(relative),
            "Use a worktree-relative UTF-8 file path without .."
        );
        let absolute = self.root.join(relative);
        ensure!(
            !denied(&absolute) && !self.protected.iter().any(|path| absolute.starts_with(path)),
            "Access denied to a protected write path"
        );
        let parent = absolute.parent().context("Missing parent directory")?;
        // Walk the real directory components with O_NOFOLLOW, then keep the
        // descriptor for the final entry operations. No aliases are writable.
        let directory = open_no_symlinks(parent)
            .context("Could not open the destination parent without symlinks")?;
        ensure!(
            directory.metadata()?.is_dir(),
            "The destination parent must be a directory"
        );
        Ok((
            directory,
            relative.file_name().context("Missing file name")?.into(),
        ))
    }

    fn prepare(&self, path: PathBuf, operation: Operation) -> Result<Staged> {
        self.closed.check()?;
        let (directory, name) = self.destination(&path)?;
        let before = read(&directory, &name)?;
        let after = match operation {
            Operation::Write(content) => content,
            Operation::Edit { old, new } => edit(
                &before.as_ref().context("File does not exist")?.text,
                &old,
                &new,
            )?,
            Operation::Patch(patch) => apply_patch(
                &path,
                before.as_ref().map(|file| file.text.as_str()),
                &patch,
            )?,
        };
        check_text(&after)?;
        let diff = unified_diff(
            &path,
            before.as_ref().map(|file| file.text.as_str()),
            &after,
        )?;
        Ok(Staged {
            path,
            parent: identity(&directory)?,
            before,
            after,
            diff,
        })
    }

    fn commit(
        &self,
        tool: Tool,
        staged: Staged,
        cancel: &Cancellation,
        approved: bool,
    ) -> Result<Value> {
        let _lock = self.commit_guard(cancel)?;
        let content_hash = digest(staged.after.as_bytes());
        let changed = staged.changed();
        let (directory, name) = self.destination(&staged.path)?;
        ensure!(
            identity(&directory)? == staged.parent && read(&directory, &name)? == staged.before,
            "The destination changed while preparing the write. Read it again and retry."
        );
        let mut audit_id = None;
        if changed {
            let record = Audit {
                tool: tool.name(),
                path: &staged.path,
                at_ms: chrono::Utc::now().timestamp_millis(),
                previous_sha256: staged
                    .before
                    .as_ref()
                    .map(|file| digest(file.text.as_bytes())),
                sha256: &content_hash,
                approved,
                outcome: "pending",
            };
            let audit = write_audit(&self.recovery, None, &record)?;
            let result = replace(
                &directory,
                &name,
                staged.before.as_ref(),
                &staged.after,
                || {
                    cancel.check()?;
                    self.closed.check()
                },
            );
            let outcome = if result.is_ok() { "written" } else { "failed" };
            let recorded = write_audit(&self.recovery, Some(&audit), &Audit { outcome, ..record });
            result?;
            recorded.context("The file was written; the audit record still shows pending")?;
            audit_id = Some(audit);
        }
        Ok(
            json!({"path": staged.path, "changed": changed, "diff": staged.diff, "sha256": content_hash, "audit_id": audit_id}),
        )
    }
}

pub(crate) fn denied(path: &Path) -> bool {
    folder_grants::sensitive(path)
        || path.iter().any(|part| {
            let bytes = part.as_encoded_bytes();
            bytes.eq_ignore_ascii_case(b".git") || super::workspace_read::secret_name(bytes)
        })
}

#[derive(PartialEq, Eq)]
pub(super) struct Existing {
    pub(super) text: String,
    device: u64,
    inode: u64,
    mode: u32,
    ctime: (i64, i64),
}

fn identity(file: &File) -> Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(super) fn read(parent: &File, name: &Path) -> Result<Option<Existing>> {
    let file = match unix::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(error).context("Could not open the destination without following links");
        }
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "Only regular files without hard links are writable"
    );
    let mut text = String::new();
    file.take((FILE_LIMIT + 1) as u64)
        .read_to_string(&mut text)?;
    check_text(&text)?;
    Ok(Some(Existing {
        text,
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    }))
}

fn check_text(text: &str) -> Result<()> {
    ensure!(
        text.len() <= FILE_LIMIT && !text.contains('\0'),
        "Use UTF-8 text of at most 256 KiB without NUL bytes"
    );
    Ok(())
}

struct Staged {
    path: PathBuf,
    parent: (u64, u64),
    before: Option<Existing>,
    after: String,
    diff: String,
}

impl Staged {
    fn changed(&self) -> bool {
        self.before
            .as_ref()
            .is_none_or(|before| before.text != self.after)
    }
}

fn edit(text: &str, old: &str, new: &str) -> Result<String> {
    ensure!(!old.is_empty(), "The old string must be nonempty");
    check_text(old)?;
    check_text(new)?;
    // Search again one character after the first match so overlaps count as ambiguous.
    let unique = |needle: &str| {
        let first = text.find(needle)?;
        let next = first + needle.chars().next()?.len_utf8();
        (!text[next..].contains(needle)).then_some(first)
    };
    if let Some(index) = unique(new) {
        let end = index + new.len();
        // A replacement can itself contain the old string. Recognize it only
        // when all old matches are wholly inside the unique replacement.
        if text
            .find(old)
            .zip(text.rfind(old))
            .is_none_or(|(first, last)| first >= index && last + old.len() <= end)
        {
            return Ok(text.into());
        }
    }
    ensure!(
        unique(old).is_some(),
        "The old string does not occur uniquely in the file"
    );
    Ok(text.replacen(old, new, 1))
}

pub(super) fn replace(
    parent: &File,
    name: &Path,
    before: Option<&Existing>,
    content: &str,
    check_cancelled: impl FnOnce() -> Result<()>,
) -> Result<()> {
    check_text(content)?;
    let temporary = format!(".fuzukue-write-{}", crate::mail::session_key());
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC;
    let mut file = File::from(unix::openat(
        parent,
        temporary.as_str(),
        flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = (|| {
        file.write_all(content.as_bytes())?;
        if let Some(before) = before {
            unix::fchmod(&file, Mode::from_bits_truncate(before.mode & 0o777))?;
        }
        file.sync_all()?;
        check_cancelled()?;
        if before.is_some() {
            ensure!(
                read(parent, name)?.as_ref() == before,
                "The destination changed during the write"
            );
            unix::renameat(parent, temporary.as_str(), parent, name)?;
        } else {
            unix::linkat(parent, temporary.as_str(), parent, name, AtFlags::empty())?;
        }
        Ok(())
    })();
    let _ = unix::unlinkat(parent, temporary.as_str(), AtFlags::empty());
    result
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn git(directory: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env(
            "GIT_CEILING_DIRECTORIES",
            directory
                .parent()
                .context("Temporary directory has no parent")?,
        )
        .args(args)
        .output()
        .context("Could not run Git for the text diff")
}

fn unified_diff(path: &Path, before: Option<&str>, after: &str) -> Result<String> {
    if before == Some(after) {
        return Ok(String::new());
    }
    let temp = tempfile::tempdir()?;
    if let Some(before) = before {
        fs::write(temp.path().join("old"), before)?;
    }
    fs::write(temp.path().join("new"), after)?;
    let old = if before.is_some() { "old" } else { "/dev/null" };
    let output = git(
        temp.path(),
        &[
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--text",
            "--",
            old,
            "new",
        ],
    )?;
    ensure!(
        output.status.code() == Some(1),
        "Git could not produce a unified diff"
    );
    let raw = String::from_utf8(output.stdout)?;
    let path = path.to_str().context("Expected a UTF-8 destination")?;
    let old = if before.is_some() {
        format!("a/{path}\t")
    } else {
        "/dev/null".into()
    };
    match raw.find("\n@@ ") {
        Some(hunk) => Ok(format!("--- {old}\n+++ b/{path}\t\n{}", &raw[hunk + 1..])),
        None if before.is_none() && after.is_empty() => Ok(format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n"
        )),
        None => bail!("Missing unified diff hunks"),
    }
}

fn apply_patch(path: &Path, before: Option<&str>, patch: &str) -> Result<String> {
    ensure!(
        patch.len() <= PATCH_LIMIT && !patch.contains('\0'),
        "Expected a text patch of at most 768 KiB"
    );
    let path = path.to_str().context("Expected a UTF-8 destination")?;
    let mut lines = patch.split_inclusive('\n');
    let mut old = lines.next().context("Missing unified diff headers")?;
    if let Some(header) = old.strip_prefix("diff --git ") {
        ensure!(
            header.trim_end() == format!("a/{path} b/{path}"),
            "The Git diff header must match path"
        );
        old = lines.next().context("Missing --- header")?;
        if matches!(old, "new file mode 100644\n" | "new file mode 100644") && before.is_none() {
            let Some(next) = lines.next() else {
                return Ok(String::new());
            };
            old = next;
        }
        if old.starts_with("index ") {
            old = lines.next().context("Missing --- header")?;
        }
    }
    let header = |line: &str, prefix: &str, expected: &str| {
        line.strip_prefix(prefix).is_some_and(|line| {
            let label = line
                .trim_end_matches('\n')
                .split('\t')
                .next()
                .unwrap_or_default();
            label == expected
                || expected
                    .strip_prefix("a/")
                    .or_else(|| expected.strip_prefix("b/"))
                    == Some(label)
        })
    };
    let new = lines.next().context("Missing +++ header")?;
    let expected_old = if before.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".into()
    };
    ensure!(
        header(old, "--- ", &expected_old) && header(new, "+++ ", &format!("b/{path}")),
        "Patch headers must match path (a/path and b/path, or /dev/null for creation)"
    );
    // Give Git a fixed, private filename. Only hunk lines may follow the
    // verified headers, and Git must see exactly one patch for that filename.
    let hunks: String = lines.collect();
    ensure!(
        hunks.starts_with("@@ ")
            && hunks.lines().all(|line| {
                line.starts_with([' ', '+', '-'])
                    || line.starts_with("@@ ")
                    || line == "\\ No newline at end of file"
            }),
        "Expected only unified diff hunks"
    );
    let temp = tempfile::tempdir()?;
    if let Some(before) = before {
        fs::write(temp.path().join("content"), before)?;
    }
    let old = if before.is_some() {
        "a/content"
    } else {
        "/dev/null"
    };
    fs::write(
        temp.path().join("patch"),
        format!("--- {old}\n+++ b/content\n{hunks}"),
    )?;
    let stats = git(temp.path(), &["apply", "--numstat", "-z", "patch"])?;
    ensure!(
        stats.status.success()
            && stats
                .stdout
                .strip_suffix(b"\tcontent\0")
                .is_some_and(|row| !row.contains(&0)),
        "Expected exactly one valid file diff"
    );
    let applied = git(
        temp.path(),
        &["apply", "--unidiff-zero", "--whitespace=nowarn", "patch"],
    )?;
    ensure!(
        applied.status.success(),
        "Patch did not apply: {}",
        String::from_utf8_lossy(&applied.stderr).trim()
    );
    fs::read_to_string(temp.path().join("content"))
        .context("The patch must create or update the file")
}

#[derive(Serialize)]
struct Audit<'a> {
    tool: &'static str,
    path: &'a Path,
    at_ms: i64,
    previous_sha256: Option<String>,
    sha256: &'a str,
    approved: bool,
    outcome: &'static str,
}

pub(crate) fn recovery_dir(state: &Path, job: &str) -> Result<PathBuf> {
    let id = job
        .strip_prefix(fuzukue_proto::DELEGATE_JOB)
        .context("Expected a full delegate job ID")?;
    ensure!(
        !id.is_empty()
            && id.len() <= 100
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "Invalid delegate job ID"
    );
    Ok(state.join(RECOVERY).join(id))
}

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == ErrorKind::AlreadyExists && fs::symlink_metadata(path)?.is_dir() =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn prepare_recovery(path: &Path) -> Result<()> {
    private_dir(path.parent().context("Missing recovery root")?)?;
    private_dir(path)
}

fn write_audit(recovery: &Path, id: Option<&str>, record: &Audit<'_>) -> Result<String> {
    write_audit_record(recovery, id, record, record.at_ms)
}

fn write_audit_record(
    recovery: &Path, id: Option<&str>, record: &impl Serialize, at_ms: i64,
) -> Result<String> {
    prepare_recovery(recovery)?;
    let audit = recovery.join("write-audit");
    private_dir(&audit)?;
    let mut temp = tempfile::NamedTempFile::new_in(&audit)?;
    serde_json::to_writer(&mut temp, record)?;
    temp.as_file().sync_all()?;
    let name = id.map_or_else(
        || {
            let now = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or(at_ms);
            format!("{now:020}-{}", crate::mail::session_key())
        },
        str::to_owned,
    );
    let path = audit.join(format!("{name}.json"));
    if id.is_none() {
        temp.persist_noclobber(path)?;
    } else {
        temp.persist(path)?;
    }
    File::open(audit)?.sync_all()?;
    Ok(name)
}

pub(crate) fn audit_page(
    state: &Path,
    job: &str,
    before: Option<&str>,
    limit: i128,
) -> Result<Value> {
    let audit = recovery_dir(state, job)?.join("write-audit");
    let mut entries = match fs::read_dir(&audit) {
        Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    let mut rows = Vec::new();
    let limit = AUDIT_LIMIT.clamp(limit);
    let mut next_before = None;
    for entry in entries {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|file| file.strip_suffix(".json"))
            .map(str::to_owned)
        else {
            continue;
        };
        if before.is_some_and(|before| id.as_str() >= before) || !entry.file_type()?.is_file() {
            continue;
        }
        if rows.len() == limit {
            next_before = rows
                .last()
                .and_then(|row: &Value| row["id"].as_str().map(str::to_owned));
            break;
        }
        let mut record: Value = serde_json::from_reader(open_no_symlinks(&entry.path())?)?;
        record["id"] = json!(id);
        rows.push(record);
    }
    Ok(json!({"job": job, "writes": rows, "next_before": next_before}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scope_does_not_follow_links_or_allow_protected_paths() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?.join("worktree");
        let state = fixture.path().join("state");
        fs::create_dir(&root)?;
        fs::create_dir(&state)?;
        fs::create_dir(root.join("src"))?;
        fs::write(root.join("src/regular"), "old")?;
        fs::write(fixture.path().join("outside"), "untouched")?;
        symlink(fixture.path(), root.join("escape"))?;
        symlink(fixture.path().join("outside"), root.join("src/file"))?;
        symlink(root.join("src"), root.join("alias"))?;
        fs::hard_link(root.join("src/regular"), root.join("src/hardlink"))?;
        let writes = Arc::new(WriteSet::new(
            &root,
            Some(vec!["src/**".into()]),
            "",
            || None,
        )?);
        let scope = Scope::new(root, writes, &state, "delegate:scope-test")?;
        for path in [
            "../outside",
            "src/../outside",
            "escape/outside",
            "src/file",
            "alias/regular",
            "src/hardlink",
            ".GiT/config",
            ".sSh/authorized_keys",
            "src/.config/gh/hosts.yml",
            "src/.env.local",
            "",
            ".",
            "/etc/hosts",
        ] {
            assert!(
                scope
                    .prepare(path.into(), Operation::Write("new".into()))
                    .is_err(),
                "{path}"
            );
        }
        let in_scope = scope.prepare("src/new".into(), Operation::Write("new".into()))?;
        assert!(scope.writes.contains(&in_scope.path));
        let out = scope.prepare("other".into(), Operation::Write("new".into()))?;
        assert!(!scope.writes.contains(&out.path));
        assert_eq!(
            fs::read_to_string(fixture.path().join("outside"))?,
            "untouched"
        );
        Ok(())
    }

    #[test]
    fn exact_replacement_and_retry_preserve_content() -> Result<()> {
        assert_eq!(
            edit("one\nbefore\nthree\n", "before", "after")?,
            "one\nafter\nthree\n"
        );
        assert_eq!(
            edit("one\nafter\nthree\n", "before", "after")?,
            "one\nafter\nthree\n"
        );
        assert_eq!(edit("let x = 1", "1", "10")?, "let x = 10");
        assert_eq!(edit("let x = 10", "1", "10")?, "let x = 10");
        assert!(edit("aaaa", "aa", "x").is_err());
        assert!(edit("x x", "missing", "x").is_err());
        assert!(edit("x", "", "more").is_err());
        Ok(())
    }

    #[test]
    fn unified_diff_round_trips_creation_and_edits() -> Result<()> {
        let path = Path::new("dir/file name.txt");
        for (before, after) in [(Some("a\nb\n"), "a\nc"), (None, "new\n"), (Some("a"), "")] {
            let diff = unified_diff(path, before, after)?;
            assert!(diff.contains("@@ "));
            assert_eq!(apply_patch(path, before, &diff)?, after);
        }
        assert_eq!(unified_diff(path, Some("same"), "same")?, "");
        let empty = Path::new("empty");
        let patch = unified_diff(empty, None, "")?;
        assert!(patch.contains("new file mode"));
        assert_eq!(apply_patch(empty, None, &patch)?, "");
        assert!(
            apply_patch(
                path,
                None,
                "--- /dev/null\n+++ b/elsewhere\n@@ -0,0 +1 @@\n+new\n"
            )
            .is_err()
        );
        assert!(
            apply_patch(
                path,
                Some("old\n"),
                "--- a/dir/file name.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n"
            )
            .is_err()
        );
        let patch = "--- /dev/null\n+++ b/dir/file name.txt\n@@ -0,0 +1 @@\n+new\n--- /dev/null\n+++ b/extra\n@@ -0,0 +1 @@\n+extra\n";
        assert!(apply_patch(path, None, patch).is_err());
        Ok(())
    }
}
