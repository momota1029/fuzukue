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

/// The duration selected explicitly by an authenticated operator.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullApprovalDuration {
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "8h")]
    EightHours,
    #[default]
    #[serde(rename = "24h")]
    TwentyFourHours,
    UntilDisabled,
}

/// A workspace settings change. The default enabled period is twenty-four hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum FullApprovalChange {
    Off,
    Full {
        #[serde(default)]
        duration: FullApprovalDuration,
    },
}

/// An active operator opt-in. Timestamps are Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullApprovalMode {
    pub duration: FullApprovalDuration,
    pub enabled_at: i64,
    pub expires_at: Option<i64>,
    pub enabled_by: String,
}

/// A redacted approval decision from the append-only queue history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullApprovalAudit {
    pub approval_id: String,
    pub at: i64,
    pub granted_by: String,
    pub thread: String,
    pub job: String,
    pub risk: ApprovalRisk,
    pub action: ApprovalDisplayAction,
}

/// Current settings and an optional page of past automatic decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullApprovalSettings {
    pub active: Option<FullApprovalMode>,
    pub audit: Vec<FullApprovalAudit>,
    pub next_before: Option<usize>,
    pub last_24_hours_by_tool: std::collections::BTreeMap<String, usize>,
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
    #[serde(default)]
    pub automatic: bool,
    /// Transcript tool item, when the producer supplies a correlation ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_item: Option<String>,
    /// The preceding transcript item at request time, for decisions without an executed tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_item: Option<String>,
    pub action: ApprovalDisplayAction,
}

/// The `expires_at` of a request that waits for a human without a deadline.
/// Clients show "no deadline" instead of a date for this value.
pub const APPROVAL_NO_EXPIRY: i64 = i64::MAX;

impl ApprovalSummary {
    /// The request deadline, or `None` when it waits without one.
    pub fn deadline(&self) -> Option<i64> {
        (self.expires_at != APPROVAL_NO_EXPIRY).then_some(self.expires_at)
    }
}

pub const APPROVAL_RECEIPT_KEY: &str = "approval-receipt";

/// Compact display metadata sent next to the corresponding saved transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalReceipt {
    pub id: String,
    pub job: String,
    pub tool: String,
    pub tool_item: Option<String>,
    pub anchor_item: Option<String>,
    pub state: ApprovalState,
    pub automatic: bool,
    pub updated_at: i64,
}

impl ApprovalSummary {
    pub fn receipt(&self) -> Option<ApprovalReceipt> {
        matches!(self.state, ApprovalState::Granted | ApprovalState::Denied).then(|| ApprovalReceipt {
            id: self.approval_id.clone(),
            job: self.owner.job.clone(),
            tool: self.action.tool.clone(),
            tool_item: self.tool_item.clone(),
            anchor_item: self.anchor_item.clone(),
            state: self.state,
            automatic: self.automatic,
            updated_at: self.updated_at,
        })
    }
}

impl ApprovalReceipt {
    pub fn event(&self, session: &str) -> crate::ServerEvent {
        crate::ServerEvent::LocalNotice {
            session: session.into(),
            item: format!("{APPROVAL_RECEIPT_KEY}:{}", self.id),
            text: String::new(),
            localized: Some(crate::LocalizedMessage::new(APPROVAL_RECEIPT_KEY, [
                ("receipt", serde_json::to_string(self).expect("serializable approval receipt")),
            ])),
        }
    }

    pub fn from_message(message: &crate::LocalizedMessage) -> Option<Self> {
        if message.key != APPROVAL_RECEIPT_KEY {
            return None;
        }
        serde_json::from_str(message.args.get("receipt")?).ok()
    }

    pub fn job(&self) -> Option<&str> {
        (self.job != "front_desk").then_some(self.job.as_str())
    }

    pub fn history_anchor(&self) -> Option<&str> {
        if self.state == ApprovalState::Granted {
            self.tool_item.as_deref().or(self.anchor_item.as_deref())
        } else {
            self.anchor_item.as_deref().or_else(|| self.job())
        }
    }
}
