//! サーバーを再起動しても続けられるように、プロジェクトごとの状態をディスクに置く。
//! 置き場所: `$XDG_STATE_HOME/fuzukue/<プロジェクトのパスを平らにした名前>/`
//! （既定は `~/.local/state/fuzukue/...`）。プロジェクトのリポジトリは汚さない。
//!
//! - `thread-timeline.jsonl`: タイムラインのイベント（Envelope）を 1 行ずつ
//! - `thread-sessions.json`: 会話 → { Claude のセッション ID, 作業ディレクトリ, チャット設定 }

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use fuzukue_proto::Envelope;
use serde::{Deserialize, Serialize};

const LEGACY_TIMELINE: &str = "timeline.jsonl";
const TIMELINE: &str = "thread-timeline.jsonl";
const SESSIONS: &str = "thread-sessions.json";

pub fn root() -> PathBuf {
    let base = fuzukue_proto::compat::state_home();
    #[cfg(test)]
    fuzukue_proto::compat::assert_test_user_dir(&base);
    let legacy = base.join(fuzukue_proto::compat::LEGACY_APP_DIR);
    if crate::self_update::legacy_handoff() && legacy.is_dir() {
        // The old supervisor still owns its lock, health marker, and version pointers here.
        return legacy;
    }
    let current = fuzukue_proto::compat::user_dir(&base);
    if current != legacy {
        if let Err(error) = crate::self_update::relocate_build_links(&legacy, &current) {
            eprintln!("Could not relocate server build pointers: {error:#}");
        }
    }
    current
}

pub fn config_dir() -> Option<PathBuf> {
    let base = fuzukue_proto::compat::config_home()?;
    #[cfg(test)]
    fuzukue_proto::compat::assert_test_user_dir(&base);
    let legacy = base.join(fuzukue_proto::compat::LEGACY_APP_DIR);
    if crate::self_update::legacy_handoff() && legacy.is_dir() {
        Some(legacy)
    } else {
        Some(fuzukue_proto::compat::user_dir(&base))
    }
}

pub fn config_file(path: PathBuf) -> PathBuf {
    let Some(base) = fuzukue_proto::compat::config_home() else { return path };
    #[cfg(test)]
    fuzukue_proto::compat::assert_test_user_dir(&base);
    let old = base.join(fuzukue_proto::compat::LEGACY_APP_DIR);
    match path.strip_prefix(old) {
        Ok(relative) => config_dir().map(|dir| dir.join(relative)).unwrap_or(path),
        Err(_) => path,
    }
}

pub fn dir_for(project: &Path) -> PathBuf {
    let flat: String = project.to_string_lossy().trim_start_matches('/').replace('/', "%");
    root().join(flat)
}

/// タイムラインの記録を読み戻す（最後の `keep` 件）。
#[cfg(test)]
pub fn load_timeline(dir: &Path, keep: usize) -> Vec<Envelope> {
    let mut out = std::collections::VecDeque::new();
    read_timeline(dir, |env| {
        out.push_back(env);
        if out.len() > keep {
            out.pop_front();
        }
    });
    out.into()
}

/// 旧タイムラインも読み込み、新しいイベントは別ファイルへ追記する。
pub fn read_timeline(dir: &Path, mut visit: impl FnMut(Envelope)) {
    for name in [LEGACY_TIMELINE, TIMELINE] {
        read_events(&dir.join(name), &mut visit);
    }
}

fn read_events(path: &Path, mut visit: impl FnMut(Envelope)) {
    let Ok(file) = std::fs::File::open(path) else { return };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(env) = serde_json::from_str::<Envelope>(&line) {
            visit(env);
        }
    }
}

pub struct TimelineWriter {
    file: std::fs::File,
}

