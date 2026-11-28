//! Owned Git acquisition and traversal of commits since the last report poll.

use std::path::Path;

use anyhow::Result;

use super::model::{Runtime, Watch};
use super::poll::{self, Observation};

pub(super) async fn observe(watch: Watch, runtime: Runtime) -> Observation {
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || handle.block_on(poll::observe(&watch, &runtime)));
    match task.await {
        Ok(observation) => observation,
        Err(error) => Observation::RepoError(format!("Git acquisition task failed: {error}")),
    }
}

pub(super) async fn commits(repo: &Path, checkpoint: &str, tip: &str) -> Result<String> {
    let exclude = format!("^{checkpoint}");
    poll::git_output(
        repo,
        &["rev-list", "--reverse", "--topo-order", tip, &exclude, "--"],
        poll::TREE_LIMIT,
    )
    .await
}
