//! Review committed worker changes and integrate them into the workspace base.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{
    BranchDiff, BranchDiffFile, BranchReviewOutcome as Outcome, BranchReviewRequest as Request,
    GitCommit, HistoryBranch, ReviewBranch, ServerEvent,
};

use crate::{
    git::{self, Repo},
    issue_threads, Workspace,
};

const HEADS: &str = "refs/heads/";
const REMOTES: &str = "refs/remotes/";
const PATCH_LIMIT: usize = 256 * 1024;

struct Review {
    repo: Repo,
    base: String,
    base_tip: String,
    branches: BTreeMap<String, ReviewBranch>,
}

fn fields(value: &str) -> impl Iterator<Item = &str> {
    value.split_terminator('\0')
}

fn issue(branch: &str) -> Option<u64> {
    let number = branch.strip_prefix("work/issue-")?;
    number
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| number.parse().ok())
        .flatten()
        .filter(|&number| number != 0)
}

async fn dirty(path: &Path) -> Result<bool> {
    let result = git::run(
        path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        1,
    )
    .await?;
    ensure!(result.status.success(), "{}", git::error("status", &result));
    Ok(!result.stdout.is_empty())
}

impl Review {
    async fn load(root: &Path) -> Result<Self> {
        let repo = Repo::open(root).await?;
        let refs = repo
            .git(&[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)%00%(committerdate:unix)%00%(subject)",
                HEADS,
                "refs/remotes/origin/",
            ])
            .await?;
        let mut refs_by_name = BTreeMap::new();
        for line in refs.lines() {
            let mut parts = line.splitn(4, '\0');
            let name = parts.next().context("missing ref")?;
            let tip = parts.next().context("missing tip")?;
            let time = parts
                .next()
                .context("missing commit time")?
                .parse::<i64>()?;
            let subject = parts.next().context("missing commit subject")?;
            refs_by_name.insert(name, (tip, time.saturating_mul(1000), subject));
        }

        let origin_head = git::run(
            &repo.root,
            &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
            4096,
        )
        .await?;
        let origin_head = if origin_head.status.success() {
            Some(String::from_utf8(origin_head.stdout)?.trim_end().to_owned())
        } else {
            None
        };
        let base = origin_head
            .as_deref()
            .and_then(|remote| remote.strip_prefix("refs/remotes/origin/"))
            .map(|name| format!("{HEADS}{name}"))
            .filter(|name| refs_by_name.contains_key(name.as_str()))
            .or_else(|| origin_head.filter(|name| refs_by_name.contains_key(name.as_str())))
            .unwrap_or_else(|| format!("{HEADS}main"));
        let base_tip = refs_by_name
            .get(base.as_str())
            .context("基準ブランチが見つからない")?
            .0
            .to_owned();
        // Porcelain preserves paths containing spaces and newlines. Its first
        // entry is the primary worktree, which must never be removed here.
        let listing = repo.git(&["worktree", "list", "--porcelain", "-z"]).await?;
        let mut trees = BTreeMap::new();
        let mut excluded = Vec::new();
        for (index, entry) in listing.split("\0\0").enumerate() {
            let mut parts = fields(entry);
            let Some(path) = parts
                .next()
                .and_then(|field| field.strip_prefix("worktree "))
            else {
                continue;
            };
            let Some(branch) = parts.find_map(|field| field.strip_prefix("branch ")) else {
                continue;
            };
            if index == 0 || Path::new(path) == repo.root {
                excluded.push(branch);
            } else {
                trees.insert(branch, path);
            }
        }

