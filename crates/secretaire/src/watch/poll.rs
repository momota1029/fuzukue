use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{Context, Result, ensure};

use super::history;
use super::model::{Heartbeat, Runtime, Source, Watch, report_matcher, report_number};
use crate::git;

const REF_PREFIX: &str = "refs/fuzukue/watch/";
pub(super) const TREE_LIMIT: usize = 16 * 1024 * 1024;
const REPORT_LIMIT: usize = 4 * 1024 * 1024;
const HEARTBEAT_LIMIT: usize = 64 * 1024;

type RepoMutex = tokio::sync::Mutex<()>;
static REPOS: LazyLock<Mutex<HashMap<PathBuf, Weak<RepoMutex>>>> = LazyLock::new(Mutex::default);

pub(super) enum Observation {
    MissingRepo,
    RepoError(String),
    Sources(Vec<Result<Sample>>),
}

pub(super) enum Sample {
    BranchHeads(BTreeMap<String, String>),
    Reports {
        reports: Vec<Report>,
        checkpoint: Option<String>,
    },
    Heartbeat(Option<Heartbeat>),
}

pub(super) struct Report {
    pub number: u64,
    pub path: String,
    pub text: String,
}

fn user_home() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME が設定されていない")?);
    home.canonicalize().context("ホームディレクトリを開けない")
}

fn lock_repo(repo: &Path) -> Arc<RepoMutex> {
    // Linked worktrees share the private refs in their common Git directory.
    let key = git::metadata_dir(repo).map_or_else(
        || repo.to_owned(),
        |metadata| {
            std::fs::read_to_string(metadata.join("commondir"))
                .ok()
                .and_then(|common| metadata.join(common.trim_end()).canonicalize().ok())
                .unwrap_or(metadata)
        },
    );
    let mut repos = REPOS.lock().unwrap();
    repos.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = repos.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(RepoMutex::default());
    repos.insert(key, Arc::downgrade(&lock));
    lock
}

pub(super) async fn validate(repo: &Path, remote: &str) -> Result<PathBuf> {
    let repo = crate::workspaces::checked_dir(&user_home()?, repo)?;
    let lock = lock_repo(&repo);
    let _git = lock.lock().await;
    check_git(&repo, remote).await?;
    Ok(repo)
}

async fn check_git(repo: &Path, remote: &str) -> Result<()> {
    let prefix = git_output(repo, &["rev-parse", "--show-prefix"], 4096).await?;
    ensure!(prefix.trim().is_empty(), "Git リポジトリのルートを指定する");
    git_output(repo, &["remote", "get-url", "--", remote], 4096).await?;
    Ok(())
}

pub(super) async fn observe(watch: &Watch, runtime: &Runtime) -> Observation {
    match observe_in(watch, runtime, user_home()).await {
        Ok(Some(samples)) => Observation::Sources(samples),
        Ok(None) => Observation::MissingRepo,
        Err(error) => Observation::RepoError(format!("{error:#}")),
    }
}

pub(super) async fn observe_in(
    watch: &Watch,
    runtime: &Runtime,
    home: Result<PathBuf>,
) -> Result<Option<Vec<Result<Sample>>>> {
    let repo = match watch.repo.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("監視対象のリポジトリを開けない"),
    };
    let home = home?;
    ensure!(
        repo.is_dir() && repo != home && repo.starts_with(home),
        "ホーム配下のリポジトリを指定する"
    );
    let lock = lock_repo(&repo);
    let _git = lock.lock().await;
    check_git(&repo, &watch.remote).await?;

    let branches: BTreeSet<_> = watch.sources.iter().filter_map(Source::branch).collect();
    let branch_refs: Vec<_> = branches
        .iter()
        .map(|branch| format!("refs/heads/{branch}"))
        .collect();
    let mut args = vec!["ls-remote", "--heads", "--", &watch.remote];
    args.extend(branch_refs.iter().map(String::as_str));
    // Exact file branches also provide their heads for the dots preset.
    let heads = if branches.is_empty() {
        Ok(BTreeMap::new())
    } else {
        remote_heads(&repo, &args).await
    };
    let mut snapshots = HashMap::new();
    let mut samples = Vec::new();
    for source in &watch.sources {
        let sample = match source {
            Source::BranchHeads { pattern }
                if pattern
                    .strip_prefix("refs/heads/")
                    .is_some_and(|branch| branches.contains(branch)) =>
            {
                let branch = pattern.strip_prefix("refs/heads/").unwrap();
                heads
                    .as_ref()
                    .map(|heads| {
                        Sample::BranchHeads(
                            heads
                                .get(branch)
                                .map(|sha| (branch.to_owned(), sha.clone()))
                                .into_iter()
                                .collect(),
                        )
                    })
                    .map_err(|error| anyhow::anyhow!("{error:#}"))
            }
            Source::BranchHeads { pattern } => remote_heads(
                &repo,
                &["ls-remote", "--heads", "--", &watch.remote, pattern],
            )
            .await
            .map(Sample::BranchHeads),
            _ => {
                let branch = source.branch().unwrap();
                if !snapshots.contains_key(branch) {
                    let snapshot = match &heads {
                        Ok(heads) if heads.contains_key(branch) => {
                            fetch_snapshot(&repo, &watch.remote, branch).await.map(Some)
                        }
                        Ok(_) => Ok(None),
                        Err(error) => Err(anyhow::anyhow!("{error:#}")),
                    };
                    snapshots.insert(branch, snapshot);
                }
                match &snapshots[branch] {
                    Ok(Some((tip, tree))) => {
                        sample_files(&repo, tip, tree, watch, source, runtime).await
                    }
                    Ok(None) if matches!(source, Source::NewFile { .. }) => Ok(Sample::Reports {
                        reports: Vec::new(),
                        checkpoint: None,
                    }),
                    Ok(None) => Ok(Sample::Heartbeat(None)),
                    Err(error) => Err(anyhow::anyhow!("{error:#}")),
                }
            }
        };
        samples.push(sample);
    }
    Ok(Some(samples))
}

