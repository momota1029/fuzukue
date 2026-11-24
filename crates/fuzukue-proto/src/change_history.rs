use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum HistoryScope {
    Global,
    Card { deck: String, card: String },
    Turn { thread: String, turn: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HistoryRequest {
    List {
        scope: HistoryScope,
    },
    Diff {
        record: String,
    },
    Revert {
        record: String,
        clean_only: Option<String>,
    },
    RestoreCard {
        record: String,
        deck: String,
        card: String,
    },
}

/// Thread IDs mapped to turn IDs and message IDs with retained file changes.
pub type ChangedTurns = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub id: String,
    pub actor: String,
    pub thread: Option<String>,
    pub turn: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    pub time_ms: i64,
    pub files: Vec<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<HistoryBranch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryBranch {
    pub name: String,
    pub tip: String,
    pub discarded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryOutcome {
    Records {
        records: Vec<HistoryRecord>,
    },
    Diff {
        text: String,
        truncated: bool,
    },
    Conflict {
        hunks: Vec<String>,
        clean_only: Option<String>,
    },
    Done,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerEvent;

    #[test]
    fn history_changed_keeps_the_previous_wire_shape_compatible() {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum PreviousEvent {
            HistoryChanged,
        }

        let old = r#"{"type":"history_changed"}"#;
        assert!(matches!(
            serde_json::from_str::<ServerEvent>(old).unwrap(),
            ServerEvent::HistoryChanged { changed_turns: None }
        ));
        let turns: ChangedTurns = [("thread".into(), ["reply".into()].into())].into();
        let event = ServerEvent::HistoryChanged { changed_turns: Some(turns.clone()) };
        let json = serde_json::to_string(&event).unwrap();
        assert!(matches!(serde_json::from_str::<PreviousEvent>(&json), Ok(PreviousEvent::HistoryChanged)));
        assert!(matches!(
            serde_json::from_str::<ServerEvent>(&json).unwrap(),
            ServerEvent::HistoryChanged { changed_turns: Some(received) } if received == turns
        ));
    }
}
