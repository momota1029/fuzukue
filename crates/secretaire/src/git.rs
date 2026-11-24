//! Git operations scoped to the root of the open workspace.

use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{Actor, GitCommit, GitFile, GitOutcome, GitRequest, GitStatus, ServerEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::Workspace;
use crate::cancellation::ProcessGroup;
use crate::mutations::Mutation;

pub(crate) fn prompt() -> &'static str {
    harness_text!(
        "\n\nUse git_status and git_diff to show changes. When the user asks for a commit or the agreed changes are complete, use git_commit at meaningful checkpoints with messages that capture the changes. Use git_push only when the user asks. Before calling merge_branch, ask the user to approve the specific branch and cleanup choice. Call it only after the user confirms.",
        "\n\n変更を示すときは git_status と git_diff を使う。利用者から頼まれたときや合意した変更が完了したときは、作業の区切りごとに要点が伝わるメッセージを書いて git_commit する。プッシュは利用者から頼まれたときだけ git_push で行う。merge_branch の前に対象ブランチの取り込みと作業場所の削除を利用者に確認し、承認を受けてから呼ぶ。",
    )
}

const DIFF_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const DIFF_NOTE: &str = "\n… 差分は上限の 64 KiB で打ち切った\n";

pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

async fn read_capped(mut stream: impl AsyncRead + Unpin, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // Both pipe reads can suspend together. Keep their buffers out of the Git future.
    let mut buf = vec![0; 8192];
    loop {
        let count = stream.read(&mut buf).await?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buf[..count.min(limit.saturating_sub(bytes.len()))]);
    }
}

pub(crate) async fn run(root: &Path, args: &[&str], limit: usize) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // The server's inherited Git environment must not redirect operations to another repo.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        command.env_remove(name);
    }
    let mut child = command.spawn().context("Git を起動できない")?;
    let mut process_group = ProcessGroup::new(&child)?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(
            child.wait(),
            read_capped(stdout, limit + 1),
            read_capped(stderr, 8192)
        )
    })
    .await
    .context("Git の処理がタイムアウトした")??;
    process_group.finished();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

