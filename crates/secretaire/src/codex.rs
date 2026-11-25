//! `codex app-server`（JSON-RPC over stdio）を 1 つ持ち、Codex のスレッドを動かす。
//!
//! 作業スレッドの通知はタイムラインのイベント（`Actor::Codex`）に写す。Claude から MCP の
//! `codex_run` で呼ばれたスレッドは `parent_tool_use` に Claude のツール呼び出し ID を持ち、
//! クライアントはその下に表示する（cards/design/agent-co-op）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use fuzukue_proto::{Actor, ChatSettings, CodexModel, PermissionMode, ServerEvent};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, OnceCell};

use crate::agent_usage::RunMetadata;
use crate::front_desk::{self, RelayReply, TurnKind};
use crate::hub::Hub;
use crate::session_stats::{SessionStats, SessionStatsStore};
use crate::usage::Usage;

/// ツールの出力をタイムラインに載せるときの上限（文字数）。
const OUTPUT_LIMIT: usize = 4000;
const WORKER_PERMISSIONS: &str = "labo-worker";
pub(crate) const WORKER_APPROVAL_PREFIX: &str = "codex-worker-";

struct ThreadInfo {
    session: String,
    parent_tool_use: Option<String>,
    read_only: bool,
    worker_execution: Option<crate::worker_execution::Policy>,
    current_turn: Option<String>,
    exec: Option<(String, Arc<crate::cancellation::Cancellation>)>,
    /// 最後の final_answer
    last_answer: String,
    acted_or_asked: bool,
    waiter: Option<oneshot::Sender<Result<String>>>,
    compaction: Option<Compaction>,
    stats: Option<ThreadStats>,
    observed: RunMetadata,
    streamed: std::collections::HashSet<String>,
    relay_reply: Option<RelayReply>,
    partner: Option<PartnerOptions>,
    background_commands: crate::background_commands::Codex,
}

struct Compaction {
    complete: oneshot::Sender<Result<crate::commands::ContextCounts>>,
    cancel: Arc<crate::cancellation::Cancellation>,
    before: Option<u64>,
    after: Option<u64>,
    boundary: bool,
}

#[derive(Clone)]
pub(crate) struct PartnerOptions {
    pub settings: ChatSettings,
    pub context: String,
    pub mcp_url: String,
    pub mcp_key: String,
    pub agent_settings: fuzukue_proto::AgentSettings,
}

impl PartnerOptions {
    fn mode(&self) -> PermissionMode {
        self.settings.permission_mode
    }

    fn policy(&self) -> &'static str {
        match self.mode() {
            PermissionMode::Default => "untrusted",
            PermissionMode::Plan | PermissionMode::BypassPermissions => "never",
            _ => "on-request",
        }
    }

    fn sandbox(&self) -> &'static str {
        match self.mode() {
            PermissionMode::Default | PermissionMode::Plan => "read-only",
            _ => "workspace-write",
        }
    }

    fn turn_params(&self, project: &Path) -> Value {
        let sandbox = if self.sandbox() == "read-only" {
            json!({"type": "readOnly", "networkAccess": false})
        } else {
            json!({"type": "workspaceWrite", "writableRoots": [project], "networkAccess": false})
        };
        json!({
            "model": self.settings.model,
            "effort": self.settings.effort,
            "approvalPolicy": self.policy(),
            "approvalsReviewer": "user",
            "sandboxPolicy": sandbox,
            "summary": "auto",
        })
    }
}

fn model_entry(entry: &Value) -> Option<CodexModel> {
    let slug = entry["model"].as_str().or_else(|| entry["slug"].as_str())?;
    if slug.is_empty() || slug.contains("auto-review") || entry["hidden"] == true
        || entry["visibility"].as_str().is_some_and(|v| v != "list")
    {
        return None;
    }
    let effort_levels = entry["supportedReasoningEfforts"].as_array()
        .or_else(|| entry["supported_reasoning_efforts"].as_array())
        .into_iter().flatten()
        .filter_map(|v| v["reasoningEffort"].as_str().or_else(|| v["effort"].as_str()))
        .map(str::to_owned).collect();
    Some(CodexModel {
        slug: slug.into(),
        label: entry["displayName"].as_str().or_else(|| entry["display_name"].as_str())
            .unwrap_or(slug).into(),
        effort_levels,
        default_effort: entry["defaultReasoningEffort"].as_str()
            .or_else(|| entry["default_reasoning_level"].as_str()).map(str::to_owned),
    })
}

pub(crate) async fn cli_available() -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(5), Command::new("codex")
        .arg("--version").kill_on_drop(true).output())
        .await.is_ok_and(|result| result.is_ok_and(|output| output.status.success()))
}

#[cfg(test)]
pub(crate) fn cached_models() -> Vec<CodexModel> {
    Vec::new()
}

#[cfg(not(test))]
pub(crate) fn cached_models() -> Vec<CodexModel> {
    let path = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
    });
    let Some(cache) = path.and_then(|p| std::fs::read(p.join("models_cache.json")).ok())
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
    else { return Vec::new() };
    cache["models"].as_array().into_iter().flatten().filter_map(model_entry).collect()
}

struct PendingApproval {
    rpc_id: Value,
    thread_id: String,
    human_only: bool,
    permissions: Option<Value>,
    sweepable: bool,
    _alive: Arc<()>,
}

struct SummaryThread {
    codex: Arc<Codex>,
    thread: Option<String>,
}

impl Drop for SummaryThread {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        let codex = self.codex.clone();
        // An approval may be answered by the human while the one-shot is running.
        tokio::spawn(async move {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), codex.interrupt(&thread)).await;
            codex.close_worker(&thread).await;
        });
    }
}

#[derive(Default)]
struct ThreadStats {
    model: String,
    effort: Option<String>,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
    input_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    cache_read_tokens: u64,
}

impl ThreadStats {
    fn from_start(response: &Value) -> Self {
        Self {
            model: response["model"].as_str().unwrap_or_default().into(),
            effort: response["reasoningEffort"].as_str().filter(|effort| !effort.is_empty()).map(String::from),
            ..Default::default()
        }
    }

    fn update_usage(&mut self, stats: &mut SessionStats, usage: &Value) {
        let total = &usage["total"];
        stats.input_tokens = stats.input_tokens.saturating_add(increase(&mut self.input_tokens, &total["inputTokens"]));
        stats.output_tokens = stats.output_tokens.saturating_add(increase(&mut self.output_tokens, &total["outputTokens"]));
        if total["reasoningOutputTokens"].is_u64() {
            let thinking = increase(&mut self.thinking_tokens, &total["reasoningOutputTokens"]);
            stats.thinking_tokens = Some(stats.thinking_tokens.unwrap_or_default().saturating_add(thinking));
        }
        if total["cachedInputTokens"].is_u64() {
            let cached = increase(&mut self.cache_read_tokens, &total["cachedInputTokens"]);
            stats.cache_read_tokens = Some(stats.cache_read_tokens.unwrap_or_default().saturating_add(cached));
        }
        self.context_tokens = usage["last"]["inputTokens"].as_u64().or(self.context_tokens);
        self.context_window = usage["modelContextWindow"].as_u64().or(self.context_window);
        self.apply_metadata(stats);
    }

    fn apply_metadata(&self, stats: &mut SessionStats) {
        stats.model.clone_from(&self.model);
        stats.effort.clone_from(&self.effort);
        stats.context_tokens = self.context_tokens;
        stats.context_window = self.context_window;
    }
}

// 各スレッドの累計の増分を、会話全体に一度だけ加える。
fn increase(previous: &mut u64, value: &Value) -> u64 {
    let total = value.as_u64().unwrap_or(*previous).max(*previous);
    total - std::mem::replace(previous, total)
}

#[derive(Default)]
struct SkillRoots {
    // Older directories can still be referenced by already running threads.
    snapshots: HashMap<[u8; 32], tempfile::TempDir>,
    active: Option<[u8; 32]>,
    unsupported: bool,
}

fn approval_result(permissions: Option<Value>, decision: &str) -> Value {
    match permissions {
        Some(permissions) => json!({"scope": "turn", "permissions": if decision == "accept" && permissions.is_object() {
            permissions
        } else {
            json!({})
        }}),
        None => json!({"decision": decision}),
    }
}

pub struct Codex {
    last_activity: Mutex<std::time::Instant>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    threads: Mutex<HashMap<String, ThreadInfo>>,
    /// クライアントに見せる承認 ID → JSON-RPC の要求 ID
    approvals: Mutex<HashMap<String, PendingApproval>>,
    models: OnceCell<Vec<CodexModel>>,
    hub: Arc<Hub>,
    usage: Arc<Usage>,
    session_stats: Arc<SessionStatsStore>,
    project_dir: PathBuf,
    skill_roots: tokio::sync::Mutex<SkillRoots>,
    job: crate::job_process::Job,
}

impl Codex {
    pub async fn spawn(
        project_dir: &Path,
        hub: Arc<Hub>,
        usage: Arc<Usage>,
        session_stats: Arc<SessionStatsStore>,
    ) -> Result<Arc<Self>> {
        let mut command = Command::new("codex");
        command.arg("app-server");
        Self::spawn_command(command, project_dir, hub, usage, session_stats).await
    }