impl TimelineWriter {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(TIMELINE))?;
        Ok(Self { file })
    }

    pub fn append(&mut self, env: &Envelope) {
        if let Ok(line) = serde_json::to_string(env) {
            let _ = writeln!(self.file, "{line}");
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub chat_settings: fuzukue_proto::ChatSettings,
    /// Claude Code のセッション ID（`--resume` に渡す）
    pub claude_session_id: Option<String>,
    pub codex_thread_id: Option<String>,
    #[serde(default)]
    pub last_partner_codex: bool,
    /// A command reset the backend. Start from the seed, if any, on the first turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fresh_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_seed: Option<String>,
    /// Backend sessions replaced by a local command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_sessions: Vec<PreviousSession>,
    /// 作業ディレクトリ（Issue の worktree など）。なければプロジェクト
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviousSession {
    pub backend: fuzukue_proto::AgentBackend,
    pub id: String,
}

pub fn load_sessions(dir: &Path) -> BTreeMap<String, SessionRecord> {
    fn read(dir: &Path, name: &str) -> BTreeMap<String, SessionRecord> {
        std::fs::read_to_string(dir.join(name)).ok()
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    let mut records = read(dir, "sessions.json");
    records.extend(read(dir, SESSIONS));
    records
}

pub fn legacy_timeline_timestamp(dir: &Path) -> Option<i64> {
    let modified = std::fs::metadata(dir.join(LEGACY_TIMELINE)).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).timestamp_millis())
}

pub fn save_sessions(dir: &Path, sessions: &BTreeMap<String, SessionRecord>) {
    let _ = save_sessions_checked(dir, sessions);
}

pub fn save_sessions_checked(dir: &Path, sessions: &BTreeMap<String, SessionRecord>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let serialized = serde_json::to_string_pretty(sessions)?;
    let tmp = dir.join("thread-sessions.json.tmp");
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(tmp, dir.join(SESSIONS))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::ServerEvent;

    #[test]
    fn timeline_roundtrip_keeps_tail() {
        let dir = std::env::temp_dir().join(format!("fuzukue-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = TimelineWriter::open(&dir).unwrap();
        for seq in 1..=5 {
            w.append(&Envelope { workspace: None, seq, event: ServerEvent::FileChanged { path: seq.to_string() } });
        }
        let back = load_timeline(&dir, 3);
        assert_eq!(back.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4, 5]);
    }
}

pub fn load_delegate_settings(dir: &Path) -> fuzukue_proto::DelegateSettings {
    std::fs::read(dir.join("delegate-settings.json")).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save_delegate_settings(dir: &Path, settings: &fuzukue_proto::DelegateSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("delegate-settings.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
    std::fs::rename(tmp, dir.join("delegate-settings.json"))?;
    Ok(())
}

pub(crate) fn load_worker_approval_policy(dir: &Path) -> fuzukue_proto::WorkerApprovalPolicy {
    use fuzukue_proto::WorkerApprovalPolicy;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        delegate: Delegate,
    }

    #[derive(Default, Deserialize)]
    struct Delegate {
        #[serde(default)]
        worker_approvals: WorkerApprovalPolicy,
    }

    // Configuration here is owned by the server. Do not follow a file alias into
    // a checkout a delegated worker can edit.
    fn read(path: &Path) -> std::io::Result<Vec<u8>> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Expected a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Expected a private file"));
            }
        }
        std::fs::read(path)
    }

    match read(&dir.join("worker-approval-policy.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            read(&dir.join("fuzukue.toml")).ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| toml::from_str::<Config>(&text).ok())
                .map(|config| config.delegate.worker_approvals)
                .unwrap_or_default()
        }
        Err(_) => WorkerApprovalPolicy::Ask,
    }
}

pub(crate) fn save_worker_approval_policy(
    dir: &Path, policy: fuzukue_proto::WorkerApprovalPolicy,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec(&policy)?)?;
    file.persist(dir.join("worker-approval-policy.json"))?;
    Ok(())
}

pub fn load_partner_default(dir: &Path) -> fuzukue_proto::ChatSettings {
    std::fs::read(dir.join("partner-default.json")).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub(crate) fn has_role_settings(dir: &Path) -> bool {
    ["partner-default.json", "delegate-settings.json", "review-settings.json", "agent-settings.json"]
        .into_iter().any(|name| !matches!(dir.join(name).symlink_metadata(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound))
}

pub fn save_partner_default(dir: &Path, settings: &fuzukue_proto::ChatSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("partner-default.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
    std::fs::rename(tmp, dir.join("partner-default.json"))?;
    Ok(())
}

pub fn load_review_settings(dir: &Path) -> fuzukue_proto::ReviewSettings {
    std::fs::read(dir.join("review-settings.json")).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save_review_settings(dir: &Path, settings: &fuzukue_proto::ReviewSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let temp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(temp.path(), serde_json::to_vec_pretty(settings)?)?;
    temp.persist(dir.join("review-settings.json"))?;
    Ok(())
}


pub fn load_agent_settings(dir: &Path) -> fuzukue_proto::AgentSettings {
    let path = dir.join("agent-settings.json");
    match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|bytes| {
        serde_json::from_slice(&bytes).map_err(anyhow::Error::from)
    }) {
        Ok(settings) => settings,
        Err(error) => {
            if !error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
                tracing::warn!("Failed to load {}: {error:#}", path.display());
            }
            Default::default()
        }
    }
}

pub fn save_agent_settings(dir: &Path, settings: &fuzukue_proto::AgentSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let temp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(temp.path(), serde_json::to_vec_pretty(settings)?)?;
    temp.persist(dir.join("agent-settings.json"))?;
    Ok(())
}
