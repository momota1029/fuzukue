use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSummary {
    pub path: String,
    pub title: String,
    pub date: String,
    pub source: String,
    pub tasks: usize,
    pub workers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffTask {
    pub id: String,
    pub title: String,
    pub markdown: String,
    pub issue: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffWorker {
    pub thread_id: String,
    pub role: String,
    pub worktree: String,
    pub branch: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSkill {
    pub name: String,
    pub path: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffDetail {
    pub session: String,
    pub note: HandoffSummary,
    pub transcript: String,
    pub tasks: Vec<HandoffTask>,
    pub workers: Vec<HandoffWorker>,
    pub skills: Vec<HandoffSkill>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffIssueSelection {
    pub id: String,
    #[serde(default)]
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffWorkerMode {
    Adopt,
    Wait,
    View,
    Stop,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HandoffRequest {
    CreateFromTranscript {
        source: String,
    },
    Receive {
        path: String,
    },
    Detail {
        session: String,
    },
    CreateIssues {
        session: String,
        tasks: Vec<HandoffIssueSelection>,
    },
    Worker {
        session: String,
        thread_id: String,
        mode: HandoffWorkerMode,
    },
    EnableSkill {
        session: String,
        name: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", content = "text", rename_all = "snake_case")]
pub enum HandoffWorkerStatus {
    Adopting,
    Adopted,
    Stopped,
    Waiting,
    Viewing,
    Output(String),
    Finished(String),
    Failed(String),
}