        let branches = refs_by_name
            .into_iter()
            .filter_map(|(reference, (tip, time_ms, subject))| {
                let name = reference.strip_prefix(HEADS)?;
                let worktree = trees.get(reference).map(|path| (*path).to_owned());
                (reference != base
                    && !excluded.contains(&reference)
                    && (name.starts_with("work/") || worktree.is_some()))
                .then(|| {
                    (
                        name.to_owned(),
                        ReviewBranch {
                            name: name.to_owned(),
                            tip: tip.to_owned(),
                            subject: subject.to_owned(),
                            time_ms,
                            ahead: 0,
                            behind: 0,
                            worktree,
                            dirty: false,
                            issue: issue(name),
                        },
                    )
                })
            })
            .collect();
        Ok(Self {
            repo,
            base,
            base_tip,
            branches,
        })
    }

    fn base_name(&self) -> &str {
        self.base
            .trim_start_matches(HEADS)
            .trim_start_matches(REMOTES)
    }

    fn branch(&self, name: &str) -> Result<&ReviewBranch> {
        self.branches
            .get(name)
            .with_context(|| format!("確認対象のブランチが見つからない: {name}"))
    }

    async fn list(mut self) -> Result<Outcome> {
        let base = self.base_name().to_owned();
        for branch in self.branches.values_mut() {
            let range = format!("{}...{}", self.base_tip, branch.tip);
            let counts = self
                .repo
                .git(&["rev-list", "--left-right", "--count", &range, "--"])
                .await?;
            let (behind, ahead) = counts
                .trim_end()
                .split_once('\t')
                .context("invalid rev-list counts")?;
            branch.behind = behind.parse()?;
            branch.ahead = ahead.parse()?;
            if let Some(path) = &branch.worktree {
                // An inaccessible or prunable checkout cannot be treated as clean.
                branch.dirty = dirty(Path::new(path)).await.unwrap_or(true);
            }
        }
        Ok(Outcome::Branches {
            base,
            branches: self.branches.into_values().collect(),
        })
    }

    async fn diff(&self, app: &Workspace, name: &str) -> Result<BranchDiff> {
        let branch = self.branch(name)?;
        let range = format!("{}...{}", self.base_tip, branch.tip);
        let log_range = format!("{}..{}", self.base_tip, branch.tip);
        let common = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--ignore-submodules=none",
            "--submodule=short",
            &range,
        ];
        let names = self
            .repo
            .git(&[common.as_slice(), &["--name-status", "-z", "--"]].concat())
            .await?;
        let stat = self
            .repo
            .git(&[common.as_slice(), &["--stat=100", "--"]].concat())
            .await?;
        let log = self
            .repo
            .git(&[
                "log",
                "--no-color",
                "--no-show-signature",
                "--format=%H%x00%cI%x00%s",
                &log_range,
                "--",
            ])
            .await?;
        let commits = log
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, '\0');
                Ok(GitCommit {
                    hash: parts.next().context("missing commit hash")?.into(),
                    date: parts.next().context("missing commit date")?.into(),
                    subject: parts.next().context("missing commit subject")?.into(),
                })
            })
            .collect::<Result<_>>()?;

        // Numstat carries the binary flag even for a pure rename or when the
        // display patch reaches its byte limit.
        let stats = self
            .repo
            .git(&[common.as_slice(), &["--numstat", "-z", "--"]].concat())
            .await?;
        let mut stats = fields(&stats);
        let mut binaries = BTreeMap::new();
        while let Some(record) = stats.next() {
            let mut parts = record.splitn(3, '\t');
            let binary = parts.next() == Some("-");
            let path = parts.nth(1).context("invalid diff numstat")?;
            let path = if path.is_empty() {
                stats.nth(1).context("missing renamed path in numstat")?
            } else {
                path
            };
            binaries.insert(path, binary);
        }

        let args = [
            common.as_slice(),
            &["--patch", "--src-prefix=a/", "--dst-prefix=b/", "--"],
        ]
        .concat();
        let output = git::run(&self.repo.root, &args, PATCH_LIMIT).await?;
        ensure!(output.status.success(), "{}", git::error("diff", &output));
        let truncated = output.stdout.len() > PATCH_LIMIT;
        let raw = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(PATCH_LIMIT)]);
        let mut patches = raw.split_inclusive('\n').peekable();
        let mut names = fields(&names);
        let mut files = Vec::new();
        while let Some(status) = names.next() {
            let first = names.next().context("missing diff path")?;
            let (old_path, path) = if status.starts_with(['R', 'C']) {
                (
                    Some(first.to_owned()),
                    names.next().context("missing destination path")?,
                )
            } else {
                (None, first)
            };
            let mut patch = String::new();
            // A type change is represented by a removal and an addition in
            // the patch even though name-status reports a single entry.
            let sections = if status == "T" { 2 } else { 1 };
            for _ in 0..sections {
                patch.push_str(patches.next().unwrap_or_default());
                while let Some(&line) = patches
                    .peek()
                    .filter(|line| !line.starts_with("diff --git "))
                {
                    patch.push_str(line);
                    patches.next();
                }
            }
            files.push(BranchDiffFile {
                path: path.into(),
                old_path,
                status: status.into(),
                patch,
                binary: binaries.get(path).copied().unwrap_or(false),
            });
        }
        ensure!(
            patches.next().is_none(),
            "Git の差分とファイル一覧が一致しない"
        );
        let mut lean = if files.iter().any(|file| file.path.ends_with(".lean")
            || file.old_path.as_ref().is_some_and(|path| path.ends_with(".lean")))
        {
            let base = self.repo.git(&["merge-base", &self.base_tip, &branch.tip]).await?;
            crate::lean_reading::branch_claims(&self.repo, base.trim(), &branch.tip, &files).await?
        } else { None };
        if let Some(lean) = &mut lean {
            lean.confirmation = read_confirmation(app, &self.base_tip, &branch.tip)?;
            let declarations: Vec<_> = lean.changes.iter().map(|change| (change.name.clone(), change.card.clone())).collect();
            let links: BTreeMap<_, _> = if declarations.is_empty() {
                BTreeMap::new()
            } else {
                let project = app.project_dir.clone();
                tokio::task::spawn_blocking(move || {
                    crate::lean_reading::reading_cards(&project, "", &declarations).into_iter()
                        .map(|link| (link.declaration.clone(), link)).collect()
                }).await?
            };
            for change in &mut lean.changes {
                if let Some(link) = links.get(&change.name) {
                    change.card = Some(link.id.clone());
                    change.card_deck = Some(link.deck.clone());
                }
            }
        }
        Ok(BranchDiff {
            base: self.base_name().into(),
            base_tip: self.base_tip.clone(),
            branch: name.into(),
            tip: branch.tip.clone(),
            stat,
            commits,
            files,
            truncated,
            lean,
        })
    }

    async fn check_tip(&self, branch: &ReviewBranch, tip: &str) -> Result<()> {
        ensure!(branch.tip == tip, "ブランチが更新された。差分を開き直す");
        let reference = format!("{HEADS}{}", branch.name);
        let current = self
            .repo
            .git(&["rev-parse", "--verify", &reference])
            .await?;
        ensure!(
            current.trim_end() == tip,
            "ブランチが更新された。差分を開き直す"
        );
        Ok(())
    }

    async fn check_base(&self, tip: &str) -> Result<()> {
        ensure!(
            self.base_tip == tip,
            "基準ブランチが更新された。差分を開き直す"
        );
        let current = self
            .repo
            .git(&["symbolic-ref", "-q", "HEAD"])
            .await
            .context("基準ブランチをチェックアウトしてから取り込む")?;
        ensure!(
            current.trim_end() == self.base,
            "基準ブランチ {} をチェックアウトしてから取り込む",
            self.base_name()
        );
        let current = self.repo.git(&["rev-parse", "--verify", "HEAD"]).await?;
        ensure!(
            current.trim_end() == tip,
            "基準ブランチが更新された。差分を開き直す"
        );
        ensure!(
            !dirty(&self.repo.root).await?,
            "基準ブランチの作業ツリーに未コミットの変更がある"
        );
        let metadata = git::metadata_dir(&self.repo.root).context("Git の作業ツリー情報がない")?;
        for state in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ] {
            ensure!(
                !metadata.join(state).exists(),
                "Git の操作が進行中: {state}"
            );
        }
        Ok(())
    }

    async fn clean_worker(&self, branch: &ReviewBranch) -> Result<()> {
        if let Some(path) = &branch.worktree {
            Self::check_worker_checkout(branch, path).await?;
            ensure!(
                !dirty(Path::new(path)).await?,
                "作業ブランチの作業ツリーに未コミットの変更がある: {path}"
            );
        }
        Ok(())
    }

    async fn check_worker_checkout(branch: &ReviewBranch, path: &str) -> Result<()> {
        let reference = format!("{HEADS}{}", branch.name);
        let out = git::run(Path::new(path), &["symbolic-ref", "-q", "HEAD"], 4096).await?;
        ensure!(
            out.status.success() && String::from_utf8(out.stdout)?.trim_end() == reference,
            "作業ツリーのブランチが切り替わった: {path}"
        );
        Ok(())
    }

    async fn remove_worktree(&self, branch: &ReviewBranch) -> Result<()> {
        if let Some(path) = &branch.worktree {
            Self::check_worker_checkout(branch, path).await?;
            self.repo.git(&["worktree", "remove", "--", path]).await?;
        }
        Ok(())
    }

    async fn merge(
        self,
        app: &Workspace,
        branch: ReviewBranch,
        base_tip: &str,
        cleanup: bool,
    ) -> Result<Outcome> {
        self.check_base(base_tip).await?;
        self.clean_worker(&branch).await?;
        let trial = git::run(
            &self.repo.root,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                base_tip,
                &branch.tip,
            ],
            4 * 1024 * 1024,
        )
        .await?;
        if trial.status.code() == Some(1) {
            let output = String::from_utf8_lossy(&trial.stdout);
            let files: Vec<_> = fields(&output)
                .skip(1)
                .take_while(|file| !file.is_empty())
                .map(str::to_owned)
                .collect();
            ensure!(!files.is_empty(), "{}", git::error("merge-tree", &trial));
            return Ok(Outcome::Conflicts { files });
        }
        ensure!(
            trial.status.success(),
            "{}",
            git::error("merge-tree", &trial)
        );

        let history = app.history.clone();
        let meta = branch_history(&branch, false);
        let checked = branch.clone();
        let base_tip = base_tip.to_owned();
        let (review, commit, warning) = tokio::task::spawn_blocking(move || {
            let (commit, warning) = history.branch_merge(meta, || {
                tokio::runtime::Handle::current().block_on(async {
                    self.check_base(&base_tip).await?;
                    self.check_tip(&checked, &checked.tip).await?;
                    self.clean_worker(&checked).await?;
                    // Merge the exact object examined by merge-tree.
                    let message = format!("Merge branch '{}'", checked.name);
                    if let Err(error) = self
                        .repo
                        .git(&[
                            "merge",
                            "--no-edit",
                            "--no-autostash",
                            "--no-overwrite-ignore",
                            "--no-squash",
                            "--commit",
                            "-s",
                            "ort",
                            "-m",
                            &message,
                            &checked.tip,
                        ])
                        .await
                    {
                        if git::metadata_dir(&self.repo.root)
                            .is_some_and(|dir| dir.join("MERGE_HEAD").exists())
                        {
                            self.repo
                                .git(&["merge", "--abort"])
                                .await
                                .context(format!("{error:#}"))?;
                        }
                        return Err(error);
                    }
                    Ok(self
                        .repo
                        .git(&["rev-parse", "--verify", "HEAD"])
                        .await?
                        .trim_end()
                        .to_owned())
                })
            })?;
            Ok::<_, anyhow::Error>((self, commit, warning))
        })
        .await??;

        let mut warnings: Vec<_> = warning.into_iter().collect();
        if cleanup {
            let result = async {
                review
                    .repo
                    .git(&["merge-base", "--is-ancestor", &branch.tip, &commit])
                    .await?;
                review.check_tip(&branch, &branch.tip).await?;
                review.remove_worktree(&branch).await?;
                review.check_tip(&branch, &branch.tip).await?;
                review
                    .repo
                    .git(&["branch", "-D", "--", &branch.name])
                    .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                warnings.push(format!("{error:#}"));
            }
        }
        Ok(Outcome::Merged { commit, warnings })
    }

    async fn discard(&self, app: &Workspace, branch: &ReviewBranch) -> Result<Outcome> {
        self.clean_worker(branch).await?;
        self.check_tip(branch, &branch.tip).await?;
        // Keep discarded objects reachable after normal Git garbage collection.
        let recovery = format!("refs/labo/discarded/{}", branch.tip);
        self.repo
            .git(&["update-ref", &recovery, &branch.tip])
            .await?;
        self.remove_worktree(branch).await?;
        self.check_tip(branch, &branch.tip).await?;
        let history = app.history.clone();
        let meta = branch_history(branch, true);
        tokio::task::spawn_blocking(move || history.branch_discard(meta)).await??;
        self.check_tip(branch, &branch.tip).await?;
        self.repo.git(&["branch", "-D", "--", &branch.name]).await?;
        Ok(Outcome::Discarded {
            tip: branch.tip.clone(),
        })
    }
}

