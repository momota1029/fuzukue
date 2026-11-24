use serde::{Deserialize, Serialize};

/// Folder access requests use the existing approval event and lifecycle.
pub const FOLDER_ACCESS_REQUEST: &str = "folder-access-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderGrantExpiry {
    ThisTurn,
    OneHour,
    Today,
    UntilThreadEnds,
    NoExpiry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderGrant {
    pub id: String,
    pub session: String,
    pub path: String,
    pub expiry: FolderGrantExpiry,
    /// Unix milliseconds for wall-clock expiries.
    pub expires_at: Option<i64>,
    /// Principal of the authenticated client that granted read access.
    pub granted_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderGrantRead {
    pub session: String,
    pub grant_id: String,
    pub path: String,
    pub time: i64,
}
