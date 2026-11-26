//! プロジェクトと一緒に持ち運べる会話の記録。イベントは Hub と同じ順序で処理する。

use fuzukue_proto::{GitHubActivity, GitHubAuthor, GitHubLink, GitHubPolicy, ThreadGitHub};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::time::{Instant, SystemTime};
use std::sync::Arc;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use fuzukue_proto::{
    Actor, ClaudeSessionStart, ClaudeSessionView, CommentApplication, CommentApplicationState,
    MathMacro, ServerEvent, ThreadView,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{durable, state, workspace};

pub(crate) mod payloads;
pub(crate) mod payload_migration;
mod save_files;
use payloads::{Blobs, Record, Refs, Source};
use save_files::SaveFiles;

use fuzukue_proto::{AgentBackend, AgentSelection, ChatSettings, DelegateJob, DelegateReport, DelegateSettings, JobStatus, JobSummary};

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    preview_payloads: bool,
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
    partner_pinned: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Refs::is_empty")]
    payloads: Refs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovered_input: Option<Box<payloads::JournalPrefixes>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovered_output: Option<Box<payloads::JournalPrefixes>>,
    #[serde(skip)]
    input_source: Option<Source>,
    #[serde(skip)]
    output_source: Option<Source>,
}

impl ToolSummary {
    fn preview_inline(&mut self, record: Record, delegate: bool) {
        if let Some(input) = self.input.as_mut().filter(|_| self.payloads.input.is_none()) {
            if let Some(encoded) = payloads::oversized_input(input) {
                *input = payloads::input_preview(input, &encoded, delegate);
                self.input_source = Some(Source::Inline(record.clone()));
            }
        }
        if let Some(output) = &mut self.output {
            if self.payloads.output.is_some() {
                // Older sidecar-backed records can carry larger previews.
                if !delegate {
                    *output = payloads::output_preview(output, false);
                }
            } else if payloads::needs_output_preview(output) {
                *output = payloads::output_preview(output, delegate);
                self.output_source = Some(Source::Inline(record));
            }
        }
    }

    fn full_input(&self, blobs: &Blobs) -> Result<Option<serde_json::Value>> {
        match (&self.payloads.input, &self.input_source) {
            (Some(digest), _) => Ok(Some(blobs.input(digest)?)),
            (_, Some(source)) => Ok(Some(source.input()?)),
            _ => self.input.clone().map(Some).context("tool input was not saved"),
        }
    }

    fn full_output(&self, blobs: &Blobs) -> Result<Option<String>> {
        match (&self.payloads.output, &self.output_source) {
            (Some(digest), _) => Ok(Some(blobs.output(digest)?)),
            (_, Some(source)) => Ok(Some(source.output()?)),
            _ if self.output.is_none() && self.ok.is_some() => anyhow::bail!("tool output was not saved"),
            _ => Ok(self.output.clone()),
        }
    }

    fn artifact_paths(&self, project: &Path, blobs: &Blobs) -> Vec<String> {
        if self.ok != Some(true) || !crate::thread_artifacts::may_record_path(&self.name) {
            return Vec::new();
        }
        let input = self.full_input(blobs).ok().flatten();
        let output = if crate::thread_artifacts::has_structured_result(&self.name) {
            self.full_output(blobs).ok().flatten()
        } else {
            None
        };
        crate::thread_artifacts::tool_paths(&self.name, input.as_ref(), output.as_deref())
            .into_iter()
            .filter_map(|path| crate::thread_artifacts::relative_path(project, &path))
            .collect()
    }
}

/// A preview and its stable data sources, usable after the Hub lock is released.
pub(crate) struct ToolDetail {
    blobs: Blobs,
    tool: ToolSummary,
}

impl ToolDetail {
    pub(crate) fn input(&self) -> Result<Option<serde_json::Value>> {
        self.tool.full_input(&self.blobs)
    }