fn branch_history(branch: &ReviewBranch, discarded: bool) -> HistoryBranch {
    HistoryBranch {
        name: branch.name.clone(),
        tip: branch.tip.clone(),
        discarded,
    }
}

fn confirmation_path(app: &Workspace, base: &str, tip: &str) -> std::path::PathBuf {
    app.state_dir.join("lean-claims").join(format!("{base}-{tip}.json"))
}

fn read_confirmation(app: &Workspace, base: &str, tip: &str) -> Result<Option<fuzukue_proto::LeanClaimConfirmation>> {
    match std::fs::read(confirmation_path(app, base, tip)) {
        Ok(bytes) => {
            let value: fuzukue_proto::LeanClaimConfirmation = serde_json::from_slice(&bytes)?;
            Ok((value.base_tip == base && value.tip == tip).then_some(value))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) async fn revisions(app: &Workspace, branch: &str) -> Result<(String, String)> {
    let _git = app.git_lock.lock().await;
    let review = Review::load(&app.project_dir).await?;
    Ok((review.branch(branch)?.tip.clone(), review.base_tip))
}

pub(crate) async fn request(app: Arc<Workspace>, id: u64, request: Request) -> ServerEvent {
    let outcome = execute(&app, request)
        .await
        .unwrap_or_else(|error| Outcome::Error {
            message: format!("{error:#}"),
        });
    ServerEvent::BranchReviewResult { id, outcome }
}

pub(crate) async fn execute(app: &Workspace, request: Request) -> Result<Outcome> {
    let _action = if matches!(request, Request::List | Request::Diff { .. }) {
        None
    } else {
        Some(
            app.branch_review_lock
                .try_lock()
                .context("ブランチの操作が進行中")?,
        )
    };
    let git = app.git_lock.lock().await;
    let review = Review::load(&app.project_dir).await?;
    let override_lean = matches!(request, Request::MergeWithUnconfirmedLean { .. });
    match request {
        Request::List => review.list().await,
        Request::Diff { branch } => Ok(Outcome::Diff {
            diff: review.diff(app, &branch).await?,
        }),
        Request::Test { branch, tip } => {
            let _work = app.admit_work()?;
            let branch = review.branch(&branch)?;
            review.check_tip(branch, &tip).await?;
            let path = branch
                .worktree
                .as_deref()
                .context("ブランチの作業ツリーがない")?;
            Review::check_worker_checkout(branch, path).await?;
            drop(git);
            Ok(Outcome::Tests {
                runs: crate::project_tests::run_in_worktree(Path::new(path)).await?,
            })
        }
        Request::ConfirmLeanClaims { branch, tip, base_tip } => {
            review.check_tip(review.branch(&branch)?, &tip).await?;
            review.check_base(&base_tip).await?;
            let mut diff = review.diff(app, &branch).await?;
            let claims = diff.lean.as_mut().context("Lean の主張の差分がない")?;
            ensure!(!claims.changes.is_empty(), "Lean の主張の差分がない");
            ensure!(claims.incomplete.is_empty(), "Lean の差分に解析できないファイルがある");
            let who = review.repo.git(&["var", "GIT_AUTHOR_IDENT"]).await?;
            let who = who.trim().rsplitn(3, ' ').last().unwrap_or(who.trim()).to_owned();
            let confirmation = fuzukue_proto::LeanClaimConfirmation {
                who, time: chrono::Utc::now().to_rfc3339(), tip, base_tip,
            };
            let path = confirmation_path(app, &confirmation.base_tip, &confirmation.tip);
            let dir = path.parent().context("確認の保存先がない")?;
            std::fs::create_dir_all(dir)?;
            let temp = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(temp.as_file(), &confirmation)?;
            crate::durable::persist(temp, path)?;
            claims.confirmation = Some(confirmation);
            Ok(Outcome::Diff { diff })
        }
        Request::Merge {
            branch,
            tip,
            base_tip,
            cleanup,
            confirm,
        } | Request::MergeWithUnconfirmedLean {
            branch, tip, base_tip, cleanup, confirm,
        } => {
            ensure!(confirm, "取り込みには利用者の確認が必要");
            let branch = review.branch(&branch)?.clone();
            review.check_tip(&branch, &tip).await?;
            review.check_base(&base_tip).await?;
            if !override_lean {
                let diff = review.diff(app, &branch.name).await?;
                ensure!(!diff.lean.as_ref().is_some_and(|lean| lean.unconfirmed()),
                    "確認していない Lean の主張がある。取り込み待ち画面で主張を確認するか、警告を明示的に承認する");
            }
            let result = review.merge(app, branch.clone(), &base_tip, cleanup).await;
            app.git_changed();
            drop(git);
            let mut result = result?;
            if let (Outcome::Merged { commit, warnings }, Some(number)) =
                (&mut result, branch.issue)
            {
                let message = format!("Merged `{}` locally at `{commit}`.", branch.name);
                if let Err(error) = async {
                    let link = issue_threads::link(app, number)?;
                    issue_threads::comment(app, &link, &message, None).await
                }.await {
                    warnings.push(format!("{error:#}"));
                }
                let add = ["dot:done".into()];
                let remove = [
                    "dot:review".into(),
                    "dot:running".into(),
                    "dot:ready".into(),
                ];
                if let Err(error) = async {
                    let link = issue_threads::link(app, number)?;
                    issue_threads::set_labels(app, &link, &add, &remove).await
                }.await {
                    warnings.push(format!("{error:#}"));
                }
                app.issues_refresh.notify_one();
            }
            Ok(result)
        }
        Request::Discard {
            branch,
            tip,
            confirm,
        } => {
            ensure!(confirm, "破棄には利用者の確認が必要");
            let branch = review.branch(&branch)?;
            review.check_tip(branch, &tip).await?;
            let result = review.discard(app, branch).await;
            app.git_changed();
            result
        }
        Request::SendBack { branch, comment } => {
            ensure!(!comment.trim().is_empty(), "差し戻しのコメントが空");
            let Some(number) = review.branch(&branch)?.issue else {
                bail!("ブランチに Issue が紐づいていない");
            };
            drop(git);
            issue_threads::comment(app, &issue_threads::link(app, number)?, &comment, None).await?;
            app.issues_refresh.notify_one();
            Ok(Outcome::SentBack)
        }
    }
}
