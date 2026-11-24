use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ExtensionPermissions {
    pub read: Vec<String>,
    pub write: Option<Vec<String>>,
    pub network: Vec<String>,
    pub agent_tools: bool,
    pub log: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub source: String,
    pub version: String,
    pub permissions: ExtensionPermissions,
    pub enabled: bool,
    pub view: bool,
    pub approval: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionNode {
    Heading {
        text: String,
    },
    Markdown {
        text: String,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    List {
        items: Vec<String>,
    },
    Input {
        id: String,
        label: String,
        value: String,
    },
    Button {
        id: String,
        text: String,
    },
    Tabs {
        id: String,
        tabs: Vec<ExtensionTab>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionTab {
    pub title: String,
    pub children: Vec<ExtensionNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionFileChange {
    pub path: String,
    pub status: String,
}
