use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrashTarget {
    Thread {
        id: String,
    },
    Memo {
        id: String,
    },
    Ledger {
        key: String,
    },
    Submission {
        deck: String,
        id: String,
    },
    Report {
        deck: String,
        submission: String,
        number: u32,
    },
    Comment {
        deck: String,
        draft: super::CommentView,
    },
    Extension {
        id: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrashKind {
    Thread,
    Memo,
    Ledger,
    Submission,
    Report,
    Comment,
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashItem {
    pub id: String,
    pub kind: TrashKind,
    // The display label used by clients and records predating title and source_id.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TrashAction {
    List,
    Delete { target: TrashTarget },
    Restore { id: String },
    Purge { id: String },
}