async fn remote_heads(repo: &Path, args: &[&str]) -> Result<BTreeMap<String, String>> {
    let output = git_output(repo, args, TREE_LIMIT).await?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let (sha, reference) = line.split_once('\t')?;
            Some((reference.strip_prefix("refs/heads/")?.into(), sha.into()))
        })
        .collect())
}

async fn fetch_snapshot(
    repo: &Path,
    remote: &str,
    branch: &str,
) -> Result<(String, BTreeSet<String>)> {
    let reference = format!("{REF_PREFIX}{branch}");
    let refspec = format!("+refs/heads/{branch}:{reference}");
    git_output(
        repo,
        &[
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "-q",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--refmap=",
            "--",
            remote,
            &refspec,
        ],
        8192,
    )
    .await?;
    let tip = git_output(repo, &["rev-parse", "--verify", &reference], 128).await?;
    let tip = tip.trim().to_owned();
    let output = complete_bytes(repo, &["ls-tree", "-rz", "--name-only", &tip], TREE_LIMIT).await?;
    let tree = git_paths(&output).map(str::to_owned).collect();
    Ok((tip, tree))
}

fn git_paths(output: &[u8]) -> impl Iterator<Item = &str> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok())
}

async fn sample_files(
    repo: &Path,
    tip: &str,
    tree: &BTreeSet<String>,
    watch: &Watch,
    source: &Source,
    runtime: &Runtime,
) -> Result<Sample> {
    match source {
        Source::NewFile { glob, regex, .. } => {
            let matcher = report_matcher(glob.as_deref(), regex.as_deref())?;
            let last = runtime.last_report(watch, source);
            let checkpoint = runtime.report_checkpoints.get(&source.key(watch));
            let mut paths = BTreeMap::new();
            if checkpoint.is_none_or(|checkpoint| checkpoint != tip) {
                for path in tree {
                    if let Some(number) = report_number(&matcher, path)
                        .filter(|number| last.flatten().is_none_or(|last| *number > last))
                    {
                        paths.entry(number).or_insert_with(|| (tip.to_owned(), path.clone()));
                    }
                }
                if let Some(checkpoint) = checkpoint {
                    for commit in history::commits(repo, checkpoint, tip).await?.lines() {
                        if commit != tip {
                            collect_added_reports(repo, commit, &matcher, last.flatten(), &mut paths)
                                .await?;
                        }
                    }
                }
            }
            let mut reports = Vec::new();
            if last.is_some() {
                for (number, (commit, path)) in paths {
                    let text = show(repo, &commit, &path, REPORT_LIMIT, false).await?;
                    reports.push(Report { number, path, text });
                }
            } else if let Some((number, (_, path))) = paths.into_iter().next_back() {
                reports.push(Report { number, path, text: String::new() });
            }
            Ok(Sample::Reports { reports, checkpoint: Some(tip.to_owned()) })
        }
        Source::Heartbeat { file, .. } => {
            let heartbeat = if tree.contains(file) {
                Some(Heartbeat::parse(
                    &show(repo, tip, file, HEARTBEAT_LIMIT, true).await?,
                )?)
            } else {
                None
            };
            Ok(Sample::Heartbeat(heartbeat))
        }
        Source::BranchHeads { .. } => unreachable!("branch heads come from ls-remote"),
    }
}

// Files still at the tip keep their current content. For a deleted report,
// take the first addition in parent-before-child order.
async fn collect_added_reports(
    repo: &Path,
    commit: &str,
    matcher: &regex::Regex,
    last: Option<u64>,
    paths: &mut BTreeMap<u64, (String, String)>,
) -> Result<()> {
    let output = complete_bytes(
        repo,
        &[
            "diff-tree", "--root", "-m", "--no-commit-id", "-r", "-z",
            "--name-only", "--diff-filter=A", "--no-renames", commit, "--",
        ],
        TREE_LIMIT,
    )
    .await?;
    for path in git_paths(&output) {
        if let Some(number) = report_number(matcher, path)
            .filter(|number| last.is_none_or(|last| *number > last))
        {
            paths.entry(number).or_insert_with(|| (commit.to_owned(), path.to_owned()));
        }
    }
    Ok(())
}

async fn show(
    repo: &Path,
    commit: &str,
    path: &str,
    limit: usize,
    require_complete: bool,
) -> Result<String> {
    let spec = format!("{commit}:{path}");
    let mut bytes = git_bytes(repo, &["show", "--no-textconv", &spec, "--"], limit).await?;
    if require_complete {
        ensure!(bytes.len() <= limit, "ハートビートのファイルが大きすぎる");
    }
    bytes.truncate(limit);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) async fn git_output(repo: &Path, args: &[&str], limit: usize) -> Result<String> {
    Ok(String::from_utf8(complete_bytes(repo, args, limit).await?)?)
}

async fn complete_bytes(repo: &Path, args: &[&str], limit: usize) -> Result<Vec<u8>> {
    let bytes = git_bytes(repo, args, limit).await?;
    ensure!(bytes.len() <= limit, "Git の出力が上限を超えた");
    Ok(bytes)
}

async fn git_bytes(repo: &Path, args: &[&str], limit: usize) -> Result<Vec<u8>> {
    let output = git::run(repo, args, limit).await?;
    ensure!(
        output.status.success(),
        "{}",
        git::error(&args.join(" "), &output)
    );
    Ok(output.stdout)
}
