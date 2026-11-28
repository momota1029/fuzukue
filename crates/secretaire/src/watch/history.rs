//! Owned Git acquisition and traversal of commits since the last report poll.

use std::path::Path;

use anyhow::Result;

use super::model::{Runtime, Watch};
use super::poll::{self, Observation};

pub(super) async fn observe(watch: Watch, runtime: Runtime, startup: bool) -> Observation {
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        handle.block_on(poll::observe(&watch, &runtime, startup))
    });
    match task.await {
        Ok(observation) => observation,
        Err(error) => Observation::RepoError(format!("Git acquisition task failed: {error}")),
    }
}

pub(super) async fn commits(repo: &Path, checkpoint: &str, tip: &str) -> Result<(String, bool)> {
    anyhow::ensure!(
        matches!(checkpoint.len(), 40 | 64) && checkpoint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid report checkpoint: {checkpoint}"
    );
    let commit = format!("{checkpoint}^{{commit}}");
    let exists = git_check(repo, &["rev-parse", "--verify", "--quiet", &commit]).await?;
    let connected =
        exists && git_check(repo, &["merge-base", "--is-ancestor", checkpoint, tip]).await?;
    let exclude = format!("^{checkpoint}");
    let mut args = vec!["rev-list", "--reverse", "--topo-order", tip];
    // After a rewrite, scan the replacement history from its roots.
    if connected {
        args.push(&exclude);
    }
    args.push("--");
    let commits = poll::git_output(repo, &args, poll::TREE_LIMIT).await?;
    Ok((commits, !connected))
}

async fn git_check(repo: &Path, args: &[&str]) -> Result<bool> {
    let output = crate::git::run(repo, args, 128).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!("{}", crate::git::error(&args.join(" "), &output)),
    }
}
