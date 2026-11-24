use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Optional fields advertised by the server in its response to Hello.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct WelcomeCapabilities {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approval_queue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRisk {
    Low,
    Mid,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalUnattended {
    Auto,
    DeferAndContinue,
    NeverUnattended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Granted,
    Denied,
    Withdrawn,
    Expired,
}

/// Grant deadlines are Unix milliseconds. A once grant uses the request deadline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ApprovalGrantScope {
    #[default]
    Once,
    Scoped {
        expires_at: i64,
    },
    TimeLimited {
        expires_at: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Grant {
        #[serde(default)]
        scope: ApprovalGrantScope,
    },
    Deny,
    Withdraw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOwner {
    pub thread: String,
    pub job: String,
    pub requester: String,
}

/// Display-only action. The producer supplies an explicitly redacted view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDisplayAction {
    pub tool: String,
    pub args: Value,
    pub scope: Value,
}

/// Client-visible view of a request. Timestamps are Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSummary {
    pub approval_id: String,
    pub workspace: String,
    #[serde(flatten)]
    pub owner: ApprovalOwner,
    pub risk: ApprovalRisk,
    pub unattended: ApprovalUnattended,
    pub reason: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub updated_at: i64,
    pub state: ApprovalState,
    pub grant_scope: Option<ApprovalGrantScope>,
    pub consumed_at: Option<i64>,
    pub action: ApprovalDisplayAction,
}
