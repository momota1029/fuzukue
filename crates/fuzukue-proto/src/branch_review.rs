use serde::{Deserialize, Serialize};

use crate::{GitCommit, TestRun};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BranchReviewRequest {
    List,
    Diff {
        branch: String,
    },
    Test {
        branch: String,
        tip: String,
    },
    Merge {
        branch: String,
        tip: String,
        base_tip: String,
        cleanup: bool,
        confirm: bool,
    },
    ConfirmLeanClaims {
        branch: String,
        tip: String,
        base_tip: String,
    },
    MergeWithUnconfirmedLean {
        branch: String,
        tip: String,
        base_tip: String,
        cleanup: bool,
        confirm: bool,
    },
    Discard {
        branch: String,
        tip: String,
        confirm: bool,
    },
    SendBack {
        branch: String,
        comment: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewBranch {
    pub name: String,
    pub tip: String,
    pub subject: String,
    pub time_ms: i64,
    pub ahead: u64,
    pub behind: u64,
    pub worktree: Option<String>,
    pub dirty: bool,
    pub issue: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchDiff {
    pub base: String,
    pub base_tip: String,
    pub branch: String,
    pub tip: String,
    pub stat: String,
    pub commits: Vec<GitCommit>,
    pub files: Vec<BranchDiffFile>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lean: Option<LeanBranchReview>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeanBranchReview {
    pub changes: Vec<LeanClaimChange>,
    pub proof_only: usize,
    pub incomplete: Vec<String>,
    pub confirmation: Option<LeanClaimConfirmation>,
}

impl LeanBranchReview {
    pub fn unconfirmed(&self) -> bool {
        self.confirmation.is_none() && (!self.changes.is_empty() || !self.incomplete.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeanClaimChange {
    pub name: String,
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub before_doc: String,
    pub after_doc: String,
    pub card: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_deck: Option<String>,
    pub stale_doc: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeanClaimConfirmation {
    pub who: String,
    pub time: String,
    pub tip: String,
    pub base_tip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchDiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub binary: bool,
    /// Unified patch for this file, including its headers.
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchReviewOutcome {
    Branches {
        base: String,
        branches: Vec<ReviewBranch>,
    },
    Diff {
        diff: BranchDiff,
    },
    Tests {
        runs: Vec<TestRun>,
    },
    Conflicts {
        files: Vec<String>,
    },
    Merged {
        commit: String,
        warnings: Vec<String>,
    },
    Discarded {
        tip: String,
    },
    SentBack,
    Error {
        message: String,
    },
}