pub(crate) fn error(operation: &str, output: &Output) -> anyhow::Error {
    anyhow::anyhow!(
        "Git {operation} に失敗した（{}）\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn text(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes).context("Git の出力に UTF-8 で表せない名前がある")?)
}

/// Directory containing worktree-local metadata such as HEAD and the index.
pub(crate) fn metadata_dir(project: &Path) -> Option<PathBuf> {
    let dot_git = project.join(".git");
    if dot_git.is_dir() {
        return dot_git.canonicalize().ok();
    }
    let pointer = std::fs::read_to_string(dot_git).ok()?;
    let path = pointer
        .strip_prefix("gitdir: ")?
        .trim_end_matches(['\n', '\r']);
    project.join(path).canonicalize().ok()
}

pub(crate) struct Repo {
    pub(crate) root: PathBuf,
    read_only: bool,
}

impl Repo {
    pub(crate) async fn open(project: &Path) -> Result<Self> {
        let root = project.canonicalize()?;
        let repo = Self {
            root,
            read_only: false,
        };
        // --show-prefix is empty only at the top of the worktree.
        let prefix = repo
            .git(&["rev-parse", "--is-inside-work-tree", "--show-prefix"])
            .await
            .context("ワークスペースに Git リポジトリがない")?;
        ensure!(
            prefix == "true\n\n",
            "Git のルートをワークスペースとして開く"
        );
        let common = repo
            .git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .await?;
        let common = Path::new(common.strip_suffix('\n').unwrap_or(&common)).canonicalize()?;
        // Keep the read-only flag available for tests and future workspace policies.
        let _ = common;
        Ok(repo)
    }

    fn writable(&self) -> Result<()> {
        ensure!(!self.read_only, "読み取り専用のリポジトリ");
        Ok(())
    }

    fn path(&self, path: &str) -> Result<()> {
        let relative = Path::new(path);
        ensure!(
            !path.is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(name) if name != ".git")),
            "ワークスペース相対の Git パスを指定する: {path}"
        );
        // A deleted file can be used in a pathspec. Check the nearest existing
        // ancestor, including dangling symlinks so they cannot hide an escape.
        let mut full = self.root.join(relative);
        loop {
            match full.symlink_metadata() {
                Ok(_) => {
                    ensure!(
                        full.canonicalize().is_ok_and(|p| p.starts_with(&self.root)),
                        "パスをワークスペース内に解決できない: {path}"
                    );
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ensure!(full.pop(), "パスが見つからない: {path}");
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub(crate) async fn git(&self, args: &[&str]) -> Result<String> {
        let output = run(&self.root, args, OUTPUT_LIMIT).await?;
        ensure!(output.status.success(), "{}", error(args[0], &output));
        ensure!(
            output.stdout.len() <= OUTPUT_LIMIT,
            "Git の出力が上限を超えた"
        );
        text(output.stdout)
    }

    async fn status(&self) -> Result<GitStatus> {
        let raw = self
            .git(&[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
                "-z",
            ])
            .await?;
        parse_status(&raw, self.read_only)
    }

    async fn log(&self, limit: u32) -> Result<Vec<GitCommit>> {
        let limit = format!("-{}", limit.clamp(1, 100));
        let output = run(
            &self.root,
            &[
                "log",
                &limit,
                "--encoding=UTF-8",
                "--format=%H%x00%aI%x00%s",
                "-z",
            ],
            OUTPUT_LIMIT,
        )
        .await?;
        if !output.status.success() {
            // An unborn branch also has a useful, empty history.
            if output.status.code() == Some(128)
                && self.git(&["symbolic-ref", "-q", "HEAD"]).await.is_ok()
                && self.git(&["rev-parse", "--verify", "HEAD"]).await.is_err()
            {
                return Ok(Vec::new());
            }
            return Err(error("log", &output));
        }
        ensure!(
            output.stdout.len() <= OUTPUT_LIMIT,
            "Git の履歴が上限を超えた"
        );
        let raw = text(output.stdout)?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut fields = raw.split_terminator('\0');
        let mut commits = Vec::new();
        while let Some(hash) = fields.next() {
            commits.push(GitCommit {
                hash: hash.into(),
                date: fields.next().context("Git の履歴に日付がない")?.into(),
                subject: fields.next().context("Git の履歴に件名がない")?.into(),
            });
        }
        Ok(commits)
    }

    async fn diff(&self, path: Option<&str>, staged: bool) -> Result<String> {
        if let Some(path) = path {
            self.path(path)?;
        }
        let options = ["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
        let mut args = options.to_vec();
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.extend(path);
        let old_path = if let Some(path) = path.filter(|_| staged) {
            self.status()
                .await?
                .files
                .into_iter()
                .find(|file| file.path == path)
                .and_then(|file| file.old_path)
        } else {
            None
        };
        if let Some(old) = old_path.as_deref() {
            self.path(old)?;
            args.push(old);
        }
        let mut bytes = self.diff_bytes(&args, DIFF_LIMIT, false).await?;
        if !staged && bytes.len() <= DIFF_LIMIT {
            let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
            args.extend(path);
            let files = self.git(&args).await?;
            for file in files.split_terminator('\0') {
                // --no-index treats the path as an OS path rather than a Git pathspec.
                let file = format!("./{file}");
                bytes.extend(
                    self.diff_bytes(
                        &[&options[..], &["--no-index", "--", "/dev/null", &file]].concat(),
                        DIFF_LIMIT - bytes.len(),
                        true,
                    )
                    .await?,
                );
                if bytes.len() > DIFF_LIMIT {
                    break;
                }
            }
        }
        let capped = bytes.len() > DIFF_LIMIT;
        bytes.truncate(DIFF_LIMIT);
        let mut diff = String::from_utf8_lossy(&bytes).into_owned();
        if capped || diff.len() > DIFF_LIMIT {
            diff.truncate(diff.floor_char_boundary(diff.len().min(DIFF_LIMIT)));
            diff.push_str(DIFF_NOTE);
        }
        Ok(diff)
    }

    async fn diff_bytes(&self, args: &[&str], limit: usize, no_index: bool) -> Result<Vec<u8>> {
        let out = run(&self.root, args, limit).await?;
        ensure!(
            out.status.success() || no_index && out.status.code() == Some(1),
            "{}",
            error("diff", &out)
        );
        Ok(out.stdout)
    }

    async fn commit(&self, paths: Option<&[String]>, message: &str) -> Result<String> {
        self.writable()?;
        ensure!(!message.trim().is_empty(), "コミットメッセージが空");
        ensure!(
            self.git(&["diff", "--name-only", "--diff-filter=U", "-z"])
                .await?
                .is_empty(),
            "未解決の競合がある"
        );
        let mut commit = vec!["commit", "--cleanup=whitespace", "-m", message];
        if let Some(paths) = paths {
            ensure!(!paths.is_empty(), "コミットするパスを指定する");
            for path in paths {
                self.path(path)?;
            }
            // Register new paths without staging unrelated edits. Git itself
            // updates the real index only for the paths committed with --only.
            let mut untracked = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
            untracked.extend(paths.iter().map(String::as_str));
            let untracked = self.git(&untracked).await?;
            if !untracked.is_empty() {
                let mut add = vec!["add", "--intent-to-add", "--"];
                add.extend(untracked.split_terminator('\0'));
                self.git(&add).await?;
            }
            commit.extend(["--only", "--"]);
            commit.extend(paths.iter().map(String::as_str));
        } else {
            self.git(&["add", "-A", "--", "."]).await?;
            ensure!(
                !self
                    .git(&["diff", "--cached", "--name-only", "-z"])
                    .await?
                    .is_empty(),
                "コミットする変更がない"
            );
        }
        self.git(&commit).await?;
        Ok(self.git(&["rev-parse", "HEAD"]).await?.trim_end().into())
    }

    async fn branch(&self, action: BranchAction, name: Option<&str>) -> Result<Value> {
        if action == BranchAction::List {
            ensure!(name.is_none(), "一覧にはブランチ名を指定しない");
            let refs = self
                .git(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
                .await?;
            return Ok(
                json!({"current": self.status().await?.branch, "branches": refs.lines().collect::<Vec<_>>() }),
            );
        }
        self.writable()?;
        let name = name.context("ブランチ名を指定する")?;
        ensure!(!name.starts_with('-'), "ブランチ名が不正");
        let reference = format!("refs/heads/{name}");
        self.git(&["check-ref-format", &reference]).await?;
        if action == BranchAction::Switch {
            self.git(&["show-ref", "--verify", &reference]).await?;
            self.git(&["switch", "--no-guess", "--", name]).await?;
        } else {
            self.git(&["branch", "--", name]).await?;
        }
        Ok(json!({"branch": name, "action": action}))
    }

    async fn upstream(&self, operation: RemoteOperation) -> Result<Upstream> {
        self.writable()?;
        let head = self
            .git(&["symbolic-ref", "HEAD"])
            .await
            .context("ブランチを選ぶ")?;
        let head = head.trim_end().to_owned();
        let format = "%(upstream:remotename)%09%(upstream:remoteref)";
        let raw = self
            .git(&["for-each-ref", "--count=1", "--format", format, &head])
            .await?;
        let (remote, target) = raw
            .trim_end_matches('\n')
            .split_once('\t')
            .context("追跡先ブランチが設定されていない")?;
        ensure!(
            !remote.is_empty() && target.starts_with("refs/heads/"),
            "追跡先ブランチが設定されていない"
        );
        let mut args = vec!["remote", "get-url", "--all"];
        if operation == RemoteOperation::Push {
            args.push("--push");
        }
        // Git uses '.' for a local upstream branch.
        let urls = if remote == "." {
            self.root.display().to_string()
        } else {
            args.extend(["--", remote]);
            self.git(&args).await?.trim_end().into()
        };
        let tip = self.git(&["rev-parse", "HEAD"]).await?;
        Ok(Upstream {
            head,
            tip,
            remote: remote.into(),
            target: target.into(),
            urls,
        })
    }

    async fn sync(&self, operation: RemoteOperation, upstream: &Upstream) -> Result<()> {
        let Upstream {
            remote,
            target,
            tip,
            ..
        } = upstream;
        match operation {
            RemoteOperation::Push => {
                self.git(&[
                    "push",
                    "--no-follow-tags",
                    "--recurse-submodules=no",
                    "--",
                    remote,
                    &format!("{}:{target}", tip.trim_end()),
                ])
                .await?;
            }
            RemoteOperation::Pull => {
                self.git(&[
                    "pull",
                    "--ff-only",
                    "--no-rebase",
                    "--no-autostash",
                    "--no-recurse-submodules",
                    "--",
                    remote,
                    target,
                ])
                .await?;
            }
        }
        Ok(())
    }
}

fn parse_status(raw: &str, read_only: bool) -> Result<GitStatus> {
    let mut status = GitStatus {
        branch: None,
        upstream: None,
        ahead: 0,
        behind: 0,
        files: Vec::new(),
        read_only,
    };
    let mut records = raw.split_terminator('\0');
    while let Some(record) = records.next() {
        if let Some(branch) = record.strip_prefix("# branch.head ") {
            status.branch = (branch != "(detached)").then(|| branch.into());
        } else if let Some(upstream) = record.strip_prefix("# branch.upstream ") {
            status.upstream = Some(upstream.into());
        } else if let Some(counts) = record.strip_prefix("# branch.ab ") {
            let (ahead, behind) = counts
                .split_once(' ')
                .context("Git の進み・遅れを読み取れない")?;
            status.ahead = ahead.trim_start_matches('+').parse()?;
            status.behind = behind.trim_start_matches('-').parse()?;
        } else if let Some(path) = record.strip_prefix("? ") {
            status.files.push(GitFile {
                status: "??".into(),
                path: path.into(),
                old_path: None,
            });
        } else {
            let (fields, renamed) = match record.as_bytes().first() {
                Some(b'1') => (9, false),
                Some(b'2') => (10, true),
                Some(b'u') => (11, false),
                _ => continue,
            };
            let mut parts = record.splitn(fields, ' ');
            let code = parts
                .nth(1)
                .context("Git の短い状態を読み取れない")?
                .replace('.', " ");
            let path = parts.last().context("Git の変更パスを読み取れない")?.into();
            let old_path = if renamed {
                Some(records.next().context("Git の移動元がない")?.into())
            } else {
                None
            };
            status.files.push(GitFile {
                status: code,
                path,
                old_path,
            });
        }
    }
    Ok(status)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BranchAction {
    #[default]
    List,
    Switch,
    Create,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteOperation {
    Push,
    Pull,
}

#[derive(Debug, PartialEq, Eq)]
struct Upstream {
    head: String,
    tip: String,
    remote: String,
    target: String,
    urls: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffArgs {
    path: Option<String>,
    #[serde(default)]
    staged: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogArgs {
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    10
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitArgs {
    paths: Option<Vec<String>>,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchArgs {
    #[serde(default)]
    pub action: BranchAction,
    name: Option<String>,
}

impl Workspace {
    pub(crate) fn git_changed(&self) {
        self.hub.publish_snapshot(ServerEvent::GitChanged);
    }

    pub(crate) async fn git_status(&self) -> Result<GitStatus> {
        let _guard = self.git_lock.lock().await;
        Repo::open(&self.project_dir).await?.status().await
    }

    pub(crate) async fn git_diff(&self, args: DiffArgs) -> Result<String> {
        let _guard = self.git_lock.lock().await;
        let repo = Repo::open(&self.project_dir).await?;
        tokio::time::timeout(
            Duration::from_secs(45),
            repo.diff(args.path.as_deref(), args.staged),
        )
        .await
        .context("Git の差分取得がタイムアウトした")?
    }

    pub(crate) async fn git_log(&self, args: LogArgs) -> Result<Vec<GitCommit>> {
        let _guard = self.git_lock.lock().await;
        Repo::open(&self.project_dir).await?.log(args.limit).await
    }

    pub(crate) async fn git_commit(&self, args: CommitArgs) -> Result<String> {
        let _guard = self.git_lock.lock().await;
        let result = Repo::open(&self.project_dir)
            .await?
            .commit(args.paths.as_deref(), &args.message)
            .await;
        self.git_changed();
        result
    }

    pub(crate) async fn git_branch(&self, args: BranchArgs) -> Result<Value> {
        let _guard = self.git_lock.lock().await;
        let result = Repo::open(&self.project_dir)
            .await?
            .branch(args.action, args.name.as_deref())
            .await;
        if args.action != BranchAction::List {
            self.git_changed();
        }
        result
    }

    pub(crate) async fn git_remote(
        &self,
        session: &str,
        actor: Actor,
        operation: RemoteOperation,
    ) -> Result<String> {
        let (key, label, message) = match operation {
            RemoteOperation::Push => ("approval-git-push", "Git push", "プッシュした"),
            RemoteOperation::Pull => ("approval-git-pull", "Git pull", "取り込みが完了した"),
        };
        let mutation = Mutation::new(self, session, label.into())?;
        let upstream = mutation.prepare(async {
            let _guard = self.git_lock.lock().await;
            let repo = Repo::open(&self.project_dir).await?;
            repo.upstream(operation).await
        }).await?;
        let branch = upstream.head.trim_start_matches("refs/heads/");
        let target = upstream.target.trim_start_matches("refs/heads/");
        let (source, destination) = match operation {
            RemoteOperation::Push => (branch.to_owned(), format!("{}/{target}", upstream.remote)),
            RemoteOperation::Pull => (format!("{}/{target}", upstream.remote), branch.to_owned()),
        };
        let urls = upstream
            .urls
            .lines()
            .map(|url| {
                // The approval is recorded in the thread. Avoid persisting URL credentials.
                if let Ok(mut url) = url::Url::parse(url) {
                    let _ = url.set_username("");
                    let _ = url.set_password(None);
                    url.set_query(None);
                    url.to_string()
                } else {
                    url.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary = fuzukue_proto::LocalizedMessage::new(
            key, [("source", source), ("destination", destination), ("urls", urls)],
        );
        mutation.approve(fuzukue_proto::GIT_APPROVAL, actor, summary).await?;
        let result = mutation.run(async {
            let _guard = self.git_lock.lock().await;
            let repo = Repo::open(&self.project_dir).await?;
            ensure!(
                repo.upstream(operation).await? == upstream,
                "承認待ちの間にブランチまたは追跡先が変わった"
            );
            mutation.begin_write();
            repo.sync(operation, &upstream).await
        }).await;
        self.git_changed();
        result?;
        Ok(message.into())
    }

    async fn git_ui(&self, request: GitRequest) -> Result<GitOutcome> {
        match request {
            GitRequest::Status => {
                let _guard = self.git_lock.lock().await;
                let repo = Repo::open(&self.project_dir).await?;
                Ok(GitOutcome::Status {
                    status: repo.status().await?,
                    commits: repo.log(5).await?,
                })
            }
            GitRequest::Diff { path, staged } => {
                let text = self
                    .git_diff(DiffArgs {
                        path: Some(path),
                        staged,
                    })
                    .await?;
                Ok(GitOutcome::Diff { text })
            }
            GitRequest::Commit { message } => {
                let hash = self
                    .git_commit(CommitArgs {
                        paths: None,
                        message,
                    })
                    .await?;
                Ok(GitOutcome::Done {
                    message: format!("コミットした · {}", &hash[..8]),
                })
            }
            GitRequest::Push { ref session } | GitRequest::Pull { ref session } => {
                ensure!(self.hub.has_thread(session), "承認を表示するスレッドがない");
                let operation = if matches!(request, GitRequest::Push { .. }) {
                    RemoteOperation::Push
                } else {
                    RemoteOperation::Pull
                };
                let message = self.git_remote(session, Actor::System, operation).await?;
                Ok(GitOutcome::Done { message })
            }
        }
    }
}

pub(crate) async fn ui_request(app: Arc<Workspace>, id: u64, request: GitRequest) -> ServerEvent {
    let outcome = app
        .git_ui(request)
        .await
        .unwrap_or_else(|error| GitOutcome::Error {
            message: format!("{error:#}"),
        });
    ServerEvent::GitResult { id, outcome }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git(root: &Path, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn configure(root: &Path) -> Result<()> {
        setup_git(root, &["config", "user.name", "Git test"])?;
        setup_git(root, &["config", "user.email", "test@example.test"])?;
        setup_git(root, &["config", "commit.gpgSign", "false"])
    }

    async fn repo(root: &Path) -> Result<Repo> {
        setup_git(root, &["init", "-q", "-b", "main"])?;
        configure(root)?;
        Repo::open(root).await
    }

    #[tokio::test]
    async fn status_diff_log_and_commit() -> Result<()> {
        let temp = TempDir::new()?;
        let repo = repo(temp.path()).await?;
        assert!(repo.log(10).await?.is_empty());
        fs::write(temp.path().join("initial.txt"), "initial\n")?;
        fs::write(temp.path().join("staged.txt"), "first\n")?;
        assert!(repo.commit(None, " \n ").await.is_err());
        let initial = repo.commit(None, "初回").await?;
        assert!(repo.commit(None, "empty").await.is_err());
        assert!(repo.status().await?.files.is_empty());

        setup_git(temp.path(), &["mv", "initial.txt", "new name.txt"])?;
        fs::write(temp.path().join("staged.txt"), "second\n")?;
        setup_git(temp.path(), &["add", "--", "staged.txt"])?;
        fs::write(temp.path().join("staged.txt"), "third\n")?;
        let unusual = ":literal\nfile.txt";
        fs::write(temp.path().join(unusual), "untracked\n")?;
        let status = repo.status().await?;
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.files.len(), 3);
        assert_eq!(status.files[0].status, "R ");
        assert_eq!(status.files[0].old_path.as_deref(), Some("initial.txt"));
        assert_eq!(status.files[1].status, "MM");
        assert_eq!(status.files[2].path, unusual);
        assert!(repo
            .diff(Some("new name.txt"), true)
            .await?
            .contains("rename from initial.txt"));
        assert!(repo
            .diff(Some("staged.txt"), true)
            .await?
            .contains("+second"));
        assert!(repo
            .diff(Some("staged.txt"), false)
            .await?
            .contains("+third"));
        assert!(repo
            .diff(Some(unusual), false)
            .await?
            .contains("+untracked"));
        assert!(repo.diff(None, false).await?.contains("+untracked"));

        let hash = repo
            .commit(Some(&[unusual.to_owned()]), "# 選んだファイル")
            .await?;
        assert_ne!(hash, initial);
        assert_eq!(
            repo.git(&["show", "--format=%B", "--no-patch", "HEAD"])
                .await?
                .trim(),
            "# 選んだファイル"
        );
        assert_eq!(repo.status().await?.files.len(), 2);
        let log = repo.log(20).await?;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].hash, hash);
        assert_eq!(log[0].subject, "# 選んだファイル");
        assert!(chrono::DateTime::parse_from_rfc3339(&log[0].date).is_ok());

        fs::write(temp.path().join("staged.txt"), "long\n".repeat(DIFF_LIMIT))?;
        assert!(repo
            .diff(Some("staged.txt"), false)
            .await?
            .ends_with(DIFF_NOTE));
        repo.commit(None, "残りの変更").await?;
        assert!(repo.status().await?.files.is_empty());
        repo.branch(BranchAction::Create, Some("topic")).await?;
        assert_eq!(
            repo.branch(BranchAction::List, None).await?["branches"],
            json!(["main", "topic"])
        );
        repo.branch(BranchAction::Switch, Some("topic")).await?;
        assert_eq!(repo.status().await?.branch.as_deref(), Some("topic"));
        Ok(())
    }

    #[tokio::test]
    async fn refuses_paths_outside_workspace_and_honours_read_only() -> Result<()> {
        let home = TempDir::new()?;
        let root = home.path().join("labo");
        fs::create_dir(&root)?;
        let mut repo = repo(&root).await?;
        let outside = home.path().join("outside");
        fs::write(&outside, "secret\n")?;
        std::os::unix::fs::symlink(home.path(), root.join("escape"))?;
        for path in ["../outside", "escape/outside", outside.to_str().unwrap()] {
            assert!(repo.diff(Some(path), false).await.is_err(), "{path}");
            assert!(
                repo.commit(Some(&[path.into()]), "outside").await.is_err(),
                "{path}"
            );
        }
        fs::create_dir(root.join("nested"))?;
        assert!(Repo::open(&root.join("nested")).await.is_err());
        repo.read_only = true;
        assert!(repo.status().await?.read_only);
        assert!(repo.commit(None, "protected").await.is_err());
        assert!(repo
            .branch(BranchAction::Create, Some("protected"))
            .await
            .is_err());
        assert!(repo
            .branch(BranchAction::Switch, Some("main"))
            .await
            .is_err());
        for operation in [RemoteOperation::Push, RemoteOperation::Pull] {
            assert_eq!(
                repo.upstream(operation).await.unwrap_err().to_string(),
                "読み取り専用のリポジトリ"
            );
        }
        Ok(())
    }

    async fn next_approval(
        events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>,
    ) -> Result<(String, String)> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let ServerEvent::ApprovalRequested {
                    request_id,
                    summary,
                    ..
                } = events.recv().await?.event
                {
                    return Ok((request_id, summary));
                }
            }
        })
        .await
        .context("Git の承認イベントが届かない")?
    }

    #[tokio::test]
    async fn push_and_pull_wait_for_human_approval_and_use_upstream() -> Result<()> {
        use fuzukue_proto::{DrainProgress, SelfUpdateWaitAction};
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new()?;
        let root = temp.path().join("workspace");
        let bare = temp.path().join("remote.git");
        let peer = temp.path().join("peer");
        fs::create_dir(&root)?;
        let repo = repo(&root).await?;
        fs::write(root.join("tracked"), "initial\n")?;
        let initial = repo.commit(None, "initial").await?;
        setup_git(
            temp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "upstream",
                bare.to_str().unwrap(),
            ],
        )?;
        setup_git(&root, &["remote", "add", "origin", bare.to_str().unwrap()])?;
        setup_git(&root, &["push", "-u", "origin", "main:upstream"])?;
        setup_git(
            temp.path(),
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                peer.to_str().unwrap(),
            ],
        )?;
        configure(&peer)?;
        let bare = Repo {
            root: bare,
            read_only: false,
        };
        let state = temp.path().join("state");
        let registry = crate::App::new(temp.path().into(), root.clone(), state.clone(), "".into(), None)?;
        let update = Arc::new(crate::self_update::Update::shutdown_only(&root, &state));
        assert!(registry.self_update.set(update.clone()).is_ok());
        let app = registry.select(None)?;
        let session = app.hub.edit_threads(|threads| threads.create(None))?;
        let (_, mut events) = app.hub.subscribe(u64::MAX);
        crate::auto_approve(app.clone());
        let operation = |operation| {
            let app = app.clone();
            let session = session.clone();
            let request = match operation {
                RemoteOperation::Push => GitRequest::Push { session },
                RemoteOperation::Pull => GitRequest::Pull { session },
            };
            tokio::spawn(async move { app.git_ui(request).await })
        };
        fs::write(root.join("tracked"), "local\n")?;
        let local = repo.commit(None, "local").await?;
        assert_eq!(repo.status().await?.ahead, 1);
        let denied = operation(RemoteOperation::Push);
        let (id, summary) = next_approval(&mut events).await?;
        assert!(summary.contains("main → origin/upstream"));
        crate::resolve_approval(&app, id.clone(), "accept".into(), true).await?;
        assert_eq!(bare.git(&["rev-parse", "HEAD"]).await?.trim(), initial);
        crate::resolve_approval(&app, id, "deny".into(), false).await?;
        assert!(denied.await?.is_err());

        let hook = bare.root.join("hooks/pre-receive");
        let entered = bare.root.join("push-entered");
        let release = bare.root.join("push-release");
        fs::write(&hook, "#!/bin/sh\ntouch push-entered\nwhile [ ! -f push-release ]; do sleep 0.02; done\n")?;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        let push = operation(RemoteOperation::Push);
        let (id, _) = next_approval(&mut events).await?;
        let late = operation(RemoteOperation::Push);
        let (late_id, _) = next_approval(&mut events).await?;
        // Resolve the first approval before Draining, then let its async work start during Draining.
        crate::resolve_approval(&app, id, "accept".into(), false).await?;
        let mut status = update.subscribe_drain();
        let (_, ready) = update.test_update_drain(registry.clone())?;
        crate::test_support::wait_for_path(&entered).await?;
        tokio::time::timeout(Duration::from_secs(5), status.wait_for(|status| {
            status.as_ref().is_some_and(|s| s.progress == DrainProgress::Draining && s.blocking.gate_held
                && s.blocking.blockers.iter().any(|blocker| blocker.id == late_id))
        })).await??;
        crate::resolve_approval(&app, late_id.clone(), "accept".into(), false).await?;
        assert!(tokio::time::timeout(Duration::from_secs(5), late).await??.unwrap_err().to_string().contains("progress"));
        tokio::time::timeout(Duration::from_secs(5), status.wait_for(|status| {
            status.as_ref().is_some_and(|s| s.progress == DrainProgress::Draining && s.blocking.gate_held
                && !s.blocking.blockers.iter().any(|blocker| blocker.id == late_id))
        })).await??;
        assert!(!ready.is_finished());
        assert_eq!(bare.git(&["rev-parse", "HEAD"]).await?.trim(), initial);
        fs::write(&release, "")?;
        push.await??;
        tokio::time::timeout(Duration::from_secs(5), ready).await???;
        assert_eq!(bare.git(&["rev-parse", "HEAD"]).await?.trim(), local);
        assert_eq!(repo.status().await?.ahead, 0);

        fs::remove_file(&release)?;
        fs::remove_file(&entered)?;
        fs::write(root.join("tracked"), "interrupted\n")?;
        repo.commit(None, "interrupted").await?;
        let push = operation(RemoteOperation::Push);
        let (id, _) = next_approval(&mut events).await?;
        let pending = operation(RemoteOperation::Push);
        let (pending_id, _) = next_approval(&mut events).await?;
        crate::resolve_approval(&app, id, "accept".into(), false).await?;
        crate::test_support::wait_for_path(&entered).await?;
        let (wait, ready) = update.test_update_drain(registry.clone())?;
        update.control_wait(wait, SelfUpdateWaitAction::Force)?;
        tokio::time::timeout(Duration::from_secs(5), ready).await???;
        assert!(app.drain.records(&app.state_dir)?.iter().any(|record| {
            record.review.as_deref().is_some_and(|review| review.contains("Git push — needs review"))
        }));
        assert!(push.await?.unwrap_err().to_string().contains("needs review"));
        assert!(pending.await?.unwrap_err().to_string().contains("progress"));
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut cancelled, mut needs_review) = (false, false);
            while !(cancelled && needs_review) {
                match events.recv().await?.event {
                    ServerEvent::ApprovalResolved { request_id, decision, .. } if request_id == pending_id => {
                        cancelled = decision == "cancelled";
                    }
                    ServerEvent::LocalNotice { text, .. } if text.contains("Git push — needs review") => needs_review = true,
                    _ => {}
                }
            }
            anyhow::Ok(())
        }).await??;
        fs::remove_file(&hook)?;
        assert_eq!(bare.git(&["rev-parse", "HEAD"]).await?.trim(), local);
        setup_git(&root, &["reset", "--hard", &local])?;

        setup_git(&peer, &["pull", "--ff-only"])?;
        fs::write(peer.join("tracked"), "peer\n")?;
        setup_git(&peer, &["commit", "-am", "peer"])?;
        setup_git(&peer, &["push"])?;
        setup_git(&root, &["fetch"])?;
        assert_eq!(repo.status().await?.behind, 1);
        let pull = operation(RemoteOperation::Pull);
        let (id, summary) = next_approval(&mut events).await?;
        assert!(summary.contains("origin/upstream → main"));
        crate::resolve_approval(&app, id.clone(), "accept".into(), true).await?;
        assert_eq!(fs::read_to_string(root.join("tracked"))?, "local\n");
        crate::resolve_approval(&app, id, "accept".into(), false).await?;
        pull.await??;
        assert_eq!(fs::read_to_string(root.join("tracked"))?, "peer\n");

        fs::write(peer.join("tracked"), "new peer\n")?;
        setup_git(&peer, &["commit", "-am", "new peer"])?;
        setup_git(&peer, &["push"])?;
        fs::write(root.join("tracked"), "diverged\n")?;
        let diverged = repo.commit(None, "diverged").await?;
        let pull = operation(RemoteOperation::Pull);
        let (id, _) = next_approval(&mut events).await?;
        crate::resolve_approval(&app, id, "accept".into(), false).await?;
        assert!(pull.await?.is_err());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await?.trim(), diverged);
        Ok(())
    }
}
