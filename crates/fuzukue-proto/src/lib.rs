//! secretaire と fuzukue のあいだのワイヤ形式。
//!
//! 通信は WebSocket 上の JSON テキストフレーム 1 通 = 1 メッセージ。
//! サーバーから流れるイベントには単調増加の `seq` を振り、クライアントは
//! 再接続時に `Hello { resume_from }` で取りこぼしを取り戻す。
//! PDF などの大きなファイルは WebSocket に流さず、`GET /file/<path>` で取る。

use std::collections::BTreeMap;

pub mod compat;
pub mod slash;

use serde::{Deserialize, Serialize};

// v4 clients on main know threads. v5 adds chat settings and attachments.
pub const PLAN_PROTOCOL: u32 = 16;
pub const AGENT_MANAGEMENT_PROTOCOL: u32 = 17;
pub const SYMBOL_PROTOCOL: u32 = 18;
pub const EDITOR_PROTOCOL: u32 = 19;
pub const OPEN_FILE_PROTOCOL: u32 = 20;
pub const TESTS_PROTOCOL: u32 = 21;
pub const LOCALE_PROTOCOL: u32 = 22;
pub const LIBRARY_PROTOCOL: u32 = 23;
pub const MEMOS_PROTOCOL: u32 = 24;
pub const REVIEW_REPORTS_PROTOCOL: u32 = 25;
pub const LEDGER_PROTOCOL: u32 = 26;
pub const EXTENSIONS_PROTOCOL: u32 = 27;
pub const DISTRIBUTION_PROTOCOL: u32 = 28;
pub const CHANGE_HISTORY_PROTOCOL: u32 = 29;
pub const LIBRARY_DECKS_PROTOCOL: u32 = 30;
pub const CLOSE_TRASH_PROTOCOL: u32 = 31;
pub const RESEARCH_ISSUE_DETAILS_PROTOCOL: u32 = 32;
pub const REVIEW_RECOVERY_PROTOCOL: u32 = 33;
pub const BRANCH_REVIEW_PROTOCOL: u32 = 34;
pub const SELF_UPDATE_PROTOCOL: u32 = 35;
pub const CLIENT_SCREENSHOT_PROTOCOL: u32 = 37;
pub const LSP_PROTOCOL: u32 = 36;
pub const ISSUE_THREADS_PROTOCOL: u32 = 38;
pub const LITERATE_LEAN_PROTOCOL: u32 = 39;
pub const STYLE_TEXTLINT_PROTOCOL: u32 = 40;
pub const HANDOFF_PROTOCOL: u32 = 41;
pub const ONE_VOICE_TIERS_PROTOCOL: u32 = 42;
pub const ISSUE_RESPOND_PROTOCOL: u32 = 43;
pub const FILE_BROWSER_PROTOCOL: u32 = 44;
pub const SELF_UPDATE_CANARY_PROTOCOL: u32 = 45;
pub const SELF_UPDATE_WAIT_PROTOCOL: u32 = 46;
pub const CHAT_WAITING_PROTOCOL: u32 = 47;
pub const DRAIN_PROTOCOL: u32 = 48;
pub const CLIENT_COMMAND_PROTOCOL: u32 = 49;
pub const GOAL_PAUSE_PROTOCOL: u32 = 50;
pub const SAFE_CLOSE_PROTOCOL: u32 = 51;
pub const SLASH_COMMAND_PROTOCOL: u32 = 52;
pub const GITHUB_REPORT_PROTOCOL: u32 = 53;
pub const BACKGROUND_COMMANDS_PROTOCOL: u32 = 54;
pub const QUESTION_PROTOCOL: u32 = 55;
pub const WORKER_APPROVAL_PROTOCOL: u32 = 56;
pub const CHAT_ACTIVITY_PROTOCOL: u32 = 57;
pub const ISSUE_INTAKE_PROTOCOL: u32 = 58;
pub const NOTIFICATIONS_PROTOCOL: u32 = 59;
pub const AGENT_MODELS_PROTOCOL: u32 = 60;
pub const GITHUB_CONNECT_PROTOCOL: u32 = 61;
pub const TRANSLATION_DRIFT_PROTOCOL: u32 = 62;
/// Opt-in thread history paging supported alongside protocol 62.
pub const THREAD_HISTORY_PROTOCOL: u32 = 62;
pub const REVIEW_FINDINGS_PROTOCOL: u32 = 63;
pub const FOLDER_GRANTS_PROTOCOL: u32 = 65;
pub const PROTOCOL_VERSION: u32 = FOLDER_GRANTS_PROTOCOL;

mod approvals;
pub use approvals::*;
mod folder_grants;
pub use folder_grants::*;
pub const MAX_OPEN_FILE_BYTES: u64 = 200 * 1024 * 1024;

mod files;
pub use files::*;

mod review_findings;
pub use review_findings::*;

mod questions;
pub use questions::*;

mod handoff;
pub use handoff::*;

