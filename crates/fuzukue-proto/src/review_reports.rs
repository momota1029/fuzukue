use serde::{Deserialize, Serialize};

pub const MAX_REVIEW_REPORT_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_REVIEW_REPORT_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionMetadata {
    pub venue: String,
    pub revision: String,
    pub submitted: String,
    #[serde(default)]
    pub decision: String,
    #[serde(default)]
    pub deadline: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPointDraft {
    pub lines: [usize; 2],
    pub summary: String,
    #[serde(default)]
    pub cards: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPointStatus {
    #[default]
    Todo,
    InProgress,
    Fixed,
    Rebut,
    WontFix,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPoint {
    pub id: String,
    pub report: u32,
    pub lines: [usize; 2],
    pub quote: String,
    pub summary: String,
    #[serde(default)]
    pub cards: Vec<String>,
    #[serde(default)]
    pub status: ReviewPointStatus,
    #[serde(default)]
    pub fixed_locations: Vec<String>,
    #[serde(default)]
    pub response: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReviewPointUpdate {
    pub cards: Option<Vec<String>>,
    pub status: Option<ReviewPointStatus>,
    pub fixed_locations: Option<Vec<String>>,
    pub response: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewReport {
    pub number: u32,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSubmission {
    pub id: String,
    pub metadata: SubmissionMetadata,
    pub reports: Vec<ReviewReport>,
    pub points: Vec<ReviewPoint>,
    pub split_reports: Vec<u32>,
    pub response: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSplit {
    pub submission: String,
    pub report: u32,
    pub state: ReviewSplitState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewSplitState {
    Running,
    Proposed { points: Vec<ReviewPointDraft> },
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ReviewReportSource {
    Paste { text: String },
    File { name: String, data: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReviewReportsAction {
    List,
    Create {
        metadata: SubmissionMetadata,
    },
    Add {
        submission: String,
        source: ReviewReportSource,
    },
    Split {
        submission: String,
        report: u32,
    },
    Confirm {
        submission: String,
        report: u32,
        points: Vec<ReviewPointDraft>,
    },
    Update {
        submission: String,
        point: String,
        update: ReviewPointUpdate,
    },
    Fix {
        submission: String,
        point: String,
    },
    WriteResponse {
        submission: String,
    },
}
