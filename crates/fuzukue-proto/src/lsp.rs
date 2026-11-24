//! Language-server requests and connection-scoped editor events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspStatus {
    Unavailable,
    Starting,
    Running,
    Failed,
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspBuffer {
    Open {
        path: String,
        version: u64,
        text: String,
    },
    Change {
        version: u64,
        text: String,
    },
    Save {
        version: u64,
        text: String,
    },
    Close,
}

/// Zero-based line and UTF-16 column, as in LSP.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspQuery {
    Hover {
        position: LspPosition,
    },
    Completion {
        position: LspPosition,
        context: Value,
    },
    Definition {
        position: LspPosition,
    },
    References {
        position: LspPosition,
    },
    DocumentSymbols,
    WorkspaceSymbols {
        query: String,
    },
    CallHierarchy {
        position: LspPosition,
    },
    LeanGoal {
        position: LspPosition,
    },
    Diagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspUpdate {
    Status {
        status: LspStatus,
    },
    /// The version sent by this editor, after mapping from the server's version.
    Diagnostics {
        version: u64,
        diagnostics: Vec<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspOutcome {
    Value { value: Value },
    Error { message: String },
}