mod lsp;
pub use lsp::*;
pub const MAX_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SCREENSHOT_NOTE_BYTES: usize = 64 * 1024;
pub const SCREENSHOT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotDestination {
    Memo,
    Dogfood,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum ScreenshotResponse {
    Captured(String),
    Denied,
    Unsupported,
    Failed,
}

mod branch_review;
pub use branch_review::*;

mod trash;
pub use trash::*;

mod change_history;
pub use change_history::*;

pub const CLIENT_PLATFORMS: &[&str] = &["windows-x86_64", "linux-x86_64", "macos-aarch64"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientManifest {
    pub platform: String,
    pub commit: String,
    pub built_at: String,
    pub sha256: String,
    pub size: u64,
}

mod review_reports;
pub use review_reports::*;
mod ledger;
pub use ledger::*;
mod extensions;
pub use extensions::*;
pub const PLAN_NOTE: &str = "thread-plan:";
pub const RESEARCH_ISSUE_APPROVAL: &str = "research-issue";

pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
pub const ATTACHMENT_CHUNK_BYTES: usize = 512 * 1024;
pub const MAX_ATTACHMENTS: usize = 8;
pub const CLAUDE_MODELS: &[&str] = &[
    "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2",
];

/// A UTF-8 byte range in the exact file revision identified by base_hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEdit {
    pub path: String,
    pub base_hash: String,
    pub range: Option<SourceRange>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceSaveOutcome {
    Saved { text: String, range: Option<SourceRange> },
    Conflict { latest: String },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    #[default]
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn effective(self, mail_configured: bool) -> Self {
        match (mail_configured, self) {
            (true, Self::AcceptEdits | Self::BypassPermissions) => Self::Default,
            _ => self,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexModel {
    pub slug: String,
    pub label: String,
    pub effort_levels: Vec<String>,
}

pub const CLAUDE_OPUS: &str = "claude-opus-5-5";
pub const DELEGATE_JOB: &str = "delegate:";
pub const WORKSHOP_REPORT: &str = "workshop-report:";
pub const GIT_APPROVAL: &str = "git:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateSettings {
    pub model: String,
    pub effort: String,
}

/// Who decides eligible approval requests raised by a delegated worker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerApprovalPolicy {
    #[default]
    Ask,
    Advisor,
    Auto,
}

/// A saved Bash approval in one canonical working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellApprovalRule {
    pub id: String,
    pub directory: String,
    pub command: String,
    /// The server-checked prefix, when the user chose to allow a command family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl Default for DelegateSettings {
    fn default() -> Self {
        Self { model: "gpt-6.2-sol".into(), effort: "high".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewSettings {
    pub style_model: Option<String>,
    pub style_effort: Option<String>,
    pub math_model: String,
    pub math_effort: String,
    pub apply_in_side_thread: bool,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            style_model: None,
            style_effort: None,
            math_model: "gpt-6.2-sol".into(),
            math_effort: "medium".into(),
            apply_in_side_thread: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Light,
    #[default]
    Standard,
    Heavy,
}

impl Tier {
    pub const ALL: [Self; 3] = [Self::Light, Self::Standard, Self::Heavy];

    pub fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Standard => "standard",
            Self::Heavy => "heavy",
        }
    }

    pub fn lower(self) -> Self {
        match self {
            Self::Heavy => Self::Standard,
            _ => Self::Light,
        }
    }

    pub fn higher(self) -> Option<Self> {
        match self {
            Self::Light => Some(Self::Standard),
            Self::Standard => Some(Self::Heavy),
            Self::Heavy => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierRequest {
    pub tier: Option<Tier>,
    pub min_tier: Option<Tier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierModel {
    #[serde(flatten)]
    pub selection: AgentSelection,
    pub cost_weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TierSettings {
    /// Each list is ordered by preference. Later entries provide backend alternatives.
    pub models: BTreeMap<Tier, Vec<TierModel>>,
    pub max_escalations: u8,
}

impl Default for TierSettings {
    fn default() -> Self {
        Self { models: BTreeMap::new(), max_escalations: 2 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierDecision {
    pub tier: Tier,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetails {
    pub selection: AgentSelection,
    pub tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierAttempt {
    pub tier: Tier,
    pub selection: AgentSelection,
    pub duration_ms: u64,
    pub outcome: AgentOutcome,
    pub tokens: Option<u64>,
    /// Provider-reported USD when available.
    pub cost_usd: Option<f64>,
    /// Weighted tokens when a cost weight is configured.
    pub weighted_tokens: Option<f64>,
    pub escalation_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateJob {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Short label in the user's language, shown as the heading of the parallel work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub model: String,
    pub started_at_ms: Option<i64>,
    #[serde(default)]
    pub selection: Option<AgentSelection>,
    #[serde(default)]
    pub routing: Option<TierDecision>,
    /// Effective workspace-relative write globs. An empty list declares read-only work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touches: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    Claude,
    Codex,
}

impl AgentBackend {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    // Existing partner and unnamed worker settings store a model slug only.
    pub fn for_model(model: Option<&str>) -> Self {
        if model.is_none_or(|model| CLAUDE_MODELS.contains(&model)) {
            Self::Claude
        } else {
            Self::Codex
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    pub fn usage_key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSelection {
    pub backend: AgentBackend,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl AgentSelection {
    pub fn from_model(model: Option<String>, effort: Option<String>) -> Self {
        Self { backend: AgentBackend::for_model(model.as_deref()), model, effort }
    }

    pub fn skill_default() -> Self {
        Self::from_model(Some("claude-sonnet-5-5".into()), None)
    }

    pub fn summary_default() -> Self {
        Self::from_model(Some("claude-haiku-5-5".into()), None)
    }

    pub fn chat_settings(&self) -> ChatSettings {
        ChatSettings { model: self.model.clone(), effort: self.effort.clone(), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCaller {
    FrontDesk,
    Planner,
    Worker,
}

impl AgentCaller {
    pub const ALL: [Self; 3] = [Self::FrontDesk, Self::Planner, Self::Worker];

    pub fn label(self) -> &'static str {
        match self {
            Self::FrontDesk => "受付",
            Self::Planner => "方針役",
            Self::Worker => "作業役",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPolicy {
    pub enabled: bool,
    pub callers: Vec<AgentCaller>,
    pub when: String,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self { enabled: true, callers: vec![AgentCaller::FrontDesk, AgentCaller::Planner], when: String::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPolicyOverride {
    pub enabled: Option<bool>,
    pub callers: Option<Vec<AgentCaller>>,
    pub when: Option<String>,
}

impl AgentPolicy {
    pub fn with_override(&self, policy: Option<&AgentPolicyOverride>) -> Self {
        match policy {
            Some(policy) => Self {
                enabled: policy.enabled.unwrap_or(self.enabled),
                callers: policy.callers.clone().unwrap_or_else(|| self.callers.clone()),
                when: policy.when.clone().unwrap_or_else(|| self.when.clone()),
            },
            None => self.clone(),
        }
    }

    pub fn allows(&self, caller: AgentCaller) -> bool {
        self.enabled && self.callers.contains(&caller)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutcome {
    Ok,
    Failed,
    Cancelled,
    Stalled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInvocation {
    pub started_at_ms: i64,
    pub workspace: String,
    pub session: String,
    pub caller: Option<AgentCaller>,
    pub agent: String,
    pub selection: Option<AgentSelection>,
    pub duration_ms: u64,
    pub outcome: AgentOutcome,
    pub tokens: Option<u64>,
    pub job: String,
    pub title: String,
    #[serde(default)]
    pub routing: Option<TierDecision>,
    #[serde(default)]
    pub attempts: Vec<TierAttempt>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentPeriodStats {
    pub uses: usize,
    pub success_rate: Option<f64>,
    pub median_duration_ms: Option<u64>,
    pub total_tokens: u64,
    #[serde(default)]
    pub tiers: BTreeMap<Tier, TierStats>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TierStats {
    pub attempts: usize,
    pub passes: usize,
    pub stalls: usize,
    pub total_tokens: u64,
    pub cost_usd: Option<f64>,
    pub weighted_tokens: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStats {
    pub days_7: AgentPeriodStats,
    pub days_30: AgentPeriodStats,
    pub last_used_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub overrides: BTreeMap<String, AgentSelection>,
    pub summary: Option<AgentSelection>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub fallbacks: BTreeMap<String, AgentSelection>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, AgentPolicyOverride>,
    pub tiers: TierSettings,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tier_models: BTreeMap<Tier, AgentSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consult: Option<AgentSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_title: Option<AgentSelection>,
}

impl AgentSettings {
    pub fn summary_model(&self) -> AgentSelection {
        self.summary.clone().unwrap_or_else(AgentSelection::summary_default)
    }

    pub fn thread_title_model(&self) -> AgentSelection {
        self.thread_title.clone().unwrap_or_else(|| self.summary_model())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentModelRole {
    Consult,
    ThreadTitle,
    WorkerTier(Tier),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub backend: AgentBackend,
    pub model: String,
    pub effort: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub prompt: String,
    pub path: Option<String>,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(flatten)]
    pub policy: AgentPolicy,
}

impl AgentInfo {
    pub fn default_selection(&self) -> AgentSelection {
        AgentSelection {
            backend: self.backend,
            model: (!self.model.is_empty()).then(|| self.model.clone()),
            effort: self.effort.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPace {
    Full,
    #[default]
    Normal,
    Economy,
}

impl GoalPace {
    pub const ALL: [Self; 3] = [Self::Full, Self::Normal, Self::Economy];

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "全力",
            Self::Normal => "標準",
            Self::Economy => "節約",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalParallelLimits {
    pub normal: usize,
    pub economy: usize,
}

impl Default for GoalParallelLimits {
    fn default() -> Self {
        Self { normal: 2, economy: 1 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Active,
    WaitingForUser,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub task: String,
    pub agent: Option<String>,
    pub reason: String,
    #[serde(flatten)]
    pub routing: TierRequest,
    #[serde(default)]
    pub tier_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<StepBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedApproach {
    pub approach: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub done_condition: String,
    pub status: PlanStatus,
    pub pace: Option<GoalPace>,
    pub done: Vec<String>,
    pub next: Vec<PlanStep>,
    pub rejected: Vec<RejectedApproach>,
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused_by_user: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBlocker {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<StepNeed>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepNeed {
    Decision,
    DeviceCheck,
    Other,
}

/// The reason for a live notification. The receiving client supplies the display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    TurnFinished,
    TurnFailed,
    Approval,
    Question,
    GoalBlocked,
    WorkerFinished,
    WorkerFailed,
    WorkerStalled,
}

impl NotificationKind {
    pub fn needs_user(self) -> bool {
        matches!(self, Self::Approval | Self::Question | Self::GoalBlocked)
    }

    pub fn is_worker(self) -> bool {
        matches!(self, Self::WorkerFinished | Self::WorkerFailed | Self::WorkerStalled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GoalProgress {
    Ready,
    Slowed,
    Paused { resume_at: Option<i64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateReport {
    pub elapsed_ms: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub changes: String,
    #[serde(default)]
    pub attempts: Vec<TierAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<DelegateFileObservation>,
}

impl DelegateReport {
    pub fn interrupted(elapsed_ms: u64) -> Self {
        Self {
            elapsed_ms,
            text: "The server stopped before the worker finished.".into(),
            interrupted: true,
            changes: String::new(),
            attempts: Vec::new(),
            files: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateWriteSetSource {
    Declared,
    Prompt,
    PreviousJob,
    Unknown,
}

/// Workspace file changes observed during an admitted job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateFileObservation {
    pub source: DelegateWriteSetSource,
    pub touches: Option<Vec<String>>,
    /// None when no before/after comparison was available.
    pub changed_files: Option<Vec<String>>,
}

/// どのエージェントの発言・作業か。Codex のスレッドは Claude のツール呼び出しの
/// 子として表示するため、親のツール呼び出し ID を持てるようにしておく。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    User,
    Claude,
    Codex { thread_id: String, parent_tool_use: Option<String> },
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GitRequest {
    Status,
    Diff { path: String, staged: bool },
    Commit { message: String },
    Push { session: String },
    Pull { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFile {
    pub status: String,
    pub path: String,
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub files: Vec<GitFile>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub hash: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitOutcome {
    Status { status: GitStatus, commits: Vec<GitCommit> },
    Diff { text: String },
    Done { message: String },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSuite {
    pub id: String,
    pub kind: String,
    pub cwd: String,
    pub command: String,
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestState {
    Running,
    Passed,
    Failed,
    Cancelled,
    TimedOut,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestFailure {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRun {
    pub suite: String,
    pub revision: u64,
    pub state: TestState,
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<TestFailure>,
    // Kept for the GUI. MCP returns the counts and failures separately.
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSession {
    pub session: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadHistoryFailure {
    Unavailable,
    NotFound,
    Stale,
}

/// A read-only component schema understood by the connecting client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownComponentCapability {
    pub tag: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    SelfUpdateStatus,
    RequestDrain { reason: DrainReason },
    CancelDrain { started_at: i64 },
    /// Skip waiting. An ID targets an existing shutdown, and no ID starts an immediate shutdown.
    ForceShutdown { started_at: Option<i64> },
    StartSelfUpdate,
    RollbackServer,
    SelfUpdateCanary { action: SelfUpdateCanaryAction },
    SelfUpdateWait { started_at: i64, action: SelfUpdateWaitAction },
    ListExtensions,
    GrantExtension { id: String, approval: String },
    SetExtensionEnabled { id: String, enabled: bool },
    UpdateExtension { id: String },
    RenderExtension { id: String, request: u64 },
    ExtensionUiEvent { id: String, request: u64, element: String, value: Option<String> },
    Hello {
        protocol: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        markdown_components: Vec<MarkdownComponentCapability>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        thread_history: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        selection_scoped_transcripts: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cached_sessions: Vec<CachedSession>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<String>,
        resume_from: Option<u64>,
        /// v11 の再接続では resume_from の属するワークスペースを指定する。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_id: Option<String>,
    },
    SetLocale { locale: String },
    ListPendingApprovals { request: u64 },
    DecideApproval { request: u64, approval_id: String, decision: ApprovalDecision, reason: Option<String> },
    ListWorkspaces,
    RemoveWorkspace { id: String },
    Trash { action: TrashAction },
    RewriteReply { session: String, item: String },
    Git { id: u64, request: GitRequest },
    History { id: u64, request: HistoryRequest },
    BranchReview { id: u64, request: BranchReviewRequest },
    OpenWorkspace { path: String },
    SwitchWorkspace { id: String },
    CompleteWorkspacePath { prefix: String },
    BrowseWorkspace { request: u64, path: String, hidden: bool },
    ListFiles { request: u64, dir: String, hidden: bool },
    FindFiles { request: u64, query: String, hidden: bool, recent: Vec<String> },
    ResolveFile { request: u64, path: String, lookup_deck: bool },
    CreateWorkspace { request: u64, parent: String, name: String, kind: WorkspaceKind },
    /// 切り替え前にキューへ入った操作が、切り替え先へ届くのを防ぐ。
    InWorkspace { id: String, request: Box<ClientMsg> },
    /// ユーザーの入力をセッションへ送る。
    Prompt { session: String, text: String },
    SetDelegateSettings { settings: DelegateSettings },
    SetWorkerApprovalPolicy { policy: WorkerApprovalPolicy },
    ListFolderGrants {
        session: String,
        audit_before: Option<u64>,
    },
    AddFolderGrant {
        session: String,
        path: String,
        expiry: FolderGrantExpiry,
    },
    RevokeFolderGrant {
        session: String,
        grant_id: String,
    },
    DecideFolderAccess {
        request_id: String,
        expiry: Option<FolderGrantExpiry>,
    },
    RememberShellApproval {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    RevokeShellApproval { id: String },
    SetPartnerDefault { settings: ChatSettings },
    SetReviewSettings { settings: ReviewSettings },
    SetThreadWorker { session: String, settings: DelegateSettings },
    ListAgents,
    SetAgentPolicy { name: String, policy: Option<AgentPolicyOverride> },
    SetAgentCaller { name: String, caller: AgentCaller, allowed: bool },
    GetAgentStats,
    GetAgentHistory { agent: Option<String>, session: Option<String> },
    SetAgentOverride { name: String, selection: Option<AgentSelection> },
    SetThreadAgentOverride { session: String, name: String, selection: Option<AgentSelection> },
    SetSummaryModel { selection: Option<AgentSelection> },
    SetAgentRoleModel { role: AgentModelRole, selection: Option<AgentSelection> },
    SetAgentFallback { name: String, selection: Option<AgentSelection> },
    Replan { session: String, reason: String },
    SetGoalPace { session: String, pace: Option<GoalPace> },
    PauseGoal { session: String },
    WorkerPrompt { session: String, job: String, text: String, paths: Vec<String> },
    StopWorker { session: String, job: String },
    RolePrompt { session: String, text: String, paths: Vec<String>, to_worker: bool },
    SetChatSettings { session: String, settings: ChatSettings },
    /// チャンクは接続内で昇順に送る。最後のチャンクで受信済みパスを返す。
    UploadAttachment { id: u64, name: String, size: u64, offset: u64, data: String, #[serde(default, skip_serializing_if = "Option::is_none")] session: Option<String> },
    FileInbox { session: String, path: String, to: String },
    Ledger { request: u64, action: LedgerAction },
    SearchLibrary {
        request: u64,
        query: String,
        #[serde(default)]
        kinds: Vec<LibraryKind>,
        deck: Option<String>,
        #[serde(default)]
        sort: LibrarySort,
        #[serde(default)]
        offset: usize,
        limit: usize,
    },
    AddMemo { request: u64, text: String, from: Option<String> },
    MoveMemo { request: u64, id: String, deck: String },
    ReviewReports { deck: String, #[serde(default, skip_serializing_if = "Option::is_none")] request: Option<u64>, #[serde(flatten)] action: ReviewReportsAction },
    SearchDecks { query: String },
    CancelAttachment { id: u64 },
    SaveScreenshot { request: u64, destination: ScreenshotDestination, data: String, note: String, pane: String, session: Option<String> },
    ClientScreenshotResponse { id: u64, response: ScreenshotResponse },
    PromptWithAttachments { session: String, text: String, paths: Vec<String> },
    NewThread,
    NewAttachedThread { deck: String },
    AttachThread { id: String, decks: Vec<String> },
    DetachThread { id: String, deck: String },
    RenameThread { id: String, title: String },
    ArchiveThread { id: String, archived: bool },
    PinThread { id: String, pinned: bool },
    /// 実行中のターンを止める。
    Interrupt { session: String },
    /// Issue をローカルの作業者（Claude）に取り組ませる。スレッドができる。
    WorkIssue { number: u64 },
    IssueThreads { action: IssueThreadAction },
    GitHubReport { request: u64, action: GitHubReportAction },
    Handoff { request: HandoffRequest },
    /// Issue のラベルを付け替える（`dot:ready` にする、など）。
    SetIssueLabels { number: u64, add: Vec<String>, remove: Vec<String> },
    /// Issue の一覧を今すぐ取り直す。
    RefreshIssues,
    StartResearchIssue { number: u64 },
    AcceptResearchIssue { number: u64 },
    /// PDF の位置から、そこを書いているカードを逆引きする（synctex）。`page` は 0 始まり、
    /// `x` / `y` はページ左上を原点とする 0〜1。答えは `Located`。
    LocatePdf { pdf: String, page: usize, x: f32, y: f32 },
    GetPaperPdfMap { deck: String, pdf: String },
    /// プロジェクト内のファイルを書き換える（カードの編集など）。既にあるファイルだけ。
    SaveFile { path: String, text: String },
    SaveSource { request: u64, edit: SourceEdit },
    LspBuffer { document: u64, update: LspBuffer },
    LspRequest { request: u64, document: u64, query: LspQuery },
    EditorClosed { path: String },
    /// 表示するカードのデッキを切り替える（プロジェクトからの相対パス。"" はプロジェクトそのもの）。
    SelectDeck { deck: String },
    /// エージェントが開いた論文を要求元の接続にだけ表示する。
    SelectPaper { deck: String },
    LookupSymbol { request: u64, lookup: SymbolLookup },
    /// 保存結果は要求元の接続にだけ返す。
    SaveCard { request: u64, deck: String, card: String, part: CardEditPart, body: String, base_hash: String },
    RecordSourceFingerprint { request: u64, deck: String, card: String, revision: TranslationRevision },
    /// デッキ（論文・本）から PDF を作る。答えは `BuildResult`。
    BuildPaper { deck: String },
    CancelBuild { deck: String },
    ListTests,
    RunTests { suite: String, filter: Option<String> },
    StopTests { suite: String },
    /// カードが指す Lean の宣言を確かめ直す。答えは `LeanStatus`。
    CheckLean,
    GetLeanReadingCards {
        request: u64,
        deck: String,
        declarations: Vec<(String, Option<String>)>,
    },
    /// デッキの文章（"style"）、Lean（"lean"）、数学（"math"）をバックグラウンドで査読する。
    RequestReview { deck: String, kind: String },
    CancelReview { deck: String, kind: String },
    /// 論文（デッキ）のカードにコメントをためる。
    AddComment {
        deck: String,
        card: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proof: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<CommentSource>,
    },
    RemoveComment { deck: String, index: usize },
    /// ためたコメントを、まとめて会話に送り、消す。
    SendComments {
        deck: String,
        session: String,
        /// 送った接続にだけ CommentsSent を返す。
        #[serde(default)]
        confirm: bool,
    },
    ApplyComments { deck: String, origin: String },
    GetThreadArtifacts { session: String },
    /// Fetch whole transcript entries strictly before the supplied cursor.
    GetThreadTail { session: String, request: u64 },
    GetThreadHistory {
        session: String,
        request: u64,
        before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
    },
    CompactSession { session: String },
    /// 使用量による休憩を待たずに、今すぐ再開する。
    ResumeNow { session: String },
    /// エージェントからの承認要求に答える。
    Resolve { request_id: String, decision: String },
    AnswerQuestion {
        request_id: String,
        #[serde(default)]
        answers: BTreeMap<String, QuestionAnswer>,
        #[serde(default)]
        dismiss: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardEditPart {
    Body,
    Proof,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardEditSource {
    pub body: String,
    pub base_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CardSaveOutcome {
    Saved,
    Conflict { latest: CardEditSource },
    TranslationConflict,
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationStatus {
    Current,
    Stale,
    Unverified,
    SourceUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationRevision {
    pub base_hash: String,
    pub source_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationCheck {
    pub status: TranslationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<TranslationRevision>,
}

/// A card region in PDF page coordinates, normalized from the top-left.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfCardRegion {
    pub page: usize,
    pub card: String,
    pub proof: bool,
    pub rect: [f32; 4],
}

/// Post-build checks for one PDF, identified by its workspace-relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfAudit {
    pub pdf: String,
    pub links: PdfAuditCheck,
    pub bookmarks: PdfAuditCheck,
    pub synctex: PdfAuditCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfAuditCheck {
    pub status: PdfAuditStatus,
    pub findings: Vec<PdfAuditFinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfAuditStatus {
    Pass,
    Fail,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfAuditFinding {
    pub code: String,
    pub location: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

/// クライアントに見せるカード 1 枚。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardView {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub path: String,
    /// (rel, to)
    pub links: Vec<(String, String)>,
    /// (rel, from)
    pub backlinks: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
    pub files: Vec<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_body: Option<CardEditSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_proof: Option<CardEditSource>,
    pub external: bool,
    /// 出典（`books/x.pdf#123 Theorem 2.4` など）
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<TranslationCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_origin: Option<ResearchOrigin>,
    /// フォルダのカードの .tex（プロジェクトからの相対）
    #[serde(default)]
    pub tex: Vec<String>,
    /// 形式化した Lean の宣言
    #[serde(default)]
    pub lean: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchOrigin {
    pub deck: String,
    pub card: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    #[serde(default, skip_serializing_if = "ThreadGitHub::is_default")]
    pub github: ThreadGitHub,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_attached: Vec<DeckRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<DeckRoute>,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untitled: bool,
    /// Last activity (Unix milliseconds).
    pub updated: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default)]
    pub attached: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub math_macros: Vec<MathMacro>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<ClaudeSessionView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partner_settings: Option<ChatSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_settings: Option<DelegateSettings>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_overrides: BTreeMap<String, AgentSelection>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadGitHub {
    pub links: Vec<GitHubLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intake_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intake_from: Option<String>,
    // Retain the old field for saved threads and clients through protocol 42.
    pub policy: GitHubPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_policies: Option<GitHubPolicies>,
    pub unread: usize,
}

impl ThreadGitHub {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn policies(&self) -> GitHubPolicies {
        self.class_policies.unwrap_or_else(|| match self.policy {
            GitHubPolicy::Notify => GitHubPolicies::default(),
            policy => GitHubPolicies::uniform(policy),
        })
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubAuthor {
    Owner,
    Agent,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitHubPolicies {
    pub owner: GitHubPolicy,
    pub agent: GitHubPolicy,
    pub other: GitHubPolicy,
}

impl Default for GitHubPolicies {
    fn default() -> Self {
        Self { owner: GitHubPolicy::Ask, ..Self::uniform(GitHubPolicy::Notify) }
    }
}

impl GitHubPolicies {
    pub fn uniform(policy: GitHubPolicy) -> Self {
        Self { owner: policy, agent: policy, other: policy }
    }

    pub fn get(&self, author: GitHubAuthor) -> GitHubPolicy {
        match author {
            GitHubAuthor::Owner => self.owner,
            GitHubAuthor::Agent => self.agent,
            GitHubAuthor::Other => self.other,
        }
    }

    pub fn set(&mut self, author: GitHubAuthor, policy: GitHubPolicy) {
        match author {
            GitHubAuthor::Owner => self.owner = policy,
            GitHubAuthor::Agent => self.agent = policy,
            GitHubAuthor::Other => self.other = policy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GitHubLink {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubPolicy {
    #[default]
    Notify,
    Ask,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubItem {
    pub link: GitHubLink,
    pub url: String,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub pull: bool,
    pub checks: Vec<GitHubCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCheck {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubActivity {
    pub id: String,
    pub link: GitHubLink,
    pub actor: String,
    #[serde(default)]
    pub author: GitHubAuthor,
    pub kind: GitHubActivityKind,
    pub excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub url: String,
    pub at: String,
    pub review_comment: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub intake: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubActivityKind {
    Comment,
    Review,
    ReviewComment,
    Labeled,
    Unlabeled,
    Closed,
    Reopened,
    Merged,
    Check,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueThreadAction {
    SetIntake { session: String, repo: String, move_from: Option<String> },
    RemoveIntake { session: String, repo: String },
    OpenIntakeItem { session: String, event: String },
    Link { session: String, reference: String },
    Unlink { session: String, number: u64 },
    Policy { session: String, policy: GitHubPolicy },
    AuthorPolicy { session: String, author: GitHubAuthor, policy: GitHubPolicy },
    Respond { session: String, events: Vec<String> },
    View { session: Option<String>, item: Option<u64> },
    Reply { session: String, event: String, body: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckRoute {
    pub deck: String,
    pub title: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxProposal {
    pub to: String,
    pub title: Option<String>,
    pub reference_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentApplication {
    pub thread: String,
    pub title: String,
    pub count: usize,
    pub state: CommentApplicationState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommentApplicationState {
    Running,
    Applied { cards: Vec<String> },
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadArtifact {
    pub path: String,
    pub git_status: Option<String>,
    pub exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MathMacro {
    pub name: String,
    pub body: String,
    pub arguments: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeSessionView {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<ClaudeSessionStart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeSessionStart {
    New,
    Resumed,
    FromSummary,
}

pub fn issue_thread_id(number: u64) -> String {
    format!("thread-issue-{number}")
}

/// 論文に紐づく、送信前のコメント。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentView {
    pub card: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<CommentSource>,
    /// 書いた時刻（Unix 秒）
    pub created: i64,
}

/// Original UTF-8 byte range and text in the body of the card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentSource {
    pub range: std::ops::Range<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardDiagnostic {
    pub error: bool,
    pub path: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<CardStyleDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardStyleDiagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub textlint_rule: Option<String>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<CardStyleNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardStyleNote {
    /// Half-open UTF-8 byte ranges in the assembled card body.
    pub range: std::ops::Range<usize>,
    pub excerpt: std::ops::Range<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// 使用量の枠 1 つ。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    /// "claude" / "codex"
    pub agent: String,
    /// 枠の名前（"5時間" / "7日" など、表示用）
    pub name: String,
    /// 使用率 0.0〜1.0
    pub used: f32,
    /// リセットの時刻（Unix 秒）
    pub resets_at: Option<i64>,
    /// 上限に達して断られている
    pub rejected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchIssueState {
    Ready,
    Running,
    NeedsReview,
    Incomplete,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchIssueView {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub kind: String,
    pub acceptance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ResearchIssueDetails>,
    pub cards: Vec<String>,
    pub card_workspaces: Vec<Option<String>>,
    pub state: ResearchIssueState,
    pub assignee: String,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchIssueDetails {
    pub body: String,
    pub created_at: Option<String>,
    pub closed_at: Option<String>,
    pub comments: Vec<ResearchIssueComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchIssueComment {
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub body: String,
}

/// GitHub の作業場の Issue 1 件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueView {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub body: String,
    pub url: String,
    pub updated_at: String,
    /// 本文の `cards:` 行と `[[id]]` から拾った入口カード
    pub cards: Vec<String>,
    /// `work/issue-<N>` ブランチがあれば、その最後のコミット
    pub branch: Option<BranchView>,
    pub comments: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchView {
    pub name: String,
    pub last_commit_at: String,
    pub last_message: String,
}

/// 論文の並び（ハーネスの structure.yaml）の節。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSection {
    pub title: String,
    pub items: Vec<DocItem>,
}

/// PDF を作ったときに確定した相互参照と文献の表示番号。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocReferences {
    pub labels: BTreeMap<String, DocReference>,
    pub citations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocReference {
    pub kind: String,
    pub number: String,
    pub page: String,
    pub title: String,
    pub card: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocItem {
    Card { id: String },
    Subsection { title: String },
}

/// 縦タブ 1 つ（研究 1 つ。研究の区分がないプロジェクトはプロジェクト全体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabView {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub status: String,
    pub next: String,
    /// 計画のファイル（research/<id>/plan.md）。deck.toml の形では None。
    pub plan: Option<String>,
    /// このタブ自身のデッキ（deck.toml の形の研究・本・原稿）。タブを選ぶと開く。
    #[serde(default)]
    pub deck: Option<String>,
    /// "research" / "paper" / "book" / "slides" / "document"
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_exploration: bool,
    pub papers: Vec<PaperView>,
    /// 研究ノート（Markdown）
    pub notes: Vec<String>,
    /// 関係する Lean のパッケージ（lean/<name>）
    pub lean: Vec<String>,
    /// 原稿を共有する、ほかのタブ
    pub related: Vec<RelatedTab>,
}

/// An exploration node in depth-first order for display in a research deck.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorationNode {
    pub id: String,
    pub depth: usize,
    pub title: String,
    pub kind: ExplorationKind,
    pub state: ExplorationState,
    pub record: Option<ExplorationRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationKind {
    Category,
    Question,
    Definition,
    Conjecture,
    Theorem,
    Lemma,
    Example,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationState {
    Queued,
    Active,
    Blocked,
    Done,
    Parked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExplorationRecord {
    Card { path: String, deck: String, card: String },
    File { path: String },
    Unavailable { path: String },
}

/// タブに関係する原稿 1 つ（writing/<name>）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperView {
    pub name: String,
    /// 研究ノートからの参照の数
    pub refs: usize,
    /// カードのデッキ（プロジェクトからの相対パス）
    pub decks: Vec<String>,
    pub pdfs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedTab {
    pub id: String,
    /// 共有している原稿
    pub shared: Vec<String>,
}

/// `seq == 0` の Envelope は「状態のスナップショット」で、backlog に残らず、
/// 再開位置（resume_from）にも数えない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    /// v11 のワークスペース内イベントに付く。seq はワークスペースごと。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Research,
    Software,
    Paper,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GitHubReportAction {
    Status,
    Connect,
    Disconnect,
    Submit { repo: String, login: String, title: String, body: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReportStatus {
    pub enabled: bool,
    pub login: Option<String>,
    pub report_repo: Option<String>,
    pub workspace_kind: WorkspaceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitHubReportUpdate {
    Status { status: GitHubReportStatus },
    Device { user_code: String, verification_uri: String, expires_at: i64 },
    Submitted { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitHubConnectState {
    NotConfigured,
    Disconnected,
    Pending { code: String, url: String, expires_at: i64 },
    Connected { login: String },
    Denied,
    Expired,
    Failed { error: GitHubReportError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubAvatar {
    pub login: String,
    pub etag: String,
    /// A server-validated, downscaled PNG encoded as base64.
    pub png: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubReportError {
    Disabled,
    Config,
    Storage,
    Offline,
    Rejected(u16),
    Response,
    Denied,
    Expired,
    Cancelled,
    AlreadyConnected,
    NotConnected,
    AccountChanged,
    Repository,
    EmptyReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDirectory {
    /// Canonical path relative to the server home, with a leading ~.
    pub path: String,
    pub entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub directory: bool,
    pub workspace: bool,
    pub git: bool,
    #[serde(alias = "fuzukue", rename = "labo")]
    pub fuzukue: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub title: String,
    pub last_opened: Option<i64>,
    pub running: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Card,
    Term,
    Label,
    Macro,
    Lean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SymbolLookup {
    At { deck: String, card: String, text: String, start: usize, end: usize },
    Name { deck: String, name: String, kind: SymbolKind },
    References { id: String },
    LeanDependencies { path: String, line: usize },
}

pub const LIBRARY_PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Reference,
    Card,
    Thread,
    Comment,
    Memo,
    ReviewReport,
    Deck,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    #[default]
    Relevance,
    Newest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LibraryLocation {
    Reference {
        key: String,
        file: String,
        path: String,
        #[serde(flatten)]
        part: LibraryReferencePart,
    },
    Card { card: String },
    Memo { card: String, path: String },
    Thread {
        thread: String,
        message_index: Option<usize>,
        item: Option<String>,
    },
    Comment { index: usize, card: String },
    ReviewReport { submission: String, report: u32, point: Option<String> },
    Deck,
}

impl LibraryLocation {
    pub fn kind(&self) -> LibraryKind {
        match self {
            Self::Reference { .. } => LibraryKind::Reference,
            Self::Card { .. } => LibraryKind::Card,
            Self::Memo { .. } => LibraryKind::Memo,
            Self::Thread { .. } => LibraryKind::Thread,
            Self::Comment { .. } => LibraryKind::Comment,
            Self::ReviewReport { .. } => LibraryKind::ReviewReport,
            Self::Deck => LibraryKind::Deck,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LibraryReferencePart {
    Page { page: usize },
    Lines { lines: [usize; 2] },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryHit {
    pub title: String,
    pub deck: Option<String>,
    #[serde(flatten)]
    pub location: LibraryLocation,
    pub snippet: String,
    /// Half-open UTF-8 byte ranges in the snippet.
    pub highlights: Vec<[usize; 2]>,
    pub score: f64,
    /// Unix milliseconds.
    pub updated: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolLocation {
    pub deck: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolHit {
    pub id: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified: String,
    pub origin: String,
    pub location: SymbolLocation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<SymbolLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub location: SymbolLocation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<SymbolLocation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolResults {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<SymbolHit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<SymbolReference>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeanReadingCard {
    pub declaration: String,
    pub deck: String,
    pub id: String,
    pub title: String,
    pub statement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeanDocFinding {
    pub path: String,
    pub declaration: String,
    pub line: u32,
    pub card: Option<String>,
    pub code: String,
}

/// A catalog key and named substitutions for application-generated text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalizedMessage {
    pub key: String,
    pub args: BTreeMap<String, String>,
}

impl LocalizedMessage {
    pub fn new<const N: usize>(key: &str, args: [(&str, String); N]) -> Self {
        Self {
            key: key.into(),
            args: args.into_iter().map(|(name, value)| (name.into(), value)).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatWaitReason {
    /// `resume_at` is a Unix timestamp in seconds when known.
    UsagePaused { resume_at: Option<i64> },
    SelfUpdate { stage: SelfUpdateStepKind },
    SelfUpdateCanaryApproval,
    Worker,
    BackgroundTest,
    BackgroundBuild,
    QueuedTurn,
    Compacting,
    Goal,
    Watch,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatWaiting {
    pub workspace: Option<ChatWaitReason>,
    /// An absent entry inherits the workspace reason. A null entry suppresses it.
    pub threads: std::collections::BTreeMap<String, Option<ChatWaitReason>>,
    /// Static goal status for clients that support it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub goal_status: std::collections::BTreeMap<String, GoalTail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCommand {
    pub id: String,
    pub summary: String,
    /// Unix timestamp in milliseconds.
    pub started_at_ms: u64,
    pub state: BackgroundCommandState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundCommandState {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GoalTail {
    PendingVerification { reason: String },
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadGroup {
    Pinned,
    Current,
    Archived,
}

impl ThreadGroup {
    pub fn of(thread: &ThreadView) -> Self {
        match (thread.archived, thread.pinned) {
            (true, _) => Self::Archived,
            (false, true) => Self::Pinned,
            _ => Self::Current,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Current => "current",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", content = "args", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientCommand {
    ReorderThread { id: String, target: String, after: bool },
    SetGroupState { group: ThreadGroup, open: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    GitHubReportResult { request: u64, result: Result<GitHubReportUpdate, GitHubReportError> },
    GitHubAccountChanged { state: GitHubConnectState },
    GitHubAvatar { avatar: GitHubAvatar },
    GitHubConnectCard { session: String, attempt: String, state: GitHubConnectState },
    ClientCommand { #[serde(flatten)] command: ClientCommand },
    Consulting { session: String, active: bool },
    SelfUpdateStatus { status: SelfUpdateStatus },
    DrainStatus { status: DrainStatus },
    ShutdownWork { work: ShutdownWork },
    ShutdownError { message: String },
    ServerSwapNotice,
    SelfUpdateError { message: String },
    TrashItems { items: Vec<TrashItem> },
    TrashChanged,
    TrashFailed { message: String },
    ThreadRestored { session: String, events: Vec<ServerEvent> },
    /// Prefix of a partial attach. Workers are the sparse rows preceding the contiguous tail.
    ThreadHistoryStart { session: String, before: u64, workers: Vec<String> },
    ThreadTail {
        session: String,
        request: u64,
        generation: u64,
        through: u64,
        events: Vec<ServerEvent>,
        workers: Vec<String>,
        next_before: Option<u64>,
    },
    ThreadHistoryPage {
        session: String,
        request: u64,
        events: Vec<ServerEvent>,
        next_before: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<u64>,
    },
    ThreadHistoryFailed {
        session: String,
        request: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ThreadHistoryFailure>,
    },
    ThreadSync { epoch: String, through: u64, reset_sessions: Vec<String> },
    ThreadHistoryInvalidated { session: String },
    ReplySuperseded { session: String, items: Vec<String> },
    Extensions { items: Vec<ExtensionInfo> },
    ExtensionPermissionPrompt { item: ExtensionInfo },
    ExtensionView { id: String, request: u64, result: Result<Vec<ExtensionNode>, String> },
    ExtensionError { message: String },
    ExtensionsChanged,
    ExtensionUpdated { id: String, changed_files: Option<Vec<ExtensionFileChange>> },
    SourceSaveResult { request: u64, outcome: SourceSaveOutcome },
    LspDocument { document: u64, update: LspUpdate },
    LspResult { request: u64, outcome: LspOutcome },
    EditorClosed { path: String },
    BuildStarted { deck: String },
    TestSuites { suites: Vec<TestSuite>, error: Option<String> },
    TestRun { run: TestRun },
    GitChanged,
    SymbolResults { request: u64, results: SymbolResults },
    GitResult { id: u64, outcome: GitOutcome },
    HistoryResult { id: u64, outcome: HistoryOutcome },
    BranchReviewResult { id: u64, outcome: BranchReviewOutcome },
    Handoffs { notes: Vec<HandoffSummary> },
    HandoffOpened { session: String },
    HandoffGenerated { path: String },
    HandoffFailed { message: String },
    HandoffDetail { detail: HandoffDetail },
    HandoffWorkerUpdated { session: String, thread_id: String, status: HandoffWorkerStatus },
    HistoryChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed_turns: Option<ChangedTurns>,
    },
    Welcome {
        protocol: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        selection_scoped_transcripts: bool,
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_manifest: Option<ClientManifest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_tailnet_url: Option<String>,
    },
    ClientUpdateAvailable,
    ApprovalQueue { request: u64, result: Result<Vec<ApprovalSummary>, String> },
    ApprovalQueueChanged { approval: ApprovalSummary },
    ApprovalDecisionResult { request: u64, result: Result<ApprovalSummary, String> },
    Workspaces { workspaces: Vec<WorkspaceInfo> },
    WorkspaceSelected { id: String, replay: bool },
    WorkspaceError { message: String },
    WorkspacePaths { prefix: String, paths: Vec<String> },
    WorkspaceDirectory { request: u64, result: Result<WorkspaceDirectory, String> },
    FileList { request: u64, result: Result<FilePage, String> },
    FileMatches { request: u64, result: Result<FilePage, String> },
    FileResolved { request: u64, result: Result<ResolvedFile, String> },
    WorkspaceCreated { request: u64, result: Result<String, String> },
    Threads { threads: Vec<ThreadView> },
    OpenPaper { session: String, deck: String, card: Option<String> },
    /// The partner asked to show a project file in the user's editor; `lines` is 1-based and inclusive.
    OpenFile { session: String, path: String, lines: Option<(u32, u32)> },
    CardSaveResult { request: u64, outcome: CardSaveOutcome },
    ThreadCreated { id: String },
    CommentApplication { session: String, application: CommentApplication },
    ThreadArtifacts { session: String, artifacts: Vec<ThreadArtifact> },
    CompactStatus { session: String, state: String, message: String },
    SessionStarted { session: String, project: String },
    ChatCapabilities {
        effort_levels: Vec<String>,
        mail_restricted: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mcp_tools: Vec<String>,
    },
    DelegateSettingsChanged { settings: DelegateSettings },
    WorkerApprovalPolicyChanged {
        policy: WorkerApprovalPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_rules: Option<Vec<ShellApprovalRule>>,
    },
    PartnerDefaultChanged { settings: ChatSettings },
    AgentModelCapabilities { claude_available: bool, codex_available: bool, front_desk_configured: bool },
    ReviewSettingsChanged { settings: ReviewSettings },
    CodexModels { models: Vec<CodexModel> },
    CodexPartner { session: String, thread_id: Option<String> },
    ChatSettingsChanged { session: String, settings: ChatSettings },
    ClientScreenshotRequested { id: u64 },
    ScreenshotSaved { request: u64, path: Option<String>, error: Option<String> },
    AttachmentProgress { id: u64, offset: u64 },
    AttachmentUploaded {
        id: u64, path: Option<String>, error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal: Option<InboxProposal>,
    },
    InboxFiled { path: String, destination: Option<String>, error: Option<String> },
    LedgerResult { request: u64, view: Option<LedgerView> },
    LedgerChanged,
    LibraryResults {
        request: u64,
        hits: Vec<LibraryHit>,
        more: bool,
        decks: Vec<String>,
    },
    MemoResult { request: u64, error: Option<String> },
    ReviewReports {
        deck: String,
        revision: u64,
        submissions: Vec<ReviewSubmission>,
        splits: Vec<ReviewSplit>,
        fixing: Vec<String>,
    },
    ReviewReportsResult { deck: String, request: Option<u64>, ok: bool },
    ReviewFindings { deck: String, outcome: ReviewFindingsOutcome },
    DeckSearchResults { query: String, decks: Vec<DeckRoute> },
    LocalNotice {
        session: String,
        item: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        localized: Option<LocalizedMessage>,
    },
    UserPrompt {
        session: String, item: String, text: String, paths: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        to_worker: bool,
    },
    /// ストリーミング中のテキスト断片。同じ `item` に追記していく。
    TextDelta { session: String, item: String, actor: Actor, text: String },
    MessageDetails { session: String, item: String, details: RunDetails },
    Agents { agents: Vec<AgentInfo> },
    AgentSettingsChanged { settings: AgentSettings },
    AgentStats { stats: BTreeMap<String, AgentStats>, never_used: Vec<String> },
    AgentHistory { agent: Option<String>, session: Option<String>, entries: Vec<AgentInvocation> },
    AgentUsageChanged,
    DelegateProgress { session: String, job: String, summary: String },
    PlanChanged {
        session: String,
        plan: Plan,
        progress: Option<GoalProgress>,
        #[serde(default)]
        parallel_limits: GoalParallelLimits,
    },
    ToolCall { session: String, item: String, actor: Actor, name: String, input: serde_json::Value },
    ToolResult { session: String, item: String, ok: bool, output: String },
    FolderGrants {
        session: String,
        grants: Vec<FolderGrant>,
        audit: Vec<FolderGrantRead>,
        audit_before: Option<u64>,
    },
    FolderGrantsChanged {
        session: String,
    },
    ApprovalRequested {
        session: String,
        request_id: String,
        actor: Actor,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        localized: Option<LocalizedMessage>,
        /// The working directory of an eligible literal Bash command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_directory: Option<String>,
        /// The prefix the server can save for this request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_prefix: Option<String>,
    },
    /// 承認要求に答えが出た（どのクライアントから答えたかによらず全員に流す）。
    ApprovalResolved {
        request_id: String,
        decision: String,
        /// サーバーの自動承認処理で許可した。
        #[serde(default)]
        automatic: bool,
    },
    QuestionAsked {
        session: String,
        request_id: String,
        actor: Actor,
        questions: Vec<Question>,
    },
    QuestionResolved {
        session: String,
        request_id: String,
        outcome: QuestionOutcome,
        /// Display values only; secret values are masked by the server.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        answers: BTreeMap<String, Vec<String>>,
    },
    TurnActivity { session: String, running: bool },
    /// Authoritative chat activity after replay, at the point of live subscription.
    ChatActivity {
        turns: std::collections::BTreeSet<String>,
        workers: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    },
    TurnEnded { session: String, actor: Actor },
    Notify {
        session: String,
        kind: NotificationKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// 監視しているファイルが変わった（PDF の再ビルドなど）。
    FileChanged { path: String },
    Error { message: String },
    /// プロジェクト内の生成物（今は PDF）の一覧。接続時と、増減したとき。
    Artifacts { pdfs: Vec<String> },
    /// GitHub の作業場（開いている Issue）のスナップショット。
    Issues { repo: String, issues: Vec<IssueView>, error: Option<String> },
    ResearchIssues { repo: String, issues: Vec<ResearchIssueView>, error: Option<String> },
    GitHubItems { repo: String, items: Vec<GitHubItem>, error: Option<String>, retry_at: Option<i64> },
    GitHubActivity { session: String, item: String, events: Vec<GitHubActivity> },
    IssueIntakeMove { session: String, repo: String, from: String, title: String },
    IssueIntakeRepos { repos: Vec<String> },
    GitHubReply { session: String, event: String, error: Option<String> },
    /// `LocatePdf` の答え。`card` は見つかったカードの id。
    Located { pdf: String, file: String, line: u64, card: Option<String> },
    /// `SaveFile` が済んだ。
    Saved { path: String },
    /// 使用量のスナップショット。`pause_at` はこの率を超えたら休む閾値。
    Usage { windows: Vec<UsageWindow>, pause_at: f32 },
    /// v2 以降のスナップショット。キーは (session, agent) で、agent は "claude" / "codex"。
    SessionStats {
        session: String,
        agent: String,
        /// 下流からモデル名を取得できない場合は空文字列。
        #[serde(default)]
        model: String,
        effort: Option<String>,
        cost_usd: Option<f64>,
        /// 直近に報告されたリクエストの入力トークン数。
        context_tokens: Option<u64>,
        context_window: Option<u64>,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        thinking_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_breakdown: Option<std::collections::BTreeMap<String, u64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_compact_threshold: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_tools: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connected: Option<bool>,
        #[serde(default)]
        turns: u64,
    },
    /// 使用量が上限に近いので休んでいる。`until`（Unix 秒）に自動で再開する。
    Paused { session: String, until: i64, reason: String, queued: usize },
    Resumed { session: String },
    ChatWaiting { waiting: ChatWaiting },
    /// Workspace snapshot of current commands, keyed by conversation thread.
    BackgroundCommands { threads: BTreeMap<String, Vec<BackgroundCommand>> },
    BuildResult {
        deck: String,
        ok: bool,
        pdf: String,
        log: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pdf_audit: Vec<PdfAudit>,
    },
    PaperPdfMap { deck: String, pdf: String, digest: String, regions: Vec<PdfCardRegion>, rebuilt: bool },
    PaperJobsReset,
    ReviewDecksPruned { decks: Vec<String> },
    BuildStatus { deck: String, state: String, message: String, pdf: Option<String> },
    /// Lean の宣言ごとの状態: "proved" / "sorry" / "missing" / "axioms: …" / "error: …"。`running` は確認中。
    LeanReadingCards { request: u64, cards: Vec<LeanReadingCard> },
    LeanStatus {
        decls: Vec<(String, String)>,
        running: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        doc_findings: Vec<LeanDocFinding>,
    },
    /// バックグラウンド査読の進み具合。モデル、開始時刻、件数とカードは v13 で追加。
    ReviewStatus {
        deck: String,
        kind: String,
        state: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<RunDetails>,
    },
    /// 論文（デッキ）の、送信前のコメント。
    Comments { deck: String, items: Vec<CommentView> },
    /// コメント送信と下書きの消去を確定した。要求した接続にだけ返す。
    CommentsSent { deck: String, session: String },
    /// 縦タブ（研究）の一覧と、今表示しているデッキ。
    Workspace { tabs: Vec<TabView>, deck: String },
    /// カードの並び（structure.yaml）と、数式のマクロ（harness.toml の mathjax_macros）。
    /// 並びのないデッキでは `sections` が空。
    Document {
        title: String,
        sections: Vec<DocSection>,
        macros: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        references: Option<DocReferences>,
    },
    /// カード全体のスナップショット（接続時と、カードが変わったとき）。
    Cards {
        #[serde(default)]
        deck: String,
        cards: Vec<CardView>,
        diagnostics: Vec<CardDiagnostic>,
    },
}

/// Reserved LocalNotice item for the latest status beside a thread's composer.
pub const COMPOSER_NOTICE_ITEM: &str = "composer-status";

impl ServerEvent {
    /// The session whose saved transcript is updated by this event.
    pub fn transcript_session(&self) -> Option<&str> {
        match self {
            Self::LocalNotice { session, item, .. } if item != COMPOSER_NOTICE_ITEM => Some(session),
            Self::TextDelta { session, .. } | Self::UserPrompt { session, .. }
            | Self::ReplySuperseded { session, .. } | Self::GitHubActivity { session, .. }
            | Self::CommentApplication { session, .. } | Self::DelegateProgress { session, .. }
            | Self::ToolCall { session, .. } | Self::ToolResult { session, .. }
            | Self::MessageDetails { session, .. } | Self::TurnEnded { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Whether the event is only meaningful to a connected client.
    pub fn is_live_only(&self) -> bool {
        matches!(self, Self::OpenFile { .. } | Self::OpenPaper { .. } | Self::ClientCommand { .. }
            | Self::Notify { .. } | Self::IssueIntakeMove { .. }
            | Self::ApprovalQueue { .. } | Self::ApprovalQueueChanged { .. } | Self::ApprovalDecisionResult { .. }
            | Self::FolderGrants { .. } | Self::FolderGrantsChanged { .. }
            | Self::ThreadHistoryStart { .. } | Self::ThreadHistoryPage { .. }
            | Self::ThreadHistoryFailed { .. } | Self::ThreadTail { .. } | Self::ThreadSync { .. })
            || matches!(self, Self::LocalNotice { item, .. } if item == COMPOSER_NOTICE_ITEM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_markdown_components_are_additive() {
        let legacy = serde_json::json!({"type": "hello", "protocol": PROTOCOL_VERSION, "resume_from": null});
        let hello: ClientMsg = serde_json::from_value(legacy.clone()).unwrap();
        assert!(matches!(&hello, ClientMsg::Hello { markdown_components, .. } if markdown_components.is_empty()));
        assert_eq!(serde_json::to_value(hello).unwrap(), legacy);

        let mut offered = legacy.clone();
        offered["markdown_components"] = serde_json::json!([{"tag": "callout", "version": 1}]);
        let hello: ClientMsg = serde_json::from_value(offered.clone()).unwrap();
        assert!(matches!(&hello, ClientMsg::Hello { markdown_components, .. }
            if markdown_components == &[MarkdownComponentCapability { tag: "callout".into(), version: 1 }]));
        assert_eq!(serde_json::to_value(hello).unwrap(), offered);

        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum LegacyClient {
            Hello { protocol: u32 },
        }
        let LegacyClient::Hello { protocol } = serde_json::from_value(offered).unwrap();
        assert_eq!(protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn saved_shell_approval_fields_are_optional_and_updates_use_request_ids() {
        let legacy = serde_json::json!({"type": "worker_approval_policy_changed", "policy": "ask"});
        let event = serde_json::from_value::<ServerEvent>(legacy.clone()).unwrap();
        assert!(matches!(event, ServerEvent::WorkerApprovalPolicyChanged { shell_rules: None, .. }));
        assert_eq!(serde_json::to_value(event).unwrap(), legacy);
        let new = ServerEvent::WorkerApprovalPolicyChanged { policy: WorkerApprovalPolicy::Ask, shell_rules: Some(vec![]) };
        assert_eq!(serde_json::to_value(new).unwrap()["shell_rules"], serde_json::json!([]));
        let legacy = serde_json::json!({
            "type": "approval_requested",
            "session": "s", "request_id": "live", "actor": {"kind": "claude"},
            "summary": "Bash: rg word notes.md", "shell_directory": "/work",
        });
        let approval = serde_json::from_value::<ServerEvent>(legacy.clone()).unwrap();
        assert!(matches!(&approval, ServerEvent::ApprovalRequested { shell_prefix: None, .. }));
        assert_eq!(serde_json::to_value(approval).unwrap(), legacy);
        let mut offered = legacy;
        offered["shell_prefix"] = serde_json::json!("rg");
        let approval = serde_json::from_value::<ServerEvent>(offered.clone()).unwrap();
        assert!(matches!(&approval, ServerEvent::ApprovalRequested { shell_prefix: Some(prefix), .. } if prefix == "rg"));
        assert_eq!(serde_json::to_value(approval).unwrap(), offered);
        let remember = serde_json::json!({"type": "remember_shell_approval", "request_id": "live"});
        let exact = serde_json::from_value::<ClientMsg>(remember.clone()).unwrap();
        assert!(matches!(&exact, ClientMsg::RememberShellApproval { request_id, prefix: None } if request_id == "live"));
        assert_eq!(serde_json::to_value(exact).unwrap(), remember);
        let remember = serde_json::json!({"type": "remember_shell_approval", "request_id": "live", "prefix": "rg"});
        let prefix = serde_json::from_value::<ClientMsg>(remember.clone()).unwrap();
        assert!(matches!(&prefix, ClientMsg::RememberShellApproval { prefix: Some(prefix), .. } if prefix == "rg"));
        assert_eq!(serde_json::to_value(prefix).unwrap(), remember);
    }

    #[test]
    fn github_connect_card_adds_a_public_event() {
        assert_eq!(GITHUB_CONNECT_PROTOCOL, AGENT_MODELS_PROTOCOL + 1);
        let event = ServerEvent::GitHubConnectCard {
            session: "s".into(), attempt: "a".into(),
            state: GitHubConnectState::Pending {
                code: "ABCD-1234".into(), url: "https://github.com/login/device".into(), expires_at: 100,
            },
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire, serde_json::json!({
            "type": "git_hub_connect_card", "session": "s", "attempt": "a",
            "state": {"state":"pending","code":"ABCD-1234","url":"https://github.com/login/device","expires_at":100}
        }));
        assert!(matches!(serde_json::from_value::<ServerEvent>(wire).unwrap(),
            ServerEvent::GitHubConnectCard { state: GitHubConnectState::Pending { expires_at: 100, .. }, .. }));
    }

    #[test]
    fn thread_notification_wire_format_is_live_and_contains_only_a_route_and_kind() {
        let frame = Envelope {
            seq: 0, workspace: Some("lab".into()),
            event: ServerEvent::Notify { session: "thread-1".into(), kind: NotificationKind::Question, title: None },
        };
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({
            "seq": 0, "workspace": "lab",
            "event": { "type": "notify", "session": "thread-1", "kind": "question" }
        }));
        let decoded: Envelope = serde_json::from_value(wire).unwrap();
        assert!(decoded.event.is_live_only());
        assert!(matches!(decoded.event, ServerEvent::Notify { session, kind: NotificationKind::Question, title: None } if session == "thread-1"));
    }

    #[test]
    fn github_report_protocol_round_trips() {
        for action in [
            GitHubReportAction::Status,
            GitHubReportAction::Connect,
            GitHubReportAction::Disconnect,
            GitHubReportAction::Submit {
                repo: "owner/project".into(),
                login: "reporter".into(),
                title: "題名".into(),
                body: "first\nsecond".into(),
            },
        ] {
            let message = ClientMsg::GitHubReport { request: 42, action: action.clone() };
            let wire = serde_json::to_value(message).unwrap();
            assert_eq!(wire["type"], "git_hub_report");
            let ClientMsg::GitHubReport { request, action: decoded } = serde_json::from_value(wire).unwrap() else {
                panic!("expected GitHub report request");
            };
            assert_eq!(request, 42);
            assert_eq!(decoded, action);
        }
        for result in [
            Ok(GitHubReportUpdate::Status { status: GitHubReportStatus {
                enabled: true,
                login: Some("reporter".into()),
                report_repo: Some("owner/project".into()),
                workspace_kind: WorkspaceKind::Research,
            }}),
            Ok(GitHubReportUpdate::Device {
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://github.com/login/device".into(),
                expires_at: 123,
            }),
            Ok(GitHubReportUpdate::Submitted { url: "https://github.com/owner/project/issues/1".into() }),
            Err(GitHubReportError::Rejected(403)),
        ] {
            let event = ServerEvent::GitHubReportResult { request: 42, result: result.clone() };
            let wire = serde_json::to_value(event).unwrap();
            assert_eq!(wire["type"], "git_hub_report_result");
            let ServerEvent::GitHubReportResult { request, result: decoded } = serde_json::from_value(wire).unwrap() else {
                panic!("expected GitHub report response");
            };
            assert_eq!(request, 42);
            assert_eq!(decoded, result);
        }
    }

    #[test]
    fn card_style_note_locations_are_optional_on_the_wire() {
        let legacy = serde_json::json!({
            "code": "style/no-doubled-joshi", "detail": "Repeat particle"
        });
        let mut diagnostic: CardStyleDiagnostic = serde_json::from_value(legacy.clone()).unwrap();
        assert!(diagnostic.note.is_none());
        assert_eq!(serde_json::to_value(&diagnostic).unwrap(), legacy);

        let note = CardStyleNote { range: 3..6, excerpt: 0..9, limit: None };
        diagnostic.note = Some(note.clone());
        let wire = serde_json::to_value(diagnostic).unwrap();
        let round_trip: CardStyleDiagnostic = serde_json::from_value(wire).unwrap();
        assert_eq!(round_trip.note, Some(note));
    }

    #[test]
    fn client_command_round_trips_with_typed_arguments() {
        let command = ServerEvent::ClientCommand {
            command: ClientCommand::ReorderThread { id: "a".into(), target: "b".into(), after: true },
        };
        let wire = serde_json::to_value(command).unwrap();
        assert_eq!(wire, serde_json::json!({
            "type": "client_command", "action": "reorder_thread",
            "args": {"id": "a", "target": "b", "after": true}
        }));
        assert!(matches!(serde_json::from_value(wire).unwrap(),
            ServerEvent::ClientCommand { command: ClientCommand::ReorderThread { after: true, .. } }));
    }

    #[test]
    fn roundtrip() {
        let env = Envelope {
            workspace: None,
            seq: 7,
            event: ServerEvent::TextDelta {
                session: "s".into(),
                item: "i".into(),
                actor: Actor::Codex { thread_id: "t".into(), parent_tool_use: Some("tu".into()) },
                text: "hi".into(),
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 7);
    }

    #[test]
    fn legacy_delegate_job_and_report_have_optional_file_metadata() {
        let input = serde_json::json!({
            "model": "worker", "started_at_ms": null, "selection": null, "routing": null,
        });
        let job: DelegateJob = serde_json::from_value(input.clone()).unwrap();
        assert!(job.touches.is_none() && job.waiting.is_none());
        assert_eq!(serde_json::to_value(job).unwrap(), input);
        let output = serde_json::json!({"elapsed_ms": 1, "text": "done", "attempts": []});
        let report: DelegateReport = serde_json::from_value(output.clone()).unwrap();
        assert!(report.files.is_none());
        assert_eq!(serde_json::to_value(report).unwrap(), output);
    }

    #[test]
    fn self_update_status_remains_compatible_with_old_clients() {
        let old = serde_json::json!({
            "build_id": "abc", "source_commit": null, "available": true, "can_rollback": false,
            "active": false, "steps": [], "error": null, "recovered": false,
        });
        let status: SelfUpdateStatus = serde_json::from_value(old.clone()).unwrap();
        assert!(!status.canary);
        assert!(status.canary_status.is_none());
        assert!(status.wait.is_none());
        assert_eq!(serde_json::to_value(&status).unwrap(), old);

        let canary = SelfUpdateStatus { canary: true, ..status };
        assert_eq!(serde_json::to_value(canary).unwrap()["canary"], true);
    }

    #[test]
    fn document_without_references_round_trips() {
        let old = serde_json::json!({"type": "document", "title": "Paper", "sections": [], "macros": []});
        let event: ServerEvent = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(&event, ServerEvent::Document { references: None, .. }));
        assert_eq!(serde_json::to_value(event).unwrap(), old);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SelfUpdateStatus {
    pub build_id: String,
    pub source_commit: Option<String>,
    pub available: bool,
    pub can_rollback: bool,
    pub active: bool,
    pub steps: Vec<SelfUpdateStep>,
    pub error: Option<String>,
    pub recovered: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub canary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary_status: Option<SelfUpdateCanaryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<SelfUpdateWait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_work_held: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainReason {
    Update,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainProgress {
    Draining,
    Forcing,
    Ready,
    TimedOut,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainStatus {
    /// UTC Unix timestamp in milliseconds. Also identifies this drain for cancellation.
    pub started_at: i64,
    pub reason: DrainReason,
    pub progress: DrainProgress,
    pub jobs_running: usize,
    pub blocking: SelfUpdateWaitReason,
    pub can_cancel: bool,
    pub message: Option<String>,
}

/// Running work across all loaded workspaces, before the client requests a shutdown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownWork {
    pub jobs_running: usize,
    pub threads: Vec<SelfUpdateBlockingThread>,
    pub deadline_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateWaitAction {
    Cancel,
    Force,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelfUpdateWait {
    /// UTC Unix timestamp in milliseconds.
    pub started_at: i64,
    pub elapsed_secs: u64,
    pub reason: SelfUpdateWaitReason,
    pub forcing: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdateWaitReason {
    pub threads: Vec<SelfUpdateBlockingThread>,
    pub gate_held: bool,
    pub checking_sessions: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<SelfUpdateBlocker>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdateBlocker {
    pub workspace: String,
    pub session: Option<String>,
    pub id: String,
    pub title: Option<String>,
    pub kind: SelfUpdateBlockerKind,
    /// UTC Unix timestamp in milliseconds.
    pub started_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateBlockerKind {
    Turn,
    Worker,
    Approval,
    Question,
    Build,
    Review,
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdateBlockingThread {
    pub workspace: String,
    pub id: String,
    pub title: Option<String>,
    pub running: bool,
    pub worker: bool,
    pub queued: bool,
    pub paused: bool,
    pub checking: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateCanaryAction {
    Promote,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateCanaryState {
    Building,
    Testing,
    Running,
    Promoting,
    Discarded,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelfUpdateCanaryStatus {
    pub state: SelfUpdateCanaryState,
    pub commit: Option<String>,
    pub port: Option<u16>,
    pub connect_url: Option<String>,
    /// UTC Unix timestamp in milliseconds.
    pub started_at: Option<i64>,
    /// UTC Unix timestamp in milliseconds.
    pub auto_discard_at: Option<i64>,
    pub log: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelfUpdateStep {
    pub kind: SelfUpdateStepKind,
    pub state: SelfUpdateStepState,
    pub log: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateStepKind {
    Check,
    Build,
    Test,
    Shot,
    Publish,
    Wait,
    Swap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateStepState {
    Pending,
    Running,
    Passed,
    Failed,
    Skipped,
}
