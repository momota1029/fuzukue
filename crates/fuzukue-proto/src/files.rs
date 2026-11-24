use serde::{Deserialize, Serialize};

/// A bounded listing or search result. Paths use workspace-relative slashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePage {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Directory,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedFile {
    /// Canonical path on the server, for a client with direct filesystem access.
    pub absolute: Option<String>,
    pub deck: Option<FileDeck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDeck {
    pub path: String,
    pub card: Option<String>,
}