    pub(crate) async fn spawn_command(
        mut command: Command,
        project_dir: &Path,
        hub: Arc<Hub>,
        usage: Arc<Usage>,
        session_stats: Arc<SessionStatsStore>,
    ) -> Result<Arc<Self>> {
        command.current_dir(project_dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let (mut child, job) = crate::job_process::Job::spawn(&mut command, "codex", None)
            .context("failed to spawn codex app-server")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let codex = Arc::new(Self {
            last_activity: Mutex::new(std::time::Instant::now()),
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            models: OnceCell::new(),
            hub,
            usage,
            session_stats,
            project_dir: project_dir.to_path_buf(),
            skill_roots: tokio::sync::Mutex::default(),
            job,
        });

        let waiter = Arc::downgrade(&codex);
        tokio::spawn(async move {
            let _ = child.wait().await;
            if let Some(codex) = waiter.upgrade() {
                let _ = codex.job.stop();
            }
        });

        let reader = Arc::downgrade(&codex);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(reader) = reader.upgrade() else { return };
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) => reader.dispatch(v).await,
                    Err(e) => tracing::warn!("non-json line from codex: {e}"),
                }
            }
            let Some(reader) = reader.upgrade() else { return };
            let _ = reader.job.stop();
            reader.hub.publish(ServerEvent::Error { message: "codex app-server exited".into() });
            for (_, tx) in reader.pending.lock().unwrap().drain() {
                let _ = tx.send(Err("codex app-server exited".into()));
            }
            reader.cancel_all_questions();
            reader.threads.lock().unwrap().clear();
            reader.cancel_approvals(None, None);
        });

        tokio::time::timeout(std::time::Duration::from_secs(5),
            codex.request("initialize", json!({
                "clientInfo": { "name": "fuzukue", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true },
            }))).await??;
        codex.notify("initialized", json!({})).await?;
        // 使用量は最初に一度読み、あとは通知で追う。
        if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(3),
            codex.request("account/rateLimits/read", json!({}))).await {
            codex.usage.update_codex(&r["rateLimits"]);
        }
        Ok(codex)
    }

    pub(crate) fn idle_for(&self, duration: std::time::Duration) -> bool {
        self.last_activity.lock().unwrap().elapsed() >= duration
            && self.threads.lock().unwrap().values()
                .all(|thread| thread.background_commands.running().next().is_none())
            && self.approvals.lock().unwrap().is_empty()
    }

    pub(crate) fn collect_background_commands(
        &self,
        threads: &mut BTreeMap<String, Vec<fuzukue_proto::BackgroundCommand>>,
    ) {
        for thread in self.threads.lock().unwrap().values() {
            let mut commands = thread.background_commands.running().peekable();
            if commands.peek().is_some() {
                threads.entry(thread.session.clone()).or_default().extend(commands.cloned());
            }
        }
    }

    async fn write(&self, msg: Value) -> Result<()> {
        *self.last_activity.lock().unwrap() = std::time::Instant::now();
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{msg}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(err) = self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        rx.await.map_err(|_| anyhow!("codex dropped request {method}"))?.map_err(|e| anyhow!("{method}: {e}"))
    }

    pub async fn models(&self) -> Vec<CodexModel> {
        self.models.get_or_init(|| async {
            match tokio::time::timeout(std::time::Duration::from_secs(5), self.list_models()).await {
                Ok(Ok(models)) if !models.is_empty() => models,
                _ => cached_models(),
            }
        }).await.clone()
    }

    async fn list_models(&self) -> Result<Vec<CodexModel>> {
        let mut cursor: Option<String> = None;
        let mut models: Vec<CodexModel> = Vec::new();
        loop {
            let response = self.request("model/list", json!({ "cursor": cursor })).await?;
            let data = response["data"].as_array().context("model/list: no data")?;
            for model in data.iter().filter_map(model_entry) {
                if !models.iter().any(|entry| entry.slug == model.slug) {
                    models.push(model);
                }
            }
            let next = response["nextCursor"].as_str().map(String::from);
            if next.is_none() || cursor == next {
                return Ok(models);
            }
            cursor = next;
        }
    }

    pub async fn open_partner(
        &self, session: &str, saved: Option<&str>, mut options: PartnerOptions,
    ) -> Result<(String, bool)> {
        let mut roots = self.skill_roots.lock().await;
        self.set_skill_roots(&mut roots, Default::default()).await?;
        if let Some(id) = saved {
            let mut threads = self.threads.lock().unwrap();
            if let Some(thread) = threads.get_mut(id) {
                anyhow::ensure!(thread.waiter.is_none() && thread.compaction.is_none(), "Codex は実行中");
                let previous = thread.partner.as_ref().context("Codex パートナーが見つからない")?;
                options.mcp_key.clone_from(&previous.mcp_key);
                thread.partner = Some(options);
                return Ok((id.into(), false));
            }
        }
        let project = self.project_dir.clone();
        let agent_settings = options.agent_settings.clone();
        let policy = tokio::task::spawn_blocking(move || {
            let rules = crate::rules::resolve(&project)?;
            let mut policy = rules.policy(&project)?;
            policy.push_str(&crate::agents::directory(&crate::agents::load(&project, &rules)?, &agent_settings, fuzukue_proto::AgentCaller::FrontDesk));
            policy.push_str(&crate::rules::conversation_voice());
            policy.push_str(crate::mcp::front_desk_instructions());
            policy.push_str(crate::worker_scope::SHELL_READ_GUIDANCE);
            if !crate::canary() {
                policy.push_str(crate::mcp::workspace_read::GUIDANCE);
            }
            policy.push_str(crate::review_reports::prompt());
            policy.push_str(crate::references::ledger::hint());
            Ok::<_, anyhow::Error>(policy)
        }).await??;
        let params = json!({
            "cwd": self.project_dir,
            "model": options.settings.model,
            "sandbox": options.sandbox(),
            "approvalPolicy": options.policy(),
            "approvalsReviewer": "user",
            "developerInstructions": format!("{policy}{}\n\n{}{}", crate::git::prompt(), options.context, crate::graph::prompt()),
            "config": {
                "mcp_servers": {
                    (crate::mcp::SERVER_NAME): {
                        "url": options.mcp_url,
                        "http_headers": {"Authorization": format!("Bearer {}", options.mcp_key)},
                        "tool_timeout_sec": crate::capture::mcp_tool_timeout(),
                        "tools": crate::mcp::codex_tool_policy(None),
                    }
                }
            }
        });
        let (response, fresh) = self.start_or_resume(params, saved).await?;
        let id = self.register_thread(&response, session, None, false, Some(options))?;
        Ok((id, fresh))
    }

    async fn start_or_resume(&self, params: Value, saved: Option<&str>) -> Result<(Value, bool)> {
        if let Some(id) = saved {
            let mut resume = params.clone();
            resume["threadId"] = json!(id);
            resume["excludeTurns"] = json!(true);
            match self.request("thread/resume", resume).await {
                Ok(response) => return Ok((response, false)),
                Err(error) if missing_rollout(&error.to_string()) => {
                    tracing::warn!("Codex スレッド {id} を再開できない: {error:#}");
                }
                Err(error) => return Err(error),
            }
        }
        Ok((self.request("thread/start", params).await?, true))
    }

    pub(crate) fn owns_mcp(&self, session: &str, key: &str) -> bool {
        self.threads.lock().unwrap().values().any(|thread| {
            thread.session == session && thread.partner.as_ref().is_some_and(|p| p.mcp_key == key)
        })
    }

    pub(crate) fn exec_turn(&self, session: &str, key: &str) -> Option<(String, Arc<crate::cancellation::Cancellation>)> {
        let threads = self.threads.lock().unwrap();
        let thread = threads.values().find(|thread| thread.session == session && thread.waiter.is_some()
            && thread.partner.as_ref().is_some_and(|p| p.mcp_key == key && p.mode() != PermissionMode::Plan))?;
        let exec = thread.exec.as_ref()?;
        exec.1.check().ok()?;
        Some(exec.clone())
    }

    pub(crate) fn check_mcp(
        &self, session: &str, key: &str, writes: bool, always_confirm: bool,
    ) -> Result<Option<Actor>> {
        let threads = self.threads.lock().unwrap();
        let (id, thread) = threads.iter().find(|(_, thread)| {
            thread.session == session && thread.partner.as_ref().is_some_and(|p| p.mcp_key == key)
        }).context("Codex の MCP セッションが見つからない")?;
        anyhow::ensure!(thread.waiter.is_some(), "MCP の道具は実行中の Codex ターンで利用する");
        let mode = thread.partner.as_ref().unwrap().mode();
        anyhow::ensure!(!writes || mode != PermissionMode::Plan,
            "計画モードではプロジェクトを変更するツールを利用できない");
        let confirm = always_confirm || writes && mode == PermissionMode::Default;
        Ok(confirm.then(|| Actor::Codex { thread_id: id.clone(), parent_tool_use: None }))
    }

    /// 新しいスレッドを作る。作業ディレクトリへの書き込みは許し、それ以外は承認を求めさせる。
    pub async fn start_thread(&self, session: &str, parent_tool_use: Option<String>, launch: Option<&crate::delegate::WorkerLaunch>) -> Result<String> {
        // A legacy worker can start before its parent tool event arrives.
        let parent_tool_use = Some(parent_tool_use.unwrap_or_else(|| format!("codex-run:{}", crate::mail::session_key())));
        self.start_thread_with_policy(&self.project_dir, session, parent_tool_use, false, None, None, launch).await
    }

    pub async fn run_worker(
        &self, cwd: &Path, session: &str, job: &str, settings: ChatSettings, prompt: &str,
        steering: &mut crate::delegate::JobInput, launch: &crate::delegate::WorkerLaunch,
        metadata: &mut RunMetadata, on_model: impl Fn(&RunMetadata) + Send, resume: Option<&str>,
    ) -> Result<String> {
        anyhow::ensure!(launch.scope.is_some(), "worker filesystem scope is missing");
        let cancel = steering.cancellation();
        cancel.check()?;
        let thread = self.start_thread_with_policy(
            cwd, session, Some(job.into()), false, Some(settings), resume, Some(launch),
        ).await?;
        let observed = self.threads.lock().unwrap().get(&thread).map(|thread| RunMetadata {
            model: thread.observed.model.clone(),
            effort: thread.observed.effort.clone(),
            ..Default::default()
        });
        if let Some(observed) = observed {
            on_model(&observed);
        }
        if resume.is_some() {
            crate::handoff::workers::update(&self.hub, session, &thread, fuzukue_proto::HandoffWorkerStatus::Adopted);
        }
        let result = async {
            let mut prompt = prompt.to_owned();
            loop {
                let mut deferred = None;
                let answer = self.run_turn_with_schema(
                    &thread, &prompt, &[], None, Some(&cancel), Some((steering, &mut deferred)), TurnKind::Conversation, || Ok(()),
                ).await?;
                let Some(next) = deferred.or_else(|| steering.next_or_finish()) else {
                    anyhow::ensure!(!answer.trim().is_empty(), "The Codex worker returned no final report");
                    return Ok(answer);
                };
                prompt = next;
            }
        }.await;
        self.collect_metadata(&thread, metadata);
        self.close_worker(&thread).await;
        result
    }

    fn collect_metadata(&self, thread: &str, metadata: &mut RunMetadata) {
        if let Some(thread) = self.threads.lock().unwrap().get_mut(thread) {
            *metadata = std::mem::take(&mut thread.observed);
        }
    }

    pub async fn close_worker(&self, thread: &str) {
        if let Some(info) = self.threads.lock().unwrap().remove(thread) {
            self.hub.cancel_questions(&info.session, &Actor::Codex {
                thread_id: thread.into(), parent_tool_use: info.parent_tool_use,
            }, None);
        }
        self.cancel_approvals(Some(thread), None);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5),
            self.request("thread/unsubscribe", json!({ "threadId": thread }))).await;
    }

    pub async fn run_review(
        &self,
        project: &Path,
        session: &str,
        prompt: &str,
        settings: ChatSettings,
        cancel: &crate::cancellation::Cancellation,
        metadata: &mut RunMetadata,
        schema: Value,
    ) -> Result<String> {
        self.run_workspace_review(project, session, prompt, settings, cancel, metadata, schema, None).await
    }

    pub async fn run_workspace_review(
        &self,
        project: &Path,
        session: &str,
        prompt: &str,
        settings: ChatSettings,
        cancel: &crate::cancellation::Cancellation,
        metadata: &mut RunMetadata,
        schema: Value,
        launch: Option<&crate::delegate::WorkerLaunch>,
    ) -> Result<String> {
        cancel.check()?;
        let thread = self.start_thread_with_policy(project, session, None, true, Some(settings), None, launch).await?;
        let result = self.run_turn_with_schema(
            &thread, prompt, &[], Some(schema), Some(cancel), None, TurnKind::Conversation, || Ok(()),
        ).await;
        self.collect_metadata(&thread, metadata);
        self.close_worker(&thread).await;
        result
    }

    pub async fn worker_summary(
        self: &Arc<Self>, session: &str, evidence: &str, launch: &crate::delegate::WorkerLaunch, settings: ChatSettings,
        timeout: std::time::Duration, metadata: &mut RunMetadata,
    ) -> Result<String> {
        use std::time::Duration;
        let dir = tempfile::tempdir()?;
        let thread = tokio::time::timeout(Duration::from_secs(10), self.start_thread_with_policy(
            dir.path(), session, None, true, Some(settings), None, Some(launch),
        )).await??;
        let mut cleanup = SummaryThread { codex: self.clone(), thread: Some(thread.clone()) };
        let result = tokio::time::timeout(timeout,
            self.run_turn_with_schema(&thread, evidence, &[], None, None, None, TurnKind::Conversation, || Ok(())),
        ).await;
        if result.is_err() {
            let _ = tokio::time::timeout(Duration::from_secs(5), self.interrupt(&thread)).await;
        }
        self.collect_metadata(&thread, metadata);
        self.close_worker(&thread).await;
        cleanup.thread = None;
        result?
    }

    async fn set_skill_roots(&self, roots: &mut SkillRoots, skills: crate::rules::Skills) -> Result<()> {
        if roots.unsupported || (skills.is_empty() && roots.active.is_none()) {
            return Ok(());
        }
        let (digest, skills) = tokio::task::spawn_blocking(move || {
            Ok::<_, anyhow::Error>((skills.digest()?, skills))
        }).await??;
        if digest == roots.active {
            return Ok(());
        }
        if let Some(key) = digest {
            if let std::collections::hash_map::Entry::Vacant(entry) = roots.snapshots.entry(key) {
                let dir = tokio::task::spawn_blocking(move || {
                    let dir = tempfile::Builder::new().prefix("fuzukue-codex-skills-").tempdir()?;
                    skills.copy_to(dir.path())?;
                    Ok::<_, anyhow::Error>(dir)
                }).await??;
                entry.insert(dir);
            }
        }
        let paths: Vec<_> = digest.into_iter().map(|key| roots.snapshots[&key].path()).collect();
        if let Err(error) = self.request("skills/extraRoots/set", json!({ "extraRoots": paths })).await {
            if error.to_string().contains("-32601") {
                tracing::warn!("Codex does not support skills/extraRoots/set: {error}");
                roots.unsupported = true;
                return Ok(());
            }
            return Err(error);
        }
        roots.active = digest;
        Ok(())
    }

    async fn start_thread_with_policy(
        &self,
        project: &Path,
        session: &str,
        parent_tool_use: Option<String>,
        read_only: bool,
        worker: Option<ChatSettings>,
        saved: Option<&str>,
        launch: Option<&crate::delegate::WorkerLaunch>,
    ) -> Result<String> {
        let mut skill_roots = self.skill_roots.lock().await;
        let rule_project = if worker.is_some() { project.to_path_buf() } else { self.project_dir.clone() };
        let workspace_review = launch.is_some_and(crate::mcp::workspace_read::is_review_launch);
        let mut instructions = if let Some(launch) = launch.filter(|launch| !workspace_review && !launch.instructions.is_empty()) {
            self.set_skill_roots(&mut skill_roots, Default::default()).await?;
            launch.instructions.clone()
        } else {
            let (policy, skills) = tokio::task::spawn_blocking(move || {
                let rules = crate::rules::resolve(&rule_project)?;
                Ok::<_, anyhow::Error>((rules.policy(&rule_project)?, rules.skills(&rule_project)?))
            }).await??;
            self.set_skill_roots(&mut skill_roots, skills).await?;
            if parent_tool_use.is_some() {
                policy
            } else {
                format!("{policy}{}", crate::rules::conversation_voice())
            }
        };
        if workspace_review {
            instructions.push_str(crate::mcp::workspace_read::GUIDANCE);
        }
        if parent_tool_use.is_some() {
            instructions.push_str(&crate::rules::worker_voice());
        }
        let (sandbox, approval_policy) = if read_only {
            instructions.push_str(harness_text!("\n\nReview is read-only. Do not change the project or comment drafts, write to external services (including via MCP), or delegate edits.", "\n\n査読は読み取り専用。プロジェクトやコメントの下書きを変更しない。MCP を含む外部サービスへの書き込みや、変更作業の委任も行わない。"));
            ("read-only", "never")
        } else {
            ("workspace-write", "on-request")
        };
        let mut params = json!({
            "cwd": project,
            "sandbox": sandbox,
            "approvalPolicy": approval_policy,
            "developerInstructions": instructions,
        });
        if crate::canary() {
            params["approvalPolicy"] = json!("never");
            params["config"] = json!({
                "sandbox_workspace_write": { "network_access": false },
                "mcp_servers": {},
                "features": { "hooks": false, "plugins": false, "multi_agent": false, "multi_agent_v2": false, "skill_mcp_dependency_install": false },
            });
        }
        if let Some(worker) = &worker {
            let mode = worker.permission_mode;
            if !read_only && mode == PermissionMode::Default {
                params["sandbox"] = json!("read-only");
                params["approvalPolicy"] = json!("untrusted");
            }
            params["ephemeral"] = json!(true);
            params["model"] = json!(worker.model);
            params["approvalsReviewer"] = json!("user");
            params["config"] = json!({
                "model_reasoning_effort": worker.effort,
            });
        }
        let scope = launch.and_then(|launch| launch.scope.as_ref());
        let execution = launch.and_then(|launch| launch.execution);
        if let Some(policy) = execution {
            use crate::worker_execution::Approval;
            // bubblewrap, when selected, already constrains the app-server and its subprocesses.
            params["sandbox"] = json!("danger-full-access");
            params["approvalPolicy"] = json!(match policy.approval {
                Approval::Ask | Approval::Auto => "untrusted",
                Approval::Never => "never",
            });
            params["config"]["features"] = json!({
                "hooks": false, "plugins": false, "multi_agent": false, "multi_agent_v2": false,
                "skill_mcp_dependency_install": false,
            });
        } else if let Some(scope) = scope {
            let mut fs = serde_json::Map::new();
            fs.insert(":minimal".into(), json!("read"));
            for root in &scope.read_roots {
                if !scope.writable().iter().any(|writable| root.starts_with(writable)) {
                    fs.insert(root.to_string_lossy().into_owned(), json!("read"));
                }
            }
            let mode = worker.as_ref().map(|settings| settings.permission_mode);
            let workspace = if matches!(mode, Some(PermissionMode::Default | PermissionMode::Plan)) {
                "read"
            } else {
                "write"
            };
            fs.insert(scope.cwd.to_string_lossy().into_owned(), json!(workspace));
            fs.insert(scope.scratch.to_string_lossy().into_owned(), json!("write"));
            params.as_object_mut().unwrap().remove("sandbox");
            params["permissions"] = json!(WORKER_PERMISSIONS);
            params["runtimeWorkspaceRoots"] = json!([scope.cwd]);
            params["approvalPolicy"] = json!("never");
            params["config"]["default_permissions"] = json!(WORKER_PERMISSIONS);
            params["config"]["permissions"] = json!({
                (WORKER_PERMISSIONS): {
                    "filesystem": fs,
                    "network": { "enabled": false },
                }
            });
            params["config"]["features"] = json!({
                "hooks": false, "plugins": false, "multi_agent": false, "multi_agent_v2": false,
                "skill_mcp_dependency_install": false,
            });
            params["config"]["shell_environment_policy"] = json!({
                "set": scope.temp_env().into_iter().collect::<std::collections::BTreeMap<_, _>>()
            });
        }
        if read_only || scope.is_some() {
            params["config"]["mcp_servers"] = json!({});
        }
        if read_only && launch.is_some() {
            params["config"]["features"] = json!({
                "hooks": false, "plugins": false, "multi_agent": false, "multi_agent_v2": false,
                "skill_mcp_dependency_install": false,
            });
        }
        if let Some((launch, url)) = launch.and_then(|launch| launch.mcp_url.as_ref().map(|url| (launch, url))) {
            params["config"]["mcp_servers"] = json!({
                (crate::mcp::SERVER_NAME): {
                    "url": url,
                    "http_headers": {"Authorization": format!("Bearer {}", launch.key)},
                    "enabled_tools": launch.tools,
                    "tool_timeout_sec": crate::capture::worker_tool_timeout(&launch.tools),
                    "tools": crate::mcp::codex_tool_policy(Some(&launch.tools)),
                }
            });
        }
        let r = if let Some(id) = saved.filter(|_| scope.is_some()) {
            anyhow::ensure!(!self.threads.lock().unwrap().contains_key(id), "Codex thread is already attached in fuzukue");
            params["threadId"] = json!(id);
            params["excludeTurns"] = json!(true);
            params.as_object_mut().unwrap().remove("ephemeral");
            if params["model"].is_null() {
                params.as_object_mut().unwrap().remove("model");
                params["config"].as_object_mut().unwrap().remove("model_reasoning_effort");
            }
            let r = tokio::time::timeout(std::time::Duration::from_secs(15), self.request("thread/resume", params))
                .await.context("Codex thread/resume timed out")??;
            anyhow::ensure!(r["thread"]["id"] == id, "Codex resumed a different thread");
            let active = r["thread"]["status"]["type"] == "active";
            let cwd_matches = r["cwd"].as_str().is_some_and(|cwd| Path::new(cwd) == project);
            if active || !cwd_matches {
                self.close_worker(id).await;
                anyhow::ensure!(!active, "Codex thread still has an active turn");
                bail!("Codex resumed in a different working directory");
            }
            r
        } else {
            self.start_or_resume(params, saved).await?.0
        };
        drop(skill_roots);
        if scope.is_some() && execution.is_none() && (r["activePermissionProfile"]["id"] != WORKER_PERMISSIONS
            || r["approvalPolicy"] != "never")
        {
            if let Some(thread) = r["thread"]["id"].as_str() {
                self.close_worker(thread).await;
            }
            bail!("Codex did not activate the worker filesystem permissions profile");
        }
        if let Some(policy) = execution {
            let expected = if policy.approval == crate::worker_execution::Approval::Never { "never" } else { "untrusted" };
            if r["sandbox"]["type"] != "dangerFullAccess" || r["approvalPolicy"] != expected {
                if let Some(thread) = r["thread"]["id"].as_str() {
                    self.close_worker(thread).await;
                }
                bail!("Codex did not activate the requested worker execution policy");
            }
        }
        let id = self.register_thread(&r, session, parent_tool_use, read_only, None)?;
        if let Some(thread) = self.threads.lock().unwrap().get_mut(&id) {
            thread.worker_execution = execution;
        }
        Ok(id)
    }

    fn register_thread(
        &self, r: &Value, session: &str, parent_tool_use: Option<String>, read_only: bool,
        partner: Option<PartnerOptions>,
    ) -> Result<String> {
        let id = r["thread"]["id"].as_str().context("Codex: no thread id")?.to_string();
        if partner.is_some() {
            self.session_stats.identify(session, fuzukue_proto::AgentBackend::Codex, &id);
        }
        let stats = (!read_only && !parent_tool_use.as_deref().is_some_and(|id| id.starts_with(fuzukue_proto::DELEGATE_JOB))).then(|| {
            let thread = ThreadStats::from_start(r);
            self.session_stats.update(&self.hub, session, "codex", |stats| {
                thread.apply_metadata(stats);
            });
            thread
        });
        self.threads.lock().unwrap().insert(
            id.clone(),
            ThreadInfo {
                session: session.into(),
                parent_tool_use,
                read_only,
                worker_execution: None,
                current_turn: None,
                exec: None,
                last_answer: String::new(),
                acted_or_asked: false,
                waiter: None,
                compaction: None,
                stats,
                observed: RunMetadata {
                    model: r["model"].as_str().map(str::to_owned),
                    effort: r["reasoningEffort"].as_str().map(str::to_owned),
                    tokens: None,
                    cost_usd: None,
                },
                streamed: Default::default(),
                relay_reply: None,
                partner,
                background_commands: Default::default(),
            },
        );
        Ok(id)
    }

    /// ターンを始め、終わるまで待って最後の答えを返す。途中経過はタイムラインに流れる。
    pub async fn run_turn(&self, thread_id: &str, text: &str) -> Result<String> {
        self.run_turn_with_images(thread_id, text, &[], TurnKind::Conversation, || Ok(())).await
    }

    pub(crate) async fn run_turn_with_images(
        &self, thread_id: &str, text: &str, images: &[PathBuf], kind: TurnKind,
        before_submit: impl FnOnce() -> Result<()>,
    ) -> Result<String> {
        self.run_turn_with_schema(thread_id, text, images, None, None, None, kind, before_submit).await
    }

    pub(crate) fn partner_acted_or_asked(&self, thread_id: &str) -> bool {
        self.threads.lock().unwrap().get(thread_id).is_some_and(|t| t.acted_or_asked)
    }

    /// `schema` を渡すと、最後の返事をその JSON Schema に従わせる（turn/start の outputSchema）。
    async fn run_turn_with_schema(
        &self,
        thread_id: &str,
        text: &str,
        images: &[PathBuf],
        schema: Option<Value>,
        cancel: Option<&crate::cancellation::Cancellation>,
        steering: Option<(&mut crate::delegate::JobInput, &mut Option<String>)>,
        kind: TurnKind,
        before_submit: impl FnOnce() -> Result<()>,
    ) -> Result<String> {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        if !steering.as_ref().is_some_and(|(input, _)| input.paced) {
            if let Some((until, reason)) = self.usage.blocked("codex") {
                bail!("Codex は使用量の上限に近いので休んでいる（{reason}、再開できるのは Unix 時刻 {until}）。Codex に回さずに進めて。");
            }
        }
        let exec = crate::mcp::exec::Turn::default();
        let (tx, mut rx) = oneshot::channel();
        let overrides = {
            let mut threads = self.threads.lock().unwrap();
            let t = threads.get_mut(thread_id).context("unknown codex thread")?;
            if t.waiter.is_some() || t.compaction.is_some() {
                bail!("codex thread {thread_id} is already running a turn");
            }
            before_submit()?;
            t.waiter = Some(tx);
            t.exec = kind.user_conversation().then(|| (exec.id.clone(), exec.cancel.clone()));
            t.last_answer.clear();
            t.acted_or_asked = false;
            t.streamed.clear();
            t.relay_reply = matches!(kind, TurnKind::WorkerDigest).then(RelayReply::default);
            t.partner.as_ref().map(|options| {
                if let Some(stats) = &mut t.stats {
                    if let Some(model) = &options.settings.model {
                        stats.model.clone_from(model);
                    }
                    stats.effort.clone_from(&options.settings.effort);
                    self.session_stats.update(&self.hub, &t.session, "codex", |s| stats.apply_metadata(s));
                }
                options.turn_params(&self.project_dir)
            })
        };
        let started = self
            .request(
                "turn/start",
                {
                    let mut input = vec![json!({ "type": "text", "text": self.usage.context_line() })];
                    input.extend(turn_input(text, images));
                    let mut params = json!({ "threadId": thread_id, "input": input });
                    if let Some(overrides) = overrides {
                        params.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
                    }
                    if let Some(schema) = schema {
                        params["outputSchema"] = schema;
                    }
                    params
                },
            )
            .await;
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                if let Some(t) = self.threads.lock().unwrap().get_mut(thread_id) {
                    t.waiter = None;
                    t.relay_reply = None;
                }
                return Err(error);
            }
        };
        if let (Some(cancel), Some((input, deferred)), Some(turn_id)) =
            (cancel, steering, started["turn"]["id"].as_str())
        {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    answer = &mut rx => return answer.context("codex turn was dropped")?,
                    Some(message) = input.recv() => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break,
                            result = self.steer_turn(thread_id, turn_id, &message, &[]) => {
                                if result.is_err() {
                                    // A turn may have just ended on the app-server. Continue in the same thread.
                                    *deferred = Some(message);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(cancel) = cancel {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    if matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                        // A stop can arrive before the turn/started notification.
                        let turn = started["turn"]["id"].as_str().map(String::from).or_else(|| {
                            self.threads.lock().unwrap().get(thread_id).and_then(|t| t.current_turn.clone())
                        }).context("turn/start: no id")?;
                        self.request("turn/interrupt", json!({ "threadId": thread_id, "turnId": turn })).await?;
                        let _ = rx.await;
                    }
                    return Err(crate::cancellation::Cancelled.into());
                }
                answer = &mut rx => return answer.context("codex turn was dropped")?,
            }
        }
        rx.await.context("codex turn was dropped")?
    }

    async fn steer_turn(&self, thread_id: &str, turn_id: &str, text: &str, images: &[PathBuf]) -> Result<()> {
        self.request("turn/steer", json!({
            "threadId": thread_id,
            "expectedTurnId": turn_id,
            "input": turn_input(text, images),
        })).await?;
        Ok(())
    }

    pub(crate) async fn steer_with_images(&self, thread_id: &str, text: &str, images: &[PathBuf]) -> Result<()> {
        let turn = self.threads.lock().unwrap().get(thread_id).and_then(|t| t.current_turn.clone())
            .context("Codex のターンは終了した")?;
        self.steer_turn(thread_id, &turn, text, images).await
    }

    pub(crate) async fn compact(
        self: &Arc<Self>, thread_id: &str, cancel: &Arc<crate::cancellation::Cancellation>,
    ) -> Result<crate::commands::ContextCounts> {
        cancel.check()?;
        let (complete, mut result) = oneshot::channel();
        {
            let mut threads = self.threads.lock().unwrap();
            let thread = threads.get_mut(thread_id).context("unknown codex thread")?;
            anyhow::ensure!(thread.waiter.is_none() && thread.compaction.is_none() && thread.current_turn.is_none(),
                "Codex は実行中");
            thread.compaction = Some(Compaction {
                complete, cancel: cancel.clone(), before: thread.stats.as_ref().and_then(|stats| stats.context_tokens),
                after: None, boundary: false,
            });
        }

        if let Err(error) = self.request("thread/compact/start", json!({ "threadId": thread_id })).await {
            if let Some(thread) = self.threads.lock().unwrap().get_mut(thread_id) {
                thread.compaction = None;
            }
            if method_unavailable(&error) {
                return Err(crate::commands::NativeCompactionUnavailable.into());
            }
            return Err(error);
        }

        tokio::select! {
            biased;
            answer = &mut result => answer.context("Codex compaction was dropped")?,
            _ = cancel.cancelled() => {
                self.interrupt(thread_id).await?;
                let _ = result.await;
                Err(crate::cancellation::Cancelled.into())
            }
        }
    }

    pub async fn interrupt(&self, thread_id: &str) -> Result<()> {
        let turn = self.threads.lock().unwrap().get(thread_id).and_then(|t| {
            if let Some((_, cancel)) = &t.exec {
                cancel.cancel();
            }
            t.current_turn.clone()
        });
        if let Some(turn) = turn {
            self.request("turn/interrupt", json!({ "threadId": thread_id, "turnId": turn })).await?;
        }
        Ok(())
    }

    pub(crate) async fn interrupt_for_swap(&self) -> Result<()> {
        let threads: Vec<_> = self.threads.lock().unwrap().iter()
            .filter(|(_, thread)| thread.current_turn.is_some())
            .map(|(id, _)| id.clone()).collect();
        futures_util::future::try_join_all(threads.iter().map(|id| self.interrupt(id))).await?;
        Ok(())
    }

    /// 承認要求に答える。`decision` は "accept" / "decline" など app-server の値そのまま。
    pub async fn resolve(&self, request_id: &str, decision: &str, automatic: bool) -> Result<bool> {
        if request_id.starts_with(WORKER_APPROVAL_PREFIX) {
            anyhow::ensure!(matches!(decision, "accept" | "decline"), "Worker approvals are for a single request");
        }
        let pending = {
            let mut approvals = self.approvals.lock().unwrap();
            if approvals.get(request_id).is_some_and(|approval| {
                automatic && approval.human_only
                    || approval.sweepable && !self.hub.claim_legacy_approval(request_id)
            }) {
                return Ok(false);
            }
            approvals.remove(request_id)
        };
        let Some(pending) = pending else { return Ok(false) };
        let result = approval_result(pending.permissions, decision);
        if let Err(error) = self.write(json!({ "jsonrpc": "2.0", "id": pending.rpc_id, "result": result })).await {
            self.approval_cancelled(request_id.into());
            return Err(error);
        }
        Ok(true)
    }

    fn approval_cancelled(&self, request_id: String) {
        self.hub.publish(ServerEvent::ApprovalResolved {
            request_id, decision: "cancelled".into(), automatic: false,
        });
    }

    fn cancel_approvals(&self, thread: Option<&str>, rpc_id: Option<&Value>) {
        self.approvals.lock().unwrap().retain(|request, approval| {
            let matches = thread.is_none_or(|thread| approval.thread_id == thread)
                && rpc_id.is_none_or(|id| &approval.rpc_id == id);
            if matches && (!approval.sweepable || self.hub.claim_legacy_approval(request)) {
                self.approval_cancelled(request.clone());
            }
            !matches
        });
    }

    fn register_approval(
        self: &Arc<Self>, thread_id: &str, rpc_id: Value, summary: fuzukue_proto::LocalizedMessage,
        permissions: Option<Value>, full: Option<crate::hub::approval_store::Registration>,
    ) -> Result<crate::hub::approval_store::FullAuthorization> {
        use crate::hub::approval_store::FullAuthorization::*;
        let threads = self.threads.lock().unwrap();
        let Some(thread) = threads.get(thread_id) else { return Ok(AlreadyConsumed) };
        let mut approvals = self.approvals.lock().unwrap();
        if approvals.values().any(|pending| pending.rpc_id == rpc_id) {
            tracing::warn!(%rpc_id, "Codex reused a pending approval RPC ID");
            return Ok(AlreadyConsumed);
        }
        let actor = Actor::Codex { thread_id: thread_id.into(), parent_tool_use: thread.parent_tool_use.clone() };
        // The authenticated app-server binding identifies the worker. Even a literal
        // command description cannot constrain what a Codex permission grant executes.
        let worker = thread.parent_tool_use.is_some();
        let prefix = if worker { WORKER_APPROVAL_PREFIX } else { "codex-" };
        let request_id = format!("{prefix}{}", crate::mail::session_key());
        let alive = Arc::new(());
        let event = ServerEvent::ApprovalRequested {
            session: thread.session.clone(), request_id: request_id.clone(), actor,
            summary: crate::l10n::render(fuzukue_i18n::Locale::JAPANESE, &summary), localized: Some(summary), shell_directory: None, shell_prefix: None,
        };
        let sweepable = full.is_some();
        if let Some(registration) = full {
            let backend = Arc::downgrade(self);
            let resume_id = request_id.clone();
            let reply_id = rpc_id.clone();
            let reply_permissions = permissions.clone();
            let runtime = tokio::runtime::Handle::current();
            let authorization = self.hub.publish_sweepable_approval(event, Arc::downgrade(&alive), registration, move |granted| {
                runtime.spawn(async move {
                    if let Some(backend) = backend.upgrade() {
                        backend.finish_full_approval(&resume_id, reply_id, reply_permissions, granted).await;
                    }
                });
            })?;
            if authorization != Human {
                return Ok(authorization);
            }
        } else {
            self.hub.publish_local_approval(event, &alive);
        }
        approvals.insert(request_id, PendingApproval {
            rpc_id, thread_id: thread_id.into(), human_only: worker || thread.partner.is_some(),
            permissions, sweepable, _alive: alive,
        });
        Ok(Human)
    }

    async fn finish_full_approval(&self, request: &str, rpc_id: Value, permissions: Option<Value>, granted: bool) {
        self.approvals.lock().unwrap().remove(request);
        let result = approval_result(permissions, if granted { "accept" } else { "decline" });
        if let Err(error) = self.write(json!({ "jsonrpc": "2.0", "id": rpc_id, "result": result })).await {
            tracing::warn!(%error, request, "Could not deliver a full approval to Codex");
        }
    }

    fn thread_context(&self, thread_id: &str) -> Option<(String, Actor, bool)> {
        let threads = self.threads.lock().unwrap();
        let t = threads.get(thread_id)?;
        Some((
            t.session.clone(),
            Actor::Codex { thread_id: thread_id.into(), parent_tool_use: t.parent_tool_use.clone() },
            t.read_only,
        ))
    }

    async fn dispatch(self: &Arc<Self>, v: Value) {
        *self.last_activity.lock().unwrap() = std::time::Instant::now();
        let method = v["method"].as_str();
        match (method, v.get("id")) {
            // 自分の要求への応答
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let r = match v.get("error") {
                        Some(e) => Err(e.to_string()),
                        None => Ok(v["result"].clone()),
                    };
                    let _ = tx.send(r);
                }
            }
            // app-server からの要求（承認など）
            (Some(m), Some(id)) => self.on_server_request(m, id.clone(), &v["params"]).await,
            (Some(m), None) => self.on_notification(m, &v["params"]),
            _ => {}
        }
    }

    async fn on_server_request(self: &Arc<Self>, method: &str, id: Value, p: &Value) {
        let thread_id = p["threadId"].as_str().unwrap_or("");
        if method == "item/tool/requestUserInput" {
            self.request_user_input(id, thread_id, p).await;
            return;
        }
        let thread = self.thread_context(thread_id);
        if matches!(thread, Some((_, _, true))) {
            tracing::debug!(thread_id, method, params = %p, "Codex reviewer request");
        }
        let reason = p["reason"].as_str().map(|reason| format!(" · {reason}")).unwrap_or_default();
        let permissions = (method == "item/permissions/requestApproval" && matches!(&thread,
            Some((_, Actor::Codex { parent_tool_use: Some(_), .. }, false))))
            .then(|| p["permissions"].clone());
        let summary = match method {
            "item/commandExecution/requestApproval" => Some(fuzukue_proto::LocalizedMessage::new(
                "approval-run-command",
                [("command", p["command"].as_str().unwrap_or("?").into()), ("reason", reason)],
            )),
            "item/fileChange/requestApproval" => Some(fuzukue_proto::LocalizedMessage::new(
                "approval-change-files", [("reason", reason)],
            )),
            "item/permissions/requestApproval" if permissions.is_some() => Some(fuzukue_proto::LocalizedMessage::new(
                "approval-grant-turn-permissions", [("permissions", p["permissions"].to_string()), ("reason", reason)],
            )),
            _ => None,
        };
        let mut full = None;
        if let (Some(_), Some((session, Actor::Codex { parent_tool_use: Some(job), .. }, false))) = (&summary, &thread) {
            let execution = self.threads.lock().unwrap().get(thread_id).and_then(|thread| thread.worker_execution);
            let request = format!("{thread_id}:{id}");
            let registration = crate::full_approval::worker_registration(session, job, "codex", method, &request, p);
            let decision = self.hub.authorize_full_approval(registration.clone());
            full = Some(registration);
            use crate::hub::approval_store::FullAuthorization::*;
            let accept = match decision {
                Ok(Granted) => Some(true),
                Ok(AlreadyConsumed) => return,
                Ok(Human) if execution.is_some_and(|policy| !policy.full && policy.approval != crate::worker_execution::Approval::Ask) => Some(true),
                Ok(Human) => None,
                Err(error) => {
                    tracing::warn!(%error, %job, "Codex worker approval not granted");
                    Some(false)
                }
            };
            if let Some(accept) = accept {
                let result = approval_result(permissions, if accept { "accept" } else { "decline" });
                let _ = self.write(json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
                return;
            }
        }
        match (summary, thread) {
            (Some(_), Some((_, _, true))) => {
                let _ = self.write(json!({ "jsonrpc": "2.0", "id": id, "result": { "decision": "decline" } })).await;
            }
            (Some(summary), Some((_, _, false))) => {
                use crate::hub::approval_store::FullAuthorization::*;
                let accept = match self.register_approval(thread_id, id.clone(), summary, permissions.clone(), full) {
                    Ok(Human | AlreadyConsumed) => return,
                    Ok(Granted) => true,
                    Err(error) => {
                        tracing::warn!(%error, thread_id, "Codex approval could not be registered");
                        false
                    }
                };
                let result = approval_result(permissions, if accept { "accept" } else { "decline" });
                let _ = self.write(json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
            }
            _ => {
                // 扱えない要求は断る（放っておくと Codex が止まったままになる）。
                let _ = self
                    .write(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("fuzukue does not handle {method}") } }))
                    .await;
            }
        }
    }

    async fn request_user_input(self: &Arc<Self>, id: Value, thread_id: &str, params: &Value) {
        if params["isBlocking"] == false {
            let _ = self.write(json!({ "jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": "Fuzukue does not support asynchronous questions."
            } })).await;
            return;
        }
        let Ok(questions) = crate::questions::parse(params, false) else {
            let _ = self.write(json!({ "jsonrpc": "2.0", "id": id, "error": {
                "code": -32602, "message": "The question request was invalid."
            } })).await;
            return;
        };
        let receiver = {
            let mut threads = self.threads.lock().unwrap();
            threads.get_mut(thread_id).filter(|thread| !thread.read_only).map(|thread| {
                thread.acted_or_asked = true;
                let actor = Actor::Codex { thread_id: thread_id.into(), parent_tool_use: thread.parent_tool_use.clone() };
                self.hub.ask_question(thread.session.clone(), actor, id.clone(), questions)
            })
        };
        let Some(receiver) = receiver else {
            let _ = self.write(crate::questions::codex_response(id, None)).await;
            return;
        };
        let codex = Arc::downgrade(self);
        tokio::spawn(async move {
            let Ok(reply) = receiver.await else { return };
            let Some(codex) = codex.upgrade() else { return };
            if codex.write(crate::questions::codex_response(id, reply)).await.is_err() {
                tracing::warn!("Could not write Codex question response");
            }
        });
    }

    fn cancel_all_questions(&self) {
        for (id, info) in self.threads.lock().unwrap().iter() {
            self.hub.cancel_questions(&info.session, &Actor::Codex {
                thread_id: id.clone(), parent_tool_use: info.parent_tool_use.clone(),
            }, None);
        }
    }

    fn compaction_event(self: &Arc<Self>, id: &str, method: &str, params: &Value) -> bool {
        let mut threads = self.threads.lock().unwrap();
        let Some(thread) = threads.get_mut(id) else { return false };
        let Some(compaction) = &mut thread.compaction else { return false };
        match method {
            "turn/started" => {
                thread.current_turn = params["turn"]["id"].as_str().map(String::from);
                if compaction.cancel.check().is_err() {
                    let codex = self.clone();
                    let id = id.to_owned();
                    tokio::spawn(async move { let _ = codex.interrupt(&id).await; });
                }
            }
            "item/completed" if params["item"]["type"] == "contextCompaction" => {
                compaction.boundary = true;
            }
            "thread/compacted" => compaction.boundary = true,
            "thread/tokenUsage/updated" => {
                compaction.after = params["tokenUsage"]["last"]["inputTokens"].as_u64()
                    .or(compaction.after);
            }
            "turn/completed" => {
                let compaction = thread.compaction.take().unwrap();
                thread.current_turn = None;
                let result = match params["turn"]["status"].as_str() {
                    Some("interrupted") => Err(crate::cancellation::Cancelled.into()),
                    Some("completed") if compaction.boundary => Ok((compaction.before, compaction.after)),
                    _ => Err(anyhow!("{}", params["turn"]["error"]["message"].as_str().unwrap_or("Codex の圧縮が完了しなかった"))),
                };
                let _ = compaction.complete.send(result);
            }
            _ => {}
        }
        true
    }

    fn on_notification(self: &Arc<Self>, method: &str, p: &Value) {
        if method == "account/rateLimits/updated" {
            self.usage.update_codex(&p["rateLimits"]);
            return;
        }
        let thread_id = p["threadId"].as_str().unwrap_or("");
        let Some((session, actor, read_only)) = self.thread_context(thread_id) else { return };
        match method {
            "serverRequest/resolved" => {
                if let Some(id) = p.get("requestId") {
                    self.hub.cancel_questions(&session, &actor, Some(id));
                    self.cancel_approvals(Some(thread_id), Some(id));
                }
                return;
            }
            "turn/completed" | "thread/closed" => {
                self.hub.cancel_questions(&session, &actor, None);
                self.cancel_approvals(Some(thread_id), None);
            }
            _ => {}
        }
        if read_only {
            tracing::debug!(thread_id, method, params = %p, "Codex reviewer notification");
        }
        let item = &p["item"];
        let item_id = item["id"].as_str().unwrap_or("").to_string();
        if method == "item/started" && !item_id.is_empty() {
            let native = match item["type"].as_str() {
                Some("commandExecution") => Some("exec"),
                Some("fileChange") => Some("apply_patch"),
                _ => None,
            };
            let full = self.threads.lock().unwrap().get(thread_id)
                .is_some_and(|thread| thread.worker_execution.is_some_and(|policy| policy.full));
            if full {
                if let (Some(tool), Actor::Codex { parent_tool_use: Some(job), .. }) = (native, &actor) {
                    crate::full_approval::observe_worker_tool(&self.hub, &session, job, "codex", tool, &item_id);
                }
            }
        }
        let delegated = matches!(&actor, Actor::Codex { parent_tool_use: Some(job), .. }
            if job.starts_with(fuzukue_proto::DELEGATE_JOB));
        if !read_only && !delegated
            && matches!(method, "item/started" | "item/completed" | "turn/completed" | "thread/reverted")
        {
            if let Some(thread) = self.threads.lock().unwrap().get_mut(thread_id) {
                thread.background_commands.observe(method, p);
            }
        }
        if self.compaction_event(thread_id, method, p) && method != "thread/tokenUsage/updated" {
            return;
        }
        // Plan calls are hidden from the timeline, so observe the raw item.
        if matches!(method, "item/started" | "item/completed")
            && item["type"] == "mcpToolCall" && item["server"] == crate::mcp::SERVER_NAME
            && item["tool"].as_str().is_some_and(crate::front_desk::is_work_tool)
        {
            if let Some(t) = self.threads.lock().unwrap().get_mut(thread_id) {
                t.acted_or_asked = true;
            }
        }
        match method {
            "thread/tokenUsage/updated" => {
                if let Some(t) = self.threads.lock().unwrap().get_mut(thread_id) {
                    let total = &p["tokenUsage"]["total"];
                    if let Some(tokens) = total["totalTokens"].as_u64().or_else(|| {
                        Some(total["inputTokens"].as_u64()?.saturating_add(total["outputTokens"].as_u64()?))
                    }) {
                        t.observed.tokens = Some(tokens);
                    }
                    if let Some(stats) = &mut t.stats {
                        self.session_stats.update(&self.hub, &session, "codex", |current| {
                            stats.update_usage(current, &p["tokenUsage"]);
                        });
                    }
                }
            }
            "turn/started" => {
                if let Some(t) = self.threads.lock().unwrap().get_mut(thread_id) {
                    t.current_turn = p["turn"]["id"].as_str().map(String::from);
                }
            }
            "turn/plan/updated" if delegated => {
                let mut text = String::from("**作業計画**\n");
                if let Some(explanation) = p["explanation"].as_str().filter(|s| !s.is_empty()) {
                    text.push_str(&format!("\n{explanation}\n"));
                }
                for step in p["plan"].as_array().into_iter().flatten() {
                    let Some(description) = step["step"].as_str() else { continue };
                    let status = match step["status"].as_str() {
                        Some("completed") => "完了",
                        Some("inProgress") => "作業中",
                        _ => "未着手",
                    };
                    text.push_str(&format!("\n- {status} · {description}"));
                }
                self.hub.publish(ServerEvent::TextDelta {
                    session, item: format!("plan:{}", crate::mail::session_key()), actor, text,
                });
            }
            "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/plan/delta"
                if !read_only && (method != "item/plan/delta" || delegated) => {
                let item = p["itemId"].as_str().unwrap_or("");
                let summary = method == "item/reasoning/summaryTextDelta";
                let item = if summary {
                    format!("{item}:summary:{}", p["summaryIndex"].as_u64().unwrap_or(0))
                } else if method == "item/plan/delta" {
                    format!("plan:{item}")
                } else {
                    item.into()
                };
                let delta = p["delta"].as_str().unwrap_or("");
                let first = {
                    let mut threads = self.threads.lock().unwrap();
                    let Some(t) = threads.get_mut(thread_id) else { return };
                    let first = t.streamed.insert(item.clone());
                    if let Some(reply) = &mut t.relay_reply {
                        if !summary {
                            reply.push(item, delta.into());
                        }
                        return;
                    }
                    first
                };
                let text = if summary && first { format!("考え方の要約\n\n{delta}") } else { delta.into() };
                self.hub.publish(ServerEvent::TextDelta { session, item, actor, text });
            }
            "item/started" | "item/completed"
                if item["type"] == "mcpToolCall" && item["server"] == crate::mcp::SERVER_NAME
                    && matches!(item["tool"].as_str(), Some("plan" | "replan" | "consult")) => {}
            "item/started" if !read_only => match item["type"].as_str() {
                Some("commandExecution") => self.hub.publish(ServerEvent::ToolCall {
                    session,
                    item: item_id,
                    actor,
                    name: "exec".into(),
                    input: json!({ "command": item["command"] }),
                }),
                Some("mcpToolCall") => self.hub.publish(ServerEvent::ToolCall {
                    session,
                    item: item_id,
                    actor,
                    name: format!("{}.{}", item["server"].as_str().unwrap_or("?"), item["tool"].as_str().unwrap_or("?")),
                    input: item["arguments"].clone(),
                }),
                _ => {}
            },
            "item/completed" if item["type"] == "agentMessage" => {
                let text = item["text"].as_str().unwrap_or("");
                let streamed = {
                    let mut threads = self.threads.lock().unwrap();
                    let Some(t) = threads.get_mut(thread_id) else { return };
                    if t.partner.is_some() {
                        t.acted_or_asked |= crate::front_desk::asks_question(text);
                    }
                    if item["phase"].is_null() || item["phase"] == "final_answer" {
                        t.last_answer = text.into();
                    }
                    let streamed = t.streamed.contains(&item_id);
                    if let Some(reply) = &mut t.relay_reply {
                        if !streamed {
                            reply.push(item_id, text.into());
                        }
                        return;
                    }
                    streamed
                };
                if !read_only && !streamed && !text.is_empty() {
                    self.hub.publish(ServerEvent::TextDelta {
                        session, item: item_id, actor, text: text.into(),
                    });
                }
            }
            "item/completed" if !read_only => match item["type"].as_str() {
                Some("plan") if delegated => {
                    let item_id = format!("plan:{item_id}");
                    let streamed = self.threads.lock().unwrap().get(thread_id)
                        .is_some_and(|t| t.streamed.contains(&item_id));
                    if let Some(text) = item["text"].as_str().filter(|text| !streamed && !text.is_empty()) {
                        self.hub.publish(ServerEvent::TextDelta { session, item: item_id, actor, text: text.into() });
                    }
                }
                Some("reasoning") => {
                    if self.threads.lock().unwrap().get(thread_id).is_some_and(|t| t.relay_reply.is_some()) {
                        return;
                    }
                    for (index, text) in item["summary"].as_array().into_iter().flatten().enumerate() {
                        let Some(text) = text.as_str() else { continue };
                        let item = format!("{item_id}:summary:{index}");
                        let streamed = self.threads.lock().unwrap().get(thread_id)
                            .is_some_and(|t| t.streamed.contains(&item));
                        if !streamed {
                            self.hub.publish(ServerEvent::TextDelta {
                                session: session.clone(), item, actor: actor.clone(), text: format!("考え方の要約\n\n{text}"),
                            });
                        }
                    }
                }

                Some("commandExecution") => self.hub.publish(ServerEvent::ToolResult {
                    session,
                    item: item_id,
                    ok: item["exitCode"].as_i64() == Some(0),
                    output: truncate(item["aggregatedOutput"].as_str().unwrap_or("")),
                }),
                Some("mcpToolCall") => {
                    let result = &item["result"];
                    let error = &item["error"];
                    let output = if !error.is_null() {
                        error.as_str().or_else(|| error["message"].as_str())
                            .unwrap_or("MCP tool call failed").to_owned()
                    } else if item["server"] == crate::mcp::SERVER_NAME
                        && matches!(item["tool"].as_str(), Some(crate::capture::CAPTURE | "build_papers"))
                    {
                        result["content"].as_array().map(|parts| crate::capture::text_blocks(parts)).unwrap_or_default()
                    } else {
                        result.to_string()
                    };
                    self.hub.publish(ServerEvent::ToolResult {
                        session, item: item_id,
                        ok: error.is_null() && result["isError"] != true && item["status"] != "failed",
                        output: truncate(&output),
                    });
                }
                Some("fileChange") => {
                    let changes = item["changes"].as_array().cloned().unwrap_or_default();
                    let paths: Vec<_> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
                    let diff: String = changes.iter().filter_map(|c| c["diff"].as_str()).collect::<Vec<_>>().join("\n");
                    self.hub.publish(ServerEvent::ToolCall {
                        session: session.clone(),
                        item: item_id.clone(),
                        actor,
                        name: "edit".into(),
                        input: json!({ "paths": paths }),
                    });
                    self.hub.publish(ServerEvent::ToolResult {
                        session,
                        item: item_id,
                        ok: item["status"].as_str() != Some("failed"),
                        output: truncate(&diff),
                    });
                }
                _ => {}
            },
            "turn/completed" => {
                let waiter = {
                    let mut threads = self.threads.lock().unwrap();
                    threads.get_mut(thread_id).and_then(|t| {
                        t.current_turn = None;
                        if let Some((_, cancel)) = t.exec.take() { cancel.cancel(); }
                        if t.stats.is_some() && t.waiter.is_some() {
                            self.session_stats.update(&self.hub, &session, "codex", |stats| {
                                stats.turns = stats.turns.saturating_add(1);
                            });
                        }
                        let result = match p["turn"]["status"].as_str() {
                            Some("interrupted") if t.partner.is_some() => Err(crate::cancellation::Cancelled.into()),
                            Some(status) if status != "completed" && status != "interrupted" => {
                                let message = p["turn"]["error"]["message"].as_str().unwrap_or(status);
                                Err(anyhow!("Codex のターンが完了しなかった（{message}）"))
                            }
                            _ => Ok(t.last_answer.clone()),
                        };
                        let reply = t.relay_reply.take();
                        t.waiter.take().map(|w| (w, result, reply))
                    })
                };
                if !read_only && !self.threads.lock().unwrap().get(thread_id).is_some_and(|t| t.partner.is_some()) {
                    self.hub.publish(ServerEvent::TurnEnded { session: session.clone(), actor: actor.clone() });
                }
                if let Some((w, mut answer, reply)) = waiter {
                    if let Some(reply) = reply {
                        reply.publish(&self.hub, &session, actor);
                        if let Ok(text) = &mut answer {
                            if front_desk::suppress_relay_reply(text) {
                                text.clear();
                            }
                        }
                    }
                    let _ = w.send(answer);
                }
            }
            _ => {}
        }
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        self.cancel_all_questions();
        self.cancel_approvals(None, None);
    }
}

