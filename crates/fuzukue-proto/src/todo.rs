use serde::{Deserialize, Serialize};

/// The first protocol that sends a thread's TODO list and accepts state changes.
pub const TODO_PROTOCOL: u32 = 76;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    Todo,
    Doing,
    Waiting,
    Done,
}

/// What a waiting item is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoWait {
    /// A reply from the user.
    Reply,
    /// An action only the user can take.
    Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoAuthor {
    Front,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Thread-local ID such as `T7`. IDs are never reused.
    pub id: String,
    pub text: String,
    pub state: TodoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<TodoWait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: TodoAuthor,
    /// Unix seconds.
    pub created: i64,
    pub updated: i64,
}
