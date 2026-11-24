use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewFindingsOutcome {
    Loaded { snapshot: ReviewFindingsSnapshot },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFindingsSnapshot {
    pub open_count: usize,
    pub card_open_counts: BTreeMap<String, usize>,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    #[serde(flatten)]
    pub content: ReviewFindingContent,
    pub status: ReviewFindingStatus,
    pub transitions: Vec<ReviewFindingTransition>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingContent {
    pub summary: String,
    pub detail: String,
    pub severity: ReviewFindingSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assessment: Option<ReviewFindingAssessment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    pub locations: Vec<ReviewFindingLocation>,
    pub sources: Vec<ReviewFindingSource>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSeverity {
    Fatal,
    Major,
    Minor,
    Form,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingAssessment {
    Confirmed,
    Suspected,
    Unchecked,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<[u32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingSource {
    pub kind: ReviewFindingSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perspective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSourceKind {
    Human,
    Agent,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingStatus {
    Open,
    Fixed,
    Deferred,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ReviewFindingTransition {
    pub time_ms: i64,
    #[serde(flatten)]
    pub action: ReviewFindingTransitionAction,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingTransitionAction {
    pub status: ReviewFindingStatus,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<ReviewFindingChange>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingChange {
    pub summary: String,
    pub reference: ReviewFindingChangeReference,
    pub locations: Vec<ReviewFindingLocation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewFindingChangeReference {
    Commit { oid: String },
    CardEdit { record: String, card: String },
}
