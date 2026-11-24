use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{Context, Result, ensure};

use super::model::{Heartbeat, Runtime, Source, Watch, report_matcher, report_number};
use crate::git;

const REF_PREFIX: &str = "refs/fuzukue/watch/";
const TREE_LIMIT: usize = 16 * 1024 * 1024;
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
    Reports(Vec<Report>),
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
    let mut trees = HashMap::new();
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
                if !trees.contains_key(branch) {
                    let tree = match &heads {
                        Ok(heads) if heads.contains_key(branch) => {
                            fetch_tree(&repo, &watch.remote, branch).await
                        }
                        Ok(_) => Ok(BTreeSet::new()),
                        Err(error) => Err(anyhow::anyhow!("{error:#}")),
                    };
                    trees.insert(branch, tree);
                }
                match &trees[branch] {
                    Ok(tree) => {
                        sample_files(
                            &repo,
                            branch,
                            tree,
                            source,
                            runtime.last_report(watch, source),
                        )
                        .await
                    }
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

async fn fetch_tree(repo: &Path, remote: &str, branch: &str) -> Result<BTreeSet<String>> {
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
    let output = complete_bytes(
        repo,
        &["ls-tree", "-rz", "--name-only", &reference],
        TREE_LIMIT,
    )
    .await?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
        .collect())
}

async fn sample_files(
    repo: &Path,
    branch: &str,
    tree: &BTreeSet<String>,
    source: &Source,
    last: Option<Option<u64>>,
) -> Result<Sample> {
    match source {
        Source::NewFile { glob, regex, .. } => {
            let matcher = report_matcher(glob.as_deref(), regex.as_deref())?;
            let mut paths = BTreeMap::new();
            for path in tree {
                if let Some(number) = report_number(&matcher, path) {
                    paths.entry(number).or_insert(path);
                }
            }
            let mut reports = Vec::new();
            if let Some(last) = last {
                for (number, path) in paths {
                    if last.is_none_or(|last| number > last) {
                        let text = show(repo, branch, path, REPORT_LIMIT, false).await?;
                        reports.push(Report {
                            number,
                            path: path.clone(),
                            text,
                        });
                    }
                }
            } else if let Some((number, path)) = paths.into_iter().next_back() {
                reports.push(Report {
                    number,
                    path: path.clone(),
                    text: String::new(),
                });
            }
            Ok(Sample::Reports(reports))
        }
        Source::Heartbeat { file, .. } => {
            let heartbeat = if tree.contains(file) {
                Some(Heartbeat::parse(
                    &show(repo, branch, file, HEARTBEAT_LIMIT, true).await?,
                )?)
            } else {
                None
            };
            Ok(Sample::Heartbeat(heartbeat))
        }
        Source::BranchHeads { .. } => unreachable!("branch heads come from ls-remote"),
    }
}

async fn show(
    repo: &Path,
    branch: &str,
    path: &str,
    limit: usize,
    require_complete: bool,
) -> Result<String> {
    let spec = format!("{REF_PREFIX}{branch}:{path}");
    let mut bytes = git_bytes(repo, &["show", "--no-textconv", &spec, "--"], limit).await?;
    if require_complete {
        ensure!(bytes.len() <= limit, "ハートビートのファイルが大きすぎる");
    }
    bytes.truncate(limit);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn git_output(repo: &Path, args: &[&str], limit: usize) -> Result<String> {
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