    pub(crate) fn output(&self) -> Result<Option<String>> {
        self.tool.full_output(&self.blobs)
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
    job_facts: HashMap<String, JobFacts>,
    job_children: HashMap<String, Vec<usize>>,
    job_roots: Vec<usize>,
}

#[derive(Default)]
struct JobFacts {
    tools: usize,
    failed_tools: usize,
    last_tool: Option<usize>,
    latest_message: Option<usize>,
}

fn summary_preview(text: &str, limit: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

impl JobFacts {
    fn record(&mut self, position: usize, entry: &Entry) {
        if let Some(tool) = &entry.meta.tool {
            self.tools += 1;
            self.failed_tools += usize::from(tool.ok == Some(false));
            self.last_tool = Some(position);
        } else if matches!(entry.meta.actor, Actor::Claude | Actor::Codex { .. }) && !entry.text.trim().is_empty() {
            self.latest_message = self.latest_message.max(Some(position));
        }
    }
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
        let position = self.items.len();
        if let Some(job) = fuzukue_proto::delegate_job_id(&entry.meta.item, &entry.meta.actor) {
            self.job_children.entry(job.into()).or_default().push(position);
            self.job_facts.entry(job.into()).or_default().record(position, &entry);
        } else if entry.meta.item.starts_with(fuzukue_proto::DELEGATE_JOB)
            && entry.meta.tool.as_ref().is_some_and(|tool| tool.name == "delegate")
        {
            self.job_roots.push(position);
        }
        self.index.insert(entry.meta.item.clone(), position);
        self.items.push(entry);
    }

    fn rebuild_job_facts(&mut self) {
        self.job_facts = self.job_children.iter().map(|(job, children)| {
            let mut facts = JobFacts::default();
            for &position in children {
                facts.record(position, &self.items[position]);
            }
            (job.clone(), facts)
        }).collect();
    }

    fn job_summary(&self, job: &str) -> Option<JobSummary> {
        let &position = self.index.get(job)?;
        let root = self.items[position].meta.tool.as_ref().filter(|tool| tool.name == "delegate")?;
        let mut metadata: DelegateJob = serde_json::from_value(root.input.as_ref()?.clone()).ok()?;
        metadata.touches = None;
        let wire_report = root.output.as_deref().and_then(|output| serde_json::from_str::<serde_json::Value>(output).ok());
        let preview_changed = wire_report.as_ref().and_then(|report| report["preview_changed_file_count"].as_u64())
            .and_then(|count| usize::try_from(count).ok());
        let report = wire_report.and_then(|report| serde_json::from_value::<DelegateReport>(report).ok());
        if let Some(attempt) = report.as_ref().and_then(|report| report.attempts.last()) {
            metadata.selection = Some(attempt.selection.clone());
            let reason = metadata.routing.take().map(|route| route.reason).unwrap_or_default();
            metadata.routing = Some(fuzukue_proto::TierDecision { tier: attempt.tier, reason });
        }
        if metadata.selection.is_none() && !metadata.model.is_empty() {
            let backend = if metadata.model.to_ascii_lowercase().starts_with("claude") {
                AgentBackend::Claude
            } else {
                AgentBackend::for_model(Some(&metadata.model))
            };
            metadata.selection = Some(AgentSelection { backend, model: Some(metadata.model.clone()), effort: None });
        }
        if let Some(model) = metadata.selection.as_ref().and_then(|selection| selection.model.as_ref()) {
            metadata.model.clone_from(model);
        }
        let interrupted = report.as_ref().is_some_and(|report| report.interrupted);
        let status = match root.ok {
            Some(_) if interrupted => JobStatus::Interrupted,
            Some(true) => JobStatus::Succeeded,
            Some(false) => JobStatus::Failed,
            None if metadata.started_at_ms.is_some() => JobStatus::Running,
            None => JobStatus::Queued,
        };
        let facts = self.job_facts.get(job);
        let saved = report.as_ref().and_then(|report| report.tool_activity.as_ref());
        let recorded_tools = facts.map_or(0, |facts| facts.tools);
        let recent_tool = facts.and_then(|facts| facts.last_tool)
            .and_then(|position| self.items[position].meta.tool.as_ref())
            .map(|tool| fuzukue_view::chat::tool_summary(&tool.name, tool.input.as_ref().unwrap_or(&serde_json::Value::Null)));
        let saved_tool = saved.and_then(|activity| activity.last_tool.as_deref()).map(|tool| summary_preview(tool, 160));
        let last_tool = if saved.is_some_and(|activity| activity.count > recorded_tools) {
            saved_tool.or(recent_tool)
        } else {
            recent_tool.or(saved_tool)
        };
        let latest_message = facts.and_then(|facts| facts.latest_message)
            .map(|position| summary_preview(&self.items[position].text, 640));
        Some(JobSummary {
            id: job.into(), position, job: metadata, status,
            elapsed_ms: report.as_ref().filter(|report| !report.interrupted).map(|report| report.elapsed_ms),
            tools: recorded_tools.max(saved.map_or(0, |activity| activity.count)),
            had_tool_error: facts.is_some_and(|facts| facts.failed_tools > 0),
            last_activity: root.progress.as_deref().filter(|text| !text.trim().is_empty())
                .map(|text| summary_preview(text, 280))
                .or_else(|| last_tool.clone())
                .or_else(|| latest_message.as_deref().and_then(|text| text.lines().next())
                    .map(|line| summary_preview(line, 280))),
            last_tool, latest_message,
            changed_file_count: preview_changed.or_else(|| {
                report.as_ref().and_then(|report| report.files.as_ref()?.changed_files.as_ref()).map(Vec::len)
            }),
        })
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
                    let entry = &mut self.items[i];
                    entry.text.push_str(text);
                    if let Some(job) = fuzukue_proto::delegate_job_id(item, actor).filter(|_| entry.meta.tool.is_none()) {
                        self.job_facts.entry(job.into()).or_default().record(i, entry);
                    }
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
                            payloads: Refs::default(), recovered_input: None, recovered_output: None,
                            input_source: None, output_source: None,
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
                        tool.payloads.output = None;
                        tool.output_source = None;
                        tool.recovered_output = None;
                    }
                    let mut input = input.clone();
                    if let Some(previous) = &tool.input {
                        preserve_delegate_metadata(previous, &mut input);
                    }
                    tool.input = Some(input);
                }
            }
            ServerEvent::ToolCall { item, actor, input, .. }
                if fuzukue_proto::delegate_job_id(item, actor).is_some() =>
            {
                if let Some(tool) = self.index.get(item).and_then(|&i| self.items[i].meta.tool.as_mut()) {
                    tool.input = Some(input.clone());
                }
            }
            ServerEvent::DelegateRenamed { job, change, .. } => {
                if let Some(input) = self.delegate_input_mut(job) {
                    record_delegate_name(input, change);
                }
            }
            ServerEvent::DelegateDismissed { job, .. } => {
                if let Some(input) = self.delegate_input_mut(job) {
                    input["dismissed_from_panel"] = true.into();
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
                    let Some(tool) = &mut meta.tool else { return };
                    if let Some(job) = fuzukue_proto::delegate_job_id(item, &meta.actor) {
                        if let Some(facts) = self.job_facts.get_mut(job) {
                            facts.failed_tools = facts.failed_tools.saturating_sub(usize::from(tool.ok == Some(false)))
                                + usize::from(!ok);
                        }
                    }
                    tool.ok = Some(*ok);
                    tool.output = Some(output.clone());
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

    fn capture_sources(&mut self, event: &ServerEvent, refs: Refs, source: Option<Source>, force: bool) {
        let item = match event {
            ServerEvent::ToolCall { item, .. } | ServerEvent::ToolResult { item, .. } => item,
            _ => return,
        };
        let Some(&index) = self.index.get(item) else { return };
        let entry = &mut self.items[index];
        let delegated_call = entry.meta.is_delegated();
        let Some(tool) = &mut entry.meta.tool else { return };
        let recovered = match &source {
            Some(Source::Memory(record)) => Some(Box::new(record.journal_prefixes)),
            _ => None,
        };
        match event {
            ServerEvent::ToolCall { input, .. }
                if (force || delegated_call || tool.input.as_ref() == Some(input))
                    && (!force || tool.payloads.input.is_none()) =>
            {
                tool.payloads.input = refs.input;
                tool.recovered_input = recovered;
                tool.input_source = source;
            }
            ServerEvent::ToolResult { .. } if !force || tool.payloads.output.is_none() => {
                tool.payloads.output = refs.output;
                tool.recovered_output = recovered;
                tool.output_source = source;
            }
            _ => {}
        }
    }

    // A checkpoint can include a result whose journal append failed. Older
    // records for that item must not overwrite it when replaying the journal.
    fn supersedes_journal_payload(&self, event: &ServerEvent, record: &Record) -> bool {
        let (item, input) = match event {
            ServerEvent::ToolCall { item, .. } => (item, true),
            ServerEvent::ToolResult { item, .. } => (item, false),
            _ => return false,
        };
        let Some(tool) = self.index.get(item).and_then(|&position| self.items[position].meta.tool.as_ref()) else {
            return false;
        };
        let prefixes = if input { &tool.recovered_input } else { &tool.recovered_output };
        prefixes.as_ref().is_some_and(|prefixes| record.in_prefixes(prefixes))
    }

    fn restore_tool(&mut self, event: &ServerEvent) {
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
            ServerEvent::ToolCall { input, .. } if tool.payloads.input.is_none() => tool.input = Some(input.clone()),
            ServerEvent::ToolResult { ok, output, .. } if tool.payloads.output.is_none() => {
                tool.ok = Some(*ok);
                tool.output = Some(output.clone());
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

    fn delegate_input_mut(&mut self, job: &str) -> Option<&mut serde_json::Value> {
        let &index = self.index.get(job)?;
        let tool = self.items[index].meta.tool.as_mut().filter(|tool| tool.name == "delegate")?;
        tool.input.as_mut().filter(|input| input.is_object())
    }

    fn merge(&mut self, recovered: Self) {
        for entry in recovered.items {
            if let Some(&i) = self.index.get(&entry.meta.item) {
                let saved = &mut self.items[i];
                if entry.text.starts_with(&saved.text) {
                    saved.text = entry.text;
                }
                if let Some(mut incoming) = entry.meta.tool {
                    if let Some(tool) = &mut saved.meta.tool {
                        if incoming.ok.is_some() || tool.ok.is_none() {
                            if let (Some(previous), Some(input)) = (&tool.input, &mut incoming.input) {
                                preserve_delegate_metadata(previous, input);
                            }
                            incoming.progress = incoming.progress.or_else(|| tool.progress.take());
                            // A legacy journal has no sidecar references and may
                            // replay the same call or result over a saved checkpoint.
                            if incoming.input.is_none() || incoming.payloads.input.is_none()
                                && tool.payloads.input.is_some() && tool.recovered_input.is_none()
                            {
                                let mut input = tool.input.take();
                                if let (Some(replayed), Some(saved)) = (&incoming.input, &mut input) {
                                    preserve_delegate_metadata(replayed, saved);
                                }
                                incoming.input = input;
                                incoming.payloads.input = tool.payloads.input.take();
                                incoming.input_source = tool.input_source.take();
                                incoming.recovered_input = tool.recovered_input.take();
                            }
                            if incoming.payloads.output.is_none() && tool.payloads.output.is_some()
                                && tool.recovered_output.is_none()
                            {
                                incoming.ok = tool.ok;
                                incoming.output = tool.output.take();
                                incoming.payloads.output = tool.payloads.output.take();
                                incoming.output_source = tool.output_source.take();
                                incoming.recovered_output = tool.recovered_output.take();
                            }
                            *tool = incoming;
                        } else if incoming.progress.is_some() {
                            tool.progress = incoming.progress;
                        }
                    }
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
            let mut meta = std::borrow::Cow::Borrowed(&entry.meta);
            if let Some(tool) = entry.meta.tool.as_ref() {
                if !tool.payloads.is_empty() {
                    tool.payloads.write_marker(&mut meta.to_mut().attachments);
                }
                // Without a sidecar, write through the full source so this
                // checkpoint can be opened without its local journal.
                if let Some(source) = tool.input_source.as_ref().filter(|_| tool.payloads.input.is_none()) {
                    meta.to_mut().tool.as_mut().unwrap().input = Some(source.input()?);
                }
                if let Some(source) = tool.output_source.as_ref().filter(|_| tool.payloads.output.is_none()) {
                    meta.to_mut().tool.as_mut().unwrap().output = Some(source.output()?);
                }
            }
            let meta = serde_json::to_string(&meta)?;
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

    fn read_buffered(input: &mut BufReader<&File>, file: &Arc<File>, mut offset: u64) -> Result<Self> {
        let mut transcript = Self::default();
        let mut line = String::new();
        let mut entry = String::new();
        let mut header_record = None;
        loop {
            let len = input.read_line(&mut line)?;
            if len == 0 { break; }
            let line_offset = offset;
            offset += len as u64;
            ensure!(!line.as_bytes().contains(&0), "NUL bytes in transcript");
            if let Some(header) = line.strip_prefix(ENTRY) {
                if entry.is_empty() || entry.ends_with("\n\n") {
                    if !entry.is_empty() {
                        transcript.push(Self::decode_buffered_entry(&entry[..entry.len() - 2], header_record.take().unwrap())?);
                    }
                    entry.clear();
                    header_record = Some(Record { offset: line_offset, file: file.clone(), len });
                    entry.push_str(header);
                    line.clear();
                    continue;
                }
            }
            ensure!(!entry.is_empty(), "missing transcript entry");
            entry.push_str(&line);
            line.clear();
        }
        if !entry.is_empty() {
            let part = entry.strip_suffix("\n\n").context("missing transcript separator")?;
            transcript.push(Self::decode_buffered_entry(part, header_record.unwrap())?);
        }
        Ok(transcript)
    }

    fn decode_buffered_entry(part: &str, record: Record) -> Result<Entry> {
        let mut entry = Self::decode_entry(part)?;
        if let Some(tool) = &mut entry.meta.tool {
            tool.preview_inline(record, tool.name == "delegate");
        }
        Ok(entry)
    }

    fn decode_entry(part: &str) -> Result<Entry> {
        let (header, content) = part.split_once(" -->\n").context("broken transcript header")?;
        let mut meta: EntryMeta = serde_json::from_str(header)?;
        if let Some(tool) = &mut meta.tool {
            tool.payloads.restore_marker(&mut meta.attachments);
        }
        let mut text = String::new();
        if meta.tool.is_none() {
            let (_, content) = content.split_once("\n\n").context("missing transcript heading")?;
            let content = content.strip_suffix(&attachment_list(&meta.attachments))
                .context("missing transcript attachment list")?;
            text.reserve_exact(content.len());
            for line in content.split_inclusive('\n') {
                if line.starts_with('\\') && line.trim_start_matches('\\').starts_with(ENTRY) {
                    text.push_str(&line[1..]);
                } else {
                    text.push_str(line);
                }
            }
        }
        Ok(Entry { meta, text })
    }

    fn replay_tail(
        &self,
        id: &str,
        workers: Option<&BTreeSet<String>>,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> Vec<ServerEvent> {
        let (events, workers, before) = self.tail(id, workers, false, restore_tool);
        before.map(|before| ServerEvent::ThreadHistoryStart { session: id.into(), before, workers })
            .into_iter().chain(events).collect()
    }

    fn tail(
        &self,
        id: &str,
        workers: Option<&BTreeSet<String>>,
        compact: bool,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> (Vec<ServerEvent>, Vec<String>, Option<u64>) {
        let (tail, first) = self.replay_before(id, self.items.len(), compact, restore_tool);
        // Include workers whose rows precede the contiguous tail.
        let active_entries: BTreeSet<_> = workers.into_iter().flatten()
            .filter_map(|job| self.index.get(job).copied())
            .filter(|&index| index < first)
            .collect();
        let workers = active_entries.iter().map(|&index| self.items[index].meta.item.clone()).collect();
        let events = active_entries.into_iter()
            .flat_map(|index| self.page_events(id, &self.items[index], compact, restore_tool))
            .chain(tail)
            .collect();
        (events, workers, (first != 0).then_some(first as u64))
    }

    fn replay_before(
        &self,
        id: &str,
        before: usize,
        compact: bool,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> (Vec<ServerEvent>, usize) {
        // Entry ordinals are stable as new entries append. The loaded
        // transcript gives direct access to the requested range.
        let mut size = ReplaySize(0);
        let mut page = Vec::new();
        let mut first = before;
        let mut entries = 0;
        for index in (0..before).rev() {
            let entry = &self.items[index];
            let events = self.page_events(id, entry, compact, restore_tool);
            if !compact || !events.is_empty() {
                for event in &events {
                    serde_json::to_writer(&mut size, event).expect("serializable history event");
                }
                if entries >= INITIAL_HISTORY_ENTRIES || (!page.is_empty() && size.0 > INITIAL_HISTORY_BYTES) {
                    break;
                }
                entries += 1;
            }
            first = index;
            page.extend(events.into_iter().rev());
        }
        if compact && before == self.items.len() {
            // Jobs can occupy every slot in even a compact tail. Include the
            // latest user/assistant exchange as sparse rows before that tail.
            let present = |event: &ServerEvent| matches!(event,
                ServerEvent::UserPrompt { to_worker: false, .. }
                | ServerEvent::TextDelta { actor: Actor::User | Actor::Claude, .. }
                | ServerEvent::TextDelta { actor: Actor::Codex { parent_tool_use: None, .. }, .. }
            );
            let mut needs_user = !page.iter().any(|event| matches!(event,
                ServerEvent::UserPrompt { to_worker: false, .. } | ServerEvent::TextDelta { actor: Actor::User, .. }));
            let mut needs_partner = !page.iter().any(|event| matches!(event,
                ServerEvent::TextDelta { actor: Actor::Claude | Actor::Codex { parent_tool_use: None, .. }, .. }));
            for entry in self.items[..first].iter().rev().filter(|entry| entry.meta.tool.is_none()) {
                if !needs_user && !needs_partner { break; }
                if !(entry.meta.actor == Actor::User && needs_user || crate::routing::is_partner(&entry.meta.actor) && needs_partner) {
                    continue;
                }
                let events = self.page_events(id, entry, true, restore_tool);
                if events.iter().any(present) {
                    needs_user &= entry.meta.actor != Actor::User;
                    needs_partner &= !crate::routing::is_partner(&entry.meta.actor);
                    page.extend(events.into_iter().rev());
                }
            }
        }
        page.reverse();
        (page, first)
    }

    fn replay_from(&self, id: &str, start: usize, compact: bool) -> (Vec<ServerEvent>, Option<u64>) {
        let mut size = ReplaySize(0);
        let mut page = Vec::new();
        let mut end = start;
        let mut entries = 0;
        for entry in &self.items[start..] {
            let events = self.page_events(id, entry, compact, &bound_history_tool);
            if !compact || !events.is_empty() {
                if entries >= INITIAL_HISTORY_ENTRIES { break; }
                entries += 1;
            }
            for event in &events {
                serde_json::to_writer(&mut size, event).expect("serializable history event");
            }
            if size.0 > INITIAL_HISTORY_BYTES && !page.is_empty() {
                break;
            }
            end += 1;
            page.extend(events);
            if size.0 >= INITIAL_HISTORY_BYTES {
                break;
            }
        }
        (page, (end < self.items.len()).then_some(end as u64))
    }

    fn page_events(
        &self, id: &str, entry: &Entry, compact: bool,
        restore_tool: &impl Fn(ServerEvent) -> ServerEvent,
    ) -> Vec<ServerEvent> {
        if compact && fuzukue_proto::delegate_job_id(&entry.meta.item, &entry.meta.actor).is_some() {
            return Vec::new();
        }
        self.events(id, std::slice::from_ref(entry)).filter_map(|event| {
            let event = restore_tool(event);
            if compact {
                compact_job_event(event, |job| self.job_summary(job))
            } else {
                Some(event)
            }
        }).collect()
    }

    fn job_details(&self, id: &str, job: &str, before: Option<u64>) -> (Vec<ServerEvent>, Option<u64>) {
        let Some(&root) = self.index.get(job) else { return (Vec::new(), None) };
        let mut events: Vec<_> = if before.is_none() {
            self.events(id, &self.items[root..=root]).collect()
        } else {
            Vec::new()
        };
        let mut size = ReplaySize(0);
        for event in &events {
            serde_json::to_writer(&mut size, event).expect("serializable job event");
        }
        let mut children = Vec::new();
        let indices: &[usize] = self.job_children.get(job).map_or(&[], Vec::as_slice);
        let end = indices.partition_point(|&index| before.is_none_or(|before| (index as u64) < before));
        let mut first = end;
        for index in (end.saturating_sub(INITIAL_HISTORY_ENTRIES)..end).rev() {
            let entry = &self.items[indices[index]];
            let entry_events: Vec<_> = self.events(id, std::slice::from_ref(entry)).collect();
            for event in &entry_events {
                serde_json::to_writer(&mut size, event).expect("serializable job event");
            }
            if size.0 > INITIAL_HISTORY_BYTES && !children.is_empty() {
                break;
            }
            first = index;
            children.extend(entry_events.into_iter().rev());
            if size.0 >= INITIAL_HISTORY_BYTES { break; }
        }
        events.extend(children.into_iter().rev());
        let next_before = (first > 0 && first < end).then(|| indices[first] as u64);
        (events, next_before)
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
    pub stamp: LibraryStamp,
    pub messages: Option<Vec<LibraryMessage>>,
}

pub(crate) struct LibraryMessage {
    pub index: usize,
    pub item: String,
    pub text: String,
}

// Clean transcripts can be discarded. Pending mutations keep their transcript
// until the portable file has been successfully replaced.
struct TranscriptSlot {
    source: Option<TranscriptSource>,
    loaded: OnceCell<LoadedTranscript>,
    dirty: Cell<bool>,
    volatile_payloads: bool,
    accessed: Cell<Instant>,
}

#[derive(Clone)]
struct TranscriptSource {
    path: PathBuf,
    session: String,
    legacy: Option<String>,
    after: u64,
    timeline: state::Timeline,
}

struct LoadedTranscript {
    value: Transcript,
    error: Option<String>,
    weight: Cell<Option<usize>>,
}

impl TranscriptSlot {
    fn new(value: Transcript) -> Self {
        Self {
            source: None,
            loaded: OnceCell::from(LoadedTranscript { value, error: None, weight: Cell::new(None) }),
            dirty: Cell::new(false),
            volatile_payloads: false,
            accessed: Cell::new(Instant::now()),
        }
    }

    fn lazy(source: TranscriptSource) -> Self {
        Self {
            source: Some(source),
            loaded: OnceCell::new(),
            dirty: Cell::new(false),
            volatile_payloads: false,
            accessed: Cell::new(Instant::now()),
        }
    }

    fn get(&self) -> &LoadedTranscript {
        self.accessed.set(Instant::now());
        self.loaded.get_or_init(|| {
            let source = self.source.as_ref().expect("unloaded transcript has a source");
            match source.read() {
                Ok(value) => LoadedTranscript { value, weight: Cell::new(None), error: None },
                Err(error) => {
                    tracing::warn!(path=?source.path, %error, "could not open transcript");
                    LoadedTranscript { value: Transcript::default(), weight: Cell::new(None), error: Some(format!("{error:#}")) }
                }
            }
        })
    }

    fn check(&self) -> Result<()> {
        if let Some(error) = &self.get().error {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn cache_weight(&self) -> Option<usize> {
        let data = self.loaded.get()?;
        (self.source.is_some() && !self.dirty.get()).then(|| data.preview_weight())
    }
}

impl LoadedTranscript {
    fn preview_weight(&self) -> usize {
        fn json_heap(value: &serde_json::Value) -> usize {
            match value {
                serde_json::Value::String(text) => text.capacity(),
                serde_json::Value::Array(items) => items.capacity() * std::mem::size_of::<serde_json::Value>()
                    + items.iter().map(json_heap).sum::<usize>(),
                serde_json::Value::Object(fields) => fields.iter().map(|(key, value)| {
                    64 + key.capacity() + json_heap(value)
                }).sum(),
                _ => 0,
            }
        }

        if let Some(weight) = self.weight.get() {
            return weight;
        }
        let weight = self.value.items.iter().map(|entry| {
            std::mem::size_of::<Entry>() + 256 + entry.text.capacity()
                + entry.meta.tool.as_ref().map_or(0, |tool| {
                    tool.input.as_ref().map_or(0, json_heap)
                        + tool.output.as_ref().map_or(0, String::capacity)
                        + tool.progress.as_ref().map_or(0, String::capacity)
                        + tool.payloads.input.as_ref().map_or(0, String::capacity)
                        + tool.payloads.output.as_ref().map_or(0, String::capacity)
                })
        }).sum();
        if !self.value.items.is_empty() {
            self.weight.set(Some(weight));
        }
        weight
    }
}

impl Deref for TranscriptSlot {
    type Target = Transcript;

    fn deref(&self) -> &Transcript {
        &self.get().value
    }
}

impl DerefMut for TranscriptSlot {
    fn deref_mut(&mut self) -> &mut Transcript {
        self.get();
        self.dirty.set(true);
        let loaded = self.loaded.get_mut().unwrap();
        loaded.weight.set(None);
        &mut loaded.value
    }
}

impl TranscriptSource {
    fn read(&self) -> Result<Transcript> {
        let file = Arc::new(File::open(&self.path)?);
        let mut input = BufReader::new(file.as_ref());
        let (_, offset) = read_front(&mut input)?;
        let mut transcript = Transcript::read_buffered(&mut input, &file, offset)?;
        let mut recovered = Transcript::default();
        self.timeline.visit_session(&self.session, |mut env, refs, record| {
            if env.seq > self.after && !transcript.supersedes_journal_payload(&env.event, &record) {
                // A journal tail may begin with an update to a checkpointed tool or job.
                let existing = match &env.event {
                    ServerEvent::ToolResult { item, .. } => Some(item),
                    ServerEvent::DelegateProgress { job, .. } | ServerEvent::DelegateRenamed { job, .. }
                    | ServerEvent::DelegateDismissed { job, .. } => Some(job),
                    _ => None,
                };
                if let Some(item) = existing.filter(|item| !recovered.index.contains_key(*item)) {
                    if let Some(&position) = transcript.index.get(item) {
                        recovered.push(transcript.items[position].clone());
                    }
                }
                let source = (refs.is_empty() && payloads::bound_replay(&mut env.event))
                    .then_some(Source::Journal(record));
                recovered.observe(&env.event);
                recovered.capture_sources(&env.event, refs, source, false);
            }
        });
        transcript.merge(recovered);
        if let Some(legacy) = &self.legacy {
            self.timeline.visit_session(legacy, |mut env, refs, record| {
                if env.seq > self.after && !transcript.supersedes_journal_payload(&env.event, &record) {
                    let source = (refs.is_empty() && payloads::bound_replay(&mut env.event))
                        .then_some(Source::Journal(record));
                    transcript.restore_tool(&env.event);
                    transcript.capture_sources(&env.event, refs, source, true);
                }
            });
        }
        transcript.rebuild_job_facts();
        transcript.hide_saved_blank_replies();
        Ok(transcript)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileVersion {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl FileVersion {
    fn read(metadata: Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
        })
    }
}

struct Thread {
    path: PathBuf,
    file_version: RefCell<Option<FileVersion>>,
    front: Front,
    transcript: TranscriptSlot,
    page_revision: u64,
    library_revision: u64,
    library_updated: i64,
    active: bool,
    suggestions: Vec<fuzukue_proto::DeckRoute>,
}

fn read_front(input: &mut impl BufRead) -> Result<(Front, u64)> {
    let mut text = String::new();
    input.read_line(&mut text)?;
    ensure!(matches!(text.as_str(), "---\n" | "---\r\n"), "missing thread front matter");
    loop {
        let start = text.len();
        ensure!(input.read_line(&mut text)? != 0, "unterminated thread front matter");
        if matches!(&text[start..], "---" | "---\n" | "---\r\n") {
            break;
        }
    }
    ensure!(!text.as_bytes().contains(&0), "NUL bytes in thread front matter");
    let (front, _) = fuzukue_cards::split_frontmatter(&text).context("missing thread front matter")?;
    Ok((serde_yaml::from_str(front)?, text.len() as u64))
}

impl Thread {
    fn blobs(&self) -> Blobs {
        Blobs::new(self.path.parent().expect("thread directory"))
    }

    fn read(path: PathBuf) -> Result<Self> {
        let file = Arc::new(File::open(&path)?);
        let mut input = BufReader::new(file.as_ref());
        let (front, offset) = read_front(&mut input)?;
        Ok(Self {
            path,
            file_version: RefCell::new(Some(FileVersion::read(file.metadata()?)?)),
            front,
            transcript: TranscriptSlot::new(Transcript::read_buffered(&mut input, &file, offset)?),
            page_revision: 0,
            library_revision: 0,
            library_updated: 0,
            active: false,
            suggestions: Vec::new(),
        })
    }

    fn read_lazy(path: PathBuf, timeline: &state::Timeline) -> Result<Self> {
        let mut input = BufReader::new(File::open(&path)?);
        let file_version = FileVersion::read(input.get_ref().metadata()?)?;
        let (front, offset) = read_front(&mut input)?;
        if file_version.length > offset {
            let mut last = [0; 2];
            input.seek(SeekFrom::End(-2))?;
            input.read_exact(&mut last)?;
            ensure!(&last == b"\n\n", "missing final transcript separator");
        }
        let transcript = TranscriptSlot::lazy(TranscriptSource {
            path: path.clone(), session: front.id.clone(), legacy: front.migrated_from.clone(),
            after: 0, timeline: timeline.clone(),
        });
        Ok(Self {
            path, file_version: RefCell::new(Some(file_version)), front, transcript,
            page_revision: 0, library_revision: 0, library_updated: 0, active: false,
            suggestions: Vec::new(),
        })
    }

    fn save(&mut self) -> Result<()> {
        self.save_with(SaveFiles::open)
    }

    fn save_with(&mut self, open: impl FnOnce(&Path) -> Result<SaveFiles>) -> Result<()> {
        self.transcript.check().with_context(|| format!("Thread file {}", self.path.display()))?;
        let dir = self.path.parent().context("missing thread directory")?;
        let files = open(dir)?;
        if self.front.preview_payloads {
            let blobs = self.blobs();
            let loaded = self.transcript.loaded.get_mut().unwrap();
            loaded.weight.set(None);
            // A converted checkpoint must keep full journal and memory payloads
            // in sidecars, even when they are newer than the staged Markdown.
            for tool in loaded.value.items.iter_mut().filter_map(|entry| entry.meta.tool.as_mut()) {
                if let Some(source) = &tool.input_source {
                    match blobs.put_locked(&serde_json::to_vec(&source.input()?)?) {
                        Ok(digest) => {
                            tool.payloads.input = Some(digest);
                            tool.input_source = None;
                        }
                        Err(error) if matches!(source, Source::Journal(_)) => return Err(error),
                        Err(_) => {} // The portable Markdown can still hold the full fallback.
                    }
                }
                if let Some(source) = &tool.output_source {
                    match blobs.put_locked(source.output()?.as_bytes()) {
                        Ok(digest) => {
                            tool.payloads.output = Some(digest);
                            tool.output_source = None;
                        }
                        Err(error) if matches!(source, Source::Journal(_)) => return Err(error),
                        Err(_) => {}
                    }
                }
            }
        }
        let file = files.replace(
            &self.path,
            |output| {
                write!(output, "---\n{}---\n", serde_yaml::to_string(&self.front)?)?;
                self.transcript.write_markdown(output)
            },
            || {
                match File::open(&self.path) {
                    Ok(mut file) => {
                        let version = FileVersion::read(file.metadata()?)?;
                        ensure!(self.file_version.borrow().as_ref() == Some(&version),
                            "Thread file {} changed on disk", self.path.display());
                        files.preserve_previous(&mut file, &self.path)?;
                        ensure!(FileVersion::read(fs::symlink_metadata(&self.path)?)? == version,
                            "Thread file {} changed while saving", self.path.display());
                        Ok(())
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                        && self.file_version.borrow().is_none() => Ok(()),
                    Err(error) => Err(error).with_context(|| format!("Thread file {}", self.path.display())),
                }
            },
        )?;
        // Even when directory sync fails, the live path now names this file.
        // Keep the transcript dirty and let the next retry recognize our inode.
        self.file_version.replace(Some(FileVersion::read(file.metadata()?)?));
        files.sync_replacement(&self.path)?;
        self.transcript.dirty.set(false);
        self.transcript.volatile_payloads = false;
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

fn preserve_delegate_metadata(previous: &serde_json::Value, input: &mut serde_json::Value) {
    let Some(object) = input.as_object_mut() else { return };
    if previous["dismissed_from_panel"].as_bool() == Some(true) {
        object.insert("dismissed_from_panel".into(), true.into());
    }
    let Some(previous_history) = previous["rename_history"].as_array() else { return };
    let mut history = previous_history.clone();
    let last = previous_history.last().and_then(|change| change["at_ms"].as_i64());
    if let Some(current) = object.get("rename_history").and_then(serde_json::Value::as_array) {
        history.extend(current.iter().filter(|change| {
            change["at_ms"].as_i64() >= last && !previous_history.contains(change)
        }).cloned());
    }
    if history.len() == previous_history.len() {
        object.insert("label".into(), previous["label"].clone());
        object.insert("label_source".into(), previous["label_source"].clone());
    }
    object.insert("rename_history".into(), history.into());
}

fn record_delegate_name(input: &mut serde_json::Value, change: &fuzukue_proto::DelegateNameChange) {
    let change_value = serde_json::json!(change);
    if input["rename_history"].as_array().and_then(|history| history.last()).is_some_and(|last| {
        last == &change_value || last["at_ms"].as_i64().is_some_and(|at| at > change.at_ms)
    }) {
        return;
    }
    let mut history = input["rename_history"].as_array().cloned().unwrap_or_default();
    history.push(change_value);
    input["rename_history"] = history.into();
    input["label"] = serde_json::json!(change.label);
    input["label_source"] = serde_json::json!(change.label_source);
}

/// Use the ordinary job row events with a small summary payload. Child events
/// remain in the journal and are sent only to a connection that opens the job.
pub(crate) fn compact_job_event(
    mut event: ServerEvent, summary: impl FnOnce(&str) -> Option<JobSummary>,
) -> Option<ServerEvent> {
    if let Some((session, job)) = event.worker_child() {
        return match event {
            ServerEvent::ToolCall { .. } => summary(job).map(|summary| ServerEvent::ThreadJobActivity {
                session: session.into(), job: job.into(), tools: summary.tools,
            }),
            _ => None,
        };
    }
    match &mut event {
        ServerEvent::ToolCall { item, name, input, .. }
            if item.starts_with(fuzukue_proto::DELEGATE_JOB) && name == "delegate" =>
        {
            if let Some(fields) = input.as_object_mut() {
                fields.retain(|key, _| matches!(key.as_str(),
                    "agent" | "title" | "label" | "label_source" | "card_id" | "queued_at_ms"
                    | "model" | "started_at_ms" | "dismissed_from_panel" | "selection" | "routing" | "waiting"));
                fields.insert("details_deferred".into(), true.into());
                if let Some(summary) = summary(item) {
                    fields.insert("tool_count".into(), summary.tools.into());
                    fields.insert("job_summary".into(), serde_json::json!(summary));
                }
            }
        }
        ServerEvent::ToolResult { item, output, .. } if item.starts_with(fuzukue_proto::DELEGATE_JOB) => {
            if let Ok(mut report) = serde_json::from_str::<fuzukue_proto::DelegateReport>(output) {
                report.text.clear();
                report.changes.clear();
                report.attempts.clear();
                report.files = None;
                *output = serde_json::to_string(&report).expect("serializable job report");
            } else {
                output.clear();
            }
        }
        ServerEvent::DelegateProgress { .. } => return None,
        _ => {}
    }
    Some(event)
}

pub(crate) fn bound_history_tool(mut event: ServerEvent) -> ServerEvent {
    // Delegate reports drive the worker panel and require their typed payload.
    const LIMIT: usize = 32 * 1024;
    match &mut event {
        ServerEvent::ToolResult { item, output, .. }
            if !item.starts_with(fuzukue_proto::DELEGATE_JOB) && output.len() > LIMIT =>
        {
            let end = output.floor_char_boundary(LIMIT - '…'.len_utf8());
            output.truncate(end);
            output.push('…');
        }
        ServerEvent::ToolCall { item, input, .. }
            if !item.starts_with(fuzukue_proto::DELEGATE_JOB) =>
        {
            struct Limit(usize);
            impl Write for Limit {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                        std::io::Error::other("history tool input exceeds byte budget")
                    })?;
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            if serde_json::to_writer(Limit(LIMIT), &*input).is_err() {
                *input = serde_json::json!({"truncated": true});
            }
        }
        _ => {}
    }
    event
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
        | ServerEvent::DelegateRenamed { session, .. }
        | ServerEvent::DelegateDismissed { session, .. }
        | ServerEvent::ToolCall { session, .. }
        | ServerEvent::ToolResult { session, .. }
        | ServerEvent::ApprovalRequested { session, .. }
        | ServerEvent::MessageDetails { session, .. }
        | ServerEvent::TurnEnded { session, .. } => Some(session),
        _ => None,
    }
}

const CACHE_PAGE_BYTES: usize = 1024 * 1024;

type SessionStamp = [Option<(u64, u64)>; 2];

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HistoryRevision {
    modified: SystemTime,
    length: u64,
    journal: SessionStamp,
    legacy: Option<SessionStamp>,
    after: u64,
}

#[derive(Serialize, Deserialize)]
struct JobCheckpoint {
    revision: HistoryRevision,
    jobs: Vec<JobSummary>,
}

impl Thread {
    fn history_revision(&self) -> Option<HistoryRevision> {
        if self.transcript.dirty.get() {
            return None;
        }
        let source = self.transcript.source.as_ref()?;
        let metadata = self.path.metadata().ok()?;
        Some(HistoryRevision {
            modified: metadata.modified().ok()?,
            length: metadata.len(),
            journal: source.timeline.session_stamp(&self.front.id),
            legacy: source.legacy.as_ref().map(|id| source.timeline.session_stamp(id)),
            after: source.after,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
enum PagePosition {
    Tail(BTreeSet<String>),
    Before(u64),
    From(u64),
}

#[derive(Clone, PartialEq, Eq)]
enum PageRevision {
    Tail(HistoryRevision),
    Completed { generation: u64, revision: u64 },
}

#[derive(Clone, PartialEq, Eq)]
struct PageKey {
    session: String,
    revision: PageRevision,
    position: PagePosition,
}

#[derive(Clone, Serialize, Deserialize)]
struct HistoryPage {
    events: Vec<ServerEvent>,
    next: Option<u64>,
    #[serde(default)]
    workers: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct TailCheckpoint {
    revision: HistoryRevision,
    workers: BTreeSet<String>,
    page: HistoryPage,
}

fn damage_notice(file: String) -> ServerEvent {
    ServerEvent::Error { message: format!("スレッドファイルが破損している: {file}") }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[derive(Default)]
pub(crate) struct CacheSize {
    pub loaded_transcripts: usize,
    pub transcript_source_bytes: usize,
    pub transcript_entries: usize,
    pub history_pages: usize,
    pub timeline_offset_capacity_bytes: usize,
}

pub(crate) struct MetadataRecovery {
    id: String,
    source: TranscriptSource,
    project: PathBuf,
    version: FileVersion,
    page_revision: u64,
}

pub(crate) struct RecoveredMetadata {
    candidate: MetadataRecovery,
    paths: Vec<String>,
    transcript: Option<Transcript>,
}

impl MetadataRecovery {
    pub(crate) fn read(self) -> Result<RecoveredMetadata> {
        let transcript = self.source.read()?;
        let blobs = Blobs::new(self.source.path.parent().context("missing thread directory")?);
        let paths = transcript.items.iter().filter_map(|entry| entry.meta.tool.as_ref())
            .flat_map(|tool| tool.artifact_paths(&self.project, &blobs)).collect();
        let interrupted = transcript.items.iter().filter_map(|entry| entry.meta.application.as_ref())
            .any(|application| matches!(application.state, CommentApplicationState::Running));
        Ok(RecoveredMetadata { candidate: self, paths, transcript: interrupted.then_some(transcript) })
    }
}

pub struct Store {
    project: PathBuf,
    state_dir: PathBuf,
    timeline: state::Timeline,
    threads: BTreeMap<String, Thread>,
    damaged_files: Vec<String>,
    migrations: HashMap<String, String>,
    restored_generations: HashMap<String, u64>,
    pages: RefCell<VecDeque<(PageKey, HistoryPage)>>,
    pending_metadata: BTreeSet<String>,
    pub partner_default: ChatSettings,
    pub worker_default: DelegateSettings,
    pub worker_default_configured: bool,
}

impl Store {
    #[cfg(test)]
    pub fn load(
        project: &Path,
        state_dir: &Path,
        records: &BTreeMap<String, state::SessionRecord>,
    ) -> Self {
        Self::load_indexed(project, state_dir, records, state::Timeline::open(state_dir))
    }

    pub(crate) fn load_indexed(
        project: &Path,
        state_dir: &Path,
        records: &BTreeMap<String, state::SessionRecord>,
        timeline: state::Timeline,
    ) -> Self {
        let mut store = Self {
            project: project.into(),
            state_dir: state_dir.into(),
            timeline,
            threads: BTreeMap::new(),
            damaged_files: Vec::new(),
            restored_generations: HashMap::new(),
            pages: RefCell::new(VecDeque::new()),
            pending_metadata: BTreeSet::new(),
            migrations: std::fs::read(state_dir.join("removed-thread-migrations.json")).ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default(),
            partner_default: state::load_partner_default(state_dir),
            worker_default: state::load_delegate_settings(state_dir),
            worker_default_configured: state_dir.join("delegate-settings.json").is_file(),
        };
        store.reload_model_defaults();
        let dir = crate::safe_join(project, "threads").ok();
        if let Some(dir) = &dir {
            if let Err(error) = save_files::reconcile(dir) {
                tracing::warn!(path=?dir, %error, "could not reconcile thread save files");
            }
        }
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
            match Thread::read_lazy(entry.path(), &store.timeline) {
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
                result => {
                    let error = result.err().map_or_else(|| "invalid thread ID".into(), |e| format!("{e:#}"));
                    let path = entry.path();
                    tracing::warn!(?path, %error, "could not restore thread");
                    store.damaged_files.push(format!("{}: {error}", entry.file_name().to_string_lossy()));
                }
            }
        }

        // Discover legacy aliases from offsets. Only sessions without an existing
        // portable transcript need to be materialized for migration.
        let mut migrations = Vec::new();
        for session in store.timeline.sessions().into_iter().filter(|session| is_legacy_session(session)) {
            if let Some(id) = store.migrated_thread(&session).map(str::to_owned) {
                if let Some(source) = store.threads.get_mut(&id).and_then(|t| t.transcript.source.as_mut()) {
                    source.legacy.get_or_insert(session);
                }
            } else if old_migration_id(&session).is_some() {
                migrations.push(session);
            }
        }
        if !migrations.is_empty() {
            let tabs = workspace::scan(project, &[]);
            let timestamp = state::legacy_timeline_timestamp(state_dir)
                .unwrap_or_else(|| now().timestamp_millis());
            let last_seq = store.timeline.latest_legacy_sequence();
            for session in migrations {
                let mut transcript = Transcript::default();
                let mut last = 0;
                store.timeline.visit_session(&session, |mut env, refs, record| {
                    last = env.seq;
                    let source = (refs.is_empty() && payloads::bound_replay(&mut env.event))
                        .then_some(Source::Journal(record));
                    transcript.observe(&env.event);
                    transcript.restore_tool(&env.event);
                    transcript.capture_sources(&env.event, refs, source, true);
                });
                transcript.rebuild_job_facts();
                transcript.hide_saved_blank_replies();
                let updated = timestamp.saturating_sub(i64::try_from(last_seq.saturating_sub(last)).unwrap_or(i64::MAX));
                let session_id = records.get(&session).and_then(|record| record.claude_session_id.clone());
                if let Err(error) = store.migrate(&session, session_id, transcript, &tabs, updated) {
                    tracing::warn!("could not migrate {session}: {error:#}");
                }
            }
        }
        // Capture the startup candidates without loading their transcripts.
        store.pending_metadata = store.timeline.snapshot_recovery_sessions().into_iter()
            .map(|session| store.migrated_thread(&session).unwrap_or(&session).to_owned())
            .filter(|id| store.threads.contains_key(id))
            .collect();
        store
    }

    pub(crate) fn thread_damage(&self, id: &str) -> Option<ServerEvent> {
        let thread = self.threads.get(id)?;
        let error = thread.transcript.loaded.get()?.error.as_ref()?;
        Some(damage_notice(format!("{} ({}): {error}",
            thread.front.title, thread.path.file_name()?.to_string_lossy())))
    }

    pub(crate) fn damage_notices(&self) -> impl Iterator<Item = ServerEvent> + '_ {
        self.damaged_files.iter().map(|file| damage_notice(file.clone()))
            .chain(self.threads.keys().filter_map(|id| self.thread_damage(id)))
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(crate) fn cache_size(&self) -> CacheSize {
        let mut size = CacheSize {
            history_pages: self.pages.borrow().len(),
            timeline_offset_capacity_bytes: self.timeline.try_offset_capacity_bytes().unwrap_or(0),
            ..CacheSize::default()
        };
        for transcript in self.threads.values().filter_map(|thread| thread.transcript.loaded.get()) {
            size.loaded_transcripts += 1;
            size.transcript_source_bytes += transcript.preview_weight();
            size.transcript_entries += transcript.value.items.len();
        }
        size
    }

    pub(crate) fn trim_transcripts(&mut self) {
        const CACHE_THREADS: usize = 8;
        const CACHE_BYTES: usize = 24 * 1024 * 1024;

        let mut cached: Vec<_> = self.threads.values_mut().filter_map(|thread| {
            (!thread.active).then(|| thread.transcript.cache_weight())
                .flatten()
                .map(|weight| (thread.transcript.accessed.get(), weight, &mut thread.transcript))
        }).collect();
        cached.sort_unstable_by_key(|(accessed, _, _)| std::cmp::Reverse(*accessed));
        let mut total: usize = 0;
        for (rank, (_, weight, transcript)) in cached.into_iter().enumerate() {
            total = total.saturating_add(weight);
            // Keep the last accessed transcript until the idle timeout, even
            // when its entries alone exceed the shared cache budget.
            if rank > 0 && (rank >= CACHE_THREADS || total > CACHE_BYTES) {
                transcript.loaded.take();
            }
        }
    }

    pub(crate) fn flush_dirty_transcripts(&mut self, idle_only: bool) -> Result<()> {
        let mut errors = Vec::new();
        for thread in self.threads.values_mut().filter(|thread| {
            thread.transcript.dirty.get()
                && (!idle_only || !thread.active || thread.transcript.volatile_payloads)
        }) {
            if let Err(error) = thread.save_with(SaveFiles::try_open) {
                errors.push(format!("{}: {error:#}", thread.path.display()));
            }
        }
        ensure!(errors.is_empty(), "could not save dirty transcripts: {}", errors.join("\n"));
        Ok(())
    }

    pub(crate) fn trim_idle_transcripts(&mut self, idle: std::time::Duration) {
        for thread in self.threads.values_mut().filter(|thread| !thread.active) {
            let transcript = &mut thread.transcript;
            if transcript.accessed.get().elapsed() >= idle
                && transcript.source.is_some() && !transcript.dirty.get()
            {
                transcript.loaded.take();
            }
        }
    }

    pub(crate) fn payload_candidate(
        &self, idle: std::time::Duration, busy: impl Fn(&str) -> bool,
    ) -> Option<payload_migration::Candidate> {
        let thread = self.threads.values().filter(|thread| {
            !thread.front.preview_payloads && !thread.active
                && thread.transcript.accessed.get().elapsed() >= idle && !busy(&thread.front.id)
        }).max_by_key(|thread| thread.file_version.borrow().as_ref().map_or(0, |version| version.length))?;
        // Wait an idle interval before retrying a failed or superseded attempt.
        thread.transcript.accessed.set(Instant::now());
        Some(payload_migration::Candidate {
            id: thread.front.id.clone(), path: thread.path.clone(),
            version: thread.file_version.borrow().clone()?,
        })
    }

    pub(crate) fn publish_payloads(&mut self, staged: &mut payload_migration::Staged) -> Result<bool> {
        let Some(thread) = self.threads.get_mut(&staged.candidate.id) else { return Ok(false) };
        if thread.active || thread.front.preview_payloads
            || thread.file_version.borrow().as_ref() != Some(&staged.candidate.version)
        {
            return Ok(false);
        }
        let dirty = thread.transcript.dirty.get();
        let mut refs = std::mem::take(&mut staged.refs);
        if let Some(data) = thread.transcript.loaded.get().filter(|_| dirty) {
            // An idle worker may have left a newer, journaled result in memory.
            // Keep pending entries and switch only the old inline locations.
            for entry in &data.value.items {
                let Some(tool) = &entry.meta.tool else { continue };
                let refs = refs.get(&entry.meta.item);
                if matches!(tool.input_source, Some(Source::Inline(_))) && refs.and_then(|refs| refs.input.as_ref()).is_none()
                    || matches!(tool.output_source, Some(Source::Inline(_))) && refs.and_then(|refs| refs.output.as_ref()).is_none()
                {
                    return Ok(false);
                }
            }
        }
        let version = staged.exchange()?;
        thread.file_version.replace(Some(version));
        thread.front.preview_payloads = true;
        if let Some(data) = thread.transcript.loaded.get_mut().filter(|_| dirty) {
            data.weight.set(None);
            for entry in &mut data.value.items {
                let Some(tool) = &mut entry.meta.tool else { continue };
                let Some(refs) = refs.remove(&entry.meta.item) else { continue };
                if matches!(tool.input_source, Some(Source::Inline(_))) {
                    tool.input_source = None;
                    tool.payloads.input = refs.input;
                }
                if matches!(tool.output_source, Some(Source::Inline(_))) {
                    tool.output_source = None;
                    tool.payloads.output = refs.output;
                }
            }
        } else {
            thread.transcript.loaded.take();
        }
        Ok(true)
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
            if thread.front.partner_pinned.is_none() {
                // The old creator copied defaults into the thread. User changes also
                // went into the session record, which identifies those old overrides.
                let saved = records.get(id).map(|record| &record.chat_settings);
                let explicit = saved.is_some_and(|settings| settings.model.is_some() || settings.effort.is_some());
                thread.front.partner_pinned = Some(explicit);
                if thread.front.partner_settings.is_none() {
                    thread.front.partner_settings = saved.cloned();
                }
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

    pub(crate) fn library_thread_ids(&self) -> Vec<String> {
        self.threads.keys().cloned().collect()
    }

    pub(crate) fn library_thread(
        &mut self,
        id: &str,
        read_messages: impl FnOnce(&LibraryStamp) -> bool,
    ) -> Option<LibraryThread> {
        let thread = self.threads.get(id)?;
        let stamp = LibraryStamp {
            title: thread.front.title.clone(),
            decks: thread.front.attached.clone(),
            updated: thread.front.updated.max(thread.library_updated),
            revision: thread.library_revision,
        };
        let messages = read_messages(&stamp).then(|| {
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
        if messages.is_some() {
            self.trim_transcripts();
        }
        Some(LibraryThread { stamp, messages })
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
            let mut view = thread.view(macros.into_values().collect());
            if let Some((partner, worker)) = self.role_settings(&thread.front.id) {
                view.partner_settings = Some(partner);
                view.worker_settings = Some(worker);
            }
            view
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

    fn completed_page_key(&self, id: &str, thread: &Thread, position: PagePosition) -> PageKey {
        PageKey {
            session: id.into(), position,
            revision: PageRevision::Completed {
                generation: self.restored_generations.get(id).copied().unwrap_or(0),
                revision: thread.page_revision,
            },
        }
    }

    fn cached_page(&self, key: &PageKey) -> Option<HistoryPage> {
        let mut pages = self.pages.borrow_mut();
        let position = pages.iter().position(|(cached, _)| cached == key)?;
        let entry = pages.remove(position).unwrap();
        let page = entry.1.clone();
        pages.push_front(entry);
        Some(page)
    }

    fn cache_page(&self, key: PageKey, page: HistoryPage) {
        let mut size = ReplaySize(0);
        if serde_json::to_writer(&mut size, &page).is_err() || size.0 > CACHE_PAGE_BYTES {
            return;
        }
        let mut pages = self.pages.borrow_mut();
        pages.push_front((key, page));
        pages.truncate(8);
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.state_dir.join("thread-tails-v2").join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
    }

    fn job_checkpoint_path(&self, id: &str) -> PathBuf {
        self.state_dir.join("thread-jobs-v1").join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
    }

    fn read_tail_checkpoint(&self, key: &PageKey, workers: &BTreeSet<String>) -> Option<HistoryPage> {
        let file = std::fs::File::open(self.checkpoint_path(&key.session)).ok()?;
        if file.metadata().ok()?.len() > CACHE_PAGE_BYTES as u64 { return None; }
        let mut bytes = Vec::new();
        file.take(CACHE_PAGE_BYTES as u64 + 1).read_to_end(&mut bytes).ok()?;
        let saved: TailCheckpoint = serde_json::from_slice(&bytes).ok()?;
        (PageRevision::Tail(saved.revision) == key.revision && &saved.workers == workers).then_some(saved.page)
    }

    fn write_tail_checkpoint(&self, key: &PageKey, workers: &BTreeSet<String>, page: &HistoryPage) -> Result<()> {
        let path = self.checkpoint_path(&key.session);
        let PageRevision::Tail(revision) = &key.revision else { return Ok(()) };
        let encoded = serde_json::to_vec(&TailCheckpoint {
            revision: revision.clone(), workers: workers.clone(), page: page.clone(),
        })?;
        if encoded.len() > CACHE_PAGE_BYTES { return Ok(()); }
        durable::write(&path, |output| Ok(output.write_all(&encoded)?))
    }

    pub(crate) fn history_tail(
        &self,
        id: &str,
        workers: &BTreeSet<String>,
        compact: bool,
        restore_tool: impl Fn(ServerEvent) -> ServerEvent,
    ) -> Option<(Vec<ServerEvent>, Vec<String>, Option<u64>)> {
        let thread = self.threads.get(id).filter(|thread| thread.path.is_file())?;
        let key = compact.then(|| thread.history_revision()).flatten().map(|revision| PageKey {
            session: id.into(), revision: PageRevision::Tail(revision),
            position: PagePosition::Tail(workers.clone()),
        });
        if let Some(key) = &key {
            if let Some(page) = self.cached_page(key) {
                return Some((page.events, page.workers, page.next));
            }
            if let Some(page) = self.read_tail_checkpoint(key, workers) {
                let result = (page.events.clone(), page.workers.clone(), page.next);
                self.cache_page(key.clone(), page);
                return Some(result);
            }
        }
        thread.transcript.check().ok()?;
        let (events, sparse, next) = thread.transcript.tail(id, Some(workers), compact, &restore_tool);
        if let Some(key) = key {
            let page = HistoryPage { events: events.clone(), workers: sparse.clone(), next };
            if let Err(error) = self.write_tail_checkpoint(&key, workers, &page) {
                tracing::debug!(%error, session=id, "could not cache thread tail");
            }
            self.cache_page(key, page);
        }
        Some((events, sparse, next))
    }

    pub(crate) fn scoped_history_page(
        &self,
        id: &str,
        before: u64,
        generation: Option<u64>,
        compact: bool,
    ) -> Result<(Vec<ServerEvent>, Option<u64>), fuzukue_proto::ThreadHistoryFailure> {
        use fuzukue_proto::ThreadHistoryFailure::*;
        let thread = self.threads.get(id).ok_or(NotFound)?;
        if self.history_generation(id) != generation {
            return Err(Stale);
        }
        if !thread.path.is_file() {
            return Err(Unavailable);
        }
        self.history_page_mode(id, before, compact).ok_or(Stale)
    }

    pub(crate) fn history_page(&self, id: &str, before: u64) -> Option<(Vec<ServerEvent>, Option<u64>)> {
        self.history_page_mode(id, before, false)
    }

    fn history_page_mode(&self, id: &str, before: u64, compact: bool) -> Option<(Vec<ServerEvent>, Option<u64>)> {
        let thread = self.threads.get(id).filter(|thread| thread.path.is_file())?;
        let key = compact.then(|| self.completed_page_key(id, thread, PagePosition::Before(before)));
        if let Some(page) = key.as_ref().and_then(|key| self.cached_page(key)) {
            return Some((page.events, page.next));
        }
        thread.transcript.check().ok()?;
        let before = usize::try_from(before).ok()?;
        if before == 0 || before > thread.transcript.items.len() {
            return None;
        }
        let (events, first) = thread.transcript.replay_before(id, before, compact, &bound_history_tool);
        let next = (first != 0).then_some(first as u64);
        if let Some(key) = key.filter(|_| before < thread.transcript.items.len()) {
            self.cache_page(key, HistoryPage { events: events.clone(), workers: Vec::new(), next });
        }
        Some((events, next))
    }

    pub(crate) fn history_from(
        &self, id: &str, start: u64, generation: u64,
    ) -> Result<(Vec<ServerEvent>, Option<u64>), fuzukue_proto::ThreadHistoryFailure> {
        use fuzukue_proto::ThreadHistoryFailure::*;
        let thread = self.threads.get(id).ok_or(NotFound)?;
        if self.history_generation(id) != Some(generation) {
            return Err(Stale);
        }
        if !thread.path.is_file() { return Err(Unavailable); }
        let key = self.completed_page_key(id, thread, PagePosition::From(start));
        if let Some(page) = self.cached_page(&key) {
            return Ok((page.events, page.next));
        }
        thread.transcript.check().map_err(|_| Unavailable)?;
        let start = usize::try_from(start).map_err(|_| Stale)?;
        if start > thread.transcript.items.len() {
            return Err(Stale);
        }
        let (events, next) = thread.transcript.replay_from(id, start, true);
        if next.is_some() {
            self.cache_page(key, HistoryPage { events: events.clone(), workers: Vec::new(), next });
        }
        Ok((events, next))
    }

    pub(crate) fn job_details(&self, session: &str, job: &str, before: Option<u64>) -> (Vec<ServerEvent>, Option<u64>) {
        self.threads.get(session).filter(|thread| thread.path.is_file()).map_or_else(Default::default, |thread| {
            thread.transcript.job_details(session, job, before)
        })
    }

    pub(crate) fn job_summary_for_event<'a>(&mut self, event: &'a ServerEvent) -> Option<(&'a str, JobSummary)> {
        let (session, item) = match event {
            ServerEvent::ToolCall { session, item, name, .. } if name == "delegate" => (session.as_str(), item.as_str()),
            ServerEvent::ToolResult { session, item, .. } => (session.as_str(), item.as_str()),
            ServerEvent::DelegateProgress { session, job, .. } | ServerEvent::DelegateRenamed { session, job, .. }
            | ServerEvent::DelegateDismissed { session, job, .. } => (session.as_str(), job.as_str()),
            ServerEvent::ToolCall { .. } | ServerEvent::TextDelta { .. } => event.worker_child()?,
            _ => return None,
        };
        let transcript = &self.threads.get(session)?.transcript;
        let cold = transcript.loaded.get().is_none();
        let job = if matches!(event, ServerEvent::ToolResult { .. }) {
            transcript.index.get(item)
                .and_then(|&position| fuzukue_proto::delegate_job_id(item, &transcript.items[position].meta.actor))
                .unwrap_or(item)
        } else {
            item
        };
        let summary = transcript.job_summary(job);
        if cold {
            self.trim_transcripts();
        }
        summary.map(|summary| (session, summary))
    }

    pub(crate) fn job_summaries(&self, session: &str) -> Vec<JobSummary> {
        let Some(thread) = self.threads.get(session) else { return Vec::new() };
        let revision = thread.history_revision();
        let path = self.job_checkpoint_path(session);
        if let Some(revision) = &revision {
            if let Ok(file) = File::open(&path) {
                let checkpoint: Result<JobCheckpoint, _> = serde_json::from_reader(file.take(CACHE_PAGE_BYTES as u64));
                if let Ok(checkpoint) = checkpoint {
                    if &checkpoint.revision == revision {
                        return checkpoint.jobs;
                    }
                }
            }
        }
        if thread.transcript.check().is_err() {
            return Vec::new();
        }
        let transcript = &thread.transcript;
        let jobs: Vec<_> = transcript.job_roots.iter().filter_map(|&position| {
            transcript.job_summary(&transcript.items[position].meta.item)
        }).collect();
        if let Some(revision) = revision {
            let checkpoint = JobCheckpoint { revision, jobs };
            if let Ok(encoded) = serde_json::to_vec(&checkpoint) {
                if encoded.len() < CACHE_PAGE_BYTES {
                    if let Err(error) = durable::write(&path, |output| Ok(output.write_all(&encoded)?)) {
                        tracing::debug!(%error, session, "could not cache job summaries");
                    }
                }
            }
            return checkpoint.jobs;
        }
        jobs
    }

    pub(crate) fn delegate_target(&self, query: &str, session: Option<&str>) -> Result<(String, String)> {
        use fuzukue_proto::DELEGATE_JOB;
        let query = query.trim();
        ensure!(!query.is_empty(), "Specify a job ID or name");
        let prefix = query.strip_prefix(DELEGATE_JOB).unwrap_or(query);
        ensure!(!prefix.is_empty(), "Specify a job ID or name");
        let name = query.to_lowercase();
        let mut ids = Vec::new();
        let mut names = Vec::new();
        for (id, thread) in self.threads.iter().filter(|(id, _)| session.is_none_or(|session| session == *id)) {
            for entry in &thread.transcript.items {
                let job = &entry.meta.item;
                let Some(nonce) = job.strip_prefix(DELEGATE_JOB) else { continue };
                let Some(input) = entry.meta.tool.as_ref().filter(|tool| tool.name == "delegate")
                    .and_then(|tool| tool.input.as_ref()) else { continue };
                if job == query {
                    return Ok((id.clone(), job.clone()));
                }
                if nonce.starts_with(prefix) {
                    ids.push((id, job));
                } else if ["label", "title"].iter().any(|key| {
                    input[*key].as_str().is_some_and(|value| value.to_lowercase().contains(&name))
                }) {
                    names.push((id, job));
                }
            }
        }
        let matches = if ids.is_empty() { names } else { ids };
        match matches.as_slice() {
            [(session, job)] => Ok(((*session).clone(), (*job).clone())),
            [] => anyhow::bail!("Job {query:?} was not found in this workspace"),
            _ => anyhow::bail!("Ambiguous job {query:?}. Use a full job ID."),
        }
    }

    pub(crate) fn delegate_record(&self, session: &str, job: &str) -> Option<(&serde_json::Value, Option<bool>, Option<&str>)> {
        let thread = self.threads.get(session)?;
        let &index = thread.transcript.index.get(job)?;
        let tool = thread.transcript.items[index].meta.tool.as_ref().filter(|tool| tool.name == "delegate")?;
        Some((tool.input.as_ref()?, tool.ok, tool.output.as_deref()))
    }

    pub(crate) fn dismiss_delegate(&mut self, session: &str, job: &str) -> Result<bool> {
        let (input, ok, _) = self.delegate_record(session, job).context("Job not found in this thread")?;
        anyhow::ensure!(ok.is_some(), "Only finished jobs can be removed from the list");
        if input["dismissed_from_panel"].as_bool() == Some(true) {
            return Ok(false);
        }
        let thread = self.threads.get_mut(session).context("Thread not found")?;
        let input = thread.transcript.delegate_input_mut(job).context("Job not found in this thread")?;
        let previous = input.clone();
        input["dismissed_from_panel"] = true.into();
        if let Err(error) = thread.save() {
            *thread.transcript.delegate_input_mut(job).unwrap() = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub(crate) fn rename_delegate(
        &mut self, session: &str, job: &str, label: String, label_source: Option<String>, source: &str,
    ) -> Result<fuzukue_proto::DelegateNameChange> {
        let thread = self.threads.get_mut(session).context("Thread not found")?;
        let input = thread.transcript.delegate_input_mut(job).context("Job not found in this thread")?;
        let previous = input.clone();
        let previous_label = input["label"].as_str().or_else(|| input["title"].as_str()).map(str::to_owned);
        let change = fuzukue_proto::DelegateNameChange {
            at_ms: now().timestamp_millis(), source: source.into(), previous_label, label, label_source,
        };
        record_delegate_name(input, &change);
        if let Err(error) = thread.save() {
            *thread.transcript.delegate_input_mut(job).unwrap() = previous;
            return Err(error);
        }
        Ok(change)
    }

    pub(crate) fn previous_delegate_touches(&self, session: &str, job: &str) -> Option<Vec<String>> {
        let thread = self.threads.get(session)?;
        let previous = thread.transcript.items.iter().rev()
            .filter(|entry| entry.meta.item != job && entry.meta.item.starts_with(fuzukue_proto::DELEGATE_JOB))
            .filter_map(|entry| entry.meta.tool.as_ref())
            .find(|tool| tool.name == "delegate")?;
        let input = previous.full_input(&thread.blobs()).ok()??;
        serde_json::from_value(input.get("touches")?.clone()).ok()
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

    pub(crate) fn system_note(&mut self, id: &str, item: &str, text: String, allow_archived: bool) -> Result<Option<ServerEvent>> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        if thread.transcript.index.contains_key(item) {
            return Ok(None);
        }
        ensure!((allow_archived || !thread.front.archived) && thread.path.is_file(), "Thread is archived or missing");
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
            file_version: RefCell::new(None),
            front: Front {
                preview_payloads: true,
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
                partner_pinned: Some(false),
                worker_settings: None,
                worker_pinned: false,
                agent_overrides: Default::default(),
            },
            transcript: TranscriptSlot::new(Transcript::default()),
            page_revision: 0,
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
        thread.transcript = TranscriptSlot::new(transcript);
        thread.transcript.source = Some(TranscriptSource {
            path: thread.path.clone(), session: thread.front.id.clone(),
            legacy: migrated_from.map(str::to_owned), after: 0, timeline: self.timeline.clone(),
        });
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

    pub(crate) fn inbox_paths(&self, id: &str) -> Vec<String> {
        let Some(thread) = self.threads.get(id).filter(|thread| !thread.front.archived) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        thread.transcript.items.iter().rev()
            .filter(|entry| matches!(entry.meta.actor, Actor::User))
            .flat_map(|entry| &entry.meta.attachments)
            .filter(|path| seen.insert(path.as_str()))
            .cloned()
            .collect()
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
                    paths.extend(tool.artifact_paths(&self.project, &thread.blobs()));
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

    pub(crate) fn reload_model_defaults(&mut self) -> bool {
        let (partner, worker, worker_configured) = match state::model_defaults(&self.state_dir) {
            Ok(defaults) => defaults,
            Err(error) => {
                tracing::warn!("Keep the previous model defaults: {error:#}");
                return false;
            }
        };
        if partner == self.partner_default && worker == self.worker_default && worker_configured == self.worker_default_configured {
            return false;
        }
        (self.partner_default, self.worker_default, self.worker_default_configured) = (partner, worker, worker_configured);
        true
    }

    pub fn role_settings(&self, id: &str) -> Option<(ChatSettings, DelegateSettings)> {
        let thread = self.threads.get(id)?;
        let saved = thread.front.partner_settings.as_ref();
        let partner = if thread.front.partner_pinned == Some(true) {
            saved.cloned().unwrap_or_default()
        } else {
            ChatSettings {
                permission_mode: saved.map_or(self.partner_default.permission_mode, |settings| settings.permission_mode),
                ..self.partner_default.clone()
            }
        };
        let worker = thread.front.worker_settings.as_ref().filter(|_| thread.front.worker_pinned)
            .unwrap_or(&self.worker_default).clone();
        Some((partner, worker))
    }

    pub fn set_partner_permission(&mut self, id: &str, mode: fuzukue_proto::PermissionMode) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.partner_settings.clone();
        thread.front.partner_settings.get_or_insert_with(Default::default).permission_mode = mode;
        if let Err(error) = thread.save() {
            thread.front.partner_settings = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_partner_model_inherited(&mut self, id: &str) -> Result<()> {
        let thread = self.threads.get_mut(id).context("スレッドが見つからない")?;
        let previous = thread.front.partner_pinned.replace(false);
        if let Err(error) = thread.save() {
            thread.front.partner_pinned = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn worker_pinned(&self, id: &str) -> bool {
        self.threads.get(id).is_some_and(|thread| thread.front.worker_pinned)
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
            thread.front.partner_pinned = Some(true);
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
            durable::json(&path, &removed)?;
        }
        let files = SaveFiles::open(thread.path.parent().context("missing thread directory")?)?;
        trash.file_unrecorded(app, fuzukue_proto::TrashKind::Thread, id.into(), thread.front.title.clone(), &thread.path)?;
        if let Err(error) = files.remove_previous(&thread.path) {
            tracing::warn!(path=?thread.path, %error, "could not remove previous version of trashed thread");
        }
        self.threads.remove(id);
        self.pages.borrow_mut().retain(|(key, _)| key.session != id);
        let _ = std::fs::remove_file(self.checkpoint_path(id));
        let _ = std::fs::remove_file(self.job_checkpoint_path(id));
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
        thread.transcript.source = Some(TranscriptSource {
            path: thread.path.clone(), session: id.clone(), legacy: None,
            after: generation, timeline: self.timeline.clone(),
        });
        thread.transcript.dirty.set(false);
        let events = thread.transcript.events(&id, &thread.transcript.items).collect();
        self.threads.insert(id.clone(), thread);
        self.restored_generations.insert(id.clone(), generation);
        Ok(ServerEvent::ThreadRestored { session: id, events })
    }

    pub(crate) fn rewrite<T>(&mut self, id: &str, item: &str, validate: impl FnOnce(&str, &[String]) -> Result<T>)
        -> Result<(T, Vec<String>)> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        ensure!(!thread.active, "Thread is running");
        thread.transcript.check()?;
        let transcript: &mut Transcript = &mut thread.transcript;
        let entries = &mut transcript.items;
        let hidden = &transcript.hidden_replies;
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
        thread.page_revision = thread.page_revision.wrapping_add(1);
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

    pub(crate) fn metadata_recovery(&mut self, id: Option<&str>) -> Option<MetadataRecovery> {
        let id = match id {
            Some(id) if self.pending_metadata.remove(id) => id.to_owned(),
            Some(_) => return None,
            None => self.pending_metadata.pop_first()?,
        };
        let thread = self.threads.get(&id)?;
        Some(MetadataRecovery {
            id, source: thread.transcript.source.clone()?, project: self.project.clone(),
            version: thread.file_version.borrow().clone()?, page_revision: thread.page_revision,
        })
    }

    pub(crate) fn apply_metadata_recovery(&mut self, recovered: RecoveredMetadata) -> Result<bool> {
        let RecoveredMetadata { candidate, paths, transcript } = recovered;
        let Some(thread) = self.threads.get_mut(&candidate.id) else { return Ok(false) };
        let changed = thread.add_artifacts(paths);
        // A live change may have started or finished an application after
        // this snapshot. Only normalize interrupted applications in that snapshot.
        if let Some(value) = transcript {
            if !thread.active && candidate.page_revision == 0 && thread.page_revision == 0
                && !thread.transcript.dirty.get()
                && thread.file_version.borrow().as_ref() == Some(&candidate.version)
            {
                if thread.transcript.loaded.get().is_none() {
                    let _ = thread.transcript.loaded.set(LoadedTranscript {
                        value, error: None, weight: Cell::new(None),
                    });
                }
                if thread.finish_interrupted_applications() {
                    thread.page_revision = thread.page_revision.wrapping_add(1);
                    thread.save()?;
                }
            }
        }
        Ok(changed)
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

    pub(crate) fn event_artifact_paths(&self, event: &ServerEvent) -> Option<Vec<String>> {
        let ServerEvent::ToolResult { session, item, ok: true, output } = event else { return None };
        let thread = self.threads.get(session)?;
        let &index = thread.transcript.index.get(item)?;
        let tool = thread.transcript.items[index].meta.tool.as_ref()?;
        if !crate::thread_artifacts::may_record_path(&tool.name) {
            return Some(Vec::new());
        }
        let input = tool.full_input(&thread.blobs()).ok().flatten();
        Some(crate::thread_artifacts::tool_paths(&tool.name, input.as_ref(), Some(output)))
    }

    pub(crate) fn journal_prefixes(&self) -> payloads::JournalPrefixes {
        self.timeline.journal_prefixes()
    }

    pub(crate) fn event_blobs(&self, event: &ServerEvent) -> Option<Blobs> {
        let session = match event {
            ServerEvent::ToolCall { session, .. } | ServerEvent::ToolResult { session, .. } => session,
            _ => return None,
        };
        Some(self.threads.get(session)?.blobs())
    }

    pub(crate) fn tool_detail(&self, session: &str, item: &str) -> Option<ToolDetail> {
        let thread = self.threads.get(session).filter(|thread| thread.path.is_file())?;
        let &index = thread.transcript.index.get(item)?;
        let tool = thread.transcript.items[index].meta.tool.as_ref()?.clone();
        Some(ToolDetail { blobs: thread.blobs(), tool })
    }

    pub(crate) fn prepare_event(&self, event: &ServerEvent) {
        let Some(thread) = history_session(event).and_then(|id| self.threads.get(id)) else { return };
        // Load before append so this event reaches the transcript exactly once.
        if !matches!(event, ServerEvent::TurnActivity { .. } | ServerEvent::ThreadRestored { .. }) {
            thread.transcript.get();
        }
    }

    #[cfg(test)]
    pub fn observe(&mut self, event: &ServerEvent, completed: bool) -> Result<bool> {
        let paths = self.event_artifact_paths(event);
        let mut event = event.clone();
        let refs = match self.event_blobs(&event) {
            Some(blobs) => blobs.compact(&mut event)?,
            None => Refs::default(),
        };
        self.observe_persisted(&event, refs, None, completed, false, paths.as_deref())
    }

    pub(crate) fn observe_persisted(
        &mut self, event: &ServerEvent, refs: Refs, source: Option<Source>, completed: bool, journaled: bool,
        artifact_paths: Option<&[String]>,
    ) -> Result<bool> {
        let Some(thread) = history_session(event).and_then(|id| self.threads.get_mut(id)) else {
            return Ok(false);
        };
        if let ServerEvent::TurnActivity { running, .. } = event {
            thread.active = *running;
            return Ok(false);
        }
        if matches!(event, ServerEvent::ThreadRestored { .. }) {
            return Ok(false);
        }
        thread.transcript.check()?;
        if matches!(event,
            ServerEvent::ToolResult { .. } | ServerEvent::DelegateRenamed { .. }
            | ServerEvent::DelegateDismissed { .. } | ServerEvent::DelegateProgress { .. } | ServerEvent::CommentApplication { .. }
            | ServerEvent::MessageDetails { .. } | ServerEvent::ReplySuperseded { .. }
        ) || matches!(event, ServerEvent::ToolCall { name, .. } if name == "delegate" || event.worker_child().is_some())
            || matches!(event, ServerEvent::TextDelta { .. } if event.worker_child().is_some())
        {
            thread.page_revision = thread.page_revision.wrapping_add(1);
        }
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
        thread.transcript.volatile_payloads |= matches!(source, Some(Source::Memory(_)));
        thread.transcript.observe(event);
        thread.transcript.capture_sources(event, refs, source, false);
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
                // A large checkpoint can wait for the next turn or explicit edit.
                // The synced journal makes these worker results recoverable meanwhile.
                const LARGE_CHECKPOINT: u64 = 4 * 1024 * 1024;
                let large = thread.file_version.borrow().as_ref()
                    .is_some_and(|version| version.length >= LARGE_CHECKPOINT);
                if !journaled || !large {
                    thread.save()?;
                }
                return Ok(false);
            }
            ServerEvent::ToolResult { ok: true, .. } => {
                let paths = artifact_paths.unwrap_or_default().iter()
                    .filter_map(|path| crate::thread_artifacts::relative_path(&self.project, path)).collect();
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
            ServerEvent::DelegateProgress { .. } | ServerEvent::DelegateDismissed { .. } => return Ok(false),
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
        Ok(()) => durable::sync_dir(project)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let path = crate::safe_join(project, name)?;
    ensure!(path.is_dir(), "{name} はディレクトリではない");
    Ok(path)
}

#[cfg(test)]
mod archive_tests {
    #[test]
    fn corrupt_portable_threads_are_reported_and_kept() -> anyhow::Result<()> {
        use super::*;
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let mut threads = Store::load(project.path(), state.path(), &Default::default());
        let id = threads.create(None)?;
        let path = threads.path(&id)?;
        let original = fs::read(&path)?;
        threads.rename(&id, "Second generation")?;
        let previous = path.with_file_name(fuzukue_proto::THREAD_SAVE_DIR)
            .join(format!("{}.previous", path.file_name().unwrap().to_string_lossy()));
        assert_eq!(fs::read(&previous)?, original);
        let valid = fs::read(&path)?;
        let legacy_temp = path.with_file_name(".tmpABC123");
        File::create(&legacy_temp)?;

        for data in [vec![], vec![0; 18_800_000], [&valid[..], b"partial"].concat()] {
            fs::write(&path, &data)?;
            let mut reopened = Store::load(project.path(), state.path(), &Default::default());
            let filename = path.file_name().unwrap().to_string_lossy();
            assert!(reopened.damage_notices().any(|event|
                matches!(event, ServerEvent::Error { message } if message.contains(filename.as_ref()))));
            assert!(reopened.rename(&id, "Cannot replace").is_err());
            assert_eq!(fs::read(&path)?, data);
            assert_eq!(fs::read(&previous)?, original);
        }
        assert!(!legacy_temp.exists());
        Ok(())
    }

    #[test]
    fn previous_generation_migrates_and_tracks_the_live_filename() -> anyhow::Result<()> {
        use super::*;
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let project = project.path().canonicalize()?;
        let state = state.path();
        let mut store = Store::load(&project, state, &Default::default());
        let id = store.create(None)?;
        let path = store.path(&id)?;
        let name = path.file_name().unwrap().to_string_lossy();
        let private = path.with_file_name(fuzukue_proto::THREAD_SAVE_DIR);
        let previous = private.join(format!("{name}.previous"));
        let legacy = path.with_file_name(format!(".{name}.previous"));
        let orphan = path.with_file_name(".deleted.md.previous");

        let first = fs::read(&path)?;
        store.rename(&id, "Second generation")?;
        assert_eq!(fs::read(&previous)?, first);
        let second = fs::read(&path)?;
        store.rename(&id, "Third generation")?;
        assert_eq!(fs::read(&previous)?, second);
        assert_eq!(fs::read_dir(&private)?.count(), 2);

        fs::rename(&previous, &legacy)?;
        fs::write(&orphan, &first)?;
        fs::write(private.join("staged-thread"), b"interrupted save")?;
        fs::write(private.join("staged-previous"), b"interrupted backup")?;
        let legacy_staging = path.with_file_name(".fuzukue-thread-backup-abcd12");
        fs::create_dir(&legacy_staging)?;
        fs::write(legacy_staging.join("previous"), &first)?;
        drop(store);

        let mut store = Store::load(&project, state, &Default::default());
        assert_eq!(store.path(&id)?, path);
        assert_eq!(fs::read(&previous)?, second);
        assert!(!legacy.exists() && !orphan.exists() && !legacy_staging.exists());
        assert_eq!(fs::read_dir(&private)?.count(), 2);

        let moved = path.with_file_name("moved.md");
        fs::rename(&path, &moved)?;
        store = Store::load(&project, state, &Default::default());
        assert_eq!(store.path(&id)?, moved);
        assert!(!previous.exists());
        store.rename(&id, "Moved thread")?;
        assert!(private.join("moved.md.previous").is_file());

        let workspace = crate::Workspace::new(
            project, String::new(), state.to_owned(), std::sync::Arc::new(crate::Hub::new()),
        );
        store.trash(&workspace, &crate::trash::Store::new(state), &id)?;
        assert!(!moved.exists());
        assert!(!store.contains(&id));
        assert_eq!(fs::read_dir(&private)?.count(), 1);
        Ok(())
    }

    #[test]
    fn large_thread_recovers_worker_result_without_replacing_markdown() -> anyhow::Result<()> {
        use super::*;
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let session = hub.edit_threads(|threads| threads.create(None))?;
        let path = hub.thread_path(&session)?;
        hub.publish(ServerEvent::TextDelta {
            session: session.clone(), item: "synthetic-partner".into(), actor: Actor::Claude,
            text: "x".repeat(4 * 1024 * 1024),
        });
        hub.publish(ServerEvent::TurnEnded { session: session.clone(), actor: Actor::Claude });
        let before = fs::read(&path)?;
        let item = "delegate:synthetic-job".to_string();
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::System,
            name: "delegate".into(), input: serde_json::json!({}),
        });
        hub.publish(ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: "done".into(),
        });
        assert_eq!(fs::read(&path)?, before);
        drop(hub);

        let reopened = crate::hub::Hub::with_timeline(state.path());
        reopened.load_threads(project.path(), state.path());
        assert!(reopened.read_threads(|threads|
            matches!(threads.delegate_record(&session, &item), Some((_, Some(true), Some("done"))))));
        Ok(())
    }


    #[test]
    fn payload_survives_when_both_sidecar_and_journal_writes_fail() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let session = hub.edit_threads(|threads| threads.create(None))?;
        let item = "tool".to_owned();
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::Claude,
            name: "Read".into(), input: serde_json::json!({}),
        });
        hub.publish(ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: "old journal result".into(),
        });
        let path = hub.thread_path(&session)?;
        fs::write(Blobs::new(path.parent().unwrap()).dir(), "obstruct sidecars")?;
        hub.disable_journal_writes(state.path())?;

        let output = format!("{}hidden{}end of failure", "a".repeat(5000), "b".repeat(5000));
        hub.publish(ServerEvent::ToolResult { session: session.clone(), item: item.clone(), ok: false, output: output.clone() });
        assert_eq!(hub.tool_detail(&session, &item).unwrap().output()?, Some(output.clone()));
        // A competing writer prevents the retry from checkpointing. The full
        // payload remains readable until an idle pass can flush it.
        let lock = save_files::private_dir(&path.parent().unwrap().join(fuzukue_proto::THREAD_SAVE_DIR))?;
        assert!(hub.flush_dirty_transcripts(true).is_err());
        assert_eq!(hub.tool_detail(&session, &item).unwrap().output()?, Some(output.clone()));
        drop(lock);
        hub.flush_dirty_transcripts(true)?;
        drop(hub);

        let reopened = crate::hub::Hub::with_timeline(state.path());
        reopened.load_threads(project.path(), state.path());
        assert_eq!(reopened.tool_detail(&session, &item).unwrap().output()?, Some(output));
        reopened.publish(ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: "new journal result".into(),
        });
        drop(reopened);
        let reopened = crate::hub::Hub::with_timeline(state.path());
        reopened.load_threads(project.path(), state.path());
        assert_eq!(
            reopened.tool_detail(&session, &item).unwrap().output()?.as_deref(),
            Some("new journal result"),
        );
        Ok(())
    }

    #[test]
    fn old_artifacts_recover_on_request_without_loading_transcripts_at_startup() -> Result<()> {
        use fuzukue_proto::Envelope;

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let session = hub.edit_threads(|threads| threads.create(None))?;
        drop(hub);

        let item = "tool".to_owned();
        let mut journal = crate::state::TimelineWriter::open(state.path())?;
        journal.append(&Envelope { workspace: None, seq: 1, event: ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::Claude,
            name: "mcp__fuzukue__file_inbox".into(), input: serde_json::json!({}),
        } });
        journal.append(&Envelope { workspace: None, seq: 2, event: ServerEvent::ToolResult {
            session: session.clone(), item, ok: true,
            output: serde_json::json!({"data": "test output".repeat(1000), "path": "inbox/example.md"}).to_string(),
        } });
        drop(journal);

        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        hub.edit_threads(|store| {
            assert!(store.threads[&session].transcript.loaded.get().is_none());
            assert!(store.artifacts(&session).is_empty());
            Ok(())
        })?;
        assert_eq!(hub.thread_artifacts(&session), ["inbox/example.md"]);
        Ok(())
    }

    #[test]
    fn preview_cache_does_not_charge_the_legacy_journal_record() -> Result<()> {
        use fuzukue_proto::Envelope;

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let mut store = Store::load(project.path(), state.path(), &BTreeMap::new());
        let session = store.create(None)?;
        drop(store);

        let item = "tool".to_owned();
        let mut journal = crate::state::TimelineWriter::open(state.path())?;
        journal.append(&Envelope { workspace: None, seq: 1, event: ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::Claude,
            name: "Read".into(), input: serde_json::json!({}),
        } });
        journal.append(&Envelope { workspace: None, seq: 2, event: ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: "x".repeat(25 * 1024 * 1024),
        } });
        drop(journal);

        let mut store = Store::load(project.path(), state.path(), &BTreeMap::new());
        assert!(store.tool_detail(&session, &item).is_some());
        store.trim_transcripts();
        let transcript = &store.threads[&session].transcript;
        assert!(transcript.loaded.get().is_some());
        assert!(transcript.cache_weight().unwrap() < 4096);
        Ok(())
    }

    #[test]
    fn workers_receive_artifact_paths_and_failure_tail_from_the_original() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let session = hub.edit_threads(|threads| threads.create(None))?;
        let item = "delegate:job:tool".to_owned();
        let mut worker = hub.subscribe_worker();
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::Claude,
            name: "mcp__fuzukue__file_inbox".into(), input: serde_json::json!({}),
        });
        worker.try_recv()?;
        let output = serde_json::json!({"data": "a".repeat(5000), "path": "inbox/deliverable.md"}).to_string();
        hub.publish(ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output,
        });
        let result = worker.try_recv()?;
        assert_eq!(result.artifact_paths.as_deref(), Some(["inbox/deliverable.md".to_owned()].as_slice()));
        assert!(matches!(result.event, ServerEvent::ToolResult { output, .. }
            if serde_json::from_str::<serde_json::Value>(&output).is_err()));
        let output = format!("{}\nerror: failed at the end", "a".repeat(5000));
        hub.publish(ServerEvent::ToolResult { session, item, ok: false, output });
        let result = worker.try_recv()?;
        assert!(result.failure_tail.as_deref().unwrap().ends_with("error: failed at the end"));
        assert!(matches!(result.event, ServerEvent::ToolResult { output, .. }
            if output.starts_with("aaaa") && output.ends_with("error: failed at the end")));
        Ok(())
    }

    #[test]
    fn live_tool_payloads_stay_out_of_the_journal_and_recover_on_demand() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let session = hub.edit_threads(|threads| threads.create(None))?;
        let path = hub.thread_path(&session)?;
        let item = "tool".to_owned();
        let input = serde_json::json!({"text": format!("{}input sentinel", "あ".repeat(2500))});
        let output = format!("start{}output sentinel{}end", "あ".repeat(1250), "い".repeat(1250));
        let before = fs::read(&path)?;
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::Claude, name: "Read".into(),
            input: input.clone(),
        });
        hub.publish(ServerEvent::ToolResult { session: session.clone(), item: item.clone(), ok: false, output: output.clone() });
        assert_eq!(fs::read(&path)?, before);
        let journal = fs::read_to_string(state.path().join("thread-timeline.jsonl"))?;
        assert!(!journal.contains("sentinel"));
        assert_eq!(journal.lines().filter(|line| line.contains("tool_payloads")).count(), 2);
        // The old journal decoder sees the same tool events with previews.
        let last: fuzukue_proto::Envelope = serde_json::from_str(journal.lines().last().unwrap())?;
        assert!(matches!(last.event, ServerEvent::ToolResult { output, .. } if output.len() <= 1029 && output.starts_with("start") && output.ends_with("end")));
        drop(hub);

        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let details = |hub: &crate::hub::Hub| hub.thread_tool_details(session.clone(), item.clone(), 1);
        assert!(matches!(details(&hub), ServerEvent::ThreadToolDetails {
            input: Some(saved_input), output: Some(saved_output), unavailable: false, ..
        } if saved_input == input && saved_output == output));

        hub.publish(ServerEvent::TurnEnded { session: session.clone(), actor: Actor::Claude });
        let markdown = fs::read_to_string(&path)?;
        assert!(markdown.contains("payloads"));
        assert!(!markdown.contains("sentinel"));
        let saved = Thread::read(path)?;
        let tool = saved.transcript.items[0].meta.tool.as_ref().unwrap();
        assert_eq!(tool.full_output(&saved.blobs())?, Some(output));
        Ok(())
    }

    #[test]
    fn job_summary_recovers_from_a_checkpoint_and_a_journal_tail() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let records = Default::default();
        let mut store = Store::load(project.path(), state.path(), &records);
        let session = store.create(None)?;
        let job = "delegate:checkpoint";
        let child = "worker-tool";
        let tool_call = ServerEvent::ToolCall {
            session: session.clone(), item: child.into(), name: "Read".into(),
            actor: Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some(job.into()) },
            input: serde_json::json!({"path":"notes.md"}),
        };
        let thread = store.threads.get_mut(&session).unwrap();
        thread.transcript.observe(&ServerEvent::ToolCall {
            session: session.clone(), item: job.into(), actor: Actor::System, name: "delegate".into(),
            input: serde_json::json!({"title":"Before", "model":"gpt-5.5", "started_at_ms":20}),
        });
        thread.transcript.observe(&tool_call);
        thread.save()?;
        assert_eq!(store.job_summaries(&session)[0].last_activity.as_deref(), Some("Read notes.md"));
        store.history_tail(&session, &BTreeSet::from([job.into()]), true, bound_history_tool).unwrap();
        drop(store);

        let mut writer = state::TimelineWriter::open(state.path())?;
        let events = [
            ServerEvent::ToolResult { session: session.clone(), item: child.into(), ok: false, output: "error".into() },
            ServerEvent::DelegateProgress { session: session.clone(), job: job.into(), summary: "Checking notes".into() },
            ServerEvent::DelegateRenamed {
                session: session.clone(), job: job.into(), change: fuzukue_proto::DelegateNameChange {
                    at_ms: 30, source: "user".into(), previous_label: Some("Before".into()),
                    label: "Renamed".into(), label_source: None,
                },
            },
        ];
        for (index, event) in events.into_iter().enumerate() {
            writer.append(&fuzukue_proto::Envelope { workspace: None, seq: index as u64 + 1, event });
        }
        drop(writer);
        let store = Store::load(project.path(), state.path(), &records);
        assert!(store.threads[&session].transcript.loaded.get().is_none());
        let summaries = store.job_summaries(&session);
        let [summary] = summaries.as_slice() else { panic!("expected one job") };
        assert_eq!(summary.tools, 1);
        assert!(summary.had_tool_error);
        assert_eq!(summary.status, JobStatus::Running);
        assert_eq!(summary.elapsed_at(123), Some(103));
        assert_eq!(summary.job.selection.as_ref().unwrap().backend, fuzukue_proto::AgentBackend::Codex);
        assert_eq!(summary.job.display.label.as_deref(), Some("Renamed"));
        assert_eq!(summary.last_tool.as_deref(), Some("Read notes.md"));
        assert_eq!(summary.last_activity.as_deref(), Some("Checking notes"));
        let reopened = Store::load(project.path(), state.path(), &records);
        let restored = reopened.job_summaries(&session);
        assert_eq!(serde_json::to_value(restored)?, serde_json::to_value(summaries)?);
        assert!(reopened.threads[&session].transcript.loaded.get().is_none(), "current job checkpoint should serve the snapshot");
        Ok(())
    }

    #[test]
    fn transcript_nul_is_detected_on_first_open() -> anyhow::Result<()> {
        use super::*;
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let mut threads = Store::load(project.path(), state.path(), &Default::default());
        let id = threads.create_titled(None, Some("Synthetic thread".into()))?;
        let path = threads.path(&id)?;
        let mut file = File::options().append(true).open(&path)?;
        file.write_all(b"<!-- labo-thread {\"item\":\"test\",\"actor\":\"User\"} -->\n## User\n\ntext\0\n\n")?;
        let threads = Store::load(project.path(), state.path(), &Default::default());
        assert!(threads.history_tail(&id, &Default::default(), true, |event| event).is_none());
        assert!(matches!(threads.thread_damage(&id), Some(ServerEvent::Error { message })
            if message.contains("Synthetic thread") && message.contains("NUL")));
        Ok(())
    }

    #[test]
    fn job_summary_size_is_independent_of_worker_history() -> anyhow::Result<()> {
        let size = |tools: usize| -> anyhow::Result<usize> {
            let session = "thread-scale";
            let job = "delegate:scale";
            let mut transcript = Transcript::default();
            transcript.observe(&ServerEvent::ToolCall {
                session: session.into(), item: job.into(), actor: Actor::System, name: "delegate".into(),
                input: serde_json::json!({"model":"gpt-5.5","started_at_ms":20}),
            });
            for i in 0..tools {
                transcript.observe(&ServerEvent::ToolCall {
                    session: session.into(), item: format!("{job}:{i}"), actor: Actor::System, name: "Read".into(),
                    input: serde_json::json!({"path":"notes.md"}),
                });
            }
            let summary = transcript.job_summary(job).unwrap();
            assert_eq!(summary.tools, tools);
            assert_eq!(summary.last_tool.as_deref(), Some("Read notes.md"));
            let compact = transcript.page_events(session, &transcript.items[0], true, &bound_history_tool);
            assert!(matches!(compact.as_slice(), [ServerEvent::ToolCall { input, .. }]
                if input["job_summary"]["tools"] == tools));
            Ok(serde_json::to_vec(&compact)?.len())
        };
        assert!(size(512)?.abs_diff(size(5120)?) <= 2);
        Ok(())
    }

    #[test]
    fn worker_activity_never_displaces_conversation_in_compact_history() {
        use super::*;
        use serde_json::json;

        let session = "thread-synthetic";
        let job = "delegate:synthetic";
        let mut transcript = Transcript::default();
        for (item, actor, text) in [("first", Actor::User, "Synthetic question"), ("reply", Actor::Claude, "Synthetic reply")] {
            transcript.observe(&ServerEvent::TextDelta {
                session: session.into(), item: item.into(), actor, text: text.into(),
            });
        }
        transcript.observe(&ServerEvent::ToolCall {
            session: session.into(), item: job.into(), actor: Actor::System, name: "delegate".into(),
            input: json!({"model":"synthetic", "title":"Synthetic job", "started_at_ms":1}),
        });
        let output = "Synthetic worker output ".repeat(256);
        for index in 0..(INITIAL_HISTORY_ENTRIES * 2) {
            let item = format!("{job}:tool-{index}");
            transcript.observe(&ServerEvent::ToolCall {
                session: session.into(), item: item.clone(), name: "synthetic_tool".into(),
                actor: Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some(job.into()) },
                input: json!({"detail":output}),
            });
            transcript.observe(&ServerEvent::ToolResult {
                session: session.into(), item, ok: true, output: output.clone(),
            });
        }
        let workers = BTreeSet::from([job.into()]);
        let (legacy, _, _) = transcript.tail(session, Some(&workers), false, &bound_history_tool);
        assert!(!legacy.iter().any(|event| matches!(event, ServerEvent::TextDelta { item, .. } if item == "reply")));
        let (events, _, _) = transcript.tail(session, Some(&workers), true, &bound_history_tool);
        assert!(events.iter().any(|event| matches!(event, ServerEvent::TextDelta { item, .. } if item == "first")));
        assert!(events.iter().any(|event| matches!(event, ServerEvent::TextDelta { item, .. } if item == "reply")));
        assert!(events.iter().any(|event| matches!(event, ServerEvent::ToolCall { item, input, .. }
            if item == job && input["tool_count"] == INITIAL_HISTORY_ENTRIES * 2)));
        assert!(events.iter().all(|event| event.worker_child().is_none()));
        let compact_bytes = serde_json::to_vec(&events).unwrap().len();
        assert!(compact_bytes < 2048);
        eprintln!("worker-dominated tail json bytes: legacy={} compact={compact_bytes}", serde_json::to_vec(&legacy).unwrap().len());
        let (details, next_before) = transcript.job_details(session, job, None);
        assert!(details.iter().any(|event| matches!(event, ServerEvent::ToolResult { output: detail, .. } if detail == &output)));
        let (older, _) = transcript.job_details(session, job, next_before);
        assert!(next_before.is_some() && older.first().is_some_and(|event| event.worker_child().is_some()));
    }

    #[test]
    fn persisted_tail_does_not_materialize_history_at_either_scale() -> anyhow::Result<()> {
        use super::*;

        let page_size = |entries: usize| -> anyhow::Result<u64> {
            let project = tempfile::tempdir()?;
            let state = tempfile::tempdir()?;
            let mut store = Store::load(project.path(), state.path(), &Default::default());
            let id = store.create(None)?;
            let path = store.threads[&id].path.clone();
            let thread = store.threads.get_mut(&id).unwrap();
            for index in 0..entries {
                thread.transcript.observe(&ServerEvent::TextDelta {
                    session: id.clone(), item: format!("synthetic-{index:06}"), actor: Actor::User,
                    text: "Synthetic conversation content".repeat(4),
                });
            }
            thread.save()?;
            let workers = BTreeSet::new();
            let (expected, _, _) = store.history_tail(&id, &workers, true, bound_history_tool).unwrap();
            let checkpoint_bytes = store.checkpoint_path(&id).metadata()?.len();
            drop(store);

            let store = Store::load(project.path(), state.path(), &Default::default());
            let (actual, _, _) = store.history_tail(&id, &workers, true, bound_history_tool).unwrap();
            assert!(store.threads[&id].transcript.loaded.get().is_none());
            assert_eq!(actual.len(), INITIAL_HISTORY_ENTRIES);
            assert_eq!(serde_json::to_vec(&actual)?, serde_json::to_vec(&expected)?);
            assert!(checkpoint_bytes < path.metadata()?.len());
            Ok(checkpoint_bytes)
        };
        let one = page_size(INITIAL_HISTORY_ENTRIES * 2)?;
        let ten = page_size(INITIAL_HISTORY_ENTRIES * 20)?;
        assert!(one.abs_diff(ten) < 128);
        Ok(())
    }
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
        let (tail, mut before) = transcript.replay_before(id, count, false, &|event| event);
        let mut seen = tail.into_iter().rev().collect::<Vec<_>>();
        transcript.observe(&ServerEvent::TextDelta {
            session: id.into(), item: "live".into(), actor: Actor::User, text: "live append".into(),
        });
        let mut empty_page = false;
        while before != 0 {
            let (events, next) = transcript.replay_before(id, before, false, &|event| event);
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
    fn archived_transcript_pages_to_the_start_without_a_checkpoint() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let mut store = Store::load(project.path(), state.path(), &BTreeMap::new());
        let id = store.create(None)?;
        let count = INITIAL_HISTORY_ENTRIES * 7;
        let thread = store.threads.get_mut(&id).unwrap();
        for index in 0..count {
            thread.transcript.observe(&ServerEvent::TextDelta {
                session: id.clone(), item: format!("message-{index}"), actor: Actor::Claude,
                text: format!("Synthetic message {index}"),
            });
        }
        thread.save()?;
        store.set_archived(&id, true)?;
        drop(store);
        let store = Store::load(project.path(), state.path(), &BTreeMap::new());
        assert!(!store.checkpoint_path(&id).exists());
        let generation = store.history_generation(&id);
        for scoped in [false, true] {
            let mut cursor = Some(count as u64);
            let mut remaining = count;
            while let Some(before) = cursor {
                let (events, next) = if scoped {
                    store.scoped_history_page(&id, before, generation, true).expect("archived scoped page")
                } else {
                    store.history_page(&id, before).expect("archived legacy page")
                };
                assert!(!events.is_empty());
                remaining -= events.len();
                assert_eq!(next.unwrap_or(0), remaining as u64);
                cursor = next;
            }
            assert_eq!(remaining, 0);
        }
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
