//! プロジェクトと一緒に持ち運べる会話の記録。イベントは Hub と同じ順序で処理する。

use fuzukue_proto::{GitHubActivity, GitHubAuthor, GitHubLink, GitHubPolicy, ThreadGitHub};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use fuzukue_proto::{
    Actor, ClaudeSessionStart, ClaudeSessionView, CommentApplication, CommentApplicationState,
    MathMacro, ServerEvent, ThreadView,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{state, workspace};

use fuzukue_proto::{AgentSelection, ChatSettings, DelegateSettings};

const ENTRY: &str = "<!-- labo-thread ";
const TITLE_MESSAGE_LIMIT: usize = 2000;
const INITIAL_HISTORY_ENTRIES: usize = 256;
const INITIAL_HISTORY_BYTES: usize = 512 * 1024;
pub fn prompt() -> &'static str {
    harness_text!(
        "\nYou are in a standalone conversation thread in this project. The server checks after each turn whether to attach the thread to existing research or a paper. Include card IDs when referring to cards. To attach explicitly, find the deck with list_decks and use attach_thread. Use open_paper to show a paper. Include the card ID when showing a specific card or the result of an edit. When a new research topic takes shape, use create_research to create and attach a deck with initial draft cards. Tell the user where you attached the thread manually.\nIf the user is discussing a specific manuscript in this workspace and the thread has no attached decks, find it with search_library or list_decks, attach it with attach_thread, and read its structure and relevant cards before answering. Only do this for manuscript discussion in an unattached thread.",
        "\nあなたはプロジェクト内の独立した会話スレッドにいる。既存の研究・論文への添付は各ターン後にサーバーが自動で判断する。関連するカードを示すときはカード ID を添える。明示的に付ける必要があるときは list_decks で対象を確認し attach_thread を使う。論文を見せるときは open_paper で開く。特定のカードを示すときやカードを編集した後は、open_paper にカード ID も渡して編集結果を見せる。新しい研究テーマがまとまったら create_research でデッキと最初の下書きカードを作って付ける。手動で付けたときは付けた先をユーザーに伝える。\nデッキに付いていないスレッドでワークスペース内の特定の原稿を話題にしているときは、search_library や list_decks で探して attach_thread で付け、構成と関係するカードを読んでから答える。この自発的な添付は、原稿の話でデッキに付いていないときだけ行う。",
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Front {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<crate::handoff::Received>,
    #[serde(default, skip_serializing_if = "ThreadGitHub::is_default")]
    github: ThreadGitHub,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    github_pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    github_responses: Vec<Vec<String>>,
    id: String,
    title: String,
    #[serde(with = "local_timestamp")]
    created: i64,
    #[serde(with = "local_timestamp")]
    updated: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    archived: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pinned: bool,
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_start: Option<ClaudeSessionStart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migrated_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    promoted_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attached: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    auto_attached: Vec<fuzukue_proto::DeckRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    route_exclusions: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    titled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title_auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_partner_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partner_settings: Option<ChatSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_settings: Option<DelegateSettings>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    worker_pinned: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agent_overrides: BTreeMap<String, AgentSelection>,
}

mod local_timestamp {
    use super::*;
    use serde::{de, ser, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(millis: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        let time = DateTime::from_timestamp_millis(*millis)
            .ok_or_else(|| ser::Error::custom("thread timestamp out of range"))?;
        serializer.serialize_str(
            &time
                .with_timezone(&Local)
                .to_rfc3339_opts(SecondsFormat::Millis, false),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Timestamp {
            Epoch(i64),
            Iso(String),
        }

        match Timestamp::deserialize(deserializer)? {
            Timestamp::Epoch(millis) => Ok(millis),
            Timestamp::Iso(text) => DateTime::parse_from_rfc3339(&text)
                .map(|time| time.timestamp_millis())
                .map_err(de::Error::custom),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct EntryMeta {
    item: String,
    actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool: Option<ToolSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    turn_ends: Vec<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<fuzukue_proto::RunDetails>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    application: Option<CommentApplication>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    to_worker: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    superseded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    github: Vec<GitHubActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notice: Option<fuzukue_proto::LocalizedMessage>,
}

impl EntryMeta {
    fn is_delegated(&self) -> bool {
        self.item.starts_with(fuzukue_proto::DELEGATE_JOB)
            || matches!(&self.actor, Actor::Codex { parent_tool_use: Some(job), .. }
                if job.starts_with(fuzukue_proto::DELEGATE_JOB))
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolSummary {
    name: String,
    ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<String>,
}

impl ToolSummary {
    fn artifact_paths(&self, project: &Path) -> Vec<String> {
        if self.ok != Some(true) {
            return Vec::new();
        }
        crate::thread_artifacts::tool_paths(&self.name, self.input.as_ref(), self.output.as_deref())
            .into_iter()
            .filter_map(|path| crate::thread_artifacts::relative_path(project, &path))
            .collect()
    }
}

#[derive(Clone)]
struct Entry {
    meta: EntryMeta,
    text: String,
}

impl Entry {
    fn is_blank_partner_reply(&self) -> bool {
        crate::routing::is_partner(&self.meta.actor)
            && self.meta.tool.is_none() && self.meta.application.is_none()
            && self.meta.attachments.is_empty() && self.meta.github.is_empty()
            && crate::front_desk::blank_reply(&self.text)
    }
}

#[derive(Default)]
struct Transcript {
    items: Vec<Entry>,
    index: HashMap<String, usize>,
    hidden_replies: HashSet<String>,
}

impl Transcript {
    fn hide_saved_blank_replies(&mut self) {
        // Judge complete messages after journal recovery. Keep the original
        // entries and their turn endings in the portable transcript.
        self.hidden_replies = self.items.iter()
            .filter(|entry| entry.is_blank_partner_reply())
            .map(|entry| entry.meta.item.clone())
            .collect();
    }

    fn push(&mut self, entry: Entry) {
        self.index.insert(entry.meta.item.clone(), self.items.len());
        self.items.push(entry);
    }

    fn observe(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::UserPrompt {
                item, text, paths, to_worker, ..
            } if !self.index.contains_key(item) => {
                self.push(Entry {
                    meta: EntryMeta {
                        item: item.clone(),
                        actor: Actor::User,
                        tool: None,
                        turn_ends: Vec::new(),
                        details: None,
                        attachments: paths.clone(),
                        to_worker: *to_worker,
                        superseded: false,
                        application: None,
                        github: Vec::new(),
                        notice: None,
                    },
                    text: text.clone(),
                });
            }
            ServerEvent::LocalNotice { item, text, localized, .. }
                if !crate::planner::is_failure_notice(item, localized.as_ref()) && !self.index.contains_key(item) => {
                self.push(Entry {
                    meta: EntryMeta {
                        item: item.clone(),
                        actor: Actor::System,
                        tool: None,
                        turn_ends: Vec::new(),
                        details: None,
                        attachments: Vec::new(),
                        to_worker: false,
                        superseded: false,
                        application: None,
                        github: Vec::new(),
                        notice: localized.clone(),
                    },
                    text: text.clone(),
                });
            }
            ServerEvent::TextDelta {
                item, actor, text, ..
            } => {
                if let Some(&i) = self.index.get(item) {
                    self.items[i].text.push_str(text);
                } else {
                    self.push(Entry {
                        meta: EntryMeta {
                            item: item.clone(),
                            actor: actor.clone(),
                            tool: None,
                            turn_ends: Vec::new(),
                            details: None,
                            attachments: Vec::new(),
                            to_worker: false,
                            superseded: false,
                            application: None,
                            github: Vec::new(),
                            notice: None,
                        },
                        text: text.clone(),
                    });
                }
            }
            ServerEvent::ToolCall {
                item, actor, name, input, ..
            } if !self.index.contains_key(item) => {
                self.push(Entry {
                    meta: EntryMeta {
                        item: item.clone(),
                        actor: actor.clone(),
                        turn_ends: Vec::new(),
                        details: None,
                        attachments: Vec::new(),
                        to_worker: false,
                        superseded: false,
                        application: None,
                        github: Vec::new(),
                        notice: None,
                        tool: Some(ToolSummary {
                            name: name.split_whitespace().collect::<Vec<_>>().join(" "),
                            ok: None,
                            input: Some(input.clone()),
                            output: None,
                            progress: None,
                        }),
                    },
                    text: String::new(),
                });
            }
            ServerEvent::ToolCall { item, input, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                if let Some(tool) = self.index.get(item).and_then(|&i| self.items[i].meta.tool.as_mut()) {
                    if tool.output.as_deref().and_then(|output| serde_json::from_str::<fuzukue_proto::DelegateReport>(output).ok())
                        .is_some_and(|report| report.interrupted) {
                        tool.ok = None;
                        tool.output = None;
                    }
                    tool.input = Some(input.clone());
                }
            }
            ServerEvent::DelegateProgress { job, summary, .. } => {
                if let Some(tool) = self.index.get(job).and_then(|&i| self.items[i].meta.tool.as_mut()) {
                    tool.progress = Some(summary.clone());
                }
            }
            ServerEvent::ToolResult { item, ok, output, .. } => {
                if let Some(&index) = self.index.get(item) {
                    let meta = &mut self.items[index].meta;
                    let delegated = meta.is_delegated();
                    let Some(tool) = &mut meta.tool else { return };
                    tool.ok = Some(*ok);
                    if delegated || *ok && crate::thread_artifacts::has_structured_result(&tool.name) {
                        tool.output = Some(output.clone());
                    }
                }
            }
            ServerEvent::CommentApplication { application, .. } => {
                let item = format!("comments-{}", application.thread);
                if let Some(&i) = self.index.get(&item) {
                    self.items[i].meta.application = Some(application.clone());
                } else {
                    self.push(Entry {
                        meta: EntryMeta {
                            item,
                            actor: Actor::System,
                            tool: None,
                            turn_ends: Vec::new(),
                            details: None,
                            attachments: Vec::new(),
                            to_worker: false,
                            superseded: false,
                            application: Some(application.clone()),
                            github: Vec::new(),
                            notice: None,
                        },
                        text: format!("コメント {} 件 → {}", application.count, application.title),
                    });
                }
            }
            ServerEvent::MessageDetails { item, details, .. } => {
                if let Some(&ix) = self.index.get(item) {
                    self.items[ix].meta.details = Some(details.clone());
                }
            }
            ServerEvent::TurnEnded { actor, .. } => {
                if let Some(entry) = self.items.last_mut() {
                    entry.meta.turn_ends.push(actor.clone());
                }
            }
            _ => {}
        }
    }

    fn restore_tool(&mut self, event: ServerEvent) {
        let item = match &event {
            ServerEvent::ToolCall { item, .. } | ServerEvent::ToolResult { item, .. } => item,
            _ => return,
        };
        let Some(tool) = self
            .index
            .get(item)
            .and_then(|&i| self.items[i].meta.tool.as_mut())
        else {
            return;
        };
        match event {
            ServerEvent::ToolCall { input, .. } => tool.input = Some(input),
            ServerEvent::ToolResult { ok, output, .. } => {
                tool.ok = Some(ok);
                tool.output = Some(output);
            }
            _ => {}
        }
    }

    fn context_entries(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        let start = self.items.iter().rposition(|entry| {
            entry.meta.notice.as_ref().is_some_and(|notice| notice.key == "command-cleared")
        }).map_or(0, |index| index + 1);
        self.items[start..].iter().filter(|entry| {
            !entry.meta.superseded && entry.meta.tool.is_none() && entry.meta.application.is_none()
                && !entry.meta.to_worker && !entry.text.trim().is_empty()
                && matches!(entry.meta.actor, Actor::User | Actor::Claude | Actor::Codex { parent_tool_use: None, .. })
                && !entry.meta.item.starts_with(fuzukue_proto::slash::ECHO)
                && !entry.meta.item.contains(":summary:")
                && !self.hidden_replies.contains(&entry.meta.item)
        })
    }

    fn recovery_summary(&self) -> Option<String> {
        let messages: Vec<_> = self.context_entries().collect();
        let first = messages
            .iter()
            .position(|entry| entry.meta.actor == Actor::User);
        let recent = messages.len().saturating_sub(5);
        let mut summary = Vec::new();
        for (index, entry) in messages.into_iter().enumerate() {
            if index < recent && Some(index) != first {
                continue;
            }
            let role = match entry.meta.actor {
                Actor::User => harness_text!("User", "ユーザー"),
                Actor::Claude => "Claude",
                Actor::Codex { .. } => "Codex",
                Actor::System => harness_text!("System", "システム"),
            };
            let mut text = String::new();
            for word in entry.text.split_whitespace() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(word);
                if text.chars().count() > 300 {
                    text = text.chars().take(300).collect::<String>() + "…";
                    break;
                }
            }
            summary.push(format!("- {role}: {text}"));
        }
        (!summary.is_empty()).then(|| summary.join("\n"))
    }

    fn merge(&mut self, recovered: Self) {
        for entry in recovered.items {
            if let Some(&i) = self.index.get(&entry.meta.item) {
                let saved = &mut self.items[i];
                if entry.text.starts_with(&saved.text) {
                    saved.text = entry.text;
                }
                if entry
                    .meta
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.ok.is_some())
                {
                    saved.meta.tool = entry.meta.tool;
                }
                if entry.meta.application.as_ref().is_some_and(|application| {
                    !matches!(application.state, CommentApplicationState::Running)
                }) {
                    saved.meta.application = entry.meta.application;
                }
                if !entry.meta.attachments.is_empty() {
                    saved.meta.attachments = entry.meta.attachments;
                }
                for actor in entry.meta.turn_ends {
                    if !saved.meta.turn_ends.contains(&actor) {
                        saved.meta.turn_ends.push(actor);
                    }
                }
            } else {
                self.push(entry);
            }
        }
    }

    fn write_markdown(&self, mut body: impl Write) -> Result<()> {
        for entry in &self.items {
            let meta = if let Some(tool) = entry.meta.tool.as_ref().filter(|_| !entry.meta.is_delegated()) {
                // 通常のツール詳細は従来どおりジャーナルに残す。
                serde_json::to_string(&EntryMeta {
                    tool: Some(ToolSummary {
                        name: tool.name.clone(), ok: tool.ok, input: None, output: None, progress: None,
                    }),
                    ..entry.meta.clone()
                })?
            } else {
                serde_json::to_string(&entry.meta)?
            };
            // HTML コメントの中で "-->" が始まらないようにする。
            writeln!(body, "{ENTRY}{} -->", meta.replace('>', "\\u003e"))?;
            let role = match entry.meta.actor {
                Actor::User => "User",
                Actor::Claude => "Claude",
                Actor::Codex { .. } => "Codex",
                Actor::System => "System",
            };
            if let Some(tool) = &entry.meta.tool {
                let result = match tool.ok {
                    Some(true) => "success",
                    Some(false) => "failed",
                    None => "result unavailable",
                };
                write!(body, "- {role} called `{}` ({result})\n\n", tool.name)?;
            } else {
                write!(body, "## {role}\n\n")?;
                for (i, line) in entry.text.split('\n').enumerate() {
                    if i > 0 {
                        body.write_all(b"\n")?;
                    }
                    if line.trim_start_matches('\\').starts_with(ENTRY) {
                        body.write_all(b"\\")?;
                    }
                    body.write_all(line.as_bytes())?;
                }
                body.write_all(attachment_list(&entry.meta.attachments).as_bytes())?;
                body.write_all(b"\n\n")?;
            }
        }
        Ok(())
    }

    fn read(body: &str) -> Result<Self> {
        let mut transcript = Self::default();
        let Some(body) = body.strip_prefix(ENTRY) else {
            ensure!(body.is_empty(), "missing transcript entry");
            return Ok(transcript);
        };
        let body = body
            .strip_suffix("\n\n")
            .context("missing transcript separator")?;
        for part in body.split("\n\n<!-- labo-thread ") {
            let (header, content) = part
                .split_once(" -->\n")
                .context("broken transcript header")?;
            let meta: EntryMeta = serde_json::from_str(header)?;
            let text = if meta.tool.is_some() {
                String::new()
            } else {
                let (_, content) = content
                    .split_once("\n\n")
                    .context("missing transcript heading")?;
                let content = content
                    .strip_suffix(&attachment_list(&meta.attachments))
                    .context("missing transcript attachment list")?;
                content
                    .split('\n')
                    .map(|line| {
                        if line.starts_with('\\')
                            && line.trim_start_matches('\\').starts_with(ENTRY)
                        {
                            &line[1..]
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            transcript.push(Entry { meta, text });
        }
        Ok(transcript)
    }

    fn replay_tail(
        &self,
        id: &str,
        workers: Option<&BTreeSet<String>>,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> Vec<ServerEvent> {
        let (events, workers, before) = self.tail(id, workers, restore_tool);
        before.map(|before| ServerEvent::ThreadHistoryStart { session: id.into(), before, workers })
            .into_iter().chain(events).collect()
    }

    fn tail(
        &self,
        id: &str,
        workers: Option<&BTreeSet<String>>,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> (Vec<ServerEvent>, Vec<String>, Option<u64>) {
        let (tail, first) = self.replay_before(id, self.items.len(), restore_tool);
        // Include workers whose rows precede the contiguous tail.
        let active_entries: BTreeSet<_> = workers.into_iter().flatten()
            .filter_map(|job| self.index.get(job).copied())
            .filter(|&index| index < first)
            .collect();
        let workers = active_entries.iter().map(|&index| self.items[index].meta.item.clone()).collect();
        let events = active_entries.into_iter()
            .flat_map(|index| self.events(id, &self.items[index..=index]).map(restore_tool))
            .chain(tail)
            .collect();
        (events, workers, (first != 0).then_some(first as u64))
    }

    fn replay_before(
        &self,
        id: &str,
        before: usize,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> (Vec<ServerEvent>, usize) {
        // Entry ordinals are stable as new entries append. The loaded
        // transcript gives direct access to the requested range.
        let mut size = ReplaySize(0);
        let mut page = Vec::new();
        let mut first = before;
        let start = before.saturating_sub(INITIAL_HISTORY_ENTRIES);
        for index in (start..before).rev() {
            let events: Vec<_> = self.events(id, &self.items[index..=index]).map(restore_tool).collect();
            for event in &events {
                serde_json::to_writer(&mut size, event).expect("serializable history event");
            }
            // Keep the newest entry whole even when it exceeds the byte budget.
            if size.0 > INITIAL_HISTORY_BYTES && !page.is_empty() {
                break;
            }
            first = index;
            page.extend(events.into_iter().rev());
            if size.0 >= INITIAL_HISTORY_BYTES {
                break;
            }
        }
        page.reverse();
        (page, first)
    }

    fn events<'a>(&'a self, id: &'a str, entries: &'a [Entry]) -> impl Iterator<Item = ServerEvent> + 'a {
        entries.iter().flat_map(move |entry| {
            let EntryMeta {
                item,
                actor,
                tool,
                turn_ends,
                details,
                attachments,
                application,
                to_worker,
                superseded,
                github,
                notice,
            } = &entry.meta;
            let mut result = None;
            let event = if let Some(notice) = notice {
                ServerEvent::LocalNotice {
                    session: id.into(), item: item.clone(), text: entry.text.clone(), localized: Some(notice.clone()),
                }
            } else if !github.is_empty() {
                ServerEvent::GitHubActivity { session: id.into(), item: item.clone(), events: github.clone() }
            } else if let Some(application) = application {
                ServerEvent::CommentApplication { session: id.into(), application: application.clone() }
            } else if let Some(tool) = tool {
                result = tool.ok.map(|ok| ServerEvent::ToolResult {
                    session: id.into(),
                    item: item.clone(),
                    ok,
                    output: tool.output.clone().unwrap_or_default(),
                });
                ServerEvent::ToolCall {
                    session: id.into(),
                    item: item.clone(),
                    actor: actor.clone(),
                    name: tool.name.clone(),
                    input: tool.input.clone().unwrap_or_else(|| serde_json::json!({})),
                }
            } else if *to_worker || !attachments.is_empty() {
                ServerEvent::UserPrompt {
                    session: id.into(),
                    item: item.clone(),
                    text: entry.text.clone(),
                    paths: attachments.clone(),
                    to_worker: *to_worker,
                }
            } else {
                ServerEvent::TextDelta {
                    session: id.into(),
                    item: item.clone(),
                    actor: actor.clone(),
                    text: entry.text.clone(),
                }
            };
            let progress = tool.as_ref().and_then(|tool| tool.progress.as_ref()).map(|summary| {
                ServerEvent::DelegateProgress { session: id.into(), job: item.clone(), summary: summary.clone() }
            });
            let visible = !superseded && !self.hidden_replies.contains(item)
                && !crate::planner::is_failure_notice(item, notice.as_ref());
            Some(event).filter(|_| visible).into_iter()
                .chain(progress)
                .chain(details.as_ref().filter(|_| visible).map(|details| ServerEvent::MessageDetails {
                    session: id.into(), item: item.clone(), details: details.clone(),
                }))
                .chain(result)
                .chain(turn_ends.iter().map(|actor| ServerEvent::TurnEnded {
                    session: id.into(),
                    actor: actor.clone(),
                }))
        })
    }
}

struct ReplaySize(usize);

impl Write for ReplaySize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn attachment_list(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut list = "\n\n添付ファイル:".to_owned();
    for path in paths {
        let path = path
            .escape_debug()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        list.push_str(&format!("\n- <code>{path}</code>"));
    }
    list
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LibraryStamp {
    pub title: String,
    pub decks: Vec<String>,
    pub updated: i64,
    revision: u64,
}

pub(crate) struct LibraryThread {
    pub id: String,
    pub stamp: LibraryStamp,
    pub messages: Option<Vec<LibraryMessage>>,
}

pub(crate) struct LibraryMessage {
    pub index: usize,
    pub item: String,
    pub text: String,
}

struct Thread {
    path: PathBuf,
    front: Front,
    transcript: Transcript,
    library_revision: u64,
    library_updated: i64,
    active: bool,
    suggestions: Vec<fuzukue_proto::DeckRoute>,
}

impl Thread {
    fn read(path: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(&path)?;
        let (front, body) =
            fuzukue_cards::split_frontmatter(&text).context("missing thread front matter")?;
        Ok(Self {
            path,
            front: serde_yaml::from_str(front)?,
            transcript: Transcript::read(body)?,
            library_revision: 0,
            library_updated: 0,
            active: false,
            suggestions: Vec::new(),
        })
    }

    fn save(&self) -> Result<()> {
        let dir = self.path.parent().context("missing thread directory")?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        {
            let mut output = BufWriter::new(tmp.as_file_mut());
            write!(output, "---\n{}---\n", serde_yaml::to_string(&self.front)?)?;
            self.transcript.write_markdown(&mut output)?;
            output.flush()?;
        }
        tmp.persist(&self.path)?;
        Ok(())
    }

    fn view(&self, math_macros: Vec<MathMacro>) -> ThreadView {
        ThreadView {
            github: self.front.github.clone(),
            id: self.front.id.clone(),
            title: self.front.title.clone(),
            untitled: !self.front.titled && self.front.title == "新しいスレッド",
            updated: self.front.updated,
            archived: self.front.archived,
            pinned: self.front.pinned,
            attached: self.front.attached.clone(),
            auto_attached: self.front.auto_attached.clone(),
            suggestions: self.suggestions.iter()
                .filter(|route| !self.front.attached.contains(&route.deck)
                    && !self.front.route_exclusions.contains(&route.deck))
                .cloned().collect(),
            math_macros,
            artifacts: self.front.artifacts.clone(),
            partner_settings: self.front.partner_settings.clone(),
            worker_settings: self.front.worker_settings.clone(),
            agent_overrides: self.front.agent_overrides.clone(),
            claude_session: self.front.session_id.clone().map(|id| ClaudeSessionView {
                id,
                start: self.front.session_start,
            }),
        }
    }

    fn recover_artifacts(&mut self, project: &Path) -> bool {
        let paths = self.transcript.items.iter()
            .filter_map(|entry| entry.meta.tool.as_ref())
            .flat_map(|tool| tool.artifact_paths(project))
            .collect();
        self.add_artifacts(paths)
    }

    fn finish_interrupted_applications(&mut self) -> bool {
        let mut changed = false;
        for application in self.transcript.items.iter_mut().filter_map(|entry| entry.meta.application.as_mut()) {
            if matches!(application.state, CommentApplicationState::Running) {
                application.state = CommentApplicationState::Failed { reason: "サーバーが再起動した".into() };
                changed = true;
            }
        }
        changed
    }

    fn add_artifacts(&mut self, paths: Vec<String>) -> bool {
        let old = self.front.artifacts.len();
        self.front.artifacts.extend(paths);
        self.front.artifacts.sort_unstable();
        self.front.artifacts.dedup();
        self.front.artifacts.len() != old
    }

    fn title_is_auto(&self) -> bool {
        self.front.title_auto.unwrap_or_else(|| {
            self.user_prompts().next().and_then(prompt_title).as_deref()
                == Some(self.front.title.as_str())
        })
    }

    fn user_prompts(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.transcript.items.iter()
            .filter(|entry| matches!(entry.meta.actor, Actor::User) && !entry.meta.to_worker && !entry.meta.item.starts_with(fuzukue_proto::slash::ECHO))
            .map(|entry| entry.text.as_str())
    }

    fn rename(&mut self, title: String, automatic: bool) -> Result<()> {
        let old = self.front.clone();
        self.front.title = title;
        self.front.titled = true;
        self.front.title_auto = Some(automatic);
        if let Err(error) = self.save() {
            self.front = old;
            return Err(error);
        }
        Ok(())
    }

    fn touch(&mut self) {
        self.front.updated = now().timestamp_millis();
    }
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

pub fn is_thread(id: &str) -> bool {
    id.starts_with("thread-")
}

fn old_migration_id(session: &str) -> Option<String> {
    if session == "main" || session.starts_with("tab-") {
        Some(format!(
            "thread-legacy-{:x}",
            Sha256::digest(session.as_bytes())
        ))
    } else {
        session
            .strip_prefix("issue-")?
            .parse::<u64>()
            .ok()
            .map(fuzukue_proto::issue_thread_id)
    }
}

fn migration_id(session: &str, title: Option<&str>) -> Option<String> {
    let old = old_migration_id(session)?;
    let Some(hash) = old.strip_prefix("thread-legacy-") else {
        return Some(old);
    };
    let title = title.unwrap_or_default();
    let slug: String = title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
        .chars()
        .take(32)
        .collect();
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() {
        "conversation"
    } else {
        slug
    };
    Some(format!("thread-{slug}-{}", &hash[..8]))
}

pub fn is_legacy_session(session: &str) -> bool {
    session == "main" || session.starts_with("tab-") || session.starts_with("issue-")
}

pub fn history_session(event: &ServerEvent) -> Option<&str> {
    match event {
        ServerEvent::LocalNotice { session, .. }
        | ServerEvent::TextDelta { session, .. }
        | ServerEvent::ReplySuperseded { session, .. }
        | ServerEvent::ThreadRestored { session, .. }
        | ServerEvent::GitHubActivity { session, .. }
        | ServerEvent::UserPrompt { session, .. }
        | ServerEvent::CommentApplication { session, .. }
        | ServerEvent::TurnActivity { session, .. }
        | ServerEvent::DelegateProgress { session, .. }
        | ServerEvent::ToolCall { session, .. }
        | ServerEvent::ToolResult { session, .. }
        | ServerEvent::ApprovalRequested { session, .. }
        | ServerEvent::MessageDetails { session, .. }
        | ServerEvent::TurnEnded { session, .. } => Some(session),
        _ => None,
    }
}

pub struct Store {
    project: PathBuf,
    state_dir: PathBuf,
    threads: BTreeMap<String, Thread>,
    migrations: HashMap<String, String>,
    restored_generations: HashMap<String, u64>,
    pub partner_default: ChatSettings,
    pub worker_default: DelegateSettings,
}

impl Store {
    pub fn load(
        project: &Path,
        state_dir: &Path,
        records: &BTreeMap<String, state::SessionRecord>,
    ) -> Self {
        let mut store = Self {
            project: project.into(),
            state_dir: state_dir.into(),
            threads: BTreeMap::new(),
            restored_generations: HashMap::new(),
            migrations: std::fs::read(state_dir.join("removed-thread-migrations.json")).ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default(),
            partner_default: state::load_partner_default(state_dir),
            worker_default: state::load_delegate_settings(state_dir),
        };
        let dir = crate::safe_join(project, "threads").ok();
        for entry in dir
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.path().extension().is_none_or(|e| e != "md")
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            match Thread::read(entry.path()) {
                Ok(mut thread) if is_thread(&thread.front.id) => {
                    if let Some(deck) = thread.front.promoted_to.take() {
                        if !thread.front.attached.contains(&deck) {
                            thread.front.attached.push(deck);
                        }
                        if let Err(e) = thread.save() {
                            tracing::warn!("could not upgrade {}: {e:#}", thread.path.display());
                        }
                    }
                    if let Some(legacy) = &thread.front.migrated_from {
                        store
                            .migrations
                            .insert(legacy.clone(), thread.front.id.clone());
                    }
                    store.threads.insert(thread.front.id.clone(), thread);
                }
                Ok(_) => tracing::warn!("invalid thread ID in {}", entry.path().display()),
                Err(e) => tracing::warn!("could not restore {}: {e:#}", entry.path().display()),
            }
        }

        // Recover a turn that was in progress when the server stopped from the local journal.
        let mut recovered = HashMap::<String, Transcript>::new();
        let mut legacy = HashMap::<String, Option<(u64, Transcript)>>::new();
        let mut last_legacy_seq = 0;
        let mut legacy_tools = HashMap::<String, Vec<ServerEvent>>::new();
        state::read_timeline(state_dir, |env| {
            if let Some(id) = history_session(&env.event) {
                if store.contains(id) {
                    recovered.entry(id.into()).or_default().observe(&env.event);
                } else if is_legacy_session(id) {
                    last_legacy_seq = env.seq;
                    let migration = legacy.entry(id.into()).or_insert_with(|| {
                        (store.migrated_thread(id).is_none() && old_migration_id(id).is_some())
                            .then(|| (0, Transcript::default()))
                    });
                    if let Some((last, transcript)) = migration {
                        *last = env.seq;
                        transcript.observe(&env.event);
                    }
                    if matches!(
                        env.event,
                        ServerEvent::ToolCall { .. } | ServerEvent::ToolResult { .. }
                    ) {
                        legacy_tools.entry(id.into()).or_default().push(env.event);
                    }
                }
            }
        });
        for (id, transcript) in recovered {
            store
                .threads
                .get_mut(&id)
                .unwrap()
                .transcript
                .merge(transcript);
        }
        let mut migrations = legacy
            .into_iter()
            .filter_map(|(session, entry)| entry.map(|entry| (session, entry)))
            .peekable();
        if migrations.peek().is_some() {
            let tabs = workspace::scan(project, &[]);
            let timestamp = state::legacy_timeline_timestamp(state_dir)
                .unwrap_or_else(|| now().timestamp_millis());
            for (session, (last, transcript)) in migrations {
                // Old envelopes carry sequence numbers but no per-event timestamps.
                let seq_distance = last_legacy_seq.saturating_sub(last);
                let updated =
                    timestamp.saturating_sub(i64::try_from(seq_distance).unwrap_or(i64::MAX));
                let session_id = records
                    .get(&session)
                    .and_then(|record| record.claude_session_id.clone());
                if let Err(e) = store.migrate(&session, session_id, transcript, &tabs, updated) {
                    tracing::warn!("could not migrate {session}: {e:#}");
                }
            }
        }
        for (session, events) in legacy_tools {
            let id = store.migrated_thread(&session).map(str::to_owned);
            if let Some(thread) = id.and_then(|id| store.threads.get_mut(&id)) {
                for event in events {
                    thread.transcript.restore_tool(event);
                }
            }
        }
        for thread in store.threads.values_mut() {
            thread.transcript.hide_saved_blank_replies();
            let recovered = thread.recover_artifacts(project);
            let interrupted = thread.finish_interrupted_applications();
            if recovered || interrupted {
                if let Err(error) = thread.save() {
                    tracing::warn!("could not upgrade {}: {error:#}", thread.path.display());
                }
            }
        }
        store
    }

    fn migrated_thread(&self, session: &str) -> Option<&str> {
        if let Some(id) = self.migrations.get(session) {
            return Some(id);
        }
        let id = old_migration_id(session)?;
        self.threads.get_key_value(&id).map(|(id, _)| id.as_str())
    }

    pub fn restore_sessions(&mut self, records: &mut BTreeMap<String, state::SessionRecord>) {
        let legacy: Vec<_> = records
            .keys()
            .filter(|id| is_legacy_session(id))
            .cloned()
            .collect();
        for session in legacy {
            let record = records.remove(&session).unwrap();
            if let Some(id) = self.migrated_thread(&session) {
                records.entry(id.into()).or_insert(record);
            }
        }
        for (id, thread) in &mut self.threads {
            if thread.front.partner_settings.is_none() {
                thread.front.partner_settings = Some(records.get(id)
                    .map(|record| record.chat_settings.clone()).unwrap_or_default());
            }
            if thread.front.worker_settings.is_none() {
                thread.front.worker_settings = Some(self.worker_default.clone());
            }
            if let Some(record) = records.get(id) {
                if record.claude_session_id != thread.front.session_id {
                    thread.front.session_id.clone_from(&record.claude_session_id);
                    thread.front.session_start = None;
                }
            } else if let Some(session_id) = &thread.front.session_id {
                records
                    .entry(id.clone())
                    .or_insert_with(|| state::SessionRecord {
                        claude_session_id: Some(session_id.clone()),
                        ..Default::default()
                    });
            }
        }
    }

    fn migrate(
        &mut self,
        session: &str,
        session_id: Option<String>,
        transcript: Transcript,
        tabs: &[fuzukue_proto::TabView],
        updated: i64,
    ) -> Result<()> {
        if self.migrated_thread(session).is_some() || transcript.items.is_empty() {
            return Ok(());
        }
        let attached = session
            .strip_prefix("tab-")
            .and_then(|id| tabs.iter().find(|tab| tab.id == id))
            .and_then(|tab| tab.deck.as_deref())
            .and_then(|deck| {
                workspace::deck_path(&self.project, if deck.is_empty() { "." } else { deck }).ok()
            })
            .into_iter()
            .collect();
        let first_prompt = transcript
            .items
            .iter()
            .find(|entry| matches!(entry.meta.actor, Actor::User));
        let title = first_prompt.and_then(|entry| prompt_title(&entry.text));
        let created = first_prompt
            .and_then(|entry| {
                entry
                    .meta
                    .item
                    .strip_prefix("user:")?
                    .split_once(':')?
                    .0
                    .parse::<i64>()
                    .ok()
            })
            .unwrap_or(updated)
            .min(updated);
        let Some(id) = migration_id(session, title.as_deref()) else {
            return Ok(());
        };
        let mut thread = self.thread(id.clone(), attached, title, created, updated)?;
        thread.front.title_auto = Some(thread.front.titled);
        thread.front.completed_partner_turns = None;
        self.insert(thread, transcript, session_id, Some(session))?;
        self.migrations.insert(session.into(), id);
        Ok(())
    }

    pub(crate) fn library_threads(
        &self,
        known: impl Fn(&str, &LibraryStamp) -> bool,
    ) -> Vec<LibraryThread> {
        self.threads.iter().map(|(id, thread)| {
            let stamp = LibraryStamp {
                title: thread.front.title.clone(),
                decks: thread.front.attached.clone(),
                updated: thread.front.updated.max(thread.library_updated),
                revision: thread.library_revision,
            };
            let messages = (!known(id, &stamp)).then(|| {
                thread.transcript.items.iter().enumerate()
                    .filter(|(_, entry)| {
                        !entry.meta.superseded && entry.meta.tool.is_none() && !entry.meta.is_delegated()
                            && (matches!(entry.meta.actor, Actor::User) || crate::routing::is_partner(&entry.meta.actor))
                    })
                    .map(|(index, entry)| LibraryMessage {
                        index,
                        item: entry.meta.item.clone(),
                        text: entry.text.clone(),
                    })
                    .collect()
            });
            LibraryThread { id: id.clone(), stamp, messages }
        }).collect()
    }

    pub(crate) fn title(&self, id: &str) -> Option<String> {
        self.threads.get(id).map(|thread| thread.front.title.clone())
    }

    pub fn attached(&self, id: &str) -> Vec<String> {
        self.threads
            .get(id)
            .map(|thread| thread.front.attached.clone())
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> ServerEvent {
        let mut by_deck = HashMap::new();
        let mut threads: Vec<_> = self.threads.values().map(|thread| {
            let mut macros = BTreeMap::new();
            for deck in &thread.front.attached {
                let definitions = by_deck.entry(deck).or_insert_with(|| {
                    crate::safe_join(&self.project, deck).ok()
                        .and_then(|path| {
                            let cfg = fuzukue_cards::DeckConfig::read(&path).ok()?;
                            Some(crate::cards::math_macros(&path, &cfg).collect::<Vec<_>>())
                        })
                        .unwrap_or_default()
                });
                for definition in definitions.iter() {
                    // Keep the first attached paper's definition when names overlap.
                    macros.entry(definition.name.clone()).or_insert_with(|| definition.clone());
                }
            }
            thread.view(macros.into_values().collect())
        }).collect();
        threads.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
        ServerEvent::Threads { threads }
    }

    pub(crate) fn replay_tails<'a>(
        &'a self,
        workers: &'a BTreeMap<String, BTreeSet<String>>,
        restore_tool: impl Fn(ServerEvent) -> ServerEvent + 'a,
    ) -> impl Iterator<Item = ServerEvent> + 'a {
        self.threads.iter().flat_map(move |(id, thread)| {
            let tail = if thread.active {
                Some(ServerEvent::TurnActivity {
                    session: id.clone(),
                    running: true,
                })
            } else if !thread.transcript.items.is_empty() {
                Some(ServerEvent::TurnEnded {
                    session: id.clone(),
                    actor: Actor::Claude,
                })
            } else {
                None
            };
            thread.transcript.replay_tail(id, workers.get(id), &restore_tool).into_iter().chain(tail)
        })
    }

    pub(crate) fn history_generation(&self, id: &str) -> Option<u64> {
        self.threads.get(id)?;
        Some(self.restored_generations.get(id).copied().unwrap_or(0))
    }

    pub(crate) fn history_tail(
        &self,
        id: &str,
        workers: &BTreeSet<String>,
        restore_tool: impl Fn(ServerEvent) -> ServerEvent,
    ) -> Option<(Vec<ServerEvent>, Vec<String>, Option<u64>)> {
        let thread = self.threads.get(id).filter(|thread| thread.path.is_file())?;
        Some(thread.transcript.tail(id, Some(workers), &restore_tool))
    }

    pub(crate) fn scoped_history_page(
        &self,
        id: &str,
        before: u64,
        generation: Option<u64>,
    ) -> Result<(Vec<ServerEvent>, Option<u64>), fuzukue_proto::ThreadHistoryFailure> {
        use fuzukue_proto::ThreadHistoryFailure::*;
        let thread = self.threads.get(id).ok_or(NotFound)?;
        if self.history_generation(id) != generation {
            return Err(Stale);
        }
        if thread.front.archived || !thread.path.is_file() {
            return Err(Unavailable);
        }
        self.history_page(id, before).ok_or(Stale)
    }

    pub(crate) fn history_page(&self, id: &str, before: u64) -> Option<(Vec<ServerEvent>, Option<u64>)> {
        let thread = self.threads.get(id).filter(|thread| !thread.front.archived && thread.path.is_file())?;
        let before = usize::try_from(before).ok()?;
        if before == 0 || before > thread.transcript.items.len() {
            return None;
        }
        let (events, first) = thread.transcript.replay_before(id, before, &|event| event);
        Some((events, (first != 0).then_some(first as u64)))
    }

    pub(crate) fn previous_delegate_touches(&self, session: &str, job: &str) -> Option<Vec<String>> {
        let thread = self.threads.get(session)?;
        let previous = thread.transcript.items.iter().rev()
            .filter(|entry| entry.meta.item != job && entry.meta.item.starts_with(fuzukue_proto::DELEGATE_JOB))
            .filter_map(|entry| entry.meta.tool.as_ref())
            .find(|tool| tool.name == "delegate")?;
        serde_json::from_value(previous.input.as_ref()?.get("touches")?.clone()).ok()
    }

    pub(crate) fn hidden_replay_event(&self, event: &ServerEvent) -> bool {
        let (session, item) = match event {
            ServerEvent::TextDelta { session, item, actor, .. } if crate::routing::is_partner(actor) => (session, item),
            ServerEvent::MessageDetails { session, item, .. } => (session, item),
            _ => return false,
        };
        let thread = self.threads.get(session)
            .or_else(|| self.migrated_thread(session).and_then(|id| self.threads.get(id)));
        thread.is_some_and(|thread| thread.transcript.hidden_replies.contains(item))
    }

    pub(crate) fn hide_replies_on_reconnect(&self) -> impl Iterator<Item = ServerEvent> + '_ {
        self.threads.iter().filter_map(|(session, thread)| {
            let hidden = &thread.transcript.hidden_replies;
            (!hidden.is_empty()).then(|| ServerEvent::ReplySuperseded {
                session: session.clone(), items: hidden.iter().cloned().collect(),
            })
        })
    }

    pub fn plan_sessions(&self) -> Vec<String> {
        self.threads.iter().map(|(id, _)| id.clone()).collect()
    }

    pub fn recent_requests(&self, id: &str) -> Vec<String> {
        let Some(thread) = self.threads.get(id) else { return Vec::new() };
        let mut requests: Vec<_> = thread.user_prompts().rev().take(4)
            .map(|text| text.chars().take(1800).collect()).collect();
        requests.reverse();
        requests
    }

    pub(crate) fn compaction_input(&self, id: &str, pending: &std::collections::HashSet<String>) -> Option<String> {
        let transcript = &self.threads.get(id)?.transcript;
        let mut remaining = 96_000;
        let mut messages = Vec::new();
        for entry in transcript.context_entries().rev().filter(|entry| !pending.contains(&entry.meta.item)) {
            let count = entry.text.len().min(remaining);
            let mut end = count;
            while !entry.text.is_char_boundary(end) { end -= 1; }
            let role = if entry.meta.actor == Actor::User { "User" } else { "Assistant" };
            messages.push(format!("{role}: {}{}", &entry.text[..end], if end < entry.text.len() { "…" } else { "" }));
            remaining -= count;
            if remaining == 0 { break; }
        }
        messages.reverse();
        (!messages.is_empty()).then(|| messages.join("\n\n"))
    }

    pub fn recovery_summary(&self, id: &str) -> Option<String> {
        self.threads.get(id)?.transcript.recovery_summary()
    }

    pub fn active(&self, id: &str) -> bool {
        self.threads.get(id).is_some_and(|thread| thread.active)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.threads.contains_key(id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    pub fn create(&mut self, deck: Option<&str>) -> Result<String> {
        self.create_titled(deck, None)
    }

    pub fn create_titled(&mut self, deck: Option<&str>, title: Option<String>) -> Result<String> {
        let id = format!("thread-{:032x}", rand::random::<u128>());
        self.create_with_id(&id, deck, title)?;
        Ok(id)
    }

    pub(crate) fn create_with_id(&mut self, id: &str, deck: Option<&str>, title: Option<String>) -> Result<()> {
        self.create_with_github(id, deck, title, ThreadGitHub::default())
    }

    fn create_with_github(
        &mut self, id: &str, deck: Option<&str>, title: Option<String>, github: ThreadGitHub,
    ) -> Result<()> {
        let attached = deck
            .map(|path| workspace::deck_path(&self.project, path))
            .transpose()?
            .into_iter()
            .collect();
        let timestamp = now().timestamp_millis();
        let mut thread = self.thread(id.into(), attached, title, timestamp, timestamp)?;
        thread.front.partner_settings = Some(self.partner_default.clone());
        thread.front.worker_settings = Some(self.worker_default.clone());
        thread.front.github = github;
        self.insert(thread, Transcript::default(), None, None)
    }

    pub(crate) fn receive_handoff(&mut self, title: String, path: String) -> Result<(String, bool)> {
        if let Some(thread) = self.threads.values().find(|thread|
            !thread.front.archived && thread.front.handoff.as_ref().is_some_and(|note| note.path == path))
        {
            return Ok((thread.front.id.clone(), false));
        }
        let id = format!("thread-{:032x}", rand::random::<u128>());
        let timestamp = now().timestamp_millis();
        let mut thread = self.thread(id.clone(), Vec::new(), Some(title), timestamp, timestamp)?;
        thread.front.partner_settings = Some(self.partner_default.clone());
        thread.front.worker_settings = Some(self.worker_default.clone());
        thread.front.artifacts.push(path.clone());
        thread.front.handoff = Some(crate::handoff::Received { path, issues: Default::default() });
        self.insert(thread, Transcript::default(), None, None)?;
        Ok((id, true))
    }

    pub(crate) fn handoff(&self, id: &str) -> Option<crate::handoff::Received> {
        self.threads.get(id)?.front.handoff.clone()
    }

    pub(crate) fn record_handoff_issue(&mut self, id: &str, task: String, number: u64) -> Result<()> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        let note = thread.front.handoff.as_mut().context("Handoff not found")?;
        note.issues.insert(task, number);
        thread.save()
    }

    pub fn create_workshop_thread(
        &mut self, title: String, attached: Vec<String>, worker: DelegateSettings,
    ) -> Result<String> {
        // The workshop resolves these paths against the workspace registry first.
        let id = format!("thread-{:032x}", rand::random::<u128>());
        let timestamp = now().timestamp_millis();
        let mut thread = self.thread(id.clone(), attached, Some(title), timestamp, timestamp)?;
        thread.front.partner_settings = Some(self.partner_default.clone());
        thread.front.worker_settings = Some(worker);
        thread.front.worker_pinned = true;
        self.insert(thread, Transcript::default(), None, None)?;
        Ok(id)
    }

    pub fn github_edit(
        &mut self, id: &str, edit: impl FnOnce(&mut ThreadGitHub),
    ) -> Result<ThreadGitHub> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        let before = thread.front.github.clone();
        edit(&mut thread.front.github);
        if before != thread.front.github {
            if let Err(error) = thread.save() {
                thread.front.github = before;
                return Err(error);
            }
        }
        Ok(thread.front.github.clone())
    }

    pub fn github_state(&self, id: &str) -> Option<ThreadGitHub> {
        self.threads.get(id).map(|thread| thread.front.github.clone())
    }

    pub(crate) fn github_intake(&self, repo: &str) -> Option<&str> {
        self.threads.iter().find(|(_, thread)| {
            thread.front.github.intake_repo.as_deref() == Some(repo)
        }).map(|(id, _)| id.as_str())
    }

    pub(crate) fn github_set_intake(&mut self, id: &str, repo: &str, from: Option<&str>) -> Result<()> {
        ensure!(self.is_live(id), "Thread is archived or missing");
        if let Some(from) = from {
            self.github_edit(from, |state| state.intake_repo = None)?;
        }
        if let Err(error) = self.github_edit(id, |state| state.intake_repo = Some(repo.into())) {
            if let Some(from) = from {
                self.github_edit(from, |state| state.intake_repo = Some(repo.into()))
                    .with_context(|| format!("Restore the previous intake after: {error:#}"))?;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn github_create_work(
        &mut self, id: &str, title: String, link: &GitHubLink, intake: &str,
    ) -> Result<String> {
        let existing = self.threads.iter()
            .find(|(id, thread)| id.as_str() != intake && self.is_live(id)
                && thread.front.github.intake_repo.is_none() && thread.front.github.links.contains(link))
            .map(|(id, _)| id.clone());
        if let Some(id) = existing {
            self.github_edit(&id, |state| { state.intake_from.get_or_insert_with(|| intake.into()); })?;
            return Ok(id);
        }
        self.create_with_github(id, None, Some(title), ThreadGitHub {
            links: vec![link.clone()], intake_from: Some(intake.into()), ..Default::default()
        })?;
        Ok(id.into())
    }

    pub fn github_threads(&self) -> Vec<(String, ThreadGitHub)> {
        self.threads.iter().map(|(id, thread)| (id.clone(), thread.front.github.clone())).collect()
    }

    pub fn github_responder(&self, id: &str) -> Option<ThreadGitHub> {
        self.threads.get(id).filter(|thread| !thread.front.archived)
            .map(|thread| thread.front.github.clone())
    }

    pub fn github_activity(&self, id: &str, event: &str) -> Option<GitHubActivity> {
        self.threads.get(id)?.transcript.items.iter()
            .flat_map(|entry| &entry.meta.github).find(|activity| activity.id == event).cloned()
    }

    pub fn github_pending(&self, id: &str) -> Vec<GitHubActivity> {
        self.threads.get(id).into_iter().flat_map(|thread| {
            thread.transcript.items.iter().flat_map(|entry| &entry.meta.github)
                .filter(|event| thread.front.github_pending.contains(&event.id))
        }).cloned().collect()
    }

    pub fn github_reports(&self, id: &str) -> Vec<GitHubActivity> {
        self.threads.get(id).into_iter().flat_map(|thread| {
            thread.transcript.items.iter().rev().flat_map(|entry| entry.meta.github.iter().rev())
                .filter(|event| event.author == GitHubAuthor::Agent && event.body.is_some()
                    && thread.front.github.links.contains(&event.link))
        }).take(6).cloned().collect()
    }

    pub(crate) fn github_holds(&self, id: &str) -> Option<crate::self_update::drain::GitHubHold> {
        let thread = self.threads.get(id)?;
        if thread.front.github.links.is_empty() && thread.front.github_pending.is_empty()
            && thread.front.github_responses.is_empty()
        {
            return None;
        }
        Some(crate::self_update::drain::GitHubHold {
            policy: thread.front.github.policy, classes: thread.front.github.class_policies,
            pending: thread.front.github_pending.iter().cloned().collect(),
            responses: thread.front.github_responses.clone(),
        })
    }

    pub(crate) fn protect_github_for_rollback(&mut self, id: &str) -> Result<()> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        let before = thread.front.clone();
        thread.front.github.policy = fuzukue_proto::GitHubPolicy::Notify;
        thread.front.github.class_policies = Some(fuzukue_proto::GitHubPolicies::uniform(fuzukue_proto::GitHubPolicy::Notify));
        thread.front.github_pending.clear();
        thread.front.github_responses.clear();
        if let Err(error) = thread.save() {
            thread.front = before;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn restore_github_after_rollback(&mut self, id: &str, hold: &crate::self_update::drain::GitHubHold) -> Result<()> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        let before = thread.front.clone();
        let notify = fuzukue_proto::GitHubPolicy::Notify;
        if thread.front.github.policy == notify
            && thread.front.github.class_policies == Some(fuzukue_proto::GitHubPolicies::uniform(notify))
        {
            thread.front.github.policy = hold.policy;
            thread.front.github.class_policies = hold.classes;
        }
        for event in &hold.pending {
            if !thread.front.github_pending.contains(event) {
                thread.front.github_pending.push(event.clone());
            }
        }
        let mut responses = hold.responses.clone();
        for response in &thread.front.github_responses {
            if !responses.contains(response) { responses.push(response.clone()); }
        }
        thread.front.github_responses = responses;
        if let Err(error) = thread.save() {
            thread.front = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn github_request(&mut self, id: &str, events: &[String], active: &[GitHubActivity]) -> Result<()> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        ensure!(!thread.front.archived, "Thread is archived");
        let mut batch = Vec::new();
        let mut intake = Vec::<(GitHubLink, Vec<String>)>::new();
        for id in events {
            let event = thread.transcript.items.iter().flat_map(|entry| &entry.meta.github)
                .find(|event| &event.id == id).context("GitHub event not found")?;
            if !batch.contains(id)
                && !thread.front.github_responses.iter().flatten().any(|queued| queued == id)
                && !active.iter().any(|event| &event.id == id)
            {
                if event.intake {
                    if let Some((_, ids)) = intake.iter_mut().find(|(link, _)| link == &event.link) {
                        if !ids.contains(id) { ids.push(id.clone()); }
                    } else {
                        intake.push((event.link.clone(), vec![id.clone()]));
                    }
                } else {
                    batch.push(id.clone());
                }
            }
        }
        if !batch.is_empty() || !intake.is_empty() {
            let before = thread.front.github_responses.len();
            if !batch.is_empty() { thread.front.github_responses.push(batch); }
            thread.front.github_responses.extend(intake.into_iter().map(|(_, ids)| ids));
            if let Err(error) = thread.save() {
                thread.front.github_responses.truncate(before);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn github_response(&self, id: &str) -> Option<Vec<GitHubActivity>> {
        let batch = self.threads.get(id)?.front.github_responses.first()?;
        Some(batch.iter().filter_map(|event| self.github_activity(id, event)).collect())
    }

    pub fn github_consume(&mut self, id: &str, events: &[GitHubActivity], manual: bool) -> Result<()> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        let before = thread.front.github_pending.clone();
        let batch = manual.then(|| thread.front.github_responses.remove(0));
        thread.front.github_pending.retain(|id| !events.iter().any(|event| &event.id == id));
        if let Err(error) = thread.save() {
            thread.front.github_pending = before;
            if let Some(batch) = batch {
                thread.front.github_responses.insert(0, batch);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn is_live(&self, id: &str) -> bool {
        self.threads.get(id).is_some_and(|thread| !thread.front.archived && thread.path.is_file())
    }

    pub(crate) fn has_item(&self, id: &str, item: &str) -> bool {
        self.threads.get(id).is_some_and(|thread| thread.transcript.index.contains_key(item))
    }

    pub(crate) fn watch_note(&mut self, id: &str, item: &str, text: String) -> Result<Option<ServerEvent>> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        if thread.transcript.index.contains_key(item) {
            return Ok(None);
        }
        ensure!(!thread.front.archived && thread.path.is_file(), "Thread is archived or missing");
        let event = ServerEvent::TextDelta { session: id.into(), item: item.into(), actor: Actor::System, text };
        thread.transcript.observe(&event);
        if let Err(error) = thread.save() {
            thread.transcript.items.pop();
            thread.transcript.index.remove(item);
            return Err(error);
        }
        Ok(Some(event))
    }

    pub fn github_deliver(
        &mut self, id: &str, link: &GitHubLink, events: &[GitHubActivity],
        informational: bool,
    ) -> Result<Option<ServerEvent>> {
        let Some(thread) = self.threads.get_mut(id).filter(|thread| {
            thread.front.github.links.contains(link)
                || thread.front.github.intake_repo.as_ref() == Some(&link.repo)
        }) else {
            return Ok(None);
        };
        let seen: std::collections::HashSet<_> = thread.transcript.items.iter()
            .flat_map(|entry| &entry.meta.github).map(|event| event.id.as_str()).collect();
        let events: Vec<_> = events.iter().filter(|event| !seen.contains(event.id.as_str())).cloned().collect();
        if events.is_empty() {
            return Ok(None);
        }
        let item = format!("github:{}", events[0].id);
        let pending = thread.front.github_pending.len();
        let policies = thread.front.github.policies();
        thread.front.github_pending.extend(events.iter()
            .filter(|event| !informational && policies.get(event.author) != GitHubPolicy::Notify)
            .map(|event| event.id.clone()));
        thread.front.github.unread += events.len();
        thread.transcript.push(Entry {
            meta: EntryMeta {
                item: item.clone(), actor: Actor::System, tool: None, turn_ends: Vec::new(), details: None,
                attachments: Vec::new(), application: None, to_worker: false, superseded: false,
                github: events.clone(),
                notice: None,
            },
            text: String::new(),
        });
        if let Err(error) = thread.save() {
            thread.transcript.items.pop();
            thread.transcript.index.remove(&item);
            thread.front.github.unread -= events.len();
            thread.front.github_pending.truncate(pending);
            return Err(error);
        }
        Ok(Some(ServerEvent::GitHubActivity { session: id.into(), item, events }))
    }

    pub fn issue_thread(&mut self, number: u64) -> Result<String> {
        let id = fuzukue_proto::issue_thread_id(number);
        if !self.contains(&id) {
            let timestamp = now().timestamp_millis();
            let thread = self.thread(
                id.clone(),
                Vec::new(),
                Some(format!("#{number}")),
                timestamp,
                timestamp,
            )?;
            self.insert(thread, Transcript::default(), None, None)?;
        }
        Ok(id)
    }

    fn thread(
        &self,
        id: String,
        attached: Vec<String>,
        title: Option<String>,
        created: i64,
        updated: i64,
    ) -> Result<Thread> {
        let dir = project_dir(&self.project, "threads")?;
        let date = DateTime::from_timestamp_millis(created)
            .unwrap_or_else(now)
            .with_timezone(&Local)
            .format("%Y-%m-%d");
        let path = dir.join(format!("{date}-{id}.md"));
        let titled = title.is_some();
        Ok(Thread {
            path,
            front: Front {
                handoff: None,
                github: ThreadGitHub::default(),
                github_pending: Vec::new(),
                github_responses: Vec::new(),
                id,
                title: title.unwrap_or_else(|| "新しいスレッド".into()),
                created,
                updated,
                archived: false,
                pinned: false,
                session_id: None,
                session_start: None,
                migrated_from: None,
                promoted_to: None,
                attached,
                auto_attached: Vec::new(),
                route_exclusions: Vec::new(),
                titled,
                title_auto: Some(false),
                completed_partner_turns: Some(0),
                artifacts: Vec::new(),
                partner_settings: None,
                worker_settings: None,
                worker_pinned: false,
                agent_overrides: Default::default(),
            },
            transcript: Transcript::default(),
            library_revision: 0,
            library_updated: 0,
            active: false,
            suggestions: Vec::new(),
        })
    }

    fn insert(
        &mut self,
        mut thread: Thread,
        transcript: Transcript,
        session_id: Option<String>,
        migrated_from: Option<&str>,
    ) -> Result<()> {
        ensure!(
            !self.contains(&thread.front.id) && !thread.path.exists(),
            "スレッドファイルが既にある: {}",
            thread.path.display()
        );
        thread.transcript = transcript;
        thread.front.session_id = session_id;
        thread.front.migrated_from = migrated_from.map(str::to_owned);
        thread.save()?;
        self.threads.insert(thread.front.id.clone(), thread);
        Ok(())
    }

    pub fn attach(&mut self, id: &str, decks: &[String]) -> Result<Vec<String>> {
        ensure!(!decks.is_empty(), "付けるデッキを指定する");
        let decks = decks
            .iter()
            .map(|deck| workspace::deck_path(&self.project, deck))
            .collect::<Result<Vec<_>>>()?;
        self.edit_attachments(id, |front| {
            let attached = &mut front.attached;
            for deck in decks {
                if !attached.contains(&deck) {
                    attached.push(deck);
                }
            }
        })
    }

    pub fn detach(&mut self, id: &str, deck: &str) -> Result<Vec<String>> {
        // A chip can still be removed if the deck was deleted or moved.
        self.edit_attachments(id, |front| {
            if front.attached.iter().any(|path| path == deck) && !front.route_exclusions.iter().any(|path| path == deck) {
                front.route_exclusions.push(deck.to_owned());
            }
            front.attached.retain(|path| path != deck);
            front.auto_attached.retain(|route| route.deck != deck);
        })
    }

    fn edit_attachments(
        &mut self,
        id: &str,
        edit: impl FnOnce(&mut Front),
    ) -> Result<Vec<String>> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let old = thread.front.clone();
        edit(&mut thread.front);
        if thread.front.attached != old.attached {
            thread.touch();
            if let Err(e) = thread.save() {
                thread.front = old;
                return Err(e);
            }
        }
        Ok(thread.front.attached.clone())
    }

    pub(crate) fn routing_context(&self, id: &str) -> Option<(Vec<String>, Vec<String>)> {
        let thread = self.threads.get(id).filter(|thread| !thread.front.archived)?;
        let mut texts = Vec::new();
        let mut paths = Vec::new();
        for entry in thread.transcript.items.iter().rev().take(48) {
            if entry.meta.turn_ends.iter().any(crate::routing::is_partner) {
                break;
            }
            if let Some(tool) = &entry.meta.tool {
                if tool.ok == Some(true) {
                    paths.extend(tool.artifact_paths(&self.project));
                    if tool.name.eq_ignore_ascii_case("read") {
                        paths.extend(tool.input.as_ref()
                            .and_then(|input| input["file_path"].as_str())
                            .and_then(|path| crate::thread_artifacts::relative_path(&self.project, path)));
                    }
                }
            } else if matches!(entry.meta.actor, Actor::User) || crate::routing::is_partner(&entry.meta.actor) {
                if texts.len() < 8 && !entry.text.is_empty() {
                    texts.push(entry.text.chars().take(3000).collect());
                }
                paths.extend(entry.meta.attachments.iter().cloned());
            }
        }
        Some((texts, paths))
    }

    pub(crate) fn apply_routing(
        &mut self, id: &str, clear: bool, candidates: Vec<fuzukue_proto::DeckRoute>,
    ) -> Result<bool> {
        let Some(thread) = self.threads.get_mut(id).filter(|thread| !thread.front.archived) else {
            return Ok(false);
        };
        let eligible = |route: &fuzukue_proto::DeckRoute| {
            !thread.front.attached.contains(&route.deck)
                && !thread.front.route_exclusions.contains(&route.deck)
                && workspace::deck_path(&self.project, &route.deck).is_ok_and(|path| path == route.deck)
        };
        let candidates: Vec<_> = if clear {
            candidates.into_iter().take(1).filter(eligible).collect()
        } else {
            candidates.into_iter().filter(eligible).take(3).collect()
        };
        let suggestions = if clear {
            if let Some(route) = candidates.into_iter().next() {
                let old = thread.front.clone();
                thread.front.attached.push(route.deck.clone());
                thread.front.auto_attached.push(route);
                if let Err(err) = thread.save() {
                    thread.front = old;
                    return Err(err);
                }
                thread.suggestions.clear();
                return Ok(true);
            }
            Vec::new()
        } else {
            candidates
        };
        if thread.suggestions == suggestions {
            return Ok(false);
        }
        thread.suggestions = suggestions;
        Ok(true)
    }

    pub fn partner_message_items(&self, session: &str, actor: &Actor) -> Vec<String> {
        self.threads.get(session).into_iter().flat_map(|thread| {
            thread.transcript.items.iter().rev()
                .take_while(|entry| !entry.meta.turn_ends.contains(actor))
                .filter(|entry| &entry.meta.actor == actor && entry.meta.tool.is_none() && !entry.meta.superseded)
                .map(|entry| entry.meta.item.clone())
        }).collect()
    }

    pub fn role_settings(&self, id: &str) -> Option<(ChatSettings, DelegateSettings)> {
        let thread = self.threads.get(id)?;
        Some((
            thread.front.partner_settings.clone().unwrap_or_default(),
            thread.front.worker_settings.clone().unwrap_or_else(|| self.worker_default.clone()),
        ))
    }

    pub fn worker_pinned(&self, id: &str) -> bool {
        self.threads.get(id).is_some_and(|thread| thread.front.worker_pinned
            || thread.front.worker_settings.as_ref().is_some_and(|worker| worker != &self.worker_default))
    }

    pub fn agent_override(&self, id: &str, name: &str) -> Option<AgentSelection> {
        self.threads.get(id)?.front.agent_overrides.get(name).cloned()
    }

    pub fn set_agent_override(&mut self, id: &str, name: String, selection: Option<AgentSelection>) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.agent_overrides.clone();
        if let Some(selection) = selection {
            thread.front.agent_overrides.insert(name, selection);
        } else {
            thread.front.agent_overrides.remove(&name);
        }
        if let Err(error) = thread.save() {
            thread.front.agent_overrides = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_roles(&mut self, id: &str, partner: Option<ChatSettings>, worker: Option<DelegateSettings>) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.clone();
        if partner.is_some() {
            thread.front.partner_settings = partner;
        }
        if worker.is_some() {
            thread.front.worker_settings = worker;
            thread.front.worker_pinned = true;
        }
        if let Err(error) = thread.save() {
            thread.front = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn rename(&mut self, id: &str, title: &str) -> Result<()> {
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(!title.is_empty(), "スレッド名を入力する");
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        thread.rename(title, false)
    }

    pub(crate) fn title_summary_input(&self, id: &str) -> Option<(usize, String)> {
        let thread = self.threads.get(id)?;
        let turn = thread.front.completed_partner_turns?;
        if !matches!(turn, 1 | 6) || !thread.title_is_auto() {
            return None;
        }

        // The latest completion is recorded on the last entry.
        let is_partner = crate::routing::is_partner;
        let (last, earlier) = thread.transcript.items.split_last()?;
        let mut endings = last.meta.turn_ends.iter().filter(|actor| is_partner(actor));
        let partner = endings.next()?;
        if endings.next().is_some() {
            return None;
        }
        let reply = std::iter::once(last)
            .chain(earlier.iter().rev().take_while(|entry| !entry.meta.turn_ends.iter().any(is_partner)))
            .find(|entry| &entry.meta.actor == partner && entry.meta.tool.is_none()
                && !entry.meta.item.contains(":summary:") && !entry.text.trim().is_empty())?;
        let prompts = thread.user_prompts()
            .take(if turn == 1 { 1 } else { usize::MAX })
            .map(|prompt| prompt.chars().take(TITLE_MESSAGE_LIMIT).collect::<String>())
            .collect::<Vec<_>>();
        if prompts.is_empty() {
            return None;
        }
        let reply: String = reply.text.chars().take(TITLE_MESSAGE_LIMIT).collect();
        Some((turn, serde_json::json!({ "user_prompts": prompts, "partner_reply": reply }).to_string()))
    }

    pub(crate) fn apply_title_summary(&mut self, id: &str, turn: usize, title: Option<String>) -> Result<()> {
        if let Some(thread) = self.threads.get_mut(id) {
            // Once the sixth turn finishes, only its summary can change the title.
            if thread.title_is_auto() && (turn == 6 || thread.front.completed_partner_turns.is_some_and(|count| count < 6))
            {
                let title = title.or_else(|| thread.user_prompts().next().and_then(prompt_title));
                if let Some(title) = title.filter(|title| *title != thread.front.title) {
                    thread.rename(title, true)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.threads.get(id).context("Thread not found")?.path.clone())
    }

    pub(crate) fn trash(&mut self, app: &crate::Workspace, trash: &crate::trash::Store, id: &str) -> Result<()> {
        let thread = self.threads.get(id).context("Thread not found")?;
        ensure!(!thread.active, "Thread is running");
        if let Some(legacy) = &thread.front.migrated_from {
            // Prevent a retired legacy session from being migrated again on restart.
            let path = self.state_dir.join("removed-thread-migrations.json");
            let mut removed: HashMap<String, String> = std::fs::read(&path).ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
            removed.insert(legacy.clone(), id.into());
            let temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
            std::fs::write(temp.path(), serde_json::to_vec(&removed)?)?;
            temp.persist(path)?;
        }
        trash.file_unrecorded(app, fuzukue_proto::TrashKind::Thread, id.into(), thread.front.title.clone(), &thread.path)?;
        self.threads.remove(id);
        self.restored_generations.remove(id);
        Ok(())
    }

    pub(crate) fn restore(&mut self, path: PathBuf, generation: u64) -> Result<ServerEvent> {
        let mut thread = Thread::read(path)?;
        let id = thread.front.id.clone();
        ensure!(!self.contains(&id), "Thread already exists");
        if let Some(legacy) = &thread.front.migrated_from {
            self.migrations.insert(legacy.clone(), id.clone());
        }
        thread.transcript.hide_saved_blank_replies();
        let events = thread.transcript.events(&id, &thread.transcript.items).collect();
        self.threads.insert(id.clone(), thread);
        self.restored_generations.insert(id.clone(), generation);
        Ok(ServerEvent::ThreadRestored { session: id, events })
    }

    pub(crate) fn rewrite<T>(&mut self, id: &str, item: &str, validate: impl FnOnce(&str, &[String]) -> Result<T>)
        -> Result<(T, Vec<String>)> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        ensure!(!thread.active, "Thread is running");
        let entries = &mut thread.transcript.items;
        let hidden = &thread.transcript.hidden_replies;
        let index = entries.iter().rposition(|entry| entry.meta.actor == Actor::User && !entry.meta.to_worker)
            .context("No previous user message")?;
        let user = &entries[index];
        let prompt = validate(&user.text, &user.meta.attachments)?;
        let replies: Vec<_> = entries.iter_mut().skip(index + 1).filter(|entry| {
            !entry.meta.superseded && entry.meta.tool.is_none() && !entry.text.trim().is_empty()
                && !hidden.contains(&entry.meta.item)
                && crate::routing::is_partner(&entry.meta.actor) && !entry.meta.is_delegated()
        }).collect();
        ensure!(replies.last().is_some_and(|entry| entry.meta.item == item), "Reply is no longer the latest");
        let items: Vec<_> = replies.iter().map(|entry| entry.meta.item.clone()).collect();
        for entry in replies {
            entry.meta.superseded = true;
        }
        if let Err(error) = thread.save() {
            for entry in &mut thread.transcript.items {
                if items.contains(&entry.meta.item) {
                    entry.meta.superseded = false;
                }
            }
            return Err(error);
        }
        thread.library_revision = thread.library_revision.wrapping_add(1);
        Ok((prompt, items))
    }

    pub fn set_archived(&mut self, id: &str, archived: bool) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.archived;
        if previous != archived {
            thread.front.archived = archived;
            if let Err(error) = thread.save() {
                thread.front.archived = previous;
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.pinned;
        if previous != pinned {
            thread.front.pinned = pinned;
            if let Err(error) = thread.save() {
                thread.front.pinned = previous;
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn set_session(&mut self, id: &str, session: Option<(&str, ClaudeSessionStart)>) -> bool {
        let session_id = session.map(|(id, _)| id);
        let start = session.map(|(_, start)| start);
        if let Some(thread) = self.threads.get_mut(id) {
            if thread.front.session_id.as_deref() != session_id || thread.front.session_start != start {
                thread.front.session_id = session_id.map(str::to_owned);
                thread.front.session_start = start;
                if let Err(e) = thread.save() {
                    tracing::warn!("could not save session ID for {id}: {e:#}");
                }
                return true;
            }
        }
        false
    }

    pub fn artifacts(&self, id: &str) -> Vec<String> {
        self.threads.get(id).map(|thread| thread.front.artifacts.clone()).unwrap_or_default()
    }

    pub fn record_artifacts(&mut self, id: &str, paths: Vec<String>) -> Result<()> {
        if let Some(thread) = self.threads.get_mut(id) {
            if thread.add_artifacts(paths) {
                thread.save()?;
            }
        }
        Ok(())
    }

    pub(crate) fn history_cursor(&self, id: &str) -> usize {
        self.threads.get(id).map_or(0, |thread| thread.transcript.items.len())
    }

    pub(crate) fn history_messages(&self, id: &str, cursor: usize) -> (Vec<String>, Option<String>) {
        let mut items = Vec::new();
        let mut summary = None;
        if let Some(thread) = self.threads.get(id) {
            for entry in thread.transcript.items.iter().skip(cursor).filter(|entry| {
                entry.meta.tool.is_none() && crate::routing::is_partner(&entry.meta.actor)
            }) {
                items.push(entry.meta.item.clone());
                if !entry.text.trim().is_empty() {
                    summary = Some(entry.text.clone());
                }
            }
        }
        (items, summary)
    }

    pub fn observe(&mut self, event: &ServerEvent, completed: bool) -> Result<bool> {
        let Some(thread) = history_session(event).and_then(|id| self.threads.get_mut(id)) else {
            return Ok(false);
        };
        if matches!(event, ServerEvent::TurnEnded { actor, .. } if crate::routing::is_partner(actor)) {
            // Older transcripts recorded turn endings without distinguishing failed turns.
            let previous = thread.front.completed_partner_turns.unwrap_or_else(|| {
                thread.transcript.items.iter().flat_map(|entry| &entry.meta.turn_ends)
                    .filter(|actor| crate::routing::is_partner(actor)).count()
            });
            thread.front.completed_partner_turns = Some(previous + usize::from(completed));
        }
        if matches!(event, ServerEvent::UserPrompt { .. })
            || matches!(event, ServerEvent::TextDelta { actor, .. }
                if matches!(actor, Actor::User) || crate::routing::is_partner(actor))
        {
            thread.library_revision = thread.library_revision.wrapping_add(1);
            thread.library_updated = Utc::now().timestamp_millis();
        }
        thread.transcript.observe(event);
        match event {
            ServerEvent::TextDelta { item, actor: Actor::System, .. }
                if item.starts_with(crate::workshop::REPORT_ITEM) =>
            {
                thread.save()?;
                return Ok(false);
            }
            ServerEvent::LocalNotice { .. } | ServerEvent::CommentApplication { .. } => {
                // A live planner failure also flushes its preceding worker diagnostic.
                thread.save()?;
                return Ok(false);
            }
            ServerEvent::ToolResult { item, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                thread.save()?;
                return Ok(false);
            }
            ServerEvent::ToolResult { item, ok: true, .. } => {
                let paths = thread.transcript.index.get(item)
                    .and_then(|&i| thread.transcript.items[i].meta.tool.as_ref())
                    .into_iter().flat_map(|tool| tool.artifact_paths(&self.project)).collect();
                if thread.add_artifacts(paths) {
                    thread.save()?;
                    return Ok(true);
                }
                return Ok(false);
            }
            ServerEvent::TextDelta { item, actor: Actor::User, .. }
                if item.starts_with(fuzukue_proto::slash::ECHO) =>
            {
                thread.save()?;
                return Ok(false);
            }
            ServerEvent::TextDelta {
                actor: Actor::User,
                text,
                ..
            }
            | ServerEvent::UserPrompt { text, to_worker: false, .. } => {
                thread.active = true;
                if !thread.front.titled {
                    if let Some(title) = prompt_title(text) {
                        thread.front.title = title;
                        thread.front.titled = true;
                        thread.front.title_auto = Some(true);
                    }
                }
            }
            ServerEvent::UserPrompt { to_worker: true, .. } => {
                thread.save()?;
                return Ok(false);
            }
            ServerEvent::TurnActivity { running, .. } => {
                thread.active = *running;
                return Ok(false);
            }
            ServerEvent::TurnEnded { actor, .. } => {
                if !matches!(
                    actor,
                    Actor::Codex {
                        parent_tool_use: Some(_),
                        ..
                    }
                ) {
                    thread.active = false;
                }
            }
            ServerEvent::DelegateProgress { .. } => return Ok(false),
            ServerEvent::ToolCall { item, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                return Ok(false);
            }
            ServerEvent::TextDelta { actor: Actor::Codex { parent_tool_use: Some(job), .. }, .. }
            | ServerEvent::ToolCall { actor: Actor::Codex { parent_tool_use: Some(job), .. }, .. }
            | ServerEvent::ApprovalRequested { actor: Actor::Codex { parent_tool_use: Some(job), .. }, .. }
                if job.starts_with(fuzukue_proto::DELEGATE_JOB) => return Ok(false),
            ServerEvent::ApprovalRequested { request_id, .. }
                if request_id.starts_with(fuzukue_proto::DELEGATE_JOB) => return Ok(false),
            ServerEvent::ApprovalRequested { actor: Actor::System, request_id, .. }
                if request_id.starts_with(fuzukue_proto::GIT_APPROVAL) => return Ok(false),
            ServerEvent::TextDelta {
                actor: Actor::Claude | Actor::Codex { .. },
                ..
            }
            | ServerEvent::ToolCall { .. }
            | ServerEvent::ApprovalRequested { .. } => {
                thread.active = true;
                return Ok(false);
            }
            _ => return Ok(false),
        }
        thread.touch();
        thread.save()?;
        Ok(true)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateResearch {
    slug: String,
    title: String,
    summary: String,
    cards: Vec<DraftCard>,
    #[serde(default)]
    from_messages: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftCard {
    id: String,
    kind: DraftKind,
    title: String,
    body: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DraftKind {
    Question,
    Idea,
    Claim,
    Plan,
}

#[derive(Serialize)]
struct ResearchDeck<'a> {
    kind: &'a str,
    title: &'a str,
    summary: &'a str,
    status: &'a str,
}

#[derive(Serialize)]
struct CardFront<'a> {
    id: &'a str,
    kind: &'a DraftKind,
    title: &'a str,
    status: &'a str,
    from_thread: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    from_messages: &'a [String],
}

impl Store {
    pub fn create_research(&mut self, id: &str, args: CreateResearch) -> Result<String> {
        let thread = self
            .threads
            .get_mut(id)
            .context("create_research はスレッドから呼ぶ")?;
        ensure!(slug(&args.slug), "研究の slug が不正");
        ensure!(!args.title.trim().is_empty(), "研究の題を指定する");
        ensure!(!args.cards.is_empty(), "下書きのカードを指定する");
        let mut ids = HashSet::new();
        for card in &args.cards {
            ensure!(slug(&card.id), "カード ID が不正: {}", card.id);
            ensure!(ids.insert(&card.id), "カード ID が重複: {}", card.id);
        }
        let from_thread = format!(
            "threads/{}",
            thread.path.file_name().unwrap().to_string_lossy()
        );
        let deck_path = format!("research/{}", args.slug);
        let research = project_dir(&self.project, "research")?;
        let destination = research.join(&args.slug);
        ensure!(!destination.exists(), "{deck_path} は既にある");
        let stage = tempfile::Builder::new()
            .prefix(".fuzukue-research-")
            .tempdir_in(research)?;
        let deck = ResearchDeck {
            kind: "research",
            title: &args.title,
            summary: &args.summary,
            status: "draft",
        };
        std::fs::write(stage.path().join("deck.toml"), toml::to_string(&deck)?)?;
        let cards = stage.path().join("cards");
        std::fs::create_dir(&cards)?;
        for card in &args.cards {
            let front = CardFront {
                id: &card.id,
                kind: &card.kind,
                title: &card.title,
                status: "draft",
                from_thread: &from_thread,
                from_messages: &args.from_messages,
            };
            std::fs::write(
                cards.join(format!("{}.md", card.id)),
                format!(
                    "---\n{}---\n{}\n",
                    serde_yaml::to_string(&front)?,
                    card.body,
                ),
            )?;
        }
        std::fs::rename(stage.path(), &destination)?;
        let old = thread.front.clone();
        if !thread.front.attached.contains(&deck_path) {
            thread.front.attached.push(deck_path.clone());
        }
        let mut paths = vec![format!("{deck_path}/deck.toml")];
        paths.extend(args.cards.iter().map(|card| format!("{deck_path}/cards/{}.md", card.id)));
        thread.add_artifacts(paths);
        thread.touch();
        if let Err(e) = thread.save() {
            thread.front = old;
            if let Err(rollback) = std::fs::rename(&destination, stage.path()) {
                tracing::error!("could not roll back {deck_path}: {rollback}");
            }
            return Err(e);
        }
        Ok(deck_path)
    }
}

fn prompt_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    // 題は一覧に素の文字で出るので、Markdown の強調・見出し・コードと数式の区切りの記号は落とす。
    let line = line.trim_start_matches(['#', '>', '-', ' ']);
    let line: String = line.chars().filter(|c| !matches!(c, '*' | '`' | '$')).collect();
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut title: String = line.chars().take(48).collect();
    if line.chars().nth(48).is_some() {
        title.push('…');
    }
    Some(title)
}

fn slug(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn project_dir(project: &Path, name: &str) -> Result<PathBuf> {
    match std::fs::create_dir(project.join(name)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let path = crate::safe_join(project, name)?;
    ensure!(path.is_dir(), "{name} はディレクトリではない");
    Ok(path)
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    #[test]
    fn initial_replay_byte_budget_keeps_whole_entries_and_active_workers() {
        let id = "thread-budget";
        let mut transcript = Transcript::default();
        for (item, name) in [("delegate:running", "delegate"), ("large-tool", "Read")] {
            transcript.observe(&ServerEvent::ToolCall {
                session: id.into(), item: item.into(), actor: Actor::Claude,
                name: name.into(), input: serde_json::json!({}),
            });
        }
        transcript.observe(&ServerEvent::ToolResult {
            session: id.into(), item: "large-tool".into(), ok: true, output: String::new(),
        });
        transcript.observe(&ServerEvent::TextDelta {
            session: id.into(), item: "latest".into(), actor: Actor::Claude, text: "latest reply".into(),
        });
        let workers = BTreeSet::from(["delegate:running".to_owned()]);
        let replay = transcript.replay_tail(id, Some(&workers), &|event| match event {
            ServerEvent::ToolResult { session, item, ok, .. } => ServerEvent::ToolResult {
                session, item, ok, output: "x".repeat(INITIAL_HISTORY_BYTES),
            },
            event => event,
        });
        assert!(matches!(replay.as_slice(), [
            ServerEvent::ThreadHistoryStart { before: 2, workers, .. },
            ServerEvent::ToolCall { item: worker, .. },
            ServerEvent::TextDelta { item: latest, .. },
        ] if workers == &["delegate:running"] && worker == "delegate:running" && latest == "latest"));
    }

    #[test]
    fn history_page_cursors_cross_hidden_entries_and_stop_at_the_start() {
        let id = "thread-page";
        let count = INITIAL_HISTORY_ENTRIES * 4 + 1;
        let mut transcript = Transcript::default();
        for row in 0..count {
            let item = row.to_string();
            transcript.observe(&ServerEvent::TextDelta {
                session: id.into(), item: item.clone(), actor: Actor::User, text: "fixture".into(),
            });
            if (count - INITIAL_HISTORY_ENTRIES * 3..count - INITIAL_HISTORY_ENTRIES * 2).contains(&row) {
                transcript.hidden_replies.insert(item);
            }
        }
        let (tail, mut before) = transcript.replay_before(id, count, &|event| event);
        let mut seen = tail.into_iter().rev().collect::<Vec<_>>();
        transcript.observe(&ServerEvent::TextDelta {
            session: id.into(), item: "live".into(), actor: Actor::User, text: "live append".into(),
        });
        let mut empty_page = false;
        while before != 0 {
            let (events, next) = transcript.replay_before(id, before, &|event| event);
            assert!(next < before);
            empty_page |= events.is_empty();
            seen.extend(events.into_iter().rev());
            before = next;
        }
        assert!(empty_page);
        assert!(seen.into_iter().map(|event| match event {
            ServerEvent::TextDelta { item, .. } => item,
            _ => panic!("unexpected history event"),
        }).eq((0..count).rev().map(|row| row.to_string())
            .filter(|item| !transcript.hidden_replies.contains(item))));
    }

    #[test]
    fn pinned_state_round_trips_without_changing_activity_order() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let load = || Store::load(project.path(), state.path(), &BTreeMap::new());
        let mut store = load();
        let id = store.create(None)?;
        let updated = store.threads[&id].front.updated;
        store.set_pinned(&id, true)?;

        let mut store = load();
        let ServerEvent::Threads { threads } = store.snapshot() else { unreachable!() };
        let thread = threads.iter().find(|thread| thread.id == id).unwrap();
        assert!(thread.pinned);
        assert_eq!(thread.updated, updated);
        store.set_pinned(&id, false)?;
        assert!(!load().threads[&id].front.pinned);
        Ok(())
    }

    #[test]
    fn archived_state_round_trips_and_keeps_history_listed() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let load = || Store::load(project.path(), state.path(), &BTreeMap::new());
        let mut store = load();
        let id = store.create(None)?;
        let other = store.create(None)?;
        let path = store.threads[&id].path.clone();
        let prompt = ServerEvent::TextDelta {
            session: id.clone(),
            item: "user:1".into(),
            actor: Actor::User,
            text: "保存した会話".into(),
        };
        store.observe(&prompt, false)?;
        assert!(store.active(&id));
        store.set_archived(&id, true)?;
        assert!(store.active(&id));
        assert!(std::fs::read_to_string(&path)?.contains("\narchived: true\n"));

        let mut store = load();
        let ServerEvent::Threads { threads } = store.snapshot() else {
            unreachable!();
        };
        assert_eq!(threads.len(), 2);
        assert!(threads.iter().any(|thread| thread.id == id && thread.archived));
        assert!(threads.iter().any(|thread| thread.id == other && !thread.archived));
        assert!(store.replay_tails(&BTreeMap::new(), |event| event).any(|event| matches!(
            event,
            ServerEvent::TextDelta { session, text, .. } if session == id && text == "保存した会話"
        )));

        store.set_archived(&id, false)?;
        assert!(!std::fs::read_to_string(&path)?.contains("\narchived:"));
        assert!(!load().threads[&id].front.archived);
        Ok(())
    }
}

#[cfg(test)]
mod prompt_title_tests {
    #[test]
    fn strips_markdown_and_math_delimiters() {
        assert_eq!(
            super::prompt_title("## **Lévy族の集中**として、族$\\LevyFamily$を読む").as_deref(),
            Some("Lévy族の集中として、族\\LevyFamilyを読む")
        );
        assert_eq!(super::prompt_title("$$\n"), None);
    }
}
