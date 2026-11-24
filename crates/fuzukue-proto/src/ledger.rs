use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerStatus {
    #[default]
    Pending,
    Acquired,
    AskUser,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Paper,
    Supplement,
    Data,
    Code,
    WebPage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerLocationKind {
    Doi,
    Arxiv,
    Pmc,
    Geo,
    Zenodo,
    Github,
    Url,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LedgerLocation {
    pub kind: LedgerLocationKind,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerOutcome {
    Ok,
    #[serde(rename = "403")]
    Forbidden,
    BotWall,
    Login,
    NotFound,
    Timeout,
    Manual,
    Error,
    Started,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerAttempt {
    pub location: LedgerLocation,
    pub at: String,
    pub outcome: LedgerOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerAcquisition {
    pub file: String,
    pub sha256: String,
    pub date: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub key: String,
    pub title: String,
    pub kind: LedgerKind,
    pub reason: String,
    #[serde(default)]
    pub status: LedgerStatus,
    pub locations: Vec<LedgerLocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<LedgerAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired: Option<LedgerAcquisition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerSource {
    Text { text: String },
    Card { path: String },
    Reference { key: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LedgerAction {
    List,
    Propose { input: LedgerSource },
    Accept { ids: Vec<u64> },
    Dismiss { ids: Vec<u64> },
    Fetch { key: String },
    Abandon { key: String },
    Link { download: u64, key: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerProposal {
    pub id: u64,
    pub entry: LedgerEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerDownload {
    pub id: u64,
    pub name: String,
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerView {
    pub entries: Vec<LedgerEntry>,
    pub proposals: Vec<LedgerProposal>,
    pub downloads: Vec<LedgerDownload>,
}

impl LedgerLocation {
    pub fn browser_url(&self) -> String {
        match self.kind {
            LedgerLocationKind::Doi => format!("https://doi.org/{}", self.value),
            LedgerLocationKind::Arxiv => format!("https://arxiv.org/abs/{}", self.value),
            LedgerLocationKind::Pmc => {
                format!("https://pmc.ncbi.nlm.nih.gov/articles/{}/", self.value)
            }
            LedgerLocationKind::Geo => format!(
                "https://www.ncbi.nlm.nih.gov/geo/query/acc.cgi?acc={}",
                self.value
            ),
            LedgerLocationKind::Zenodo => format!("https://zenodo.org/records/{}", self.value),
            LedgerLocationKind::Github | LedgerLocationKind::Url => self.value.clone(),
        }
    }
}