fn method_unavailable(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.strip_prefix("thread/compact/start: ")
        .and_then(|details| serde_json::from_str::<Value>(details).ok())
        .is_some_and(|details| details["code"] == -32601
            || details["message"].as_str().is_some_and(|message| {
                let message = message.to_ascii_lowercase();
                message.contains("unknown method") || message.contains("method not found")
            }))
}

fn missing_rollout(error: &str) -> bool {
    ["no rollout", "not found", "no such thread", "unknown thread"].iter()
        .any(|message| error.to_ascii_lowercase().contains(message))
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= OUTPUT_LIMIT {
        s.to_string()
    } else {
        s.chars().take(OUTPUT_LIMIT).collect::<String>() + "\n…（省略）"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn consultation_uses_explicit_mcp_approval_in_read_only_thread() -> Result<()> {
        let project = tempfile::tempdir()?;
        let log = project.path().join("rpc.jsonl");
        let hub = Arc::new(Hub::new());
        let mut fixture = Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/codex"));
        fixture.env("FUZUKUE_CODEX_FIXTURE_LOG", &log);
        let codex = Codex::spawn_command(fixture, project.path(), hub.clone(),
            Arc::new(Usage::new(hub.clone())), Default::default()).await?;
        let launch = crate::delegate::WorkerLaunch {
            execution: None,
            instructions: String::new(), scope: None, key: "fixture-key".into(),
            mcp_url: Some("http://127.0.0.1:7878/mcp?session=owner&job=consult".into()),
            tools: crate::mcp::planning_tool_names(true),
        };
        let thread = codex.start_thread_with_policy(project.path(), "owner", None, true,
            Some(ChatSettings::default()), None, Some(&launch)).await?;
        let requests: Vec<Value> = std::fs::read_to_string(log)?.lines()
            .map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
        let start = &requests.iter().find(|item| item["method"] == "thread/start").context("missing thread/start")?["params"];
        assert_eq!(start["sandbox"], "read-only");
        assert_eq!(start["approvalPolicy"], "never");
        assert_eq!(start["ephemeral"], true);
        let server = &start["config"]["mcp_servers"]["fuzukue"];
        assert_eq!(server["enabled_tools"], json!(["search_library"]));
        assert_eq!(server["tools"], json!({"search_library": {"approval_mode": "approve"}}));
        assert!(server.get("default_tools_approval_mode").is_none());
        codex.close_worker(&thread).await;

        let thread = codex.start_thread("owner", Some("delegate:fixture".into()), None).await?;
        let mut events = hub.subscribe(u64::MAX).1;
        let message = "MCP tool call requires approval, but approval policy is never";
        for (error, result, expected) in [
            (json!({"message": message}), Value::Null, message),
            (Value::Null, json!({"isError": true}), r#"{"isError":true}"#),
        ] {
            codex.on_notification("item/completed", &json!({"threadId": thread, "item": {
                "id": "mcp-fixture", "type": "mcpToolCall", "server": "fuzukue",
                "tool": "search_library", "error": error, "result": result,
            }}));
            assert!(matches!(events.try_recv()?.event,
                ServerEvent::ToolResult { ok: false, output, .. } if output == expected));
        }
        codex.close_worker(&thread).await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delegated_codex_approval_waits_for_one_human_reply_and_cancels_on_backend_resolution() -> Result<()> {
        let project = tempfile::tempdir()?;
        let log = project.path().join("rpc.jsonl");
        let hub = Arc::new(Hub::new());
        let mut fixture = Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/codex"));
        fixture.env("FUZUKUE_CODEX_FIXTURE_LOG", &log);
        let codex = Codex::spawn_command(fixture, project.path(), hub.clone(),
            Arc::new(Usage::new(hub.clone())), Default::default()).await?;
        let thread = codex.start_thread("owner", Some("delegate:fixture".into()), None).await?;
        let mut requests = hub.subscribe(u64::MAX).1;
        let action = json!({"threadId": thread, "command": "cat README.md", "cwd": project.path()});
        let method = "item/commandExecution/requestApproval";
        let backend_id = json!(91);
        codex.on_server_request(method, backend_id.clone(), &action).await;
        let ServerEvent::ApprovalRequested { request_id: first, actor, .. } = requests.recv().await?.event else {
            bail!("expected a live Codex approval");
        };
        assert!(matches!(actor, Actor::Codex { parent_tool_use: Some(job), .. } if job == "delegate:fixture"));
        assert!(hub.live_approval_threads().contains("owner"));
        assert!(!codex.resolve(&first, "accept", true).await?);
        codex.on_notification("serverRequest/resolved", &json!({"threadId":thread, "requestId":backend_id}));
        assert!(matches!(requests.recv().await?.event, ServerEvent::ApprovalResolved { request_id, .. } if request_id == first));
        assert!(!hub.live_approval_threads().contains("owner"));

        codex.on_server_request(method, backend_id.clone(), &action).await;
        let ServerEvent::ApprovalRequested { request_id: second, .. } = requests.recv().await?.event else {
            bail!("expected a new Codex approval");
        };
        assert_ne!(first, second);
        assert!(!codex.resolve(&first, "accept", false).await?);
        assert!(codex.resolve(&second, "acceptForSession", false).await.is_err());
        let (first, duplicate) = tokio::join!(codex.resolve(&second, "decline", false), codex.resolve(&second, "accept", false));
        assert!(first? && !duplicate?);
        assert!(!hub.live_approval_threads().contains("owner"));

        codex.on_server_request("item/permissions/requestApproval", json!(92), &json!({
            "threadId": thread, "permissions": {"network": {"enabled": true}},
        })).await;
        let ServerEvent::ApprovalRequested { request_id: grant, .. } = requests.recv().await?.event else {
            bail!("expected a human permission approval");
        };
        assert!(!codex.resolve(&grant, "accept", true).await?);
        assert!(codex.resolve(&grant, "decline", false).await?);
        codex.close_worker(&thread).await;

        let legacy = codex.start_thread("owner", Some("legacy-tool-use".into()), None).await?;
        let mut requests = hub.subscribe(u64::MAX).1;
        codex.on_server_request(method, json!(93), &json!({
            "threadId": legacy, "command": "git push", "cwd": project.path(),
        })).await;
        let ServerEvent::ApprovalRequested { request_id, .. } = requests.recv().await?.event else {
            bail!("expected a legacy worker approval");
        };
        assert!(request_id.starts_with(WORKER_APPROVAL_PREFIX));
        assert!(!codex.resolve(&request_id, "accept", true).await?);
        assert!(codex.resolve(&request_id, "decline", false).await?);
        codex.close_worker(&legacy).await;

        // Resolve a live RPC under full mode, then replay it after changing leases.
        hub.init_approval_queue("fixture".into(), project.path());
        let change = fuzukue_proto::FullApprovalChange::Full { duration: fuzukue_proto::FullApprovalDuration::OneHour };
        let principal = crate::remote::SettingsPrincipal::fixture();
        let full = codex.start_thread("owner", Some("delegate:full".into()), None).await?;
        let push = json!({"threadId": full, "command": "git push", "cwd": project.path()});
        codex.on_server_request(method, json!(94), &push).await;
        let ServerEvent::ApprovalRequested { request_id: swept, .. } = requests.recv().await?.event else {
            bail!("expected a pending Codex approval before enable");
        };
        hub.set_full_approval(change.clone(), &principal)?;
        assert!(matches!(requests.recv().await?.event, ServerEvent::ApprovalResolved {
            request_id, decision, automatic: true,
        } if request_id == swept && decision == "accept"));
        assert!(!codex.resolve(&swept, "decline", false).await?);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while codex.approvals.lock().unwrap().contains_key(&swept) {
                tokio::task::yield_now().await;
            }
        }).await?;
        hub.set_full_approval(fuzukue_proto::FullApprovalChange::Off, &principal)?;
        hub.set_full_approval(change, &principal)?;
        codex.on_server_request(method, json!(94), &push).await;
        assert!(hub.live_approval_threads().is_empty());
        let audit = hub.full_approval_settings(None)?.audit;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].approval_id, swept);
        assert_eq!(audit[0].granted_by, "full_mode");
        assert_eq!(audit[0].action.args, "[redacted]");
        codex.close_worker(&full).await;

        let replies = std::fs::read_to_string(log)?.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|message| message.get("method").is_none())
            .collect::<Vec<_>>();
        assert_eq!(replies, [
            json!({"jsonrpc":"2.0", "id":backend_id, "result":{"decision":"decline"}}),
            json!({"jsonrpc":"2.0", "id":92, "result":{"scope":"turn", "permissions":{}}}),
            json!({"jsonrpc":"2.0", "id":93, "result":{"decision":"decline"}}),
            json!({"jsonrpc":"2.0", "id":94, "result":{"decision":"accept"}}),
        ]);
        Ok(())
    }

    #[test]
    fn maps_thread_usage_to_session_stats() {
        let hub = Hub::new();
        let store = SessionStatsStore::default();
        let (_, mut events) = hub.subscribe(0);
        let start: Value = serde_json::from_str(
            r#"{"thread":{"id":"t1"},"model":"gpt-6","reasoningEffort":"high"}"#,
        ).unwrap();
        let usage: Value = serde_json::from_str(r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"turn-1","tokenUsage":{"total":{"inputTokens":1300,"cachedInputTokens":200,"outputTokens":150,"reasoningOutputTokens":40,"totalTokens":1450},"last":{"inputTokens":800,"cachedInputTokens":200,"outputTokens":100,"reasoningOutputTokens":30,"totalTokens":900},"modelContextWindow":250000}}}"#).unwrap();
        let partial: Value = serde_json::from_str(
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t1","tokenUsage":{"total":{"outputTokens":155}}}}"#,
        ).unwrap();
        let child: Value = serde_json::from_str(
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t2","tokenUsage":{"total":{"inputTokens":50,"outputTokens":10},"last":{"inputTokens":50}}}}"#,
        ).unwrap();

        let mut parent = ThreadStats::from_start(&start);
        store.update(&hub, "chat", "codex", |stats| parent.apply_metadata(stats));
        assert!(matches!(events.try_recv().unwrap().event,
            ServerEvent::SessionStats { model, effort: Some(effort), context_window: None, input_tokens: 0, .. }
            if model == "gpt-6" && effort == "high"
        ));
        for (notification, output) in [(&usage, 150), (&usage, 150), (&partial, 155)] {
            store.update(&hub, "chat", "codex", |stats| parent.update_usage(stats, &notification["params"]["tokenUsage"]));
            let envelope = events.try_recv().unwrap();
            assert_eq!(envelope.seq, 0);
            assert_eq!(serde_json::to_value(envelope.event).unwrap(), json!({
                "type": "session_stats",
                "session": "chat",
                "agent": "codex",
                "model": "gpt-6",
                "effort": "high",
                "cost_usd": null,
                "input_tokens": 1300,
                "cache_read_tokens": 200,
                "output_tokens": output,
                "thinking_tokens": 40,
                "context_tokens": 800,
                "context_window": 250000,
                "turns": 0,
            }));
        }
        let mut child_thread = ThreadStats::from_start(&json!({"model": "gpt-6-mini"}));
        store.update(&hub, "chat", "codex", |stats| child_thread.update_usage(stats, &child["params"]["tokenUsage"]));
        assert!(matches!(events.try_recv().unwrap().event,
            ServerEvent::SessionStats { session, model, effort: None, input_tokens: 1350, output_tokens: 165,
                thinking_tokens: Some(40), context_tokens: Some(50), context_window: None, .. }
            if session == "chat" && model == "gpt-6-mini"
        ));
    }
}

fn turn_input(text: &str, images: &[PathBuf]) -> Vec<Value> {
    let mut input = vec![json!({ "type": "text", "text": text })];
    input.extend(images.iter().map(|path| json!({ "type": "localImage", "path": path })));
    input
}
