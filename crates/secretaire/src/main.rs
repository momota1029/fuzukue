//! secretaire: Ubuntu (WSL) 側で常駐し、エージェントとプロジェクトのファイルを持つ。
//! Windows 側の GPUI クライアントは ws://127.0.0.1:7878/ws に繋ぐ
//! （WSL2 の localhost 転送でそのまま届く）。

#[macro_use]
mod harness;

mod screenshots;
mod capture;

mod l10n;
mod auto_review;
mod auto_paths;
mod build;
mod build_jobs;
mod cancellation;
mod cards;
mod comments;
mod commands;
mod lean;
mod library;
mod memos;
mod mutations;
mod claude;
mod codex;
mod agents;
mod agent_usage;
mod isolated_model;
mod delegate;
mod extensions;
mod confined_fs;
mod exploration;
mod git;
mod github;
mod github_auth;
mod issue_threads;
mod handoff;
mod workshop;
mod graph;
mod human_edits;
mod history;
mod branch_review;
mod lean_reading;
mod hub;
mod folder_grants;
mod full_approval;
mod worker_execution;
mod mail;
mod inbox;
mod job_process;
mod folder_attachment;
#[cfg(test)]
mod folder_drop_regression;
mod routing;
mod questions;
mod mcp;
mod paper_refs;
mod pdf_audit;
mod pdf_cards;
mod recipes;
mod references;
mod remote;
mod device_tokens;
mod durable;
mod distribution;
mod android_distribution;
mod reviewers;
mod course;
mod review_findings;
mod review_reports;
mod rules;
mod session_stats;
mod source;
mod file_browser;
mod source_editor;
mod self_update;
mod service;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod server_memory;
mod state;
mod send_latency;
mod synctex;
mod threads;
mod tiers;
mod trash;
mod front_desk;
mod thread_titles;
mod thread_artifacts;
#[cfg(test)]
mod test_support;
mod usage;
mod planner;
mod background_commands;
mod chat_waiting;
mod project_tests;
mod lsp;
mod workspace;
mod workspace_requests;
mod workspaces;
mod watch;
mod worker_scope;

#[cfg(test)]
mod integration_tests;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures_util::{FutureExt, SinkExt, StreamExt};
use fuzukue_proto::AgentBackend;
use fuzukue_proto::{CardSaveOutcome, ClientMsg, Envelope, ServerEvent, PROTOCOL_VERSION};
use tokio::sync::Mutex;

use crate::claude::ClaudeSession;
use crate::codex::Codex;
use crate::front_desk::TurnKind;
use crate::hub::Hub;
use crate::session_stats::SessionStatsStore;
use crate::usage::Usage;
use crate::workspaces::App;

static SERVER_START: std::sync::LazyLock<u128> = std::sync::LazyLock::new(|| {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
});
static USER_ITEM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// セッションごとに、Claude が呼んだ `codex_run` のツール呼び出し ID を呼び出し順に積む。
pub type ToolQueue = Arc<std::sync::Mutex<HashMap<String, VecDeque<String>>>>;

pub struct Workspace {
    id: String,
    registry: std::sync::Weak<App>,
    deck: std::sync::RwLock<String>,
    deck_view: cards::browse::Cache,
    hub: Arc<Hub>,
    usage: Arc<Usage>,
    session_stats: Arc<SessionStatsStore>,
    project_dir: PathBuf,
    addr: String,
    sessions: Mutex<HashMap<String, Arc<ClaudeSession>>>,
    #[cfg(test)]
    claude_program: Option<PathBuf>,
    prompt_start: Mutex<()>,
    folder_copy_slots: Arc<tokio::sync::Semaphore>,
    front_desk_nudge: bool,
    hold_planner_during_update: bool,
    delegates: delegate::Workers,
    worker_approval_policy: std::sync::Mutex<fuzukue_proto::WorkerApprovalPolicy>,
    worker_execution: worker_execution::StoredSettings,
    worker_codex: std::sync::Mutex<Vec<std::sync::Weak<Codex>>>,
    shell_approval_rules: worker_scope::shell_rules::Rules,
    drain: self_update::drain::Journal,
    drain_codex: std::sync::Mutex<HashMap<String, i64>>,
    agent_usage: agent_usage::Log,
    plans: planner::Plans,
    agent_settings: std::sync::Mutex<fuzukue_proto::AgentSettings>,
    codex: Mutex<Option<Arc<Codex>>>,
    tool_queue: ToolQueue,
    issues_refresh: Arc<tokio::sync::Notify>,
    last_issues: github::LastIssues,
    research_issues: workshop::Queue,
    issue_threads: issue_threads::Watch,
    github_auth: github_auth::Auth,
    watches: watch::Watches,
    handoff: handoff::State,
    builds: build_jobs::Builds,
    tests: project_tests::Tests,
    lsp: lsp::Manager,
    reviews: reviewers::RunningReviews,
    review_settings: std::sync::Mutex<fuzukue_proto::ReviewSettings>,
    file_save: Arc<Mutex<()>>,
    git_lock: Mutex<()>,
    branch_review_lock: Mutex<()>,
    reference_lock: Arc<Mutex<()>>,
    reading_page_export: Arc<Mutex<()>>,
    ledger: std::sync::Mutex<references::ledger::State>,
    ledger_fetch: Arc<Mutex<()>>,
    trash_lock: Mutex<()>,
    graph: graph::WorkspaceGraph,
    library: std::sync::Mutex<library::Index>,
    extensions: extensions::Extensions,
    lean_check: Mutex<()>,
    /// Lean の宣言ごとの状態（最後に確かめた結果）
    lean_status: std::sync::Mutex<Vec<(String, String)>>,
    lean_doc_findings: std::sync::Mutex<Vec<fuzukue_proto::LeanDocFinding>>,
    /// コメントの読み込み、変更、スナップショットの配信をまとめて直列化する。
    comment_edit: Mutex<()>,
    review_reports: Mutex<review_reports::State>,
    comment_applications: Mutex<HashMap<String, comments::Application>>,
    /// 人の編集の控え（cards/design/human-edits.md）。
    human_edits: human_edits::Pending,
    history: Arc<history::History>,
    /// 会話ごとの記録（作業ディレクトリ、Claude のセッション ID）。ディスクにも置く。
    session_records: Arc<std::sync::Mutex<std::collections::BTreeMap<String, state::SessionRecord>>>,
    state_dir: PathBuf,
    screenshots: screenshots::Screenshots,
    mail: Option<Arc<mail::Store>>,
}

impl Workspace {
    fn new(project_dir: PathBuf, addr: String, state_dir: PathBuf, hub: Arc<Hub>) -> Self {
        let id = workspaces::id_for(&project_dir);
        hub.init_approval_queue(id.clone(), &state_dir);
        let research_issues = workshop::Queue::new(&project_dir, &state_dir, &id);
        let saved = hub.load_threads(&project_dir, &state_dir);
        if let Err(error) = hub.restore_full_approval() {
            tracing::warn!(%error, workspace = %id, "Could not restore full approvals");
        }
        let mut workspace = Self {
            usage: Arc::new(Usage::new(hub.clone())),
            session_stats: Arc::default(),
            history: history::History::new(&project_dir, &state_dir, hub.clone()),
            hub,
            id,
            research_issues,
            issue_threads: Default::default(),
            github_auth: github_auth::Auth::new(&state_dir),
            watches: watch::Watches::load(&project_dir, &state_dir),
            handoff: Default::default(),
            registry: Default::default(),
            deck: Default::default(),
            deck_view: Default::default(),
            lsp: lsp::Manager::new(&project_dir),
            project_dir,
            addr,
            sessions: Mutex::new(HashMap::new()),
            #[cfg(test)]
            claude_program: None,
            prompt_start: Mutex::new(()),
            folder_copy_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            front_desk_nudge: !canary() && front_desk::nudge_enabled(),
            hold_planner_during_update: true,
            codex: Mutex::new(None),
            tool_queue: Arc::default(),
            issues_refresh: Arc::default(),
            last_issues: Arc::default(),
            builds: Default::default(),
            tests: project_tests::Tests::load(&state_dir),
            reviews: Default::default(),
            review_settings: std::sync::Mutex::new(state::load_review_settings(&state_dir)),
            file_save: Arc::default(),
            git_lock: Mutex::new(()),
            branch_review_lock: Mutex::new(()),
            reference_lock: Arc::default(),
            reading_page_export: Arc::default(),
            ledger: Default::default(),
            ledger_fetch: Arc::default(),
            trash_lock: Mutex::new(()),
            graph: Default::default(),
            library: Default::default(),
            extensions: Default::default(),
            lean_check: Mutex::new(()),
            lean_status: std::sync::Mutex::new(Vec::new()),
            lean_doc_findings: std::sync::Mutex::new(Vec::new()),
            comment_edit: Mutex::new(()),
            review_reports: Default::default(),
            comment_applications: Mutex::new(HashMap::new()),
            human_edits: Default::default(),
            session_records: Arc::new(std::sync::Mutex::new(saved)),
            delegates: delegate::Workers::new(),
            worker_approval_policy: std::sync::Mutex::new(state::load_worker_approval_policy(&state_dir)),
            worker_execution: Default::default(),
            worker_codex: Default::default(),
            shell_approval_rules: worker_scope::shell_rules::Rules::load(&state_dir),
            drain: Default::default(),
            drain_codex: Default::default(),
            agent_usage: Default::default(),
            plans: Default::default(),
            agent_settings: std::sync::Mutex::new(state::load_agent_settings(&state_dir)),
            state_dir,
            screenshots: Default::default(),
            mail: None,
        };
        if !canary() && self_update::drain::Journal::exists(&workspace.state_dir) {
            if let Ok(records) = workspace.drain.records(&workspace.state_dir) {
                for record in records {
                    if let self_update::drain::Work::Usage(windows) = &record.work {
                        workspace.usage.restore(windows.clone());
                    }
                    mutations::recover(&workspace, &record);
                }
            }
        }
        workspace.restore_review_failures();
        workspace
    }

    fn admit_work(&self) -> anyhow::Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        self.update().map(|update| update.admit()).transpose()
    }

    fn update(&self) -> Option<Arc<self_update::Update>> {
        self.registry.upgrade().and_then(|app| app.self_update.get().cloned())
    }

    fn planner_work_held(&self) -> bool {
        self.hold_planner_during_update && self.update().is_some_and(|update| update.planner_work_held())
    }

    fn start_planner_work<T>(&self, start: impl FnOnce() -> T) -> Option<T> {
        if self.hold_planner_during_update {
            if let Some(update) = self.update() {
                return update.start_planner_work(start);
            }
        }
        Some(start())
    }

    fn save_drain_checkpoint(&self, session: &str, job: &str, wait: i64, directory: PathBuf, result: String, completed: bool) {
        let id = format!("drain-{wait}-{job}-{}", mail::session_key());
        let state = if completed { "Finished" } else { "Review required before resuming" };
        let result = format!("{state}. Job {job}.\n\n{result}");
        if self.hub.has_thread(session) {
            self.hub.publish(ServerEvent::TextDelta {
                session: session.into(), item: id.clone(), actor: fuzukue_proto::Actor::System,
                text: format!("Server drain checkpoint — {state} ({job}). Recovery directory: {}. The result and resume record are in {}.", directory.display(), self.state_dir.join("held-work.json").display()),
            });
        }
        self.drain.defer(&self.state_dir, session, id, self_update::drain::Work::Checkpoint { result, directory });
    }

    fn current_deck(&self) -> String {
        self.deck.read().unwrap().clone()
    }

    #[cfg_attr(test, allow(dead_code))]
    fn start_watchers(self: &Arc<Self>) {
        let _ = issue_threads::watch(self);
        if canary() {
            return;
        }
        self.hub.watch_approval_expiry();
        watch::start(self);
        handoff::watch(self);
        self.check_lean();
        planner::restore(self);
        if fuzukue_proto::compat::env("FUZUKUE_APPROVALS").map(|v| v != "ask").unwrap_or(true) {
            auto_approve(self.clone());
        }
        let changed = Arc::new(tokio::sync::Notify::new());
        if let Err(err) = cards::watch(self.clone(), changed.clone()) {
            tracing::warn!("card watcher disabled: {err}");
        }
        references::watch(self.clone(), changed);
        references::ledger::watch(self);
        trash::sweep(self);
        self.watch_idle_agents();
    }

    async fn thread_context(&self, session: &str) -> anyhow::Result<String> {
        let attached = self.hub.thread_attachments(session);
        if attached.is_empty() {
            return Ok(String::new());
        }
        let project = self.project_dir.clone();
        Ok(tokio::task::spawn_blocking(move || workspace::thread_context(&project, &attached)).await?)
    }

    async fn partner_input(&self, session: &str, prompt: String) -> anyhow::Result<String> {
        if !threads::is_thread(session) || self.issue_threads.intake_turn(session) {
            return Ok(prompt);
        }
        let attachments = self.thread_context(session).await?;
        let context = if attachments.is_empty() {
            harness_text!("This thread has no attached decks.", "このスレッドに付いているデッキはない。")
        } else {
            &attachments
        };
        let reports = issue_threads::report_context(self, session);
        let requests = self.registry.upgrade()
            .map(|registry| registry.workspace_requests.front_desk_context(self, session))
            .unwrap_or_default();
        Ok(harness_format!("(secretaire) Current thread attachments\n{context}{reports}{requests}\n\nTurn input:\n{prompt}",
            "(secretaire) このターンの添付先\n{context}{reports}{requests}\n\nターンの入力:\n{prompt}"))
    }

    fn record_artifacts<'a>(&self, session: &str, paths: impl IntoIterator<Item = &'a str>) {
        let paths: Vec<_> = paths.into_iter()
            .filter_map(|path| thread_artifacts::relative_path(&self.project_dir, path)).collect();
        if !paths.is_empty() {
            if let Err(e) = self.hub.edit_threads(|store| store.record_artifacts(session, paths)) {
                tracing::warn!("スレッドの成果物を保存できない: {e:#}");
            }
        }
    }

    async fn finish_comment_application(&self, session: &str, outcome: anyhow::Result<()>) {
        review_reports::finish_fix(self, session, outcome.is_ok()).await;
        let application = self.comment_applications.lock().await.remove(session);
        if let Some(application) = application {
            match tokio::task::spawn_blocking(move || application.finish(outcome)).await {
                Ok(event) => self.hub.publish(event),
                Err(err) => tracing::warn!("コメント反映の結果を記録できない: {err}"),
            }
        }
    }

    fn chat_settings(&self, session: &str) -> fuzukue_proto::ChatSettings {
        self.hub.role_settings(session).map(|(settings, _)| settings).unwrap_or_else(|| {
            self.session_records.lock().unwrap().get(session)
                .map(|record| record.chat_settings.clone()).unwrap_or_default()
        })
    }

    fn record_session(&self, session: &str, f: impl FnOnce(&mut state::SessionRecord)) {
        let mut recs = self.session_records.lock().unwrap();
        f(recs.entry(session.to_string()).or_default());
        state::save_sessions(&self.state_dir, &recs);
    }
}

impl Workspace {
    fn partner_turn_settings(&self, session: &str) -> fuzukue_proto::ChatSettings {
        self.hub.refresh_model_defaults();
        self.chat_settings(session)
    }

    /// Codex の app-server は、最初に要るときに起動する。
    async fn codex(&self) -> anyhow::Result<Arc<Codex>> {
        let mut slot = self.codex.lock().await;
        if let Some(codex) = slot.as_ref() {
            return Ok(codex.clone());
        }
        let codex = Codex::spawn(&self.project_dir, self.hub.clone(), self.usage.clone(), self.session_stats.clone()).await?;
        *slot = Some(codex.clone());
        Ok(codex)
    }

    async fn worker_codex(&self, launch: &delegate::WorkerLaunch) -> anyhow::Result<Arc<Codex>> {
        let Some(policy) = launch.execution else { return self.codex().await };
        let scope = launch.scope.as_ref().context("worker filesystem scope is missing")?;
        let mut command = policy.command(Path::new("codex"), scope).await?;
        policy.backend_home(&mut command, &scope.scratch, "CODEX_HOME", ".codex", &["auth.json", "config.toml"])?;
        command.arg("app-server").envs(scope.temp_env());
        let codex = Codex::spawn_command(command, &scope.cwd, self.hub.clone(), self.usage.clone(), self.session_stats.clone()).await?;
        let mut workers = self.worker_codex.lock().unwrap();
        workers.retain(|worker| worker.strong_count() > 0);
        workers.push(Arc::downgrade(&codex));
        Ok(codex)
    }

    async fn running_codex(&self) -> Option<Arc<Codex>> {
        self.codex.lock().await.clone()
    }

    async fn codex_models(&self) -> Vec<fuzukue_proto::CodexModel> {
        if let Some(codex) = self.running_codex().await {
            return codex.models().await;
        }
        match self.registry.upgrade() {
            Some(registry) => registry.codex_models(self).await,
            None => codex::cached_models(),
        }
    }

    fn trim_idle_caches(&self, idle: Duration) {
        if let Ok(mut library) = self.library.try_lock() {
            library.trim_idle(idle);
        }
        self.graph.trim_idle(idle);
        self.hub.trim_idle_transcripts(idle);
    }

    fn watch_idle_agents(self: &Arc<Self>) {
        const IDLE: Duration = Duration::from_secs(5 * 60);
        let workspace = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(workspace) = workspace.upgrade() else { break };
                workspace.trim_idle_caches(Duration::from_secs(2 * 60));
                {
                    let mut slot = workspace.codex.lock().await;
                    if slot.as_ref().is_some_and(|codex| Arc::strong_count(codex) == 1 && codex.idle_for(IDLE))
                    {
                        *slot = None;
                    }
                }
                let mut sessions = workspace.sessions.lock().await;
                let ids: Vec<_> = sessions.keys().cloned().collect();
                for id in ids {
                    let session = &sessions[&id];
                    // The session map and stdout reader each own one reference.
                    if Arc::strong_count(session) <= 2 && session.retire_if_idle(IDLE).await {
                        sessions.remove(&id);
                    }
                }
            }
        });
    }

    fn check_lean(self: &Arc<Self>) -> tokio::task::JoinHandle<anyhow::Result<Vec<(String, String)>>> {
        let app = self.clone();
        tokio::spawn(async move {
            let _check = app.lean_check.lock().await;
            let _work = app.admit_work()?;
            let current = app.lean_status.lock().unwrap().clone();
            app.hub.publish_snapshot(ServerEvent::LeanStatus { decls: current, running: true, doc_findings: Vec::new() });
            let doc_findings = lean_reading::doc_check(&app.project_dir).await.map(|check| check.findings).unwrap_or_default();
            let result = lean::check_reading(&app.project_dir).await.map(|m| m.into_iter().collect::<Vec<_>>());
            let decls = match &result {
                Ok(decls) => decls.clone(),
                Err(e) => vec![("(lean)".into(), format!("error: {e:#}"))],
            };
            *app.lean_status.lock().unwrap() = decls.clone();
            *app.lean_doc_findings.lock().unwrap() = doc_findings.clone();
            app.hub.publish_snapshot(ServerEvent::LeanStatus { decls, running: false, doc_findings });
            result
        })
    }
}

fn canary() -> bool {
    std::env::var_os("FUZUKUE_CANARY").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn canary_snapshot() -> fuzukue_proto::SelfUpdateStatus {
    fuzukue_proto::SelfUpdateStatus {
        build_id: self_update::BUILD_ID.into(),
        canary: true,
        ..Default::default()
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(not(test))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(mcp::workspace_read::jq::CHILD_COMMAND)) {
        return mcp::workspace_read::jq::child();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("release")) {
        return secretaire::release::cli(std::env::args_os().skip(2));
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    server_memory::configure_allocator();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run_server());
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run_server() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("self-swap")) {
        return self_update::supervise(std::env::args_os().skip(2));
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("service")) {
        return service::cli(std::env::args_os().skip(2));
    }
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--held-work-format") {
        println!("{}", self_update::drain::FORMAT_MARKER);
        return Ok(());
    }
    if first.as_deref() == Some("worker-scope-hook") {
        return worker_scope::claude_hook();
    }

    if first.as_deref() == Some("harness-ab") {
        return harness::run(args).await;
    }

    if first.as_deref() == Some("mail") {
        return mail::cli(&args.collect::<Vec<_>>());
    }

    if first.as_deref() == Some("token") {
        return remote::token_cli(args);
    }

    if first.as_deref() == Some("device") {
        return device_tokens::cli(args);
    }

    if first.as_deref() == Some("publish-client") {
        return distribution::cli(args);
    }
    if first.as_deref() == Some("publish-android") {
        return android_distribution::cli(args);
    }

    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
            .from_env_lossy()
            .add_directive(remote::AUDIT_DIRECTIVE.parse()?))
        .with(tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            // Transport traces can contain IMAP credentials or GitHub request details.
            !meta.target().starts_with("async_imap") && !meta.target().starts_with("ureq")
        })))
        .init();
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME が設定されていない"))?;
    let home = PathBuf::from(home).canonicalize()?;
    let project_dir = std::path::absolute(first.map(PathBuf::from).unwrap_or_else(|| ".".into()))?;
    let home = if canary() {
        project_dir.canonicalize()?.parent().context("試運転のプロジェクトの親が見つからない")?.to_path_buf()
    } else {
        home
    };
    let project_dir = workspaces::checked_dir(&home, &project_dir)?;
    let state_dir = state::dir_for(&project_dir);
    tracing::info!("state: {}", state_dir.display());
    let config = remote::Config::load()?;
    capture::configure(&config.server_config)?;
    let listen = config.listeners(canary()).await?;
    let access = Arc::new(config.access(&state_dir)?);
    let (listeners, local_addr) = if !canary() && self_update::managed() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match bind_listeners(&listen).await {
                Ok(bound) => break bound,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error),
            }
        }
    } else {
        bind_listeners(&listen).await?
    };
    if !canary() {
        job_process::check_service_isolation();
        recipes::configure(&config.server_config, &state_dir, config.server_config_path)?;
    }
    let mail = if canary() { None } else { mail::Store::load(&state_dir)? };
    let update = (!canary()).then(|| Arc::new(self_update::Update::new(
        config.self_update, &home, project_dir.clone(), local_addr, config.server_config,
    )));
    let shutdown_control = update.clone().unwrap_or_else(|| {
        Arc::new(self_update::Update::shutdown_only(&project_dir, &state_dir))
    });
    let app = App::with_delegate_config(home, project_dir, state_dir, local_addr.to_string(), mail, config.delegate)?;
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    server_memory::start_trimming();
    let _ = app.shutdown_drain_seconds.set(config.shutdown_drain_seconds);
    let _ = app.self_update.set(shutdown_control.clone());
    if let Some(update) = &update {
        let macos_package = cfg!(target_os = "macos")
            && std::env::var_os("FUZUKUE_MACOS_PACKAGE").as_deref() == Some(std::ffi::OsStr::new("1"));
        service::start_project(app.clone(), update.clone(), macos_package);
        watch::restore(app.clone(), update.clone());
        workspace_requests::start(&app);
    }

    if let Some(addr) = listeners.iter().filter_map(|(listener, _)| listener.local_addr().ok())
        .find(|addr| remote::is_tailscale_ip(addr.ip()))
    {
        let _ = app.tailnet_addr.set(addr);
    }
    let router = router(app.clone(), access);
    let shutdown_limit = Duration::from_secs(config.shutdown_drain_seconds);
    let mut stopping = shutdown_control.shutdown_ready.subscribe();
    let signals = ShutdownSignals::new()?;
    let signal_task = tokio::spawn(handle_shutdown_signals(signals, shutdown_control, app.clone(), shutdown_limit));
    let stop = async move { let _ = stopping.wait_for(|stopping| *stopping).await; }.shared();
    let mut servers = Vec::new();
    let swapping = update.as_ref().map(|update| update.swapping.subscribe());
    let swap = async move {
        if let Some(mut swapping) = swapping {
            let _ = swapping.wait_for(|swapping| *swapping).await;
        } else {
            std::future::pending::<()>().await;
        }
    }.shared();
    let server_swap = swap.clone();
    let server_stop = stop.clone();
    let shutdown = async move {
        tokio::select! {
            _ = server_stop => {}
            _ = server_swap => {}
        }
        mcp::exec::shutdown();
    }.shared();
    for (listener, mode) in listeners {
        tracing::info!(?mode, "listening on {}", listener.local_addr()?);
        let service = router.clone().layer(axum::Extension(mode)).into_make_service_with_connect_info::<SocketAddr>();
        let shutdown = shutdown.clone();
        servers.push(async move { axum::serve(listener, service).with_graceful_shutdown(shutdown).await });
    }
    if let Some(update) = &update {
        let ready = match self_update::ready(local_addr)? {
            Some(ready) => Some(ready),
            None => update.service_ready(local_addr)?,
        };
        update.follow_handoff(ready, app.clone());
    }
    let mut serving = std::pin::pin!(futures_util::future::try_join_all(servers));
    let result = tokio::select! {
        result = &mut serving => {
            if update.as_ref().is_some_and(|update| *update.swapping.borrow()) {
                // Also give upgraded WebSockets time to deliver the notice.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            result.map(|_| ())
        }
        _ = async { tokio::select! { _ = swap => {}, _ = stop => {} } } => {
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                let (result, ()) = tokio::join!(serving, tokio::time::sleep(Duration::from_millis(500)));
                result
            }).await;
            Ok(())
        }
    };
    app.stop_reviews();
    if let Some(update) = &update {
        let _ = tokio::time::timeout(Duration::from_secs(5), update.shutdown_canary()).await;
    }
    signal_task.abort();
    result?;
    tracing::info!("shutting down");
    Ok(())
}

async fn bind_listeners(
    addrs: &[remote::Listener],
) -> anyhow::Result<(Vec<(tokio::net::TcpListener, remote::ListenMode)>, SocketAddr)> {
    let mut listeners = Vec::new();
    let mut local_addr = None;
    for addr in addrs {
        let (listener, mode) = addr.bind().await?;
        let bound = listener.local_addr()?;
        if local_addr.is_none() && mode == remote::ListenMode::Direct && remote::is_loopback(bound.ip()) {
            local_addr = Some(bound);
        }
        listeners.push((listener, mode));
    }
    // MCP and health checks need a direct loopback listener, including in proxy mode.
    let local_addr = match local_addr {
        Some(addr) => addr,
        None => {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            listeners.push((listener, remote::ListenMode::Direct));
            addr
        }
    };
    Ok((listeners, local_addr))
}

fn router(app: Arc<App>, access: Arc<remote::Access>) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route(self_update::HEALTH_ROUTE, get(self_update::health))
        .route(self_update::wait_route::PATH, get(self_update::wait_route::status).post(self_update::wait_route::control))
        .route("/client/{platform}/manifest", get(distribution::manifest))
        .route("/client/{platform}", get(distribution::binary))
        .route("/client/notify", post(distribution::notify))
        .route("/android/{channel}/{asset}", get(android_distribution::asset))
        .route("/file/{*path}", get(file_handler))
        .route("/file", get(file_browser::download))
        .route("/exploration", get(exploration::tree))
        .route("/deck/cards", get(cards::browse::summaries))
        .route("/deck/card", get(cards::browse::content))
        .route("/deck/diagnostics", get(cards::browse::diagnostics))
        .route("/cards-for-file/{*path}", get(source::cards_for_file_handler))
        .route("/source/{*path}", get(source::source_handler))
        .route("/editor/{*path}", get(source_editor::read_handler))
        .route("/mcp", post(mcp::route))
        .route(full_approval::SETTINGS_PATH, get(full_approval::get).post(full_approval::set))
        .route(full_approval::EXECUTION_PATH, get(full_approval::get_execution).post(full_approval::set_execution))
        .route(reviewers::section::blind::editor::ROUTE, post(reviewers::section::blind::editor::route))
        .with_state(app)
        .layer(axum::middleware::from_fn_with_state(access, remote::enforce))
}

#[cfg(all(target_os = "linux", not(all(test, feature = "portable-fs-tests"))))]
fn shutdown_pipe() -> rustix::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use rustix::pipe::{pipe_with, PipeFlags};
    pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
fn shutdown_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

    let (rx, tx) = std::io::pipe()?;
    let (rx, tx) = (rx.into(), tx.into());
    for fd in [&rx, &tx] {
        fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    }
    Ok((rx, tx))
}

#[cfg(test)]
#[test]
fn shutdown_pipe_uses_cloexec_and_nonblocking() -> anyhow::Result<()> {
    use rustix::fs::{fcntl_getfl, OFlags};
    use rustix::io::{fcntl_getfd, Errno, FdFlags};

    let (rx, tx) = shutdown_pipe()?;
    for fd in [&rx, &tx] {
        assert!(fcntl_getfd(fd)?.contains(FdFlags::CLOEXEC));
        assert!(fcntl_getfl(fd)?.contains(OFlags::NONBLOCK));
    }
    let mut signal = [0];
    assert_eq!(rustix::io::read(&rx, &mut signal), Err(Errno::AGAIN));
    assert_eq!(rustix::io::write(&tx, &[1])?, 1);
    assert_eq!(rustix::io::read(&rx, &mut signal)?, 1);
    assert_eq!(signal, [1]);
    Ok(())
}

struct ShutdownSignals {
    pipe: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    handlers: Vec<signal_hook_registry::SigId>,
}

impl ShutdownSignals {
    fn new() -> anyhow::Result<Self> {
        use rustix::process::Signal;
        let (rx, tx) = shutdown_pipe()?;
        let tx = Arc::new(tx);
        // The AsyncFd owns the pipe's read descriptor until deregistration.
        let pipe = unsafe { tokio::io::unix::AsyncFd::register_with_interest(rx, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        let mut signals = Self { pipe, handlers: Vec::new() };
        for signal in [Signal::TERM, Signal::INT] {
            let tx = tx.clone();
            // A nonblocking write is async-signal-safe. If the pipe fills, it
            // already contains enough requests to take the immediate-exit path.
            let id = unsafe { signal_hook_registry::register(signal.as_raw(), move || {
                let _ = rustix::io::write(&*tx, &[1]);
            }) }?;
            signals.handlers.push(id);
        }
        Ok(signals)
    }

    async fn recv(&self) -> std::io::Result<()> {
        use tokio::io::Interest;
        self.pipe.async_io(Interest::READABLE, |pipe| {
            rustix::io::read(pipe, &mut [0u8; 1]).map(|_| ()).map_err(Into::into)
        }).await
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook_registry::unregister(handler);
        }
    }
}

async fn handle_shutdown_signals(
    signals: ShutdownSignals, update: Arc<self_update::Update>, app: Arc<App>, limit: Duration,
) {
    if let Err(error) = signals.recv().await {
        tracing::error!(%error, "shutdown signal listener failed");
        return;
    }
    mcp::exec::shutdown();
    if let Err(error) = update.request_shutdown(app, limit, true) {
        tracing::error!(%error, "could not start shutdown drain");
        update.shutdown_ready.send_replace(true);
    }
    if signals.recv().await.is_ok() {
        std::process::exit(0);
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    axum::Extension(remote::ClientIp(ip)): axum::Extension<remote::ClientIp>,
    device: Option<axum::Extension<remote::DeviceSession>>,
    settings: Option<axum::Extension<remote::SettingsPrincipal>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Browser scripts set Origin on WebSockets. Only native desktop connections
    // may use the stored OAuth credential, including on the loopback listener.
    let github_report_client = !headers.contains_key(axum::http::header::ORIGIN);
    let grant_principal = device.as_ref().map(|device| format!("device:{}", device.0.id()))
        .unwrap_or_else(|| if ip.is_loopback() { "local_user" } else { "shared_token_user" }.into());
    // The largest ordinary client request is a 4 MiB screenshot encoded as Base64.
    // Allow headroom for text requests while bounding incoming fragmented messages.
    ws.max_message_size(16 * 1024 * 1024).on_upgrade(move |socket| async move {
        let settings = settings.filter(|_| github_report_client).map(|extension| extension.0);
        let serve = client_loop(socket, app, ip, github_report_client, grant_principal, settings);
        if let Some(axum::Extension(device)) = device {
            device.run(serve).await;
        } else {
            serve.await;
        }
    })
}

#[derive(serde::Deserialize)]
struct WorkspaceQuery {
    workspace: Option<String>,
}

async fn file_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
    State(app): State<Arc<App>>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let not_found = |e: anyhow::Error| (axum::http::StatusCode::NOT_FOUND, e.to_string());
    let app = app.route(query.workspace).await.map_err(not_found)?;
    if fuzukue_proto::is_inbox_upload_path(Path::new(&path)) {
        let file = tokio::task::spawn_blocking(move || {
            let full = inbox::attachment(&app.project_dir, &app.state_dir, &path)?.canonicalize()?;
            mcp::workspace_read::open_regular_source(&full)
        }).await.map_err(|error| not_found(error.into()))?.map_err(not_found)?;
        return file_browser::stream_file(file, fuzukue_proto::MAX_ATTACHMENT_BYTES)
            .map_err(|status| (status, status.to_string()));
    }
    let full = safe_join(&app.project_dir, &path).map_err(not_found)?;
    if fuzukue_proto::is_thread_save_artifact(Path::new(&path))
        || fuzukue_proto::is_thread_save_artifact(&full)
    {
        return Err(not_found(anyhow::anyhow!("private thread save path")));
    }
    let bytes = tokio::fs::read(&full).await.map_err(|e| not_found(e.into()))?;
    Ok(bytes.into_response())
}

async fn client_loop(
    socket: WebSocket, registry: Arc<App>, peer: std::net::IpAddr,
    github_report_client: bool, grant_principal: String, settings: Option<remote::SettingsPrincipal>,
) {
    let (mut tx, mut rx) = socket.split();
    let (protocol, resume_from, requested, locale, platform, build_id, thread_history, mut transcripts) = match rx.next().await {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
            Ok(ClientMsg::Hello {
                protocol, resume_from, workspace, locale, platform, build_id, thread_history,
                // Component fences already travel as ordinary Markdown code blocks.
                selection_scoped_transcripts, epoch, cached_sessions, ..
            }) => {
                let agreed = protocol.min(PROTOCOL_VERSION);
                let thread_history = thread_history && agreed >= fuzukue_proto::THREAD_HISTORY_PROTOCOL;
                let transcripts = (thread_history && selection_scoped_transcripts)
                    .then_some(hub::TranscriptResume { epoch, cached_sessions });
                (
                    agreed, resume_from.unwrap_or(0), workspace,
                    l10n::negotiate(protocol, locale.as_deref()), platform, build_id,
                    thread_history, transcripts,
                )
            }
            _ => return,
        },
        _ => return,
    };
    if protocol == 0 {
        let _ = tx.send(Message::Close(None)).await;
        return;
    }
    if registry.self_update.get().is_some_and(|update| update.trial_pending()) {
        if protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL {
            let event = snapshot(ServerEvent::ServerSwapNotice);
            let _ = tx.send(wire_event(&event, protocol, None, locale)).await;
        }
        let _ = tx.send(Message::Close(None)).await;
        return;
    }
    if protocol >= fuzukue_proto::DRAIN_PROTOCOL {
        let draining = registry.self_update.get().and_then(|update| {
            let status = update.subscribe_drain().borrow().clone()?;
            matches!(status.progress, fuzukue_proto::DrainProgress::Draining
                | fuzukue_proto::DrainProgress::Forcing
                | fuzukue_proto::DrainProgress::Ready)
                .then_some(status)
                .filter(|_| update.admit().is_err())
        });
        if let Some(status) = draining {
            // Workspace admission is closed. Send the drain state before disconnecting.
            let event = snapshot(ServerEvent::DrainStatus { status });
            let _ = tx.send(wire_event(&event, protocol, None, locale)).await;
            let _ = tx.send(Message::Close(None)).await;
            return;
        }
    }
    let requested = requested.filter(|_| protocol >= 11);
    let mut resume = resume_from;
    let mut app = match select_workspace(&registry, requested.clone()).await {
        Ok(app) => app,
        Err(err) if requested.is_some() => {
            if tx.send(wire_event(&snapshot(ServerEvent::WorkspaceError { message: format!("{err:#}") }), protocol, None, locale)).await.is_err() {
                return;
            }
            resume = 0;
            let Ok(app) = select_workspace(&registry, None).await else { return };
            app
        }
        Err(err) => {
            let event = if protocol >= 11 { ServerEvent::WorkspaceError { message: format!("{err:#}") } }
                else { ServerEvent::Error { message: format!("{err:#}") } };
            let _ = tx.send(wire_event(&snapshot(event), protocol, None, locale)).await;
            return;
        }
    };
    if requested.as_deref() != Some(&app.id) {
        if let Some(transcripts) = &mut transcripts {
            *transcripts = Default::default();
            resume = 0;
        }
    }
    let mut distribution = None;
    let mut manifest = None;
    if !canary() && protocol >= fuzukue_proto::DISTRIBUTION_PROTOCOL {
        if let Some(platform) = platform.as_deref() {
            distribution = Some((peer, registry.distribution.subscribe()));
            if let Some(build_id) = build_id.as_deref().filter(|id| id.len() <= 64) {
                registry.distribution.remember(peer, platform, build_id).await;
            }
            manifest = registry.distribution.manifest(platform).await;
        }
    }
    let locale = std::sync::RwLock::new(locale);
    let mut notices = (protocol >= fuzukue_proto::NOTIFICATIONS_PROTOCOL).then(|| registry.notices.subscribe());
    let mut first = true;
    loop {
        let Some(next) = serve_workspace(&mut tx, &mut rx, &registry, app, (protocol, resume, first, thread_history), transcripts.as_ref(), &locale, manifest.take(), &mut distribution, (platform.as_deref(), build_id.as_deref()), github_report_client, &grant_principal, settings.as_ref(), &mut notices).await else { break };
        first = false;
        app = next;
        resume = 0;
        if let Some(transcripts) = &mut transcripts {
            *transcripts = Default::default();
        }
    }
}

async fn select_workspace(registry: &Arc<App>, id: Option<String>) -> anyhow::Result<Arc<Workspace>> {
    let registry = registry.clone();
    tokio::task::spawn_blocking(move || {
        let _work = registry.self_update.get().map(|update| update.admit()).transpose()?;
        registry.select(id.as_deref())
    }).await?
}

fn snapshot(event: ServerEvent) -> Envelope {
    Envelope { workspace: None, seq: 0, event }
}

fn supports_question_events(event: &ServerEvent, protocol: u32) -> bool {
    protocol >= fuzukue_proto::QUESTION_PROTOCOL
        || !matches!(event, ServerEvent::QuestionAsked { .. } | ServerEvent::QuestionResolved { .. })
}

fn wire_welcome(event: &ServerEvent, capabilities: fuzukue_proto::WelcomeCapabilities) -> Message {
    #[derive(serde::Serialize)]
    struct Welcome<'a> {
        #[serde(flatten)]
        event: &'a ServerEvent,
        #[serde(flatten)]
        capabilities: fuzukue_proto::WelcomeCapabilities,
    }

    #[derive(serde::Serialize)]
    struct Wire<'a> { seq: u64, event: Welcome<'a> }
    let fallback = ServerEvent::Error { message: "Server welcome exceeds the WebSocket size limit.".into() };
    bounded_wire(&Wire { seq: 0, event: Welcome { event, capabilities } }, 12 * 1024 * 1024)
        .unwrap_or_else(|| wire_event(&snapshot(fallback), 0, None, fuzukue_i18n::Locale::JAPANESE))
}

fn wire_event(env: &Envelope, protocol: u32, workspace: Option<&str>, locale: fuzukue_i18n::Locale) -> Message {
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        seq: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace: Option<&'a str>,
        event: &'a ServerEvent,
    }
    let localized = l10n::event(&env.event, locale, protocol);
    let converted;
    let event = match localized.as_ref().unwrap_or(&env.event) {
        ServerEvent::SelfUpdateStatus { status } if status.wait.is_some() || status.cancelled => {
            let mut status = status.clone();
            if protocol >= fuzukue_proto::SELF_UPDATE_WAIT_PROTOCOL {
                self_update::stamp_wait(&mut status);
            } else {
                status.wait = None;
                status.cancelled = false;
            }
            converted = ServerEvent::SelfUpdateStatus { status };
            &converted
        }
        ServerEvent::UserPrompt { session, item, text, paths, .. } if protocol < 5 => {
            converted = ServerEvent::TextDelta {
                session: session.clone(), item: item.clone(), actor: fuzukue_proto::Actor::User,
                text: format!("{text}\n\n{}", paths.join("\n")),
            };
            &converted
        }
        ServerEvent::LocalNotice { item, text, .. }
            if protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL && item == fuzukue_proto::COMPOSER_NOTICE_ITEM => {
            converted = ServerEvent::Error { message: text.clone() };
            &converted
        }
        ServerEvent::LocalNotice { session, item, text, .. } if protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL => {
            converted = ServerEvent::TextDelta {
                session: session.clone(), item: item.clone(), actor: fuzukue_proto::Actor::System, text: text.clone(),
            };
            &converted
        }
        ServerEvent::ChatWaiting { waiting } if protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL => {
            let mut waiting = waiting.clone();
            for reason in waiting.threads.values_mut().chain(std::iter::once(&mut waiting.workspace)) {
                if *reason == Some(fuzukue_proto::ChatWaitReason::Compacting) {
                    *reason = Some(fuzukue_proto::ChatWaitReason::QueuedTurn);
                }
            }
            converted = ServerEvent::ChatWaiting { waiting };
            &converted
        }
        ServerEvent::ThreadHistoryPage { session, request, events, next_before, generation, through } => {
            converted = ServerEvent::ThreadHistoryPage {
                session: session.clone(), request: *request, next_before: *next_before,
                generation: *generation, through: *through,
                events: events.iter().filter(|event| !event.is_live_only())
                    .map(|event| l10n::event(event, locale, protocol).unwrap_or_else(|| event.clone()))
                    .collect(),
            };
            &converted
        }
        ServerEvent::ThreadTail { session, request, generation, through, events, workers, next_before } => {
            converted = ServerEvent::ThreadTail {
                session: session.clone(), request: *request, generation: *generation, through: *through,
                workers: workers.clone(), next_before: *next_before,
                events: events.iter().filter(|event| !event.is_live_only())
                    .map(|event| l10n::event(event, locale, protocol).unwrap_or_else(|| event.clone()))
                    .collect(),
            };
            &converted
        }
        ServerEvent::ThreadRestored { session, events } => {
            converted = ServerEvent::ThreadRestored {
                session: session.clone(),
                events: events.iter().filter(|event| supports_question_events(event, protocol) && match event {
                    ServerEvent::MessageDetails { .. } => protocol >= fuzukue_proto::ONE_VOICE_TIERS_PROTOCOL,
                    ServerEvent::GitHubActivity { .. } => protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL,
                    _ => true,
                }).map(|event| {
                    let event = l10n::event(event, locale, protocol).unwrap_or_else(|| event.clone());
                    match event {
                        ServerEvent::LocalNotice { session, item, text, .. }
                            if protocol < fuzukue_proto::SLASH_COMMAND_PROTOCOL =>
                            ServerEvent::TextDelta { session, item, text, actor: fuzukue_proto::Actor::System },
                        event => event,
                    }
                }).collect(),
            };
            &converted
        }
        ServerEvent::AgentHistory { agent, session, entries } if protocol < fuzukue_proto::ONE_VOICE_TIERS_PROTOCOL => {
            let mut entries = entries.clone();
            for entry in &mut entries {
                if entry.outcome == fuzukue_proto::AgentOutcome::Stalled {
                    entry.outcome = fuzukue_proto::AgentOutcome::Failed;
                }
            }
            converted = ServerEvent::AgentHistory { agent: agent.clone(), session: session.clone(), entries };
            &converted
        }
        ServerEvent::ResearchIssues { repo, issues, error } if protocol < fuzukue_proto::RESEARCH_ISSUE_DETAILS_PROTOCOL => {
            let mut issues = issues.clone();
            for issue in &mut issues {
                issue.details = None;
            }
            converted = ServerEvent::ResearchIssues { repo: repo.clone(), issues, error: error.clone() };
            &converted
        }
        event => event,
    };
    let workspace = workspace.filter(|_| protocol >= 11);
    let budget = if matches!(event, ServerEvent::Cards { .. }) { cards::legacy_budget(protocol) } else { 12 * 1024 * 1024 };
    if let Some(frame) = bounded_wire(&Wire { seq: env.seq, workspace, event }, budget) {
        return frame;
    }
    let fallback = match event {
        ServerEvent::Cards { deck, .. } if deck.len() <= 4096 => cards::oversized_deck(deck.clone()),
        ServerEvent::HistoryResult { id, .. } => ServerEvent::HistoryResult {
            id: *id,
            outcome: fuzukue_proto::HistoryOutcome::Error { message: "History response exceeds the WebSocket size limit.".into() },
        },
        ServerEvent::GitResult { id, .. } => ServerEvent::GitResult {
            id: *id,
            outcome: fuzukue_proto::GitOutcome::Error { message: "Git response exceeds the WebSocket size limit.".into() },
        },
        ServerEvent::CardSaveResult { request, .. } => ServerEvent::CardSaveResult {
            request: *request,
            outcome: CardSaveOutcome::Error { message: "Card save response exceeds the WebSocket size limit. Reload the card for the latest content.".into() },
        },
        ServerEvent::ReviewFindings { deck, .. } if deck.len() <= 4096 => ServerEvent::ReviewFindings {
            deck: deck.clone(),
            outcome: fuzukue_proto::ReviewFindingsOutcome::Error { message: "Review findings exceed the WebSocket size limit.".into() },
        },
        _ => {
            let response = match event {
                ServerEvent::Document { .. } => "Deck structure",
                ServerEvent::SymbolResults { .. } => "Graph lookup",
                ServerEvent::LibraryResults { .. } | ServerEvent::DeckSearchResults { .. } => "Search response",
                ServerEvent::ThreadTail { .. } | ServerEvent::ThreadHistoryPage { .. } | ServerEvent::ThreadRestored { .. } => "Thread history",
                ServerEvent::LeanStatus { .. } | ServerEvent::LspDocument { .. } => "Diagnostics",
                _ => "Server response",
            };
            let message = format!("{response} exceeded the WebSocket size limit.");
            if protocol >= fuzukue_proto::LAZY_DECK_PROTOCOL {
                ServerEvent::ResponseTooLarge { message }
            } else {
                ServerEvent::Error { message }
            }
        },
    };
    bounded_wire(&Wire { seq: env.seq, workspace, event: &fallback }, budget)
        .or_else(|| bounded_wire(&Wire { seq: env.seq, workspace: None, event: &fallback }, budget))
        .expect("size fallback is bounded")
}

fn bounded_wire(value: &impl serde::Serialize, budget: usize) -> Option<Message> {
    Some(Message::Text(String::from_utf8(bounded_json(value, budget)?).ok()?.into()))
}

fn bounded_json(value: &impl serde::Serialize, budget: usize) -> Option<Vec<u8>> {
    let mut json = Vec::new();
    serde_json::to_writer(&mut BoundedJson { bytes: &mut json, budget }, value).ok()?;
    Some(json)
}

fn bounded_json_len(value: &impl serde::Serialize, budget: usize) -> Option<usize> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(data.len())
                .ok_or_else(|| std::io::Error::other("JSON size limit exceeded"))?;
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut count = Count(budget);
    serde_json::to_writer(&mut count, value).ok()?;
    Some(budget - count.0)
}

struct BoundedJson<'a> {
    bytes: &'a mut Vec<u8>,
    budget: usize,
}

impl std::io::Write for BoundedJson<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.budget - self.bytes.len() {
            return Err(std::io::Error::other("JSON size limit exceeded"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

async fn deck_event(project: &Path, protocol: u32, event: ServerEvent) -> ServerEvent {
    if let ServerEvent::DeckCardsChanged { deck } = &event {
        if protocol < fuzukue_proto::LAZY_DECK_PROTOCOL {
            static LEGACY_READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
            let permit = LEGACY_READS.acquire().await.expect("legacy readers are never closed");
            let project = project.to_path_buf();
            let root = project.join(deck);
            return tokio::task::spawn_blocking(move || {
                let _permit = permit;
                cards::legacy_snapshot(&project, &root, protocol)
            })
                .await.unwrap_or_else(|_| ServerEvent::Error { message: "Cannot read deck cards.".into() });
        }
    }
    event
}

async fn serve_workspace(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    rx: &mut futures_util::stream::SplitStream<WebSocket>,
    registry: &Arc<App>,
    app: Arc<Workspace>,
    (protocol, resume_from, first, thread_history): (u32, u64, bool, bool),
    transcripts: Option<&hub::TranscriptResume>,
    locale: &std::sync::RwLock<fuzukue_i18n::Locale>,
    client_manifest: Option<fuzukue_proto::ClientManifest>,
    distribution: &mut Option<(std::net::IpAddr, tokio::sync::broadcast::Receiver<Option<std::net::IpAddr>>)>,
    client_version: (Option<&str>, Option<&str>),
    github_report_client: bool,
    grant_principal: &str,
    settings: Option<&remote::SettingsPrincipal>,
    notices: &mut Option<tokio::sync::broadcast::Receiver<Envelope>>,
) -> Option<Arc<Workspace>> {
    let locale_for_wire = || *locale.read().unwrap();
    let desktop_cards = client_version.0 != Some("android");
    let question_protocol = if desktop_cards { protocol } else { 0 };
    let _question_client = (question_protocol >= fuzukue_proto::QUESTION_PROTOCOL)
        .then(|| app.hub.register_question_client());
    let scoped = transcripts.is_some();
    let compact_jobs = scoped && protocol >= fuzukue_proto::THREAD_WINDOW_PROTOCOL;
    let job_summaries = compact_jobs && protocol >= fuzukue_proto::JOB_SUMMARY_PROTOCOL;
    let open_jobs = std::sync::Mutex::new(std::collections::VecDeque::<(String, String)>::new());
    let project_job = |seq: u64, event: ServerEvent| {
        if !compact_jobs {
            return (Some(event), None);
        }
        let summary = app.hub.thread_job_summary(&event);
        let open = event.worker_child().or_else(|| {
            summary.as_ref().map(|(session, _, summary)| (session.as_str(), summary.id.as_str()))
        }).is_some_and(|(session, job)| {
            open_jobs.lock().unwrap().iter().any(|(owner, item)| owner == session && item == job)
        });
        let projected = if open {
            Some(event)
        } else if (job_summaries && event.worker_child().is_some())
            || matches!(&event, ServerEvent::ToolResult { item, .. }
                if summary.as_ref().is_some_and(|(_, _, summary)| &summary.id != item))
        {
            None
        } else {
            threads::compact_job_event(event, |job| summary.as_ref()
                .filter(|(_, _, summary)| summary.id == job).map(|(_, _, summary)| summary.clone()))
        };
        let update = summary.filter(|_| job_summaries).map(|(session, through, summary)| {
            Envelope {
                workspace: None,
                seq: if projected.is_none() { seq } else { 0 },
                event: ServerEvent::ThreadJobUpdated { session, through, summary },
            }
        });
        (projected, update)
    };
    let approval_queue = github_report_client && !canary() && protocol >= 11;
    use workspace_requests::ClientSupport;
    let request_support = ClientSupport::from_connection(
        protocol, client_version.0, github_report_client,
        settings.is_some_and(remote::SettingsPrincipal::paired_device),
    );
    let request_operator = request_support.can_review();
    let workspace_request_client = request_operator || request_support == ClientSupport::UnpairedDesktop;
    let request_recovery = request_support == ClientSupport::Desktop;
    let request_connection = registry.workspace_requests.connect(&app.id, request_support);
    let mut request_changes = request_operator.then(|| registry.workspace_requests.subscribe());
    let (initial, mut live) = app.session_stats.subscribe(
        &app.hub, resume_from, protocol >= 2, protocol >= 4, transcripts,
    );
    let mut registry_changes = registry.changed.subscribe();
    let update = registry.self_update.get();
    let mut update_status = update.map(|update| update.subscribe());
    let mut drain_status = update.filter(|_| protocol >= fuzukue_proto::DRAIN_PROTOCOL)
        .map(|update| update.subscribe_drain());
    let mut swapping = update.map(|update| update.swapping.subscribe());
    let safe_close = protocol >= fuzukue_proto::SAFE_CLOSE_PROTOCOL;
    let mut shutdown_work = safe_close.then(|| registry.shutdown_work());
    let mut research_issues = (protocol >= 13 && !canary()).then(|| app.research_issues.subscribe(registry, &app));
    if first {
        let server_tailnet_url = (protocol >= fuzukue_proto::DISTRIBUTION_PROTOCOL)
            .then(|| registry.tailnet_addr.get().map(|addr| format!("ws://{addr}/ws")))
            .flatten();
        let event = ServerEvent::Welcome {
            protocol, server: "secretaire".into(), client_manifest, server_tailnet_url,
            selection_scoped_transcripts: scoped,
        };
        let welcome = if approval_queue {
            wire_welcome(&event, fuzukue_proto::WelcomeCapabilities { approval_queue })
        } else {
            wire_event(&snapshot(event), protocol, None, locale_for_wire())
        };
        tx.send(welcome).await.ok()?;
    }
    if let Some(work) = &shutdown_work {
        let event = ServerEvent::ShutdownWork { work: work.clone() };
        tx.send(wire_event(&snapshot(event), protocol, None, locale_for_wire())).await.ok()?;
    }
    if protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL {
        let status = if canary() {
            Some(canary_snapshot())
        } else {
            update_status.as_mut().map(|receiver| receiver.borrow_and_update().clone())
        };
        if let Some(status) = status {
            tx.send(wire_event(&snapshot(ServerEvent::SelfUpdateStatus { status }), protocol, None, locale_for_wire())).await.ok()?;
        }
    }
    if let Some(receiver) = &mut drain_status {
        let status = receiver.borrow_and_update().clone();
        if let Some(status) = status {
            tx.send(wire_event(&snapshot(ServerEvent::DrainStatus { status }), protocol, None, locale_for_wire())).await.ok()?;
        }
    }
    if protocol >= 11 {
        for event in [
            ServerEvent::WorkspaceSelected {
                id: app.id.clone(),
                replay: transcripts.map_or(resume_from == 0, |resume| resume.epoch.is_none()),
            },
            ServerEvent::Workspaces { workspaces: registry.list() },
        ] {
            tx.send(wire_event(&snapshot(event), protocol, None, locale_for_wire())).await.ok()?;
        }
    }
    let send = |env: &Envelope| wire_event(env, protocol, Some(&app.id), locale_for_wire());
    if request_recovery {
        for event in registry.workspace_requests.draft_events(registry, &app.id) {
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }
    if protocol >= fuzukue_proto::EXTENSIONS_PROTOCOL {
        for event in extensions::snapshot(app.clone(), locale_for_wire()).await {
            tx.send(wire_event(&snapshot(event), protocol, Some(&app.id), locale_for_wire())).await.ok()?;
        }
    }
    if protocol >= fuzukue_proto::HANDOFF_PROTOCOL {
        let event = handoff::snapshot(&app).await;
        tx.send(wire_event(&snapshot(event), protocol, Some(&app.id), locale_for_wire())).await.ok()?;
    }
    let supports_chat = protocol >= 5;
    if supports_chat {
        app.hub.refresh_model_defaults();
    }
    // v1 クライアントは未知のイベントを受けると切断する。
    let supports = move |env: &Envelope| {
        if !supports_question_events(&env.event, question_protocol) {
            return false;
        }
        match env.event {
            ServerEvent::DeckCardsChanged { .. } | ServerEvent::Cards { .. } | ServerEvent::Document { .. } => desktop_cards,
            ServerEvent::WorkspaceRequests { view: Some(_) } => request_operator,
            ServerEvent::WorkspaceRequestDrafts { .. } => request_recovery,
            ServerEvent::WorkspaceRequests { view: None } | ServerEvent::WorkspaceRequestError { .. } => workspace_request_client,
            ServerEvent::FolderGrants { .. } | ServerEvent::FolderGrantsChanged { .. } =>
                github_report_client && protocol >= fuzukue_proto::FOLDER_GRANTS_PROTOCOL,
            ServerEvent::ApprovalRequested { ref request_id, .. } | ServerEvent::ApprovalResolved { ref request_id, .. }
                if request_id.starts_with(fuzukue_proto::FOLDER_ACCESS_REQUEST) =>
                github_report_client && protocol >= fuzukue_proto::FOLDER_GRANTS_PROTOCOL,
            ServerEvent::ClientCommand { .. } | ServerEvent::Consulting { .. } =>
                protocol >= fuzukue_proto::CLIENT_COMMAND_PROTOCOL,
            ServerEvent::GitHubItems { .. } | ServerEvent::GitHubActivity { .. } | ServerEvent::GitHubReply { .. } =>
                protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL,
            ServerEvent::IssueIntakeMove { .. } | ServerEvent::IssueIntakeRepos { .. } =>
                protocol >= fuzukue_proto::ISSUE_INTAKE_PROTOCOL,
            ServerEvent::GitHubAccountChanged { .. } | ServerEvent::GitHubConnectCard { .. }
            | ServerEvent::GitHubAvatar { .. } => protocol >= fuzukue_proto::GITHUB_CONNECT_PROTOCOL,
            ServerEvent::GitHubReportResult { .. } => protocol >= fuzukue_proto::GITHUB_REPORT_PROTOCOL,
            ServerEvent::AgentStats { .. } | ServerEvent::AgentHistory { .. } | ServerEvent::AgentUsageChanged =>
                protocol >= fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL,
            ServerEvent::AgentModelCapabilities { .. } => protocol >= fuzukue_proto::AGENT_MODELS_PROTOCOL,
            ServerEvent::SourceSaveResult { .. } | ServerEvent::EditorClosed { .. } | ServerEvent::BuildStarted { .. } =>
                protocol >= fuzukue_proto::EDITOR_PROTOCOL,
            ServerEvent::TrashItems { .. } | ServerEvent::TrashChanged | ServerEvent::TrashFailed { .. } | ServerEvent::ThreadRestored { .. } | ServerEvent::ReplySuperseded { .. } => protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL,
            ServerEvent::LedgerResult { .. } | ServerEvent::LedgerChanged => protocol >= fuzukue_proto::LEDGER_PROTOCOL,
            ServerEvent::LibraryResults { .. } => protocol >= fuzukue_proto::LIBRARY_PROTOCOL,
            ServerEvent::MemoResult { .. } => protocol >= fuzukue_proto::MEMOS_PROTOCOL,
            ServerEvent::ReviewReports { .. } | ServerEvent::ReviewReportsResult { .. } =>
                protocol >= fuzukue_proto::REVIEW_REPORTS_PROTOCOL,
            ServerEvent::Extensions { .. } | ServerEvent::ExtensionPermissionPrompt { .. }
            | ServerEvent::ExtensionView { .. } | ServerEvent::ExtensionError { .. }
            | ServerEvent::ExtensionUpdated { .. } | ServerEvent::ExtensionsChanged =>
                protocol >= fuzukue_proto::EXTENSIONS_PROTOCOL,
            ServerEvent::SymbolResults { .. } => protocol >= fuzukue_proto::SYMBOL_PROTOCOL,
            ServerEvent::Handoffs { .. } | ServerEvent::HandoffOpened { .. } | ServerEvent::HandoffDetail { .. }
            | ServerEvent::HandoffWorkerUpdated { .. } | ServerEvent::HandoffGenerated { .. } | ServerEvent::HandoffFailed { .. } => protocol >= fuzukue_proto::HANDOFF_PROTOCOL,
            ServerEvent::PlanChanged { .. } => protocol >= fuzukue_proto::PLAN_PROTOCOL,
            ServerEvent::DelegateRenamed { .. } => protocol >= fuzukue_proto::JOB_LABEL_PROTOCOL,
            ServerEvent::DelegateDismissed { .. } => protocol >= fuzukue_proto::JOB_DISMISS_PROTOCOL,
            ServerEvent::Agents { .. } | ServerEvent::AgentSettingsChanged { .. } | ServerEvent::DelegateProgress { .. } => protocol >= 14,
            ServerEvent::InboxFiled { .. } | ServerEvent::DeckSearchResults { .. } | ServerEvent::ReviewSettingsChanged { .. } => protocol >= 13,
            ServerEvent::HistoryChanged { .. } | ServerEvent::HistoryResult { .. } => protocol >= fuzukue_proto::CHANGE_HISTORY_PROTOCOL,
            ServerEvent::GitChanged | ServerEvent::GitResult { .. } | ServerEvent::PartnerDefaultChanged { .. } => protocol >= 12,
            ServerEvent::PaperPdfMap { .. } => protocol >= 10,
            ServerEvent::OpenPaper { .. } | ServerEvent::CardSaveResult { .. } => protocol >= 9,
            ServerEvent::OpenFile { .. } => protocol >= fuzukue_proto::OPEN_FILE_PROTOCOL,
            ServerEvent::TestSuites { .. } | ServerEvent::TestRun { .. } =>
                protocol >= fuzukue_proto::TESTS_PROTOCOL,
            ServerEvent::CodexModels { .. } | ServerEvent::CodexPartner { .. }
            | ServerEvent::DelegateSettingsChanged { .. } => protocol >= 8,
            ServerEvent::WorkerApprovalPolicyChanged { .. } => protocol >= fuzukue_proto::WORKER_APPROVAL_PROTOCOL,
            ServerEvent::ChatActivity { .. } => protocol >= fuzukue_proto::CHAT_ACTIVITY_PROTOCOL,
            ServerEvent::ChatCapabilities { .. } | ServerEvent::ChatSettingsChanged { .. }
            | ServerEvent::AttachmentUploaded { .. } | ServerEvent::AttachmentProgress { .. }
            | ServerEvent::TurnActivity { .. } => supports_chat,
            ServerEvent::ThreadHistoryStart { .. } => thread_history && !scoped,
            ServerEvent::ThreadHistoryPage { .. } | ServerEvent::ThreadHistoryFailed { .. } => thread_history,
            ServerEvent::ThreadTail { .. } | ServerEvent::ThreadSync { .. }
            | ServerEvent::ThreadHistoryInvalidated { .. } => scoped,
            ServerEvent::Threads { .. } | ServerEvent::ThreadCreated { .. } => protocol >= 4,
            ServerEvent::CommentApplication { .. } | ServerEvent::ThreadArtifacts { .. }
            | ServerEvent::CompactStatus { .. } => protocol >= 6,
            ServerEvent::BuildStatus { .. } | ServerEvent::PaperJobsReset => protocol >= 3,
            ServerEvent::ReviewDecksPruned { .. } => protocol >= fuzukue_proto::REVIEW_RECOVERY_PROTOCOL,
            ServerEvent::MessageDetails { .. } => protocol >= fuzukue_proto::ONE_VOICE_TIERS_PROTOCOL,
            ServerEvent::ReviewStatus { .. } | ServerEvent::SessionStats { .. } => protocol >= 2,
            _ => true,
        }
    };
    let dir = app.project_dir.clone();
    let deck = app.current_deck();
    let mut findings_deck = deck.clone();
    let supports_findings = protocol >= fuzukue_proto::REVIEW_FINDINGS_PROTOCOL;
    let snapshot_deck = deck.clone();
    if let Ok(((c, d, w), a)) = tokio::task::spawn_blocking(move || {
        (cards::deck_snapshot(&dir, &snapshot_deck), cards::artifacts(&dir))
    })
    .await
    {
        let issues = app.last_issues.lock().unwrap().clone();
        let lean = ServerEvent::LeanStatus { decls: app.lean_status.lock().unwrap().clone(), running: false, doc_findings: app.lean_doc_findings.lock().unwrap().clone() };
        let comments = ServerEvent::Comments { items: comments::load(&app.project_dir.join(&deck)), deck };
        for ev in [Some(w), desktop_cards.then_some(c), desktop_cards.then_some(d), Some(a), Some(app.usage.snapshot()), issues, Some(lean), Some(comments)]
            .into_iter()
            .flatten()
        {
            let event = deck_event(&app.project_dir, protocol, ev).await;
            if tx.send(send(&snapshot(event))).await.is_err() {
                return None;
            }
        }
        if supports_findings {
            let event = review_findings::snapshot(&app.project_dir, &findings_deck).await;
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }
    if protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL {
        if let Some(event) = app.issue_threads.snapshot() {
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }
    if protocol >= fuzukue_proto::ISSUE_INTAKE_PROTOCOL {
        let event = ServerEvent::IssueIntakeRepos { repos: issue_threads::repositories(&app) };
        tx.send(send(&snapshot(event))).await.ok()?;
    }
    let mut waiting_feed = chat_waiting::Feed::default();
    let mut last = if scoped { 0 } else { resume_from };
    for env in &initial {
        if let ServerEvent::ThreadSync { through, .. } = &env.event {
            last = *through;
            let update = update_status.as_ref()
                .and_then(|status| chat_waiting::update_reason(&status.borrow()));
            if let Some(waiting) = waiting_feed.latest(&app, update) {
                tx.send(send(&snapshot(ServerEvent::ChatWaiting { waiting }))).await.ok()?;
            }
        } else {
            last = last.max(env.seq);
        }
        if supports(env) {
            let (projected, update) = project_job(env.seq, env.event.clone());
            if let Some(event) = projected {
                let projected = Envelope { workspace: env.workspace.clone(), seq: env.seq, event };
                tx.send(send(&projected)).await.ok()?;
            }
            if let Some(update) = update {
                tx.send(send(&update)).await.ok()?;
            }
        }
    }

    if protocol >= fuzukue_proto::CHANGE_HISTORY_PROTOCOL {
        let history = app.history.clone();
        match tokio::task::spawn_blocking(move || history.changed_turns()).await {
            Ok(Ok(changed_turns)) => {
                tx.send(send(&snapshot(ServerEvent::HistoryChanged {
                    changed_turns: Some(changed_turns),
                }))).await.ok()?;
            }
            Ok(Err(error)) => tracing::warn!("project history: {error:#}"),
            Err(error) => tracing::warn!("project history snapshot: {error:#}"),
        }
    }

    if protocol >= fuzukue_proto::REVIEW_RECOVERY_PROTOCOL {
        let decks = app.pruned_review_decks();
        if !decks.is_empty() {
            tx.send(send(&snapshot(ServerEvent::ReviewDecksPruned { decks }))).await.ok()?;
        }
    }
    if protocol >= 3 {
        let running = std::iter::once(ServerEvent::PaperJobsReset)
            .chain(app.active_build())
            .chain(app.active_reviews());
        for event in running {
            if tx.send(send(&Envelope { workspace: None, seq: 0, event })).await.is_err() {
                return None;
            }
        }
    }

    if protocol >= fuzukue_proto::TESTS_PROTOCOL {
        for event in app.test_snapshots() {
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }

    let claude_efforts = if supports_chat {
        tokio::task::spawn_blocking(claude::cli_efforts).await.ok().flatten()
    } else { None };
    if supports_chat {
        let effort_levels = claude_efforts.unwrap_or_default().to_vec();
        let sessions: Vec<_> = app.session_records.lock().unwrap().keys().cloned().collect();
        let settings = sessions.into_iter().map(|session| ServerEvent::ChatSettingsChanged {
            settings: app.chat_settings(&session), session,
        });
        let events = std::iter::once(ServerEvent::ChatCapabilities {
            effort_levels,
            // 旧来のクライアントにも通常の承認スタイルを使わせる。
            mail_restricted: false,
            mcp_tools: mcp::available_tool_names(app.mail.is_some()),
            folder_attachments: true,
        }).chain(settings);
        for event in events {
            if tx.send(send(&Envelope { workspace: None, seq: 0, event })).await.is_err() {
                return None;
            }
        }
    }

    if protocol >= fuzukue_proto::PLAN_PROTOCOL {
        for event in planner::snapshots(&app) {
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }
    let (reply, mut replies) = tokio::sync::mpsc::unbounded_channel();
    if github_report_client && protocol >= fuzukue_proto::GITHUB_REPORT_PROTOCOL {
        let (app, reply) = (app.clone(), reply.clone());
        tokio::spawn(async move {
            let result = github_auth::handle(&app, 0, fuzukue_proto::GitHubReportAction::Status, &reply).await;
            let _ = reply.send(github_auth::event(0, result));
        });
    }
    if protocol >= fuzukue_proto::GITHUB_CONNECT_PROTOCOL {
        for event in github_auth::Auth::snapshots(&app).await {
            tx.send(send(&snapshot(event))).await.ok()?;
        }
    }
    let supports_screenshots = protocol >= fuzukue_proto::CLIENT_SCREENSHOT_PROTOCOL;
    let screenshot_client = app.screenshots.connect(reply.clone(), supports_screenshots);
    if protocol >= 14 {
        let _ = reply.send(ServerEvent::AgentSettingsChanged { settings: app.agent_settings() });
        let (app, reply) = (app.clone(), reply.clone());
        tokio::spawn(async move { let _ = reply.send(app.agent_catalog_event().await); });
    }
    if protocol >= 13 {
        let _ = reply.send(ServerEvent::ReviewSettingsChanged { settings: app.review_settings() });
    }
    if let Some(event) = research_issues.as_mut().and_then(|rx| rx.borrow_and_update().clone()) {
        let _ = reply.send(event);
    }
    if protocol >= 12 {
        let _ = reply.send(ServerEvent::PartnerDefaultChanged {
            settings: app.hub.role_defaults().0,
        });
    }
    if protocol >= fuzukue_proto::AGENT_MODELS_PROTOCOL {
        let (app, reply) = (app.clone(), reply.clone());
        tokio::spawn(async move {
            let codex_available = app.running_codex().await.is_some() || codex::cli_available().await;
            let front_desk_configured = state::has_role_settings(&app.state_dir)
                || !app.session_records.lock().unwrap().is_empty()
                || !app.hub.read_threads(threads::Store::is_empty);
            let _ = reply.send(ServerEvent::AgentModelCapabilities {
                claude_available: claude_efforts.is_some(), codex_available, front_desk_configured,
            });
        });
    }
    if protocol >= fuzukue_proto::WORKER_APPROVAL_PROTOCOL {
        let _ = reply.send(app.approval_settings());
    }
    if protocol >= 8 {
        let _ = reply.send(ServerEvent::DelegateSettingsChanged { settings: app.hub.role_defaults().1 });
        let partners: Vec<_> = app.session_records.lock().unwrap().iter()
            .filter(|(_, rec)| rec.last_partner_codex || rec.codex_thread_id.is_some())
            .map(|(session, rec)| ServerEvent::CodexPartner {
                session: session.clone(), thread_id: rec.codex_thread_id.clone(),
            }).collect();
        for event in partners {
            let _ = reply.send(event);
        }
        let (app, reply) = (app.clone(), reply.clone());
        tokio::spawn(async move {
            let _ = reply.send(ServerEvent::CodexModels { models: app.codex_models().await });
        });
    }
    let (select_paper, mut paper_selections) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
    let project = app.project_dir.clone();
    let (stop_writer, mut stop) = tokio::sync::oneshot::channel::<()>();
    let writer = async {
        if workspace_request_client {
            let event = registry.workspace_requests.event(&app.id, request_operator, !request_recovery);
            if tx.send(send(&snapshot(event))).await.is_err() { return; }
        }
        let mut background_commands = (protocol >= fuzukue_proto::BACKGROUND_COMMANDS_PROTOCOL)
            .then(background_commands::Feed::default);
        let mut waiting = (protocol >= fuzukue_proto::CHAT_WAITING_PROTOCOL).then(|| {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            (tick, waiting_feed)
        });
        // MCP opens are opt-in per connection. Legacy SelectDeck still sets the
        // shared workspace, and returns the requesting connection to that view.
        let mut paper: Option<String> = None;
        'write: loop {
            let mut updates = Vec::new();
            let next = tokio::select! {
                biased;
                _ = async { let _ = swapping.as_mut().unwrap().wait_for(|swapping| *swapping).await; }, if swapping.is_some() => {
                    if protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL {
                        let _ = tx.send(wire_event(&snapshot(ServerEvent::ServerSwapNotice), protocol, None, locale_for_wire())).await;
                    }
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
                _ = &mut stop => break,
                Ok(()) = async { request_changes.as_mut().unwrap().changed().await }, if request_changes.is_some() => {
                    if request_recovery {
                        for event in registry.workspace_requests.draft_events(registry, &app.id) {
                            if tx.send(send(&snapshot(event))).await.is_err() { break 'write; }
                        }
                    }
                    let event = registry.workspace_requests.event(&app.id, true, !request_recovery);
                    if tx.send(send(&snapshot(event))).await.is_err() { break; }
                    continue;
                }
                Ok(()) = async { update_status.as_mut().unwrap().changed().await },
                    if protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL && update_status.is_some() => {
                    let status = update_status.as_mut().unwrap().borrow_and_update().clone();
                    if tx.send(wire_event(&snapshot(ServerEvent::SelfUpdateStatus { status }), protocol, None, locale_for_wire())).await.is_err() { break; }
                    continue;
                }
                Ok(()) = async { drain_status.as_mut().unwrap().changed().await }, if drain_status.is_some() => {
                    let status = drain_status.as_mut().unwrap().borrow_and_update().clone();
                    if let Some(status) = status {
                        if tx.send(wire_event(&snapshot(ServerEvent::DrainStatus { status }), protocol, None, locale_for_wire())).await.is_err() { break; }
                    }
                    continue;
                }
                _ = async { waiting.as_mut().unwrap().0.tick().await }, if waiting.is_some() => {
                    if let Some(previous) = &mut shutdown_work {
                        let work = registry.shutdown_work();
                        if *previous != work {
                            *previous = work.clone();
                            let event = ServerEvent::ShutdownWork { work };
                            if tx.send(wire_event(&snapshot(event), protocol, None, locale_for_wire())).await.is_err() { break; }
                        }
                    }
                    let update = update_status.as_ref()
                        .and_then(|status| chat_waiting::update_reason(&status.borrow()));
                    if let Some(waiting) = waiting.as_mut().unwrap().1.latest(&app, update) {
                        if tx.send(send(&snapshot(ServerEvent::ChatWaiting { waiting }))).await.is_err() {
                            break;
                        }
                    }
                    if let Some(event) = background_commands.as_mut().and_then(|feed| feed.latest(&app)) {
                        if tx.send(send(&snapshot(event))).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                notice = async { distribution.as_mut().expect("v28").1.recv().await }, if distribution.is_some() => {
                    let peer = distribution.as_ref().expect("v28").0;
                    if (notice == Ok(None) || notice == Ok(Some(peer))
                        || matches!(notice, Err(tokio::sync::broadcast::error::RecvError::Lagged(_))))
                        && tx.send(wire_event(&snapshot(ServerEvent::ClientUpdateAvailable), protocol, None, locale_for_wire())).await.is_err()
                    {
                        break;
                    }
                    continue;
                }
                changed = registry_changes.changed(), if protocol >= 11 => {
                    if changed.is_err() { break; }
                    let event = ServerEvent::Workspaces { workspaces: registry.list() };
                    if tx.send(wire_event(&snapshot(event), protocol, None, locale_for_wire())).await.is_err() { break; }
                    continue;
                }
                Ok(()) = async { research_issues.as_mut().unwrap().changed().await }, if research_issues.is_some() => {
                    let event = research_issues.as_mut().and_then(|rx| rx.borrow_and_update().clone());
                    if let Some(event) = event {
                        if tx.send(send(&snapshot(event))).await.is_err() { break; }
                    }
                    continue;
                }
                Some(selection) = paper_selections.recv() => {
                    paper = selection;
                    let deck = paper.clone().unwrap_or_else(|| app.current_deck());
                    findings_deck.clone_from(&deck);
                    let dir = project.clone();
                    let Ok(events) = tokio::task::spawn_blocking(move || {
                        let root = dir.join(&deck);
                        let ServerEvent::Workspace { tabs, .. } = cards::workspace(&dir, &deck) else { unreachable!() };
                        [
                            ServerEvent::Workspace { tabs, deck: deck.clone() },
                            ServerEvent::DeckCardsChanged { deck: deck.clone() },
                            cards::document_for_deck(&root, None),
                            ServerEvent::Comments { items: comments::load(&root), deck },
                        ]
                    }).await else { break };
                    for event in events {
                        let event = deck_event(&project, protocol, event).await;
                        if tx.send(send(&snapshot(event))).await.is_err() {
                            break 'write;
                        }
                    }
                    if supports_findings {
                        let event = review_findings::snapshot(&project, &findings_deck).await;
                        if tx.send(send(&snapshot(event))).await.is_err() { break 'write; }
                    }
                    continue;
                }
                event = live.recv() => event,
                notice = async { notices.as_mut().unwrap().recv().await }, if notices.is_some() => {
                    if let Ok(env) = notice {
                        if tx.send(wire_event(&env, protocol, env.workspace.as_deref(), locale_for_wire())).await.is_err() { break; }
                    }
                    continue;
                }
                Some(event) = replies.recv() => Ok(Envelope { workspace: None, seq: 0, event }),
            };
            match next {
                Ok(mut env) if env.seq == 0 || env.seq > last => {
                    last = last.max(env.seq);
                    // An update from another desktop can arrive while the initial
                    // snapshot is queued. Both must report the current setting.
                    if matches!(env.event, ServerEvent::WorkerApprovalPolicyChanged { .. }) {
                        env.event = app.approval_settings();
                    }
                    if let Some(paper) = &paper {
                        match &mut env.event {
                            ServerEvent::DeckCardsChanged { .. } | ServerEvent::Cards { .. } | ServerEvent::Document { .. } => continue,
                            // The card watcher ends each workspace refresh with this snapshot.
                            ServerEvent::Workspace { deck, .. } => {
                                deck.clone_from(paper);
                                let root = project.join(paper);
                                if let Ok(document) = tokio::task::spawn_blocking(move || {
                                    cards::document_for_deck(&root, None)
                                }).await {
                                    updates.extend([ServerEvent::DeckCardsChanged { deck: paper.clone() }, document]);
                                }
                            }
                            ServerEvent::BuildStatus { deck, state, pdf: Some(pdf), .. }
                                if deck == paper && state == "done" =>
                            {
                                let (root, output) = (project.join(paper), project.join(pdf));
                                if let Ok(document) = tokio::task::spawn_blocking(move || {
                                    cards::document_for_deck(&root, Some(&output))
                                }).await {
                                    updates.push(document);
                                }
                            }
                            _ => {}
                        }
                    }
                    if scoped {
                        if let ServerEvent::ThreadRestored { session, .. } = env.event {
                            env.event = ServerEvent::ThreadHistoryInvalidated { session };
                        }
                    }
                    let (projected, update) = project_job(env.seq, env.event);
                    if let Some(update) = update {
                        if tx.send(send(&update)).await.is_err() {
                            break;
                        }
                    }
                    let Some(event) = projected else { continue };
                    env.event = event;
                    if supports(&env) {
                        if matches!(env.event, ServerEvent::DeckCardsChanged { .. }) {
                            env.event = deck_event(&project, protocol, env.event).await;
                        }
                        if tx.send(send(&env)).await.is_err() {
                            break;
                        }
                    }
                    let refresh_findings = supports_findings && match &env.event {
                        ServerEvent::Workspace { deck, .. } => {
                            findings_deck = deck.replace('\\', "/");
                            true
                        }
                        ServerEvent::FileChanged { path } => review_findings::matches_deck_file(&findings_deck, path),
                        _ => false,
                    };
                    if refresh_findings {
                        updates.push(review_findings::snapshot(&project, &findings_deck).await);
                    }
                    for event in updates {
                        let event = deck_event(&project, protocol, event).await;
                        if tx.send(send(&snapshot(event))).await.is_err() {
                            break 'write;
                        }
                    }
                }
                Ok(_) => {}
                // 遅れすぎたら切る。クライアントは resume_from で戻ってくる。
                Err(_) => break,
            }
        }
    };

    let reader = async {
        let mut uploads = inbox::Uploads::default();
        let mut folder_upload = folder_attachment::FolderUpload::default();
        let mut lsp = app.lsp.connection(reply.clone());
        let mut scoped_paper = false;
        let mut approval_updates: Option<tokio::sync::broadcast::Receiver<fuzukue_proto::ApprovalSummary>> = None;
        loop {
            let msg = tokio::select! {
                msg = rx.next() => msg,
                update = async { approval_updates.as_mut().unwrap().recv().await }, if approval_updates.is_some() => {
                    // A lagging client reconnects and requests a fresh snapshot.
                    let Ok(approval) = update else { break };
                    let _ = reply.send(ServerEvent::ApprovalQueueChanged { approval });
                    continue;
                }
            };
            let Some(Ok(msg)) = msg else { break };
            let Message::Text(t) = msg else { continue };
            let received = send_latency::Received::now();
            let mut workspace_request = false;
            let msg = match serde_json::from_str::<ClientMsg>(&t) {
                Ok(ClientMsg::InWorkspace { id, request }) if protocol >= 11 => {
                    if id != app.id { continue; }
                    if matches!(*request, ClientMsg::InWorkspace { .. } | ClientMsg::Hello { .. }) { continue; }
                    workspace_request = true;
                    Ok(*request)
                }
                other => other,
            };
            match msg {
                Ok(ClientMsg::WorkspaceRequestSurface { session, inbox_revision })
                    if request_recovery && workspace_request =>
                {
                    // The report is advisory. It never authorizes a decision.
                    if session.as_ref().is_none_or(|session| session.len() <= 1024)
                        && inbox_revision.as_ref().is_none_or(|revision| revision.len() <= 64)
                    {
                        request_connection.report(session, inbox_revision);
                    }
                }
                Ok(ClientMsg::WorkspaceRequest { action }) if workspace_request_client && workspace_request => {
                    let result = workspace_requests::client_action(registry, &app, settings, action);
                    if let Err(error) = result {
                        let _ = reply.send(workspace_requests::error_event(error));
                    }
                }
                Ok(ClientMsg::ListPendingApprovals { request }) if approval_queue => {
                    let result = app.hub.pending_approvals().map(|(pending, updates)| {
                        approval_updates = Some(updates);
                        pending
                    }).map_err(|error| error.to_string());
                    let _ = reply.send(ServerEvent::ApprovalQueue { request, result });
                }
                Ok(ClientMsg::DecideApproval { request, approval_id, decision, reason }) if approval_queue => {
                    let result = app.hub.decide_approval(&approval_id, decision, grant_principal, reason)
                        .map_err(|error| error.to_string());
                    let _ = reply.send(ServerEvent::ApprovalDecisionResult { request, result });
                }
                Ok(ClientMsg::ListPendingApprovals { .. } | ClientMsg::DecideApproval { .. }) => {}
                Ok(ClientMsg::ListFolderGrants { .. } | ClientMsg::AddFolderGrant { .. }
                    | ClientMsg::RevokeFolderGrant { .. } | ClientMsg::DecideFolderAccess { .. })
                    if protocol < fuzukue_proto::FOLDER_GRANTS_PROTOCOL || !github_report_client || canary() => {}
                Ok(ClientMsg::ListFolderGrants { session, audit_before }) => {
                    match app.hub.list_folder_grants(&session, audit_before) {
                        Ok((grants, audit, audit_before)) => {
                            let _ = reply.send(ServerEvent::FolderGrants { session, grants, audit, audit_before });
                        }
                        Err(error) => { let _ = reply.send(ServerEvent::Error { message: error.to_string() }); }
                    }
                }
                Ok(ClientMsg::AddFolderGrant { session, path, expiry }) => {
                    if let Err(error) = app.hub.add_folder_grant(&session, std::path::Path::new(&path), expiry, grant_principal) {
                        let _ = reply.send(ServerEvent::Error { message: error.to_string() });
                    }
                }
                Ok(ClientMsg::RevokeFolderGrant { session, grant_id }) => {
                    if let Err(error) = app.hub.revoke_folder_grant(&session, &grant_id) {
                        let _ = reply.send(ServerEvent::Error { message: error.to_string() });
                    }
                }
                Ok(ClientMsg::DecideFolderAccess { request_id, expiry }) => {
                    if let Err(error) = app.hub.decide_folder_access(&request_id, expiry, grant_principal) {
                        let _ = reply.send(ServerEvent::Error { message: error.to_string() });
                    }
                }
                Ok(ClientMsg::AnswerQuestion { .. }) if protocol < fuzukue_proto::QUESTION_PROTOCOL => {}
                Ok(ClientMsg::SelfUpdateStatus) if canary() && protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL => {
                    let _ = reply.send(ServerEvent::SelfUpdateStatus { status: canary_snapshot() });
                }
                Ok(ClientMsg::SelfUpdateCanary { action }) if protocol >= fuzukue_proto::SELF_UPDATE_CANARY_PROTOCOL => {
                    if let Some(update) = update {
                        use fuzukue_proto::SelfUpdateCanaryAction::*;
                        let result = match action {
                            Promote => update.promote_canary().await,
                            Discard => update.discard_canary().await,
                        };
                        if let Err(error) = result {
                            let _ = reply.send(ServerEvent::SelfUpdateError { message: format!("{error:#}") });
                        }
                    }
                }
                Ok(ClientMsg::RequestDrain { reason: fuzukue_proto::DrainReason::Shutdown })
                    if protocol >= fuzukue_proto::DRAIN_PROTOCOL => {
                    if let Some(update) = update {
                        if let Err(error) = update.request_shutdown(registry.clone(), registry.shutdown_limit(), false) {
                            let message = format!("{error:#}");
                            let _ = reply.send(if safe_close { ServerEvent::ShutdownError { message } } else { ServerEvent::Error { message } });
                        }
                    }
                }
                Ok(ClientMsg::CancelDrain { started_at })
                    if protocol >= fuzukue_proto::DRAIN_PROTOCOL => {
                    if let Some(update) = update {
                        if let Err(error) = update.cancel_shutdown(started_at) {
                            let message = format!("{error:#}");
                            let _ = reply.send(if safe_close { ServerEvent::ShutdownError { message } } else { ServerEvent::Error { message } });
                        }
                    }
                }
                Ok(ClientMsg::ForceShutdown { started_at }) if safe_close => {
                    if let Some(update) = update {
                        let (update, registry, reply) = (update.clone(), registry.clone(), reply.clone());
                        tokio::spawn(async move {
                            if let Err(error) = update.force_shutdown(&registry, started_at).await {
                                let _ = reply.send(ServerEvent::ShutdownError { message: format!("{error:#}") });
                            }
                        });
                    }
                }
                Ok(ClientMsg::RequestDrain { .. } | ClientMsg::CancelDrain { .. } | ClientMsg::ForceShutdown { .. }) => {
                    let _ = reply.send(ServerEvent::Error { message: "Shutdown drain is unavailable".into() });
                }
                Ok(ClientMsg::SelfUpdateWait { started_at, action }) => {
                    if let Some(update) = update.filter(|_| protocol >= fuzukue_proto::SELF_UPDATE_WAIT_PROTOCOL) {
                        if let Err(error) = update.control_wait(started_at, action) {
                            let _ = reply.send(ServerEvent::SelfUpdateError { message: format!("{error:#}") });
                        }
                    }
                }
                Ok(ref msg) if canary() && canary_disables(msg) => {
                    let _ = reply.send(ServerEvent::Error { message: harness_text!(
                        "This action is unavailable in the canary.",
                        "この操作は試運転中には使えないよ。",
                    ).into() });
                }
                Ok(msg @ (ClientMsg::SelfUpdateStatus | ClientMsg::StartSelfUpdate | ClientMsg::RollbackServer)) => {
                    if let Some(update) = update.filter(|_| protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL) {
                        if matches!(msg, ClientMsg::SelfUpdateStatus) {
                            let status = update.refresh().await;
                            let _ = reply.send(ServerEvent::SelfUpdateStatus { status });
                        } else if let Err(error) = update.start(registry.clone(), matches!(msg, ClientMsg::RollbackServer)) {
                            let _ = reply.send(ServerEvent::SelfUpdateError { message: format!("{error:#}") });
                        }
                    }
                }
                Ok(ClientMsg::ClientScreenshotResponse { id, response }) => {
                    screenshot_client.respond(id, response);
                }
                Ok(ClientMsg::SaveScreenshot { request, destination, data, note, pane, session })
                    if supports_screenshots =>
                {
                    let context = screenshots::ReportContext {
                        platform: client_version.0.filter(|value| value.len() <= 64).map(str::to_owned),
                        build: client_version.1.filter(|value| value.len() <= 64).map(str::to_owned),
                        pane: pane.chars().take(64).collect(),
                        session: session.filter(|id| app.hub.has_thread(id)),
                    };
                    let result = if canary() && matches!(destination, fuzukue_proto::ScreenshotDestination::Dogfood) {
                        Err(anyhow::anyhow!("試運転からは不具合を送信できないよ"))
                    } else {
                        screenshots::save(app.clone(), destination, data, note, context).await
                    };
                    let (path, error) = match result {
                        Ok(path) => (Some(path), None),
                        Err(error) => (None, Some(format!("{error:#}"))),
                    };
                    let _ = reply.send(ServerEvent::ScreenshotSaved { request, path, error });
                }
                Ok(ClientMsg::SetLocale { locale: tag }) => {
                    *locale.write().unwrap() = l10n::negotiate(protocol, Some(&tag));
                }
                Ok(msg @ (ClientMsg::ListExtensions | ClientMsg::GrantExtension { .. }
                    | ClientMsg::SetExtensionEnabled { .. } | ClientMsg::UpdateExtension { .. }
                    | ClientMsg::RenderExtension { .. } | ClientMsg::ExtensionUiEvent { .. })) => {
                    if protocol >= fuzukue_proto::EXTENSIONS_PROTOCOL {
                        for event in extensions::request(app.clone(), msg, locale_for_wire()).await {
                            let _ = reply.send(event);
                        }
                    }
                }
                Ok(ClientMsg::Handoff { request }) if protocol >= fuzukue_proto::HANDOFF_PROTOCOL => {
                    let app = app.clone();
                    let reply = reply.clone();
                    let locale = *locale.read().unwrap();
                    tokio::spawn(async move {
                        if let Err(error) = handoff::handle(&app, request, locale, &reply).await {
                            let _ = reply.send(ServerEvent::HandoffFailed { message: format!("{error:#}") });
                        }
                    });
                }
                Ok(ClientMsg::GitHubReport { request, action }) if protocol >= fuzukue_proto::GITHUB_REPORT_PROTOCOL => {
                    if !github_report_client {
                        let _ = reply.send(github_auth::event(request, Err(fuzukue_proto::GitHubReportError::Disabled)));
                        continue;
                    }
                    let app = app.clone();
                    let reply = reply.clone();
                    tokio::spawn(async move {
                        let result = github_auth::handle(&app, request, action, &reply).await;
                        let _ = reply.send(github_auth::event(request, result));
                    });
                }
                Ok(ClientMsg::GitHubReport { .. }) => {}
                Ok(ClientMsg::IssueThreads { action }) if protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL => {
                    use fuzukue_proto::IssueThreadAction;
                    if protocol < fuzukue_proto::ISSUE_INTAKE_PROTOCOL && matches!(action,
                        IssueThreadAction::SetIntake { .. } | IssueThreadAction::RemoveIntake { .. }
                        | IssueThreadAction::OpenIntakeItem { .. })
                    {
                        continue;
                    }
                    if protocol < fuzukue_proto::ISSUE_RESPOND_PROTOCOL
                        && matches!(action, IssueThreadAction::Respond { .. } | IssueThreadAction::AuthorPolicy { .. })
                    {
                        continue;
                    }
                    let app = app.clone();
                    let reply = reply.clone();
                    if matches!(action, IssueThreadAction::View { .. }) {
                        let _ = Box::pin(issue_threads::handle(&app, action)).await;
                    } else {
                        tokio::spawn(async move {
                            match Box::pin(issue_threads::handle(&app, action)).await {
                                Ok(Some(event)) => { let _ = reply.send(event); }
                                Ok(None) => {}
                                Err(error) => { let _ = reply.send(ServerEvent::Error { message: format!("{error:#}") }); }
                            }
                        });
                    }
                }
                Ok(ClientMsg::ListTests) if protocol >= fuzukue_proto::TESTS_PROTOCOL => {
                    let app = app.clone();
                    let reply = reply.clone();
                    tokio::spawn(async move {
                        let (suites, error) = match app.test_suites().await {
                            Ok(suites) => (suites, None),
                            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                        };
                        let _ = reply.send(ServerEvent::TestSuites { suites, error });
                    });
                }
                Ok(ClientMsg::RunTests { suite, filter }) if protocol >= fuzukue_proto::TESTS_PROTOCOL => {
                    if let Err(error) = app.run_tests(Some(&suite), filter).await {
                        let _ = reply.send(ServerEvent::Error { message: format!("{error:#}") });
                    }
                }
                Ok(ClientMsg::StopTests { suite }) if protocol >= fuzukue_proto::TESTS_PROTOCOL => app.stop_tests(&suite),
                Ok(ClientMsg::ListTests | ClientMsg::RunTests { .. } | ClientMsg::StopTests { .. }) => {}
                Ok(ClientMsg::Trash { action }) if protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL => {
                    let event = trash::request(&app, action, locale_for_wire()).await.unwrap_or_else(|error| {
                        ServerEvent::TrashFailed { message: format!("{error:#}") }
                    });
                    let _ = reply.send(event);
                }
                Ok(ClientMsg::RemoveWorkspace { id }) if protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL => {
                    if let Err(error) = registry.remove(&id) {
                        let _ = reply.send(ServerEvent::WorkspaceError { message: format!("{error:#}") });
                    }
                }
                Ok(ClientMsg::RewriteReply { session, item }) if protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL => {
                    if let Err(error) = rewrite_reply(&app, &session, &item).await {
                        let _ = reply.send(ServerEvent::Error { message: format!("{error:#}") });
                    }
                }
                Ok(ClientMsg::ListWorkspaces) if protocol >= 11 => {
                    let _ = reply.send(ServerEvent::Workspaces { workspaces: registry.list() });
                }
                Ok(ClientMsg::CompleteWorkspacePath { prefix }) if protocol >= 11 => {
                    let registry = registry.clone();
                    if let Ok(event) = tokio::task::spawn_blocking(move || {
                        let paths = registry.complete(&prefix);
                        ServerEvent::WorkspacePaths { prefix, paths }
                    }).await {
                        let _ = reply.send(event);
                    }
                }
                Ok(ClientMsg::BrowseWorkspace { request, path, hidden }) if protocol >= 15 => {
                    let registry = registry.clone();
                    let result = tokio::task::spawn_blocking(move || registry.browse(&path, hidden))
                        .await.map_err(anyhow::Error::from).and_then(|result| result)
                        .map_err(|error| format!("{error:#}"));
                    let _ = reply.send(ServerEvent::WorkspaceDirectory { request, result });
                }
                Ok(msg @ (ClientMsg::ListFiles { .. } | ClientMsg::FindFiles { .. } | ClientMsg::ResolveFile { .. }))
                    if protocol >= fuzukue_proto::FILE_BROWSER_PROTOCOL =>
                {
                    let app = app.clone();
                    let event = tokio::task::spawn_blocking(move || {
                        let error = |err: anyhow::Error| format!("{err:#}");
                        match msg {
                            ClientMsg::ListFiles { request, dir, hidden } => ServerEvent::FileList {
                                request, result: file_browser::list(&app.project_dir, &dir, hidden).map_err(error),
                            },
                            ClientMsg::FindFiles { request, query, hidden, recent } => ServerEvent::FileMatches {
                                request, result: file_browser::find(&app.project_dir, &query, hidden, &recent).map_err(error),
                            },
                            ClientMsg::ResolveFile { request, path, lookup_deck } => ServerEvent::FileResolved {
                                request, result: file_browser::resolve_file(&app, &path, lookup_deck).map_err(error),
                            },
                            _ => unreachable!(),
                        }
                    }).await;
                    if let Ok(event) = event {
                        let _ = reply.send(event);
                    }
                }
                Ok(ClientMsg::CreateWorkspace { request, parent, name, kind }) if protocol >= 15 => {
                    let registry = registry.clone();
                    let result = tokio::task::spawn_blocking(move || registry.create_in(&parent, name, kind))
                        .await.map_err(anyhow::Error::from).and_then(|result| result)
                        .map_err(|error| format!("{error:#}"));
                    let _ = reply.send(ServerEvent::WorkspaceCreated { request, result });
                }
                Ok(msg @ (ClientMsg::OpenWorkspace { .. } | ClientMsg::SwitchWorkspace { .. })) if protocol >= 11 => {
                    let registry = registry.clone();
                    let current = app.id.clone();
                    let next = tokio::task::spawn_blocking(move || {
                        let id = match msg {
                            ClientMsg::OpenWorkspace { path } => registry.register(&path)?,
                            ClientMsg::SwitchWorkspace { id } => id,
                            _ => unreachable!(),
                        };
                        if id == current { return Ok(None); }
                        registry.select(Some(&id)).map(Some)
                    }).await;
                    match next {
                        Ok(Ok(Some(next))) => return Some(next),
                        Ok(Ok(None)) => {
                            let _ = reply.send(ServerEvent::WorkspaceSelected { id: app.id.clone(), replay: false });
                        }
                        Ok(Err(err)) => {
                            let _ = reply.send(ServerEvent::WorkspaceError { message: format!("{err:#}") });
                        }
                        Err(_) => break,
                    }
                }
                Ok(ClientMsg::FolderAttachment { id, request }) if supports_chat => {
                    if matches!(request, fuzukue_proto::FolderAttachmentRequest::Reference { .. })
                        && (grant_principal != "local_user" || !github_report_client)
                    {
                        let _ = reply.send(ServerEvent::FolderReferenceUnavailable { id });
                        continue;
                    }
                    let state = app.state_dir.clone();
                    let slots = app.folder_copy_slots.clone();
                    match tokio::task::spawn_blocking(move || {
                        let event = folder_upload.receive(&state, slots, id, request);
                        (folder_upload, event)
                    }).await {
                        Ok((next, event)) => {
                            folder_upload = next;
                            let _ = reply.send(event);
                        }
                        Err(_) => break,
                    }
                }
                Ok(ClientMsg::UploadAttachment { id, name, size, offset, data, session }) if supports_chat => {
                    let project = app.project_dir.clone();
                    let state = app.state_dir.clone();
                    match tokio::task::spawn_blocking(move || {
                        let event = uploads.receive(&project, &state, id, name, size, offset, &data);
                        (uploads, event)
                    }).await {
                        Ok((next, mut event)) => {
                            uploads = next;
                            if protocol >= 13 && matches!(event, ServerEvent::AttachmentUploaded { path: Some(_), .. }) {
                                let app = app.clone();
                                let reply = reply.clone();
                                tokio::spawn(async move {
                                    if let ServerEvent::AttachmentUploaded { path: Some(path), proposal, .. } = &mut event {
                                        let attached = session.as_deref().map(|id| app.hub.thread_attachments(id)).unwrap_or_default();
                                        *proposal = Some(inbox::proposal(&app.project_dir, &app.state_dir, path, &attached).await
                                            .unwrap_or_else(|| fuzukue_proto::InboxProposal {
                                                to: "inbox".into(), title: None, reference_id: None,
                                            }));
                                    }
                                    let _ = reply.send(event);
                                });
                            } else {
                                let _ = reply.send(event);
                            }
                        }
                        Err(_) => break,
                    }
                }
                Ok(ClientMsg::FileInbox { session, path, to }) if protocol >= 13 => {
                    if let Err(err) = inbox::confirmed_file(&app, &session, &path, to).await {
                        let _ = reply.send(ServerEvent::InboxFiled {
                            path, destination: None, error: Some(format!("{err:#}")),
                        });
                    }
                }
                Ok(ClientMsg::PauseGoal { .. }) if protocol < fuzukue_proto::GOAL_PAUSE_PROTOCOL => {}
                Ok(ClientMsg::Replan { .. } | ClientMsg::SetGoalPace { .. } | ClientMsg::SetAgentFallback { .. })
                    if protocol < fuzukue_proto::PLAN_PROTOCOL => {}
                Ok(ClientMsg::SetAgentPolicy { .. } | ClientMsg::SetAgentCaller { .. } | ClientMsg::GetAgentStats | ClientMsg::GetAgentHistory { .. })
                    if protocol < fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL => {}
                Ok(ClientMsg::GetAgentStats) => {
                    let event = match app.agent_stats().await {
                        Ok(stats) => ServerEvent::AgentStats { never_used: agent_usage::never_used(&stats), stats },
                        Err(error) => ServerEvent::Error { message: format!("作業役の集計: {error:#}") },
                    };
                    let _ = reply.send(event);
                }
                Ok(ClientMsg::GetAgentHistory { agent, session }) => {
                    let app = app.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let entries = app.agent_usage.history(&app.state_dir, agent.as_deref(), session.as_deref())?;
                        Ok::<_, anyhow::Error>(ServerEvent::AgentHistory { agent, session, entries })
                    }).await.map_err(anyhow::Error::from).and_then(|event| event);
                    let _ = reply.send(result.unwrap_or_else(|error| ServerEvent::Error {
                        message: format!("作業役の履歴: {error:#}"),
                    }));
                }
                Ok(ClientMsg::ListAgents) if protocol >= 14 => {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let _ = reply.send(app.agent_catalog_event().await);
                    });
                }
                Ok(ClientMsg::ListAgents | ClientMsg::SetAgentOverride { .. } | ClientMsg::SetThreadAgentOverride { .. }
                    | ClientMsg::SetSummaryModel { .. } | ClientMsg::WorkerPrompt { .. } | ClientMsg::StopWorker { .. })
                    if protocol < 14 => {}
                Ok(ClientMsg::RenameJob { .. }) if protocol < fuzukue_proto::JOB_LABEL_PROTOCOL => {}
                Ok(ClientMsg::DismissJob { .. }) if protocol < fuzukue_proto::JOB_DISMISS_PROTOCOL => {}
                Ok(ClientMsg::SetAgentRoleModel { .. }) if protocol < fuzukue_proto::AGENT_MODELS_PROTOCOL => {}
                Ok(ClientMsg::SetAgentRoleModel { role: fuzukue_proto::AgentModelRole::Escalation, .. }
                    | ClientMsg::UseDefaultThreadModel { .. }) if protocol < fuzukue_proto::LIVE_MODELS_PROTOCOL => {}
                Ok(ClientMsg::Interrupt { session }) if protocol < 14 && !app.hub.thread_active(&session)
                    && app.delegates.only_job(&session).is_some() => {
                    if let Some(job) = app.delegates.only_job(&session) {
                        app.delegates.interrupt_job(&session, &job);
                    }
                }
                Ok(ClientMsg::SearchDecks { query }) if protocol >= 13 => {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let decks = routing::search(app, query.clone()).await;
                        let _ = reply.send(ServerEvent::DeckSearchResults { query, decks });
                    });
                }
                Ok(ClientMsg::Ledger { request, action }) if protocol >= fuzukue_proto::LEDGER_PROTOCOL => {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let view = match references::ledger::dispatch(&app, action).await {
                            Ok(view) => Some(view),
                            Err(e) => {
                                tracing::warn!("ledger operation: {e:#}");
                                None
                            }
                        };
                        let _ = reply.send(ServerEvent::LedgerResult { request, view });
                    });
                }
                Ok(ClientMsg::SearchLibrary { request, query, mut kinds, deck, sort, offset, limit })
                    if protocol >= fuzukue_proto::LIBRARY_PROTOCOL =>
                {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let limit = limit.clamp(1, fuzukue_proto::LIBRARY_PAGE_SIZE);
                        let legacy = protocol < fuzukue_proto::MEMOS_PROTOCOL;
                        if legacy {
                            use fuzukue_proto::LibraryKind::*;
                            if kinds.is_empty() {
                                kinds = vec![Reference, Card, Thread, Comment];
                            } else if kinds.contains(&Memo) {
                                return;
                            }
                        }
                        let search = library::Search { query, kinds, deck, sort, offset, limit, protocol };
                        let results = library::search(app, search).await.unwrap_or_default();
                        let library::Results { hits, more, mut decks } = results;
                        if legacy {
                            decks.retain(|deck| deck != memos::DECK);
                        }
                        let _ = reply.send(ServerEvent::LibraryResults { request, hits, more, decks });
                    });
                }
                Ok(ClientMsg::AddMemo { request, text, from }) if protocol >= fuzukue_proto::MEMOS_PROTOCOL => {
                    let error = memos::add(app.clone(), text, from).await.err().map(|e| format!("{e:#}"));
                    let _ = reply.send(ServerEvent::MemoResult { request, error });
                }
                Ok(ClientMsg::MoveMemo { request, id, deck }) if protocol >= fuzukue_proto::MEMOS_PROTOCOL => {
                    let error = memos::move_to(app.clone(), id, deck).await.err().map(|e| format!("{e:#}"));
                    let _ = reply.send(ServerEvent::MemoResult { request, error });
                }
                Ok(ClientMsg::ReviewReports { deck, request, action }) if protocol >= fuzukue_proto::REVIEW_REPORTS_PROTOCOL => {
                    match review_reports::handle(&app, &deck, action).await {
                        Ok(Some(event)) => { let _ = reply.send(event); }
                        Ok(None) => {
                            if request.is_some() { let _ = reply.send(ServerEvent::ReviewReportsResult { deck, request, ok: true }); }
                        }
                        Err(error) => {
                            tracing::warn!("review reports for {deck}: {error:#}");
                            let _ = reply.send(ServerEvent::ReviewReportsResult { deck, request, ok: false });
                        }
                    }
                }
                Ok(ClientMsg::GetLeanReadingCards { request, deck, declarations })
                    if protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL =>
                {
                    let project = app.project_dir.clone();
                    let reply = reply.clone();
                    tokio::task::spawn_blocking(move || {
                        let cards = lean_reading::reading_cards(&project, &deck, &declarations);
                        let _ = reply.send(ServerEvent::LeanReadingCards { request, cards });
                    });
                }
                Ok(ClientMsg::LookupSymbol { request, lookup }) if protocol >= fuzukue_proto::SYMBOL_PROTOCOL
                    && (protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL || !matches!(
                        lookup, fuzukue_proto::SymbolLookup::LeanDependencies { .. }
                    )) => {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let results = graph::WorkspaceGraph::symbol_lookup(app, lookup).await;
                        let _ = reply.send(ServerEvent::SymbolResults { request, results });
                    });
                }
                Ok(ClientMsg::SelectPaper { deck }) if desktop_cards && protocol >= 9 => {
                    let path = if deck.is_empty() { "." } else { &deck };
                    let target = if path == "." && app.project_dir.join("cards").is_dir() {
                        Ok(".".to_owned())
                    } else {
                        workspace::deck_path(&app.project_dir, path)
                    };
                    match target {
                        Ok(deck) => {
                            scoped_paper = true;
                            let _ = select_paper.send(Some(if deck == "." { String::new() } else { deck }));
                        }
                        Err(e) => { let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") }); }
                    }
                }
                Ok(msg @ ClientMsg::SelectDeck { .. }) if scoped_paper => {
                    match handle(&app, msg).await {
                        Ok(()) => {
                            scoped_paper = false;
                            let _ = select_paper.send(None);
                        }
                        Err(e) => { let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") }); }
                    }
                }
                Ok(ClientMsg::GetPaperPdfMap { deck, pdf }) if protocol >= 10 => {
                    let app = app.clone();
                    let reply = reply.clone();
                    tokio::spawn(async move {
                        let _serial = app.builds.serial.lock().await;
                        if let Some(event) = app.paper_pdf_map(deck, pdf, false).await {
                            let _ = reply.send(event);
                        }
                    });
                }
                Ok(ClientMsg::LspBuffer { document, update }) if protocol >= fuzukue_proto::LSP_PROTOCOL => {
                    lsp.update(document, update);
                }
                Ok(ClientMsg::LspRequest { request, document, query }) if protocol >= fuzukue_proto::LSP_PROTOCOL => {
                    lsp.request(request, document, query);
                }
                Ok(ClientMsg::SaveSource { request, edit }) if protocol >= fuzukue_proto::EDITOR_PROTOCOL => {
                    let guard = app.file_save.clone().lock_owned().await;
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let _guard = guard;
                        let outcome = source_editor::save(&app.project_dir, &app.state_dir, edit, Some(app.clone())).await
                            .unwrap_or_else(|error| fuzukue_proto::SourceSaveOutcome::Error {
                                message: format!("{error:#}"),
                            });
                        let _ = reply.send(ServerEvent::SourceSaveResult { request, outcome });
                    });
                }
                Ok(ClientMsg::EditorClosed { path }) if protocol >= fuzukue_proto::EDITOR_PROTOCOL => {
                    let _guard = app.file_save.lock().await;
                    if let Ok(path) = source_editor::closed_path(&app.project_dir, &path) {
                        app.human_file_closed(&app.project_dir.join(&path));
                        app.hub.publish(ServerEvent::EditorClosed { path });
                    }
                }
                Ok(ClientMsg::SaveCard { request, deck, card, part, body, base_hash }) if protocol >= 9 => {
                    let _guard = app.file_save.lock().await;
                    let edited_deck = deck.clone();
                    let noting = app.clone();
                    let saved = tokio::task::spawn_blocking(move || {
                        cards::save_card_with_build(&noting.project_dir, &edited_deck, &card, part, &body, &base_hash, Some(&noting))
                    }).await;
                    let (outcome, build) = match saved {
                        Ok(Ok(saved)) => saved,
                        Ok(Err(e)) => (CardSaveOutcome::Error { message: format!("{e:#}") }, None),
                        Err(e) => (CardSaveOutcome::Error { message: e.to_string() }, None),
                    };
                    if matches!(outcome, CardSaveOutcome::Saved) {
                        // 保存するとカードの編集画面が閉じる。編集をやめた区切りとして扱う。
                        if let Ok(root) = safe_join(&app.project_dir, &deck) {
                            app.human_deck_closed(&root, human_edits::SETTLE);
                        }
                    }
                    let _ = reply.send(ServerEvent::CardSaveResult { request, outcome });
                    if let Some(saved) = build {
                        let app = app.clone();
                        let reply = reply.clone();
                        tokio::spawn(async move {
                            let outcome = app.build_saved_card_tex(saved).await;
                            let _ = reply.send(ServerEvent::CardTexBuildResult { request, outcome });
                        });
                    }
                }
                Ok(ClientMsg::RecordSourceFingerprint { request, deck, card, revision })
                    if protocol >= fuzukue_proto::TRANSLATION_DRIFT_PROTOCOL =>
                {
                    let _guard = app.file_save.lock().await;
                    let noting = app.clone();
                    let outcome = match tokio::task::spawn_blocking(move || {
                        let outcome = cards::record_source_fingerprint(
                            &noting.project_dir, &deck, &card, &revision, Some(&noting),
                        )?;
                        if matches!(outcome, CardSaveOutcome::Saved) {
                            let root = safe_join(&noting.project_dir, &deck)?;
                            noting.human_deck_closed(&root, human_edits::SETTLE);
                        }
                        Ok::<_, anyhow::Error>(outcome)
                    }).await {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(e)) => CardSaveOutcome::Error { message: format!("{e:#}") },
                        Err(e) => CardSaveOutcome::Error { message: e.to_string() },
                    };
                    let _ = reply.send(ServerEvent::CardSaveResult { request, outcome });
                }
                Ok(ClientMsg::CancelAttachment { id }) => {
                    uploads.cancel(id);
                    match tokio::task::spawn_blocking(move || {
                        folder_upload.cancel(id);
                        folder_upload
                    }).await {
                        Ok(next) => folder_upload = next,
                        Err(_) => break,
                    }
                }
                Ok(msg @ (ClientMsg::NewThread | ClientMsg::NewAttachedThread { .. })) if protocol >= 4 => {
                    let deck = match &msg {
                        ClientMsg::NewAttachedThread { deck } => Some(deck.as_str()),
                        _ => None,
                    };
                    match app.hub.edit_threads(|store| store.create(deck)) {
                        Ok(id) => { let _ = reply.send(ServerEvent::ThreadCreated { id }); }
                        Err(e) => { let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") }); }
                    }
                }
                Ok(mut prompt @ (ClientMsg::Prompt { .. } | ClientMsg::PromptWithAttachments { .. })) if protocol >= 4 => {
                    let (session, text) = prompt_text(&prompt);
                    if let commands::Input::Command { name: "full-approval", args } = commands::parse(text) {
                        let result = full_approval::command(&app.hub, args, settings);
                        let message = match result {
                            Ok(Some(mode)) => match mode.expires_at.and_then(chrono::DateTime::from_timestamp_millis) {
                                Some(at) => fuzukue_proto::LocalizedMessage::new("server-full-approval-until", [
                                    ("at", at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %Z").to_string()),
                                ]),
                                None => fuzukue_proto::LocalizedMessage::new("server-full-approval-until-disabled", []),
                            },
                            Ok(None) => fuzukue_proto::LocalizedMessage::new("server-full-approval-off", []),
                            Err(error) => {
                                let _ = reply.send(ServerEvent::Error { message: error.to_string() });
                                continue;
                            }
                        };
                        let text = l10n::render(fuzukue_i18n::Locale::JAPANESE, &message);
                        if app.hub.has_thread(session) {
                            app.hub.publish(ServerEvent::LocalNotice {
                                session: session.into(), item: mail::session_key(), text, localized: Some(message),
                            });
                        } else {
                            let _ = reply.send(ServerEvent::Error { message: l10n::render(*locale.read().unwrap(), &message) });
                        }
                        continue;
                    }
                    if session.is_empty() && matches!(commands::parse(text), commands::Input::Command { name: "new", .. }) {
                        if let Some(id) = commands::dispatch(&app, session, text).await {
                            let _ = reply.send(ServerEvent::ThreadCreated { id });
                        }
                        continue;
                    }
                    match prepare_prompt_thread(&app, &mut prompt) {
                        Ok(Some(id)) => { let _ = reply.send(ServerEvent::ThreadCreated { id }); }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") });
                            continue;
                        }
                    }
                    let (session, text) = prompt_text(&prompt);
                    if fuzukue_proto::slash::is_command(text) && !commands::is_escalation(text) {
                        if let Some(id) = commands::dispatch(&app, session, text).await {
                            let _ = reply.send(ServerEvent::ThreadCreated { id });
                        }
                        continue;
                    }
                    let session = session.to_owned();
                    let handle = handle_from_client(&app, prompt, Some(&screenshot_client));
                    if let Err(e) = send_latency::Received::scope(received, handle).await {
                        let _ = reply.send(backend_switch_notice(&session, &e)
                            .unwrap_or_else(|| ServerEvent::Error { message: format!("{e:#}") }));
                    }
                }
                Ok(ClientMsg::ApplyComments { deck, origin }) if protocol >= 6 => {
                    let needs_origin = origin.is_empty();
                    match apply_comments(&app, &deck, origin).await {
                        Ok(origin) if needs_origin => { let _ = reply.send(ServerEvent::ThreadCreated { id: origin }); }
                        Ok(_) => {}
                        Err(e) => { let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") }); }
                    }
                }
                Ok(ClientMsg::BranchReview { id, request }) if protocol >= fuzukue_proto::BRANCH_REVIEW_PROTOCOL
                    && (protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL || !matches!(
                        request, fuzukue_proto::BranchReviewRequest::ConfirmLeanClaims { .. } |
                                 fuzukue_proto::BranchReviewRequest::MergeWithUnconfirmedLean { .. }
                    )) => {
                    let (app, reply) = (app.clone(), reply.clone());
                    tokio::spawn(async move {
                        let _ = reply.send(branch_review::request(app, id, request).await);
                    });
                }
                Ok(ClientMsg::History { id, request }) if protocol >= fuzukue_proto::CHANGE_HISTORY_PROTOCOL => {
                    let _ = reply.send(history::request(app.clone(), id, request).await);
                }
                Ok(ClientMsg::Git { id, request }) if protocol >= 12 => {
                    if canary() && matches!(request, fuzukue_proto::GitRequest::Push { .. } | fuzukue_proto::GitRequest::Pull { .. }) {
                        let _ = reply.send(ServerEvent::GitResult { id, outcome: fuzukue_proto::GitOutcome::Error {
                            message: harness_text!("Git remote actions are unavailable in the canary.", "試運転からは Git のリモート操作ができないよ。").into(),
                        }});
                    } else if matches!(request, fuzukue_proto::GitRequest::Push { .. } | fuzukue_proto::GitRequest::Pull { .. }) {
                        // The reader must remain available for the user's Resolve message.
                        let (app, reply) = (app.clone(), reply.clone());
                        tokio::spawn(async move {
                            let _ = reply.send(git::ui_request(app, id, request).await);
                        });
                    } else {
                        let _ = reply.send(git::ui_request(app.clone(), id, request).await);
                    }
                }
                Ok(ClientMsg::GetThreadJobs { session }) if job_summaries && workspace_request => {
                    let (through, jobs) = app.hub.thread_jobs(&session);
                    // Send large job lists in bounded batches.
                    if jobs.is_empty() {
                        let _ = reply.send(ServerEvent::ThreadJobs { session, through, first: true, jobs });
                    } else {
                        for (index, jobs) in jobs.chunks(128).enumerate() {
                            let _ = reply.send(ServerEvent::ThreadJobs {
                                session: session.clone(), through, first: index == 0, jobs: jobs.to_vec(),
                            });
                        }
                    }
                }
                Ok(ClientMsg::GetThreadJobReport { session, job, request }) if job_summaries && workspace_request => {
                    let _ = reply.send(app.hub.thread_job_report(session, job, request));
                }
                Ok(ClientMsg::GetThreadTail { session, request }) if scoped && workspace_request => {
                    let _ = reply.send(app.hub.thread_tail_mode(session, request, compact_jobs));
                }
                Ok(ClientMsg::GetThreadHistory { session, request, before, generation })
                    if thread_history && (!scoped || workspace_request) =>
                {
                    let _ = reply.send(app.hub.thread_history_page_mode(session, request, before, generation, scoped, compact_jobs));
                }
                Ok(ClientMsg::GetThreadHistoryFrom { session, request, start, generation })
                    if scoped && workspace_request && protocol >= fuzukue_proto::THREAD_WINDOW_PROTOCOL =>
                {
                    let _ = reply.send(app.hub.thread_history_from(session, request, start, generation));
                }
                Ok(ClientMsg::GetThreadJob { session, job, request, before }) if compact_jobs && workspace_request => {
                    {
                        let mut opened = open_jobs.lock().unwrap();
                        opened.retain(|(owner, item)| owner != &session || item != &job);
                        if request.is_some() {
                            opened.push_back((session.clone(), job.clone()));
                            if opened.len() > 4 {
                                opened.pop_front();
                            }
                        }
                    }
                    if let Some(request) = request {
                        let _ = reply.send(app.hub.thread_job_details(session, job, request, before));
                    }
                }
                Ok(ClientMsg::GetThreadTail { .. } | ClientMsg::GetThreadHistory { .. }
                    | ClientMsg::GetThreadHistoryFrom { .. } | ClientMsg::GetThreadJobs { .. }
                    | ClientMsg::GetThreadJobReport { .. } | ClientMsg::GetThreadJob { .. }) => {}
                Ok(ClientMsg::GetThreadArtifacts { session }) if protocol >= 6 => {
                    let paths = app.hub.thread_artifacts(&session);
                    let artifacts = thread_artifacts::describe(&app.project_dir, paths).await;
                    let _ = reply.send(ServerEvent::ThreadArtifacts { session, artifacts });
                }
                Ok(ClientMsg::ArchiveThread { id, archived }) if protocol >= 7 => {
                    if let Err(e) = app.hub.set_thread_archived(&id, archived) {
                        let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") });
                    }
                }
                Ok(ClientMsg::PinThread { id, pinned }) if protocol >= 15 => {
                    if let Err(e) = app.hub.edit_threads(|store| store.set_pinned(&id, pinned)) {
                        let _ = reply.send(ServerEvent::Error { message: format!("{e:#}") });
                    }
                }
                Ok(ClientMsg::CompactSession { session }) if protocol >= 6 => {
                    commands::dispatch(&app, &session, "/compact").await;
                }
                Ok(ClientMsg::SendComments { deck, session, confirm }) => {
                    let needs_thread = session.is_empty();
                    match send_comments(&app, &deck, session).await {
                        Ok(session) => {
                            if needs_thread {
                                let _ = reply.send(ServerEvent::ThreadCreated { id: session.clone() });
                            }
                            if confirm {
                                let _ = reply.send(ServerEvent::CommentsSent { deck, session });
                            }
                        }
                        Err(e) => { let _ = reply.send(ServerEvent::Error { message: e.to_string() }); }
                    }
                }
                Ok(ClientMsg::SetReviewSettings { .. }) if protocol < 13 => {}
                Ok(ClientMsg::SetWorkerApprovalPolicy { .. })
                    if protocol < fuzukue_proto::WORKER_APPROVAL_PROTOCOL || !github_report_client => {}
                Ok(ClientMsg::RememberShellApproval { .. } | ClientMsg::RevokeShellApproval { .. })
                    if protocol < fuzukue_proto::WORKER_APPROVAL_PROTOCOL || !github_report_client => {
                    let _ = reply.send(ServerEvent::Error { message: "Saved shell approvals require an authenticated native client".into() });
                }
                Ok(ClientMsg::StartResearchIssue { number } | ClientMsg::AcceptResearchIssue { number, .. })
                    if protocol < 13 || number == 0 => {
                    let _ = reply.send(ServerEvent::Error { message: "研究 Issue の要求が不正".into() });
                }
                Ok(ClientMsg::StartResearchIssue { number }) => {
                    let (registry, app, reply) = (registry.clone(), app.clone(), reply.clone());
                    tokio::spawn(async move {
                        match workshop::start(&registry, &app, "", fuzukue_proto::Actor::System, number).await {
                            Ok(Some(id)) => { let _ = reply.send(ServerEvent::ThreadCreated { id }); }
                            Ok(None) => {}
                            Err(err) => { let _ = reply.send(ServerEvent::Error { message: format!("{err:#}") }); }
                        }
                    });
                }
                Ok(ClientMsg::AcceptResearchIssue { number }) => {
                    let (registry, app, reply) = (registry.clone(), app.clone(), reply.clone());
                    tokio::spawn(async move {
                        if let Err(err) = workshop::accept(&registry, &app, "", fuzukue_proto::Actor::System, number).await {
                            let _ = reply.send(ServerEvent::Error { message: format!("{err:#}") });
                        }
                    });
                }
                Ok(m) => {
                    let confirmation = match &m {
                        ClientMsg::WorkIssue { number } => Some(ServerEvent::ThreadCreated {
                            id: fuzukue_proto::issue_thread_id(*number),
                        }),
                        _ => None,
                    };
                    let switch_session = match &m {
                        ClientMsg::SetChatSettings { session, .. } | ClientMsg::RolePrompt { session, .. } =>
                            Some(session.clone()),
                        _ => None,
                    };
                    let received = send_latency::Received::for_message(received, &m);
                    let handle = handle_from_client(&app, m, Some(&screenshot_client));
                    match send_latency::Received::scope(received, handle).await {
                        Ok(()) => {
                            if let Some(event) = confirmation {
                                let _ = reply.send(event);
                            }
                        }
                        Err(e) => {
                            if let Some(notice) = switch_session.and_then(|session| backend_switch_notice(&session, &e)) {
                                let _ = reply.send(notice);
                            } else {
                                app.hub.publish(ServerEvent::Error { message: e.to_string() });
                            }
                        }
                    }
                }
                Err(e) => app.hub.publish(ServerEvent::Error { message: format!("bad message: {e}") }),
            }
        }
        None
    };
    tokio::pin!(writer);
    tokio::select! {
        next = reader => {
            let _ = stop_writer.send(());
            writer.await;
            next
        }
        _ = &mut writer => None,
    }
}

fn prompt_text(prompt: &ClientMsg) -> (&str, &str) {
    match prompt {
        ClientMsg::Prompt { session, text } | ClientMsg::PromptWithAttachments { session, text, .. }
        | ClientMsg::RolePrompt { session, text, .. } | ClientMsg::WorkerPrompt { session, text, .. } => (session, text),
        _ => unreachable!(),
    }
}

fn prepare_prompt_thread(app: &Workspace, prompt: &mut ClientMsg) -> anyhow::Result<Option<String>> {
    let (session, text, paths) = match prompt {
        ClientMsg::Prompt { session, text } => (session, text, &[][..]),
        ClientMsg::PromptWithAttachments { session, text, paths } => (session, text, paths.as_slice()),
        _ => unreachable!(),
    };
    if !session.is_empty() {
        return Ok(None);
    }
    let _work = app.admit_work()?;
    anyhow::ensure!(!text.trim().is_empty() || !paths.is_empty(), "メッセージを入力する");
    anyhow::ensure!(paths.len() <= fuzukue_proto::MAX_ATTACHMENTS, "添付が多すぎます");
    for path in paths {
        attachment_read_path(&app.project_dir, &app.state_dir, path)?;
    }
    let id = app.hub.edit_threads(|store| store.create(None))?;
    *session = id.clone();
    Ok(Some(id))
}

async fn send_comments(app: &Arc<Workspace>, deck: &str, session: String) -> anyhow::Result<String> {
    let root = safe_join(&app.project_dir, deck)?;
    let _edit = app.comment_edit.lock().await;
    let items = comments::read(&root)?;
    anyhow::ensure!(!items.is_empty(), "送るコメントがない");
    let title = fuzukue_cards::DeckConfig::read(&root).map(|c| c.meta.title).unwrap_or_default();
    let text = comments::compose(if title.is_empty() { deck } else { &title }, deck, &items);
    let session = if session.is_empty() {
        let path = if deck.is_empty() { "." } else { deck };
        let attached = workspace::deck_path(&app.project_dir, path).ok();
        app.hub.edit_threads(|store| store.create(attached.as_deref()))?
    } else {
        session
    };
    Box::pin(handle(app, ClientMsg::Prompt { session: session.clone(), text })).await?;
    // 送ったら消す
    comments::save(&root, &[])?;
    app.hub.publish_snapshot(ServerEvent::Comments { deck: deck.into(), items: Vec::new() });
    Ok(session)
}

async fn apply_comments(app: &Arc<Workspace>, deck: &str, origin: String) -> anyhow::Result<String> {
    apply_comment_drafts(app, deck, origin, None).await
}

async fn apply_comment_drafts(
    app: &Arc<Workspace>, deck: &str, origin: String, selected: Option<Vec<fuzukue_proto::CommentView>>,
) -> anyhow::Result<String> {
    use fuzukue_proto::{CommentApplication, CommentApplicationState};

    let _work = app.admit_work()?;
    let attachment = workspace::deck_path(&app.project_dir, if deck.is_empty() { "." } else { deck })?;
    let root = app.project_dir.join(&attachment);
    let _edit = app.comment_edit.lock().await;
    let saved = comments::read(&root)?;
    let (items, remaining) = if let Some(mut selected) = selected {
        if app.comment_applications.lock().await.values().any(|application| application.root == root) {
            return Ok(origin);
        }
        let partition: (Vec<_>, Vec<_>) = saved.into_iter().partition(|item| {
            if let Some(i) = selected.iter().position(|draft| draft == item) {
                selected.swap_remove(i);
                true
            } else {
                false
            }
        });
        if partition.0.is_empty() || (!origin.is_empty() && !app.hub.has_thread(&origin)) {
            return Ok(origin);
        }
        partition
    } else {
        anyhow::ensure!(!saved.is_empty(), "送るコメントがない");
        (saved, Vec::new())
    };
    let title = fuzukue_cards::DeckConfig::read(&root).map(|c| c.meta.title).unwrap_or_default();
    let title = if title.is_empty() { attachment.as_str() } else { &title };
    let text = comments::compose(title, &attachment, &items);
    let title = format!("コメント反映: {title} ({} 件)", items.len());
    let (origin, thread) = app.hub.edit_threads(|store| {
        let origin = if origin.is_empty() {
            store.create(Some(&attachment))?
        } else {
            anyhow::ensure!(store.contains(&origin), "元のスレッドが見つからない");
            origin
        };
        Ok((origin, store.create_titled(Some(&attachment), Some(title.clone()))?))
    })?;
    let notice = CommentApplication {
        thread: thread.clone(), title, count: items.len(), state: CommentApplicationState::Running,
    };
    let paper = root.clone();
    let before = tokio::task::spawn_blocking(move || {
        fuzukue_cards::Graph::open_local(&paper).and_then(|graph| auto_review::card_files(&graph))
    }).await?;
    app.comment_applications.lock().await.insert(thread.clone(), comments::Application {
        origin: origin.clone(), notice: notice.clone(), root: root.clone(), before,
    });
    app.hub.publish(ServerEvent::CommentApplication { session: origin.clone(), application: notice });
    let result = start_side_thread(app, &thread, text).await;
    if let Err(error) = result {
        app.finish_comment_application(&thread, Err(error)).await;
        app.hub.publish(ServerEvent::TurnEnded { session: thread, actor: fuzukue_proto::Actor::Claude });
    } else {
        comments::save(&root, &remaining)?;
        app.hub.publish_snapshot(ServerEvent::Comments { deck: deck.into(), items: remaining });
    }
    Ok(origin)
}

async fn start_side_thread(app: &Arc<Workspace>, thread: &str, text: String) -> anyhow::Result<()> {
    let event = Some(prompt_event(thread, fuzukue_proto::Actor::User, &text));
    let _start = app.prompt_start.lock().await;
    let _work = app.admit_work()?;
    let settings = app.partner_turn_settings(thread);
    if AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex {
        prompt_codex(app, thread.to_owned(), settings, text, event).await
    } else {
        prompt_claude(app, thread, text, event).await
    }
}

async fn claude_session(
    app: &Arc<Workspace>,
    session: &str,
    user_event: Option<ServerEvent>,
) -> anyhow::Result<(Arc<ClaudeSession>, Option<ServerEvent>)> {
    claude_session_for(app, session, user_event, None).await
}

async fn claude_session_for(
    app: &Arc<Workspace>,
    session: &str,
    mut user_event: Option<ServerEvent>,
    settings: Option<&fuzukue_proto::ChatSettings>,
) -> anyhow::Result<(Arc<ClaudeSession>, Option<ServerEvent>)> {
    let mut sessions = app.sessions.lock().await;
    let isolated = app.issue_threads.intake_turn(session);
    if isolated || app.issue_threads.isolated_claude(session) {
        if let Some(previous) = sessions.get(session) {
            previous.retire_for_partner_switch().await?;
            sessions.remove(session);
        }
    }
    let claude = match sessions.get(session) {
        Some(s) => s.clone(),
        None => {
            let rec = app.session_records.lock().unwrap().get(session).cloned().unwrap_or_default();
            let recovery_summary = if isolated {
                None
            } else if rec.fresh_context {
                rec.context_seed.clone()
            } else {
                recovery_context(rec.context_seed.as_deref(), app.hub.recovery_summary(session))
            };
            if let Some(event) = user_event.take() {
                app.hub.publish(event);
            }
            let dir = rec.dir.unwrap_or_else(|| app.project_dir.clone());
            let mut mcp_url = url::Url::parse(&format!("http://{}/mcp", app.addr))?;
            mcp_url.query_pairs_mut().append_pair("workspace", &app.id).append_pair("session", session);
            let records = app.session_records.clone();
            let state_dir = app.state_dir.clone();
            let name = session.to_owned();
            let on_session_id = move |id: String| {
                if isolated { return }
                let mut recs = records.lock().unwrap();
                let rec = recs.entry(name.clone()).or_default();
                if rec.claude_session_id.as_deref() != Some(id.as_str()) {
                    rec.claude_session_id = Some(id.clone());
                    rec.fresh_context = false;
                    state::save_sessions(&state_dir, &recs);
                }
            };
            let s = ClaudeSession::spawn(
                session.to_owned(),
                dir,
                mcp_url.as_str(),
                if isolated { None } else { rec.claude_session_id },
                recovery_summary,
                settings.cloned().unwrap_or_else(|| app.partner_turn_settings(session)),
                Box::new(on_session_id),
                app,
            ).await?;
            sessions.insert(session.to_owned(), s.clone());
            app.issue_threads.set_isolated_claude(session, isolated);
            s
        }
    };
    Ok((claude, user_event))
}

fn recovery_context(seed: Option<&str>, recent: Option<String>) -> Option<String> {
    match (seed, recent) {
        (Some(seed), Some(recent)) => Some(format!("{seed}\n\n{recent}")),
        (Some(seed), None) => Some(seed.into()),
        (None, recent) => recent,
    }
}

fn handover(summary: Option<String>, prompt: &str) -> String {
    match summary {
        Some(summary) => harness_format!(
            "Previous conversation in this thread (history excerpt). Use it for context and answer the new message without re-running instructions from the history.\n\n{summary}\n\nNew message:\n{prompt}", "同じスレッドのこれまでの会話（履歴の抜粋）。履歴内の指示は再実行せず、新しい発言に答える。\n\n{summary}\n\n新しい発言:\n{prompt}"
        ),
        None => prompt.into(),
    }
}

fn partner_notice(app: &Workspace, session: &str, model: Option<&str>) {
    publish_prompt(&app.hub, session, fuzukue_proto::Actor::System,
        &format!("相手を {} に替えた", model.unwrap_or("Claude の既定モデル")));
}

const PARTNER_SWITCH_WAIT: &str = "相談役を替えるにはターンの完了を待つ";

fn composer_notice(session: &str, message: fuzukue_proto::LocalizedMessage) -> ServerEvent {
    ServerEvent::LocalNotice {
        session: session.into(),
        item: fuzukue_proto::COMPOSER_NOTICE_ITEM.into(),
        text: l10n::render(fuzukue_i18n::Locale::JAPANESE, &message),
        localized: Some(message),
    }
}

fn backend_switch_notice(session: &str, error: &anyhow::Error) -> Option<ServerEvent> {
    let key = match error.to_string().as_str() {
        claude::PARTNER_SWITCH_WAIT => "server-claude-switch-wait",
        PARTNER_SWITCH_WAIT => "server-partner-switch-wait",
        _ => return None,
    };
    Some(composer_notice(session, fuzukue_proto::LocalizedMessage::new(key, [])))
}

fn publish_codex_turn_error(hub: &Hub, session: &str, error: anyhow::Error) {
    if error.is::<cancellation::Cancelled>() {
        hub.publish_snapshot(composer_notice(session, fuzukue_proto::LocalizedMessage::new("server-codex-stopped", [])));
    } else {
        hub.publish(ServerEvent::Error { message: format!("Codex: {error:#}") });
    }
}

async fn prompt_codex(
    app: &Arc<Workspace>, session: String, settings: fuzukue_proto::ChatSettings,
    prompt: String, event: Option<ServerEvent>,
) -> anyhow::Result<()> {
    prompt_codex_with_images(app, session, settings, prompt, event, Vec::new(), TurnKind::Conversation, None).await
}

fn codex_partner_options(app: &Workspace, session: &str, settings: fuzukue_proto::ChatSettings) -> anyhow::Result<codex::PartnerOptions> {
    let mut context = format!("{}\n\n{}", threads::prompt(), delegate::prompt());
    context.push_str(workspaces::prompt());
    context.push_str(workspace_requests::prompt());
    context.push_str(workshop::mcp::prompt());
    context.push_str(issue_threads::prompt());
    context.push_str(handoff::prompt());
    if app.mail.is_some() {
        context.push_str(mail::prompt());
    }
    let mut url = url::Url::parse(&format!("http://{}/mcp", app.addr))?;
    url.query_pairs_mut().append_pair("workspace", &app.id).append_pair("session", session).append_pair("backend", AgentBackend::Codex.usage_key());
    Ok(codex::PartnerOptions {
        settings,
        context,
        mcp_url: url.to_string(),
        mcp_key: mail::session_key(),
        agent_settings: app.agent_settings(),
    })
}

fn github_auth_request(kind: TurnKind, prompt: &str, event: Option<&ServerEvent>) -> Option<github_auth::mcp::Tool> {
    if !kind.user_conversation() {
        return None;
    }
    let text = match event {
        Some(ServerEvent::TextDelta { text, actor: fuzukue_proto::Actor::User, .. })
        | Some(ServerEvent::UserPrompt { text, to_worker: false, .. }) => text,
        _ => prompt,
    };
    github_auth::mcp::request(text)
}

#[allow(clippy::too_many_arguments)]
async fn prompt_codex_with_images(
    app: &Arc<Workspace>, session: String, mut settings: fuzukue_proto::ChatSettings,
    prompt: String, event: Option<ServerEvent>, images: Vec<PathBuf>, kind: TurnKind, desk_record: Option<delegate::DeskDelivery>,
) -> anyhow::Result<()> {
    let github_auth = github_auth_request(kind, &prompt, event.as_ref());
    let codex = app.codex().await?;
    if settings.effort.is_none() {
        // A null turn/start effort inherits Codex's preceding turn override.
        // Send the selected model's default to reset that override.
        settings.effort = codex.models().await.into_iter()
            .find(|model| Some(model.slug.as_str()) == settings.model.as_deref())
            .and_then(|model| model.default_effort);
    }
    let isolated = app.issue_threads.intake_turn(&session);
    let record = app.session_records.lock().unwrap().get(&session).cloned().unwrap_or_default();
    let summary = if isolated {
        None
    } else if record.fresh_context {
        record.context_seed.clone()
    } else {
        recovery_context(record.context_seed.as_deref(), app.hub.recovery_summary(&session))
    };
    let switched = !record.last_partner_codex && summary.is_some();
    if !record.last_partner_codex || isolated {
        let claude = app.sessions.lock().await.get(&session).cloned();
        if let Some(claude) = claude {
            claude.retire_for_partner_switch().await?;
            app.sessions.lock().await.remove(&session);
        }
    }
    let options = codex_partner_options(app, &session, settings.clone())?;
    let saved = if isolated { None } else { record.codex_thread_id.as_deref() };
    let (thread_id, fresh) = codex.open_partner(&session, saved, options).await?;
    if isolated {
        app.issue_threads.set_intake_codex(&session, thread_id.clone());
    } else {
        app.record_session(&session, |record| {
            record.last_partner_codex = true;
            record.codex_thread_id = Some(thread_id.clone());
            record.fresh_context = false;
        });
        app.hub.publish_snapshot(ServerEvent::CodexPartner {
            session: session.clone(), thread_id: Some(thread_id.clone()),
        });
    }
    if switched && !matches!(kind, TurnKind::Escalated) {
        partner_notice(app, &session, settings.model.as_deref());
    }
    let request = matches!(kind, TurnKind::UserConversation).then(|| front_desk::Request {
        prompt: prompt.clone(), images: images.clone(),
    });
    let prompt = handover(if switched || fresh { summary } else { None }, &prompt);
    if let Some(event) = event {
        app.hub.publish(event);
    }
    app.hub.publish_partner_turn_started(&session, request, matches!(kind, TurnKind::Escalated));
    let app = app.clone();
    tokio::spawn(async move {
        let mut prompt = prompt;
        for (is_nudge, kind) in [(false, kind), (true, TurnKind::Nudge)] {
            let delivery = desk_record.as_ref().filter(|_| !is_nudge);
            let before = app.review_snapshot(&app.project_dir, &session).await;
            let history = app.start_partner_history(&app.project_dir, "codex", &session).await;
            let result = async {
                let prompt = app.partner_input(&session, prompt).await?;
                let images = if is_nudge { &[][..] } else { &images };
                if !is_nudge {
                    app.hub.set_github_auth_request(&session, github_auth);
                }
                let result = codex.run_turn_with_images(&thread_id, &prompt, images, kind, || {
                    if let Some(delivery) = delivery.filter(|delivery| delivery.report) {
                        app.drain.begin_submission(&app.state_dir, &session, std::slice::from_ref(&delivery.id))?;
                    }
                    Ok(())
                }).await;
                app.hub.set_github_auth_request(&session, None);
                result
            }.await;
            if let Some(history) = history {
                history.finish(&app.hub, &session, result.as_ref().ok().cloned()).await;
            }
            if let (Ok(_), Some(before)) = (&result, before) {
                app.schedule_turn_reviews(&app.project_dir, &session, before).await;
            }
            if !is_nudge {
                let outcome = result.as_ref().map(|_| ()).map_err(|error| anyhow::anyhow!("{error:#}"));
                app.finish_comment_application(&session, outcome).await;
            }
            let _start = app.prompt_start.lock().await;
            // The spawned turn can fail after the desk queue has handed it off.
            let retrying = delivery.is_some_and(|delivery| match &result {
                Ok(_) => {
                    app.drain.finish(&app.state_dir, &session, &delivery.id, None);
                    false
                }
                Err(error) => app.drain.delivery_failed(&app.state_dir, &session, &delivery.id, format!("{error:#}")),
            });
            let wait = app.drain_codex.lock().unwrap().remove(&session);
            if let Some(wait) = wait {
                let text = result.as_ref().map_or_else(|error| format!("{error:#}"), Clone::clone);
                app.save_drain_checkpoint(&session, &thread_id, wait, app.project_dir.clone(), text, result.is_ok());
            }
            if result.is_ok() { routing::finish_turn(&app, &session).await; }
            let from_github = app.issue_threads.automatic_policy(&session).is_some();
            app.issue_threads.finish(&session);
            finish_goal_desk_turn(&app, &session, kind);
            let outcome = match &result {
                Ok(_) => fuzukue_proto::AgentOutcome::Ok,
                Err(error) if error.is::<crate::cancellation::Cancelled>() => fuzukue_proto::AgentOutcome::Cancelled,
                Err(_) => fuzukue_proto::AgentOutcome::Failed,
            };
            let handed_off = app.end_partner_turn(&session, fuzukue_proto::Actor::Codex {
                thread_id: thread_id.clone(), parent_tool_use: None,
            }, result.is_ok(), Some((kind, outcome)), matches!(kind, TurnKind::Escalated).then_some(&settings));
            if handed_off {
                app.hub.take_user_steer(&session);
                break;
            }
            if isolated {
                drop(_start);
                codex.close_worker(&thread_id).await;
                if !retrying {
                    if let Err(error) = result {
                        publish_codex_turn_error(&app.hub, &session, error);
                    }
                }
                break;
            }
            let steered = app.hub.take_user_steer(&session);
            if steered || kind.user_conversation() {
                match &result {
                    Ok(reply) => app.hub.notify_turn_finished(&session, true, reply),
                    Err(error) if !error.is::<crate::cancellation::Cancelled>() => app.hub.notify_turn_failed(&session),
                    Err(_) => {}
                }
            }
            match result {
                Ok(text) if !from_github && app.front_desk_nudge
                    && !planner::handles_desk_nudge(&app, &session) && front_desk::needs_nudge(
                    &text, codex.partner_acted_or_asked(&thread_id), kind,
                ) => {
                    let Ok(_work) = app.admit_work() else { break };
                    prompt = front_desk::nudge_prompt().into();
                    app.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });
                }
                Ok(_) => break,
                Err(error) if !retrying => {
                    if matches!(kind, TurnKind::Escalated) && !error.is::<cancellation::Cancelled>() {
                        commands::escalation_unavailable(&app.hub, &session, format!("Codex: {error:#}"));
                    } else {
                        publish_codex_turn_error(&app.hub, &session, error);
                    }
                    break;
                }
                Err(_) => break,
            }
        }
    });
    Ok(())
}

async fn prompt_claude(
    app: &Arc<Workspace>, session: &str, prompt: String, user_event: Option<ServerEvent>,
) -> anyhow::Result<()> {
    prompt_claude_with_images(app, session, prompt, user_event, Vec::new(), TurnKind::Conversation, None).await.map(|_| ())
}

async fn prompt_escalated_partner(
    app: &Arc<Workspace>, session: &str, selection: fuzukue_proto::AgentSelection,
    prompt: String, user_event: Option<ServerEvent>, images: Vec<PathBuf>,
) -> anyhow::Result<()> {
    let settings = fuzukue_proto::ChatSettings {
        permission_mode: app.partner_turn_settings(session).permission_mode,
        ..selection.chat_settings()
    };
    match selection.backend {
        AgentBackend::Codex => prompt_codex_with_images(app, session.into(), settings, prompt, user_event, images, TurnKind::Escalated, None).await,
        AgentBackend::Claude => prompt_claude_turn(app, session, prompt, user_event, images, TurnKind::Escalated, None, Some(settings)).await.map(|_| ()),
    }
}

async fn prompt_claude_with_images(
    app: &Arc<Workspace>, session: &str, prompt: String, user_event: Option<ServerEvent>, images: Vec<PathBuf>, kind: TurnKind,
    report: Option<String>,
) -> anyhow::Result<bool> {
    prompt_claude_turn(app, session, prompt, user_event, images, kind, report, None).await
}

#[allow(clippy::too_many_arguments)]
async fn prompt_claude_turn(
    app: &Arc<Workspace>, session: &str, prompt: String, user_event: Option<ServerEvent>, images: Vec<PathBuf>, kind: TurnKind,
    report: Option<String>, escalation: Option<fuzukue_proto::ChatSettings>,
) -> anyhow::Result<bool> {
    let is_report = report.is_some();
    let github_auth = github_auth_request(kind, &prompt, user_event.as_ref());
    let (mut switched, has_claude) = app.session_records.lock().unwrap().get(session)
        .map(|record| (record.last_partner_codex, record.claude_session_id.is_some())).unwrap_or_default();
    switched &= !app.issue_threads.intake_turn(session);
    let prompt = if switched && has_claude {
        handover(app.hub.recovery_summary(session), &prompt)
    } else {
        prompt
    };
    if switched {
        anyhow::ensure!(!app.hub.thread_active(session), "Codex は実行中");
        if !is_report && escalation.is_none() {
            partner_notice(app, session, app.chat_settings(session).model.as_deref());
        }
    }
    let escalation_images = if matches!(kind, TurnKind::UserConversation) { images.clone() } else { Vec::new() };
    let images = screenshots::claude_images(images).await?;
    let (s, user_event) = match claude_session_for(app, session, user_event, escalation.as_ref()).await {
        Ok(started) => started,
        Err(error) => {
            if !is_report {
                app.hub.publish(ServerEvent::TurnEnded { session: session.into(), actor: fuzukue_proto::Actor::Claude });
            }
            return Err(error);
        }
    };
    if let Some(event) = user_event {
        app.hub.publish(event);
    }
    let result = if is_report || matches!(kind, TurnKind::WorkerDigest | TurnKind::GoalDecision(_)) {
        s.try_background_prompt(prompt, kind, report).await
    } else {
        s.prompt_with_auth(&prompt, images, kind, github_auth, escalation, escalation_images).await.map(|()| true)
    };
    match result {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(error) => {
            if !is_report {
                app.hub.publish(ServerEvent::TurnEnded { session: session.into(), actor: fuzukue_proto::Actor::Claude });
            }
            return Err(error);
        }
    }
    if switched {
        app.record_session(session, |record| record.last_partner_codex = false);
        if is_report {
            partner_notice(app, session, app.chat_settings(session).model.as_deref());
        }
    }
    Ok(true)
}

async fn delegate_follow_up(app: &Arc<Workspace>, session: &str, prompt: String, delivery: Option<delegate::DeskDelivery>) -> anyhow::Result<()> {
    let settings = app.partner_turn_settings(session);
    let codex = AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex;
    let report = delivery.as_ref().filter(|delivery| delivery.report);
    let result = if codex {
        prompt_codex_with_images(app, session.into(), settings, prompt, None, Vec::new(), TurnKind::Conversation, delivery.clone()).await
    } else if let Some(report) = report {
        prompt_claude_with_images(app, session, prompt, None, Vec::new(), TurnKind::Conversation, Some(report.id.clone()))
            .await.and_then(|started| {
                anyhow::ensure!(started, "Claude is busy");
                Ok(())
            })
    } else {
        prompt_claude(app, session, prompt, None).await
    };
    if let Some(delivery) = delivery {
        match &result {
            Err(error) => {
                if app.drain.delivery_failed(&app.state_dir, session, &delivery.id, format!("{error:#}")) {
                    tracing::debug!(%error, %session, id = %delivery.id, "desk report will retry");
                    return Ok(());
                }
            }
            Ok(()) if !codex && !delivery.report => app.drain.finish(&app.state_dir, session, &delivery.id, None),
            _ => {}
        }
    }
    result
}

async fn delegate_worker_digest(app: &Arc<Workspace>, session: &str, prompt: String) -> anyhow::Result<bool> {
    delegate_background_turn(app, session, prompt, TurnKind::WorkerDigest).await
}

async fn delegate_background_turn(app: &Arc<Workspace>, session: &str, prompt: String, kind: TurnKind) -> anyhow::Result<bool> {
    let settings = app.partner_turn_settings(session);
    if AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex {
        prompt_codex_with_images(app, session.into(), settings, prompt, None, Vec::new(), kind, None).await?;
        Ok(true)
    } else {
        prompt_claude_with_images(app, session, prompt, None, Vec::new(), kind, None).await
    }
}

fn finish_goal_desk_turn(app: &Arc<Workspace>, session: &str, kind: TurnKind) {
    if let Err(error) = planner::desk_turn_ended(app, session, kind) {
        app.hub.publish(ServerEvent::Error { message: format!("目標の確認待ち: {error:#}") });
    }
}

fn attachment_read_path(project: &Path, state: &Path, path: &str) -> anyhow::Result<PathBuf> {
    if fuzukue_proto::is_folder_attachment(path) {
        folder_attachment::resolve(state, path)
    } else {
        inbox::attachment(project, state, path)?.canonicalize().map_err(Into::into)
    }
}

fn with_attachments(project: &Path, state: &Path, text: &str, paths: &[String]) -> anyhow::Result<(String, Vec<PathBuf>)> {
    anyhow::ensure!(paths.len() <= fuzukue_proto::MAX_ATTACHMENTS, "添付が多すぎます");
    if paths.is_empty() {
        return Ok((text.to_owned(), Vec::new()));
    }
    let mut entries = Vec::new();
    let mut files = Vec::new();
    for path in paths {
        let read_path = attachment_read_path(project, state, path)?;
        if fuzukue_proto::is_folder_attachment(path) {
            entries.push(serde_json::json!({"read_path": read_path, "kind": "read-only folder"}));
        } else {
            entries.push(serde_json::json!({"path": path, "read_path": read_path}));
            files.push(read_path);
        }
    }
    let images = screenshots::png_attachments(&files)?;
    let entries = serde_json::to_string(&entries)?;
    let prompt = harness_format!(
        "{text}\n\n(secretaire) Attachments: {entries}\nUse read_path with the filesystem read tools. For read-only folders, list a small part at a time and leave the folder unchanged. Use path with file_inbox for individual files.",
        "{text}\n\n(secretaire) 添付: {entries}\n読み取りツールには read_path を使う。読み取り専用フォルダは必要な範囲だけを一覧し、変更しない。個別ファイルを file_inbox で整理するときは path を使う。"
    );
    Ok((prompt, images))
}

async fn rewrite_reply(app: &Arc<Workspace>, session: &str, item: &str) -> anyhow::Result<()> {
    let _start = app.prompt_start.lock().await;
    let _work = app.admit_work()?;
    anyhow::ensure!(!app.delegates.is_running(session), "Worker is running");
    let ((prompt, images), items) = app.hub.edit_threads(|threads| {
        threads.rewrite(session, item, |text, paths| with_attachments(&app.project_dir, &app.state_dir, text, paths))
    })?;
    app.hub.publish(ServerEvent::ReplySuperseded { session: session.into(), items });
    let prompt = harness_format!(
        "(secretaire) The user rejected your previous answer. Answer their same request again.

{prompt}",
        "(secretaire) 利用者が前の返事の作り直しを求めた。同じ依頼に改めて答える。

{prompt}",
    );
    let settings = app.partner_turn_settings(session);
    if AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex {
        prompt_codex_with_images(app, session.into(), settings, prompt, None, images, TurnKind::Conversation, None).await
    } else {
        prompt_claude_with_images(app, session, prompt, None, images, TurnKind::Conversation, None).await.map(|_| ())
    }
}

async fn start_user_prompt(
    app: &Arc<Workspace>, session: String, settings: fuzukue_proto::ChatSettings,
    prompt: String, user_event: Option<ServerEvent>, images: Vec<PathBuf>,
) -> anyhow::Result<()> {
    if AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex {
        return prompt_codex_with_images(app, session, settings, prompt, user_event, images, TurnKind::UserConversation, None).await;
    }

    prompt_claude_with_images(app, &session, prompt, user_event, images, TurnKind::UserConversation, None).await.map(|_| ())
}

fn has_held_prompt(app: &Workspace, session: &str) -> anyhow::Result<bool> {
    use self_update::drain::Work;
    Ok(app.drain.records(&app.state_dir)?.iter().any(|record| record.session == session
        && !record.claimed && record.review.is_none() && matches!(record.work, Work::Prompt { .. } | Work::Compact { .. })))
}

async fn replay_prompts(app: &Arc<Workspace>) {
    use self_update::drain::{delivery_now, Work};
    let paused: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for claude in paused {
        if let Err(error) = claude.kick_pending().await {
            tracing::warn!(%error, thread = %claude.id, "could not start queued Claude input");
        }
    }
    let _start = app.prompt_start.lock().await;
    let Ok(_work) = app.admit_work() else { return };
    let Ok(records) = app.drain.records(&app.state_dir) else { return };
    let mut seen = std::collections::HashSet::new();
    let now = delivery_now();
    for record in records {
        if record.claimed || record.review.is_some() { continue; }
        let session = &record.session;
        if !matches!(record.work, Work::Claude(_) | Work::Prompt { .. } | Work::Compact { .. }) { continue; }
        if !seen.insert(session.clone()) || app.hub.thread_running(session)
            || app.hub.partner_handoff_pending(session) || app.issue_threads.intake_turn(session) { continue; }
        if !record.ready(now) { continue; }
        if !app.hub.read_threads(|store| store.is_live(session)) {
            app.drain.finish(&app.state_dir, session, &record.id, Some("The target thread is closed".into()));
            continue;
        }
        let settings = app.partner_turn_settings(session);
        let backend = AgentBackend::for_model(settings.model.as_deref());
        let force = matches!(&record.work, Work::Claude(prompt) if prompt.force);
        if !force && app.usage.blocked(backend.usage_key()).is_some() { continue; }
        match record.work {
            Work::Claude(prompt) => {
                if prompt.not_before.is_some_and(|until| until > usage::now()) { continue; }
                let claude = if backend == AgentBackend::Claude || prompt.escalation.is_some() {
                    claude_session_for(app, session, None, prompt.escalation.as_ref()).await
                } else {
                    Err(anyhow::anyhow!("The thread's backend changed"))
                };
                match claude {
                    Ok((claude, _)) => {
                        if let Err(error) = claude.restore_pending(record.id, prompt).await {
                            tracing::warn!(%error, %session, "could not replay Claude input");
                        }
                    }
                    Err(error) => {
                        if matches!(app.drain.claim(&app.state_dir, session, &record.id), Ok(true)) {
                            app.drain.delivery_failed(&app.state_dir, session, &record.id, format!("{error:#}"));
                            tracing::debug!(%error, %session, "Claude input will retry");
                        }
                    }
                }
            }
            Work::Compact { instructions } => {
                if !commands::claude_idle(app, session).await { continue; }
                let Ok(work) = app.admit_work() else { return };
                if matches!(app.drain.claim(&app.state_dir, session, &record.id), Ok(true)) {
                    commands::start_compaction(app, session, instructions, record.id, work);
                }
            }
            Work::Prompt { text, paths } => {
                if !matches!(app.drain.claim(&app.state_dir, session, &record.id), Ok(true)) { continue; }
                let result = match with_attachments(&app.project_dir, &app.state_dir, &text, &paths) {
                    Ok((prompt, images)) => start_user_prompt(app, session.clone(), settings, prompt, None, images).await,
                    Err(error) => Err(error),
                };
                app.drain.finish(&app.state_dir, session, &record.id, result.err().map(|error| format!("{error:#}")));
            }
            _ => unreachable!(),
        }
    }
}

async fn handle(app: &Arc<Workspace>, msg: ClientMsg) -> anyhow::Result<()> {
    handle_from_client(app, msg, None).await
}

fn canary_disables(msg: &ClientMsg) -> bool {
    use ClientMsg::*;
    matches!(msg, IssueThreads { .. } | WorkIssue { .. } | RefreshIssues
        | StartResearchIssue { .. } | AcceptResearchIssue { .. } | Handoff { .. }
        | CreateWorkspace { .. } | OpenWorkspace { .. } | BrowseWorkspace { .. }
        | CompleteWorkspacePath { .. } | RemoveWorkspace { .. }
        | Replan { .. } | WorkerPrompt { .. } | BranchReview { .. } | RequestReview { .. }
        | ListExtensions | GrantExtension { .. } | SetExtensionEnabled { .. }
        | UpdateExtension { .. } | RenderExtension { .. } | ExtensionUiEvent { .. }
        | RememberShellApproval { .. } | RevokeShellApproval { .. })
}

async fn interrupt_partner(app: &Workspace, session: &str) -> anyhow::Result<()> {
    if let Some(cancel) = app.hub.compaction(session) {
        cancel.cancel();
        return Ok(());
    }
    if let Some(s) = app.sessions.lock().await.get(session) {
        s.interrupt().await?;
    }
    let partner = app.issue_threads.intake_codex(session).or_else(|| {
        app.session_records.lock().unwrap().get(session).and_then(|r| r.codex_thread_id.clone())
    });
    if let (Some(codex), Some(thread_id)) = (app.running_codex().await, partner) {
        codex.interrupt(&thread_id).await?;
    }
    Ok(())
}

async fn set_chat_settings(app: &Workspace, session: &str, settings: fuzukue_proto::ChatSettings) -> anyhow::Result<()> {
    let selected = settings.model.as_deref();
    delegate::validate_model(app, selected, settings.effort.as_deref()).await?;
    let previous = app.chat_settings(session);
    app.hub.edit_threads(|store| {
        if previous.model == settings.model && previous.effort == settings.effort
            && previous.permission_mode != settings.permission_mode {
            store.set_partner_permission(session, settings.permission_mode)
        } else {
            store.set_roles(session, Some(settings.clone()), None)
        }
    })?;
    app.record_session(session, |record| record.chat_settings = settings.clone());
    app.hub.publish_snapshot(ServerEvent::ChatSettingsChanged { session: session.into(), settings });
    Ok(())
}

impl Workspace {
    async fn set_partner_default(&self, mut settings: fuzukue_proto::ChatSettings) -> anyhow::Result<()> {
        settings.permission_mode = Default::default();
        delegate::validate_model(self, settings.model.as_deref(), settings.effort.as_deref()).await?;
        let _start = self.prompt_start.lock().await;
        self.hub.edit_threads(|store| {
            state::save_partner_default(&self.state_dir, &settings)?;
            store.partner_default = settings.clone();
            Ok(())
        })?;
        self.hub.publish_snapshot(ServerEvent::PartnerDefaultChanged { settings });
        Ok(())
    }
}

async fn handle_from_client(
    app: &Arc<Workspace>, msg: ClientMsg, client: Option<&screenshots::Connection<'_>>,
) -> anyhow::Result<()> {
    match msg {
        ClientMsg::ListPendingApprovals { .. } | ClientMsg::DecideApproval { .. } | ClientMsg::WorkspaceRequest { .. } | ClientMsg::WorkspaceRequestSurface { .. } => {}
        ClientMsg::SaveScreenshot { .. } | ClientMsg::ClientScreenshotResponse { .. }
        | ClientMsg::Trash { .. } | ClientMsg::RemoveWorkspace { .. } | ClientMsg::RewriteReply { .. }
        | ClientMsg::SelfUpdateStatus | ClientMsg::StartSelfUpdate | ClientMsg::RollbackServer
        | ClientMsg::SelfUpdateCanary { .. } | ClientMsg::SelfUpdateWait { .. }
        | ClientMsg::RequestDrain { .. } | ClientMsg::CancelDrain { .. } | ClientMsg::ForceShutdown { .. } => {}

        ClientMsg::FileInbox { .. } | ClientMsg::SearchDecks { .. } | ClientMsg::Git { .. } | ClientMsg::History { .. } | ClientMsg::BranchReview { .. } | ClientMsg::ApplyComments { .. } | ClientMsg::CompactSession { .. } | ClientMsg::GetThreadArtifacts { .. } | ClientMsg::GetThreadJobs { .. } | ClientMsg::ArchiveThread { .. } | ClientMsg::PinThread { .. }
        | ClientMsg::GetLeanReadingCards { .. } | ClientMsg::LookupSymbol { .. } | ClientMsg::ReviewReports { .. } | ClientMsg::SearchLibrary { .. } | ClientMsg::AddMemo { .. } | ClientMsg::MoveMemo { .. } | ClientMsg::Ledger { .. } | ClientMsg::SaveSource { .. } | ClientMsg::EditorClosed { .. } | ClientMsg::SaveCard { .. } | ClientMsg::SelectPaper { .. } | ClientMsg::GetPaperPdfMap { .. }
        | ClientMsg::RecordSourceFingerprint { .. } | ClientMsg::GetThreadHistory { .. } | ClientMsg::GetThreadTail { .. } | ClientMsg::GetThreadHistoryFrom { .. } | ClientMsg::GetThreadJob { .. } | ClientMsg::GetThreadJobReport { .. }
        | ClientMsg::ListExtensions | ClientMsg::GrantExtension { .. } | ClientMsg::SetExtensionEnabled { .. }
        | ClientMsg::UpdateExtension { .. } | ClientMsg::RenderExtension { .. } | ClientMsg::ExtensionUiEvent { .. }
        | ClientMsg::Hello { .. } | ClientMsg::SetLocale { .. } | ClientMsg::ListWorkspaces | ClientMsg::OpenWorkspace { .. } | ClientMsg::SwitchWorkspace { .. } | ClientMsg::CompleteWorkspacePath { .. } | ClientMsg::BrowseWorkspace { .. } | ClientMsg::ListFiles { .. } | ClientMsg::FindFiles { .. } | ClientMsg::ResolveFile { .. } | ClientMsg::CreateWorkspace { .. } | ClientMsg::InWorkspace { .. } | ClientMsg::NewThread | ClientMsg::NewAttachedThread { .. } | ClientMsg::UploadAttachment { .. } | ClientMsg::FolderAttachment { .. } | ClientMsg::CancelAttachment { .. } => {}
        ClientMsg::RenameThread { id, title } => app.hub.edit_threads(|store| store.rename(&id, &title))?,
        ClientMsg::AttachThread { id, decks } => { app.hub.edit_threads(|store| store.attach(&id, &decks))?; }
        ClientMsg::DetachThread { id, deck } => { app.hub.edit_threads(|store| store.detach(&id, &deck))?; }
        ClientMsg::SetDelegateSettings { settings } => delegate::set_settings(app, settings).await?,
        ClientMsg::SetWorkerApprovalPolicy { policy } => {
            let mut current = app.worker_approval_policy.lock().unwrap();
            if *current != policy {
                state::save_worker_approval_policy(&app.state_dir, policy)?;
                *current = policy;
                drop(current);
                app.publish_approval_settings();
            }
        }
        ClientMsg::RememberShellApproval { request_id, prefix } => {
            remember_shell_approval(app, &request_id, prefix.as_deref()).await?;
        }
        ClientMsg::RevokeShellApproval { id } => {
            app.shell_approval_rules.revoke(&app.state_dir, &id)?;
            app.publish_approval_settings();
        }
        ClientMsg::SetReviewSettings { settings } => app.set_review_settings(settings).await?,
        ClientMsg::SetPartnerDefault { settings } => app.set_partner_default(settings).await?,
        ClientMsg::UseDefaultThreadModel { session } => {
            let _start = app.prompt_start.lock().await;
            app.hub.edit_threads(|store| store.set_partner_model_inherited(&session))?;
            app.record_session(&session, |record| {
                record.chat_settings.model = None;
                record.chat_settings.effort = None;
            });
        }
        ClientMsg::SetThreadWorker { session, settings } => {
            delegate::validate_worker(app, &settings).await?;
            app.hub.edit_threads(|store| store.set_roles(&session, None, Some(settings)))?;
        }
        ClientMsg::SetChatSettings { session, settings } => {
            let _start = app.prompt_start.lock().await;
            set_chat_settings(app, &session, settings).await?;
        }
        ClientMsg::ListAgents | ClientMsg::GetAgentStats | ClientMsg::GetAgentHistory { .. } => {}
        ClientMsg::SetAgentPolicy { name, policy } => app.set_agent_policy(name, policy).await?,
        ClientMsg::SetAgentCaller { name, caller, allowed } => app.set_agent_caller(name, caller, allowed).await?,
        ClientMsg::SetAgentOverride { name, selection } => app.set_agent_override(None, name, selection).await?,
        ClientMsg::SetThreadAgentOverride { session, name, selection } => {
            app.set_agent_override(Some(&session), name, selection).await?;
        }
        ClientMsg::SetSummaryModel { selection } => app.set_summary_model(selection).await?,
        ClientMsg::SetAgentRoleModel { role, selection } => app.set_agent_role_model(role, selection).await?,
        ClientMsg::SetAgentFallback { name, selection } => app.set_agent_fallback(name, selection).await?,
        ClientMsg::SetGoalPace { session, pace } => planner::set_pace(app, &session, pace)?,
        ClientMsg::PauseGoal { session } => {
            let _start = app.prompt_start.lock().await;
            planner::pause_goal(app, &session)?;
        }
        ClientMsg::Replan { session, reason } => {
            drop(app.admit_work()?);
            anyhow::ensure!(app.hub.has_thread(&session), "スレッドが見つからない");
            let app = app.clone();
            tokio::spawn(async move {
                match planner::replan(&app, &session, reason, false, None).await {
                    Ok(_) => delegate::plan_follow_up(&app, &session),
                    Err(error) => app.hub.publish(ServerEvent::Error { message: format!("方針の見直し: {error:#}") }),
                }
            });
        }
        ClientMsg::RenameJob { session, job, label } => {
            app.delegates.rename_job(app, &job, Some(&session), &label, "desktop")?;
        }
        ClientMsg::DismissJob { session, job } => app.hub.dismiss_delegate(&session, &job)?,
        ClientMsg::StopWorker { session, job } => {
            anyhow::ensure!(app.delegates.interrupt_job(&session, &job), "作業役のジョブは終了した");
            app.hub.withdraw_approvals_for(&session, Some(&job), "worker job stopped")?;
        }
        prompt @ (ClientMsg::Prompt { .. } | ClientMsg::PromptWithAttachments { .. }
            | ClientMsg::RolePrompt { .. } | ClientMsg::WorkerPrompt { .. }) => {
            let (session, text) = prompt_text(&prompt);
            let escalating = commands::is_escalation(text);
            if fuzukue_proto::slash::is_command(text) && !escalating {
                commands::dispatch(app, session, text).await;
                return Ok(());
            }
            let (session, text, paths, job) = match prompt {
                ClientMsg::Prompt { session, text } => (session, text, Vec::new(), None),
                ClientMsg::PromptWithAttachments { session, text, paths } => (session, text, paths, None),
                ClientMsg::RolePrompt { session, text, paths, to_worker } => {
                    let job = to_worker.then(|| app.delegates.only_job(&session).context("作業役のジョブを一つ指定する")).transpose()?;
                    (session, text, paths, job)
                }
                ClientMsg::WorkerPrompt { session, job, text, paths } => (session, text, paths, Some(job)),
                _ => unreachable!(),
            };
            let text = match commands::parse(&text) {
                commands::Input::Prompt(text) => text,
                commands::Input::Command { args, .. } => args,
            };
            let text = text.to_owned();
            anyhow::ensure!(app.hub.has_thread(&session), "スレッドが見つからない");
            let (text, paths, prompt, images) = if paths.is_empty() {
                let prompt = text.clone();
                (text, paths, prompt, Vec::new())
            } else {
                let project = app.project_dir.clone();
                let state = app.state_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let (prompt, images) = with_attachments(&project, &state, &text, &paths)?;
                    Ok::<_, anyhow::Error>((text, paths, prompt, images))
                }).await??
            };
            let waiting = send_latency::before_prompt_lock();
            let _start = app.prompt_start.lock().await;
            send_latency::after_prompt_lock(waiting);
            anyhow::ensure!(app.hub.has_thread(&session), "スレッドが見つからない");
            if let Some(job) = job {
                anyhow::ensure!(!escalating, "Send escalation requests to the front desk");
                let event = ServerEvent::UserPrompt {
                    session: session.clone(), item: format!("{job}:{}", user_item()), text, paths, to_worker: true,
                };
                send_latency::expect_echo(&event, "worker_steer");
                app.delegates.steer(app, &session, &job, &prompt, event)?;
                planner::goal_event(app, &session, false);
                return Ok(());
            }
            planner::wake_goal(app, &session)?;
            if let Some(client) = client.filter(|_| !app.hub.thread_active(&session) && !app.delegates.is_running(&session)) {
                client.bind(&session);
            }
            let user_event = Some(if paths.is_empty() {
                prompt_event(&session, fuzukue_proto::Actor::User, &text)
            } else {
                ServerEvent::UserPrompt { session: session.clone(), item: user_item(), text, paths, to_worker: false }
            });
            let settings = app.partner_turn_settings(&session);
            let work = app.admit_work();
            let intake_turn = app.issue_threads.intake_turn(&session);
            let handoff_pending = app.hub.partner_handoff_pending(&session);
            if escalating {
                let result = async {
                    anyhow::ensure!(!prompt.trim().is_empty(), "Use /escalate followed by a message");
                    anyhow::ensure!(!intake_turn && !handoff_pending && !app.hub.thread_active(&session) && !has_held_prompt(app, &session)?,
                        "Wait for the current turn and queued messages to finish");
                    let _work = work?;
                    let selection = app.escalation_model(&session).await?;
                    prompt_escalated_partner(app, &session, selection, prompt, user_event, images).await
                }.await;
                if let Err(error) = result {
                    commands::escalation_unavailable(&app.hub, &session, format!("{error:#}"));
                }
                return Ok(());
            }
            let wants_codex = AgentBackend::for_model(settings.model.as_deref()) == AgentBackend::Codex;
            let running = app.hub.thread_running(&session);
            let prior_codex = app.session_records.lock().unwrap().get(&session).is_some_and(|record| record.last_partner_codex);
            if !handoff_pending && !intake_turn && work.is_ok() && running && prior_codex && wants_codex && app.hub.compaction(&session).is_none() {
                let thread = app.session_records.lock().unwrap().get(&session)
                    .and_then(|record| record.codex_thread_id.clone()).context("Codex のスレッドが見つからない")?;
                let codex = app.running_codex().await.context("Codex が停止した")?;
                let steering = app.partner_input(&session, prompt.clone()).await?;
                codex.steer_with_images(&thread, &steering, &images).await?;
                app.hub.set_github_auth_request(&session,
                    github_auth_request(TurnKind::UserConversation, "", user_event.as_ref()));
                app.hub.mark_user_steer(&session);
                let event = user_event.unwrap();
                send_latency::expect_echo(&event, "codex_steer");
                app.hub.publish(event);
                app.hub.update_partner_request(&session, front_desk::Request { prompt, images });
                return Ok(());
            }
            let switching = running && prior_codex != wants_codex;
            if handoff_pending || switching || intake_turn || work.is_err() || app.hub.compaction(&session).is_some() || has_held_prompt(app, &session)? {
                let (item, text, paths) = match user_event.as_ref().unwrap() {
                    ServerEvent::UserPrompt { item, text, paths, .. } => (item, text, paths.clone()),
                    ServerEvent::TextDelta { item, text, .. } => (item, text, Vec::new()),
                    _ => unreachable!("user prompt event"),
                };
                let store = || app.drain.enqueue(&app.state_dir, &session, item.clone(),
                    self_update::drain::Work::Prompt { text: text.clone(), paths });
                match app.registry.upgrade().and_then(|registry| registry.self_update.get().cloned()) {
                    Some(update) => { update.hold_input(store)?; }
                    None => { store()?; }
                }
                let event = user_event.unwrap();
                send_latency::expect_echo(&event, "held");
                app.hub.publish(event);
                return Ok(());
            }
            let _work = work?;
            let backend = AgentBackend::for_model(settings.model.as_deref());
            let route = match backend {
                AgentBackend::Claude => "claude",
                AgentBackend::Codex => "codex",
            };
            send_latency::expect_echo(user_event.as_ref().unwrap(), route);
            start_user_prompt(app, session, settings, prompt, user_event, images).await?;
        }
        ClientMsg::Interrupt { session } => {
            interrupt_partner(app, &session).await?;
        }
        ClientMsg::RefreshIssues => {
            app.issues_refresh.notify_one();
            app.research_issues.notify();
        }
        ClientMsg::StartResearchIssue { .. } | ClientMsg::AcceptResearchIssue { .. } => {
            anyhow::bail!("研究 Issue の要求にはプロトコル 13 が必要");
        }
        ClientMsg::BuildPaper { deck } => {
            let _guard = app.file_save.lock().await;
            let root = safe_join(&app.project_dir, &deck)?;
            // ビルドを押したら、そのデッキでたまっている人の編集を待たずに確認へ回す。
            app.human_deck_closed(&root, std::time::Duration::ZERO);
            app.build_paper(deck);
        }
        ClientMsg::ListTests | ClientMsg::RunTests { .. } | ClientMsg::StopTests { .. }
        | ClientMsg::LspBuffer { .. } | ClientMsg::LspRequest { .. } => {}
        ClientMsg::CancelBuild { deck } => app.cancel_build(&deck),
        ClientMsg::CancelReview { deck, kind } => app.cancel_review(&deck, &kind)?,
        ClientMsg::CheckLean => {
            app.check_lean();
        }
        ClientMsg::RequestReview { deck, kind } => {
            if let Err(e) = app.request_review(deck.clone(), &kind, None).await {
                app.publish_review_failure(deck, kind, &e);
            }
        }
        ClientMsg::AddComment { deck, card, text, quote, proof, source } => {
            let root = safe_join(&app.project_dir, &deck)?;
            let _edit = app.comment_edit.lock().await;
            let mut items = comments::read(&root)?;
            items.push(fuzukue_proto::CommentView { card, text, quote, proof, source, created: usage::now() });
            comments::save(&root, &items)?;
            app.hub.publish_snapshot(ServerEvent::Comments { deck, items });
        }
        ClientMsg::RemoveComment { deck, index } => trash::legacy_comment(app, &deck, index).await?,
        ClientMsg::SendComments { deck, session, .. } => { send_comments(app, &deck, session).await?; }
        ClientMsg::SelectDeck { deck } => {
            if !deck.is_empty() {
                let root = safe_join(&app.project_dir, &deck)?;
                anyhow::ensure!(root.is_dir(), "{deck} はディレクトリではない");
            }
            app.deck.write().unwrap().clone_from(&deck);
            let dir = app.project_dir.clone();
            let selected = deck.clone();
            let (c, d, w) = tokio::task::spawn_blocking(move || cards::deck_snapshot(&dir, &selected)).await?;
            app.hub.publish_snapshot(w);
            app.hub.publish_snapshot(c);
            app.hub.publish_snapshot(d);
            let _edit = app.comment_edit.lock().await;
            app.hub.publish_snapshot(ServerEvent::Comments { items: comments::load(&app.project_dir.join(&deck)), deck });
        }
        ClientMsg::LocatePdf { pdf, page, x, y } => {
            safe_join(&app.project_dir, &pdf)?;
            let r = synctex::locate(&app.project_dir, &pdf, page, x, y).await?;
            // 答えは状態のスナップショットとして配る（タイムラインには残さない）。
            app.hub.publish_snapshot(ServerEvent::Located { pdf, file: r.file, line: r.line, card: r.card });
        }
        ClientMsg::SaveFile { path, text } => {
            let _guard = app.file_save.lock().await;
            let project = app.project_dir.canonicalize()?;
            let full = safe_join(&project, &path)?;
            let noting = app.clone();
            let closed = full.clone();
            tokio::task::spawn_blocking(move || {
                cards::save_file(&noting, &project, &full, text)
            }).await??;
            // 作業台の編集画面は保存で閉じる。
            app.human_file_closed(&closed);
            app.hub.publish_snapshot(ServerEvent::Saved { path });
        }
        ClientMsg::Handoff { .. } => anyhow::bail!("Handoff actions require protocol 41"),
        ClientMsg::IssueThreads { .. } => anyhow::bail!("GitHub thread actions require protocol 38"),
        ClientMsg::GitHubReport { .. } => anyhow::bail!("GitHub report actions require a desktop connection"),
        ClientMsg::SetIssueLabels { number, add, remove } => {
            issue_threads::set_labels(app, &issue_threads::link(app, number)?, &add, &remove).await?;
            app.issues_refresh.notify_one();
        }
        ClientMsg::WorkIssue { number } => {
            let _work = app.admit_work()?;
            let dir = github::ensure_worktree(&app.project_dir, number).await?;
            let link = issue_threads::link(app, number)?;
            let prompt = github::work_prompt(&dir, &link.repo, number).await?;
            let session = app.hub.edit_threads(|store| store.issue_thread(number))?;
            issue_threads::attach(app, &session, link)?;
            app.record_session(&session, |r| r.dir = Some(dir));
            Box::pin(handle(app, ClientMsg::Prompt { session, text: prompt })).await?;
            app.issues_refresh.notify_one();
        }
        ClientMsg::ResumeNow { session } => {
            let _start = app.prompt_start.lock().await;
            if planner::wake_goal(app, &session)? {
                delegate::plan_follow_up(app, &session);
            }
            let s = app.sessions.lock().await.get(&session).cloned();
            if let Some(s) = s {
                s.resume(true).await?;
            }
        }
        ClientMsg::ListFolderGrants { .. } | ClientMsg::AddFolderGrant { .. }
        | ClientMsg::RevokeFolderGrant { .. } | ClientMsg::DecideFolderAccess { .. } => {}
        ClientMsg::Resolve { request_id, decision } => {
            resolve_approval(app, request_id, decision, false).await?;
        }
        ClientMsg::AnswerQuestion { request_id, answers, dismiss } => {
            app.hub.answer_question(&request_id, answers, dismiss)?;
        }
    }
    Ok(())
}

impl Workspace {
    fn approval_settings(&self) -> ServerEvent {
        let policy = *self.worker_approval_policy.lock().unwrap();
        ServerEvent::WorkerApprovalPolicyChanged {
            policy,
            shell_rules: (!canary()).then(|| self.shell_approval_rules.list()),
        }
    }

    fn publish_approval_settings(&self) {
        self.hub.publish_snapshot(self.approval_settings());
    }
}

async fn remember_shell_approval(app: &Workspace, request_id: &str, prefix: Option<&str>) -> anyhow::Result<()> {
    if app.delegates.remember_shell(app, request_id, prefix)? {
        return Ok(());
    }
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for session in sessions {
        if session.remember_shell(app, request_id, prefix).await? {
            app.hub.publish(ServerEvent::ApprovalResolved {
                request_id: request_id.into(), decision: "accept".into(), automatic: false,
            });
            return Ok(());
        }
    }
    anyhow::bail!("This approval is no longer pending")
}

async fn resolve_approval(app: &Workspace, request_id: String, decision: String, automatic: bool) -> anyhow::Result<()> {
    anyhow::ensure!(!request_id.starts_with(fuzukue_proto::FOLDER_ACCESS_REQUEST),
        "Use decide_folder_access for a folder request");
    if app.delegates.resolve_approval(&request_id, &decision, automatic)? {
        return Ok(());
    }
    let mut handled = false;
    if request_id.starts_with(codex::WORKER_APPROVAL_PREFIX) {
        let workers: Vec<_> = app.worker_codex.lock().unwrap().iter().filter_map(std::sync::Weak::upgrade).collect();
        for worker in workers {
            if worker.resolve(&request_id, &decision, automatic).await? {
                handled = true;
                break;
            }
        }
    }
    if !handled {
        if let Some(codex) = app.running_codex().await {
            handled = codex.resolve(&request_id, &decision, automatic).await?;
        }
    }
    if automatic && !handled && request_id.starts_with(codex::WORKER_APPROVAL_PREFIX) {
        return Ok(());
    }
    if !handled {
        let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
        for s in sessions {
            if s.resolve(&request_id, &decision).await? {
                handled = true;
                break;
            }
        }
    }
    if !handled {
        // The backend can consume its waiter before the resolution event reaches the Hub.
        anyhow::ensure!(app.hub.has_seen_approval(&request_id), "unknown approval request {request_id}");
        return Ok(());
    }
    app.hub.publish(ServerEvent::ApprovalResolved { request_id, decision, automatic });
    Ok(())
}

fn auto_approve(app: Arc<Workspace>) {
    let (_, mut rx) = app.hub.subscribe(u64::MAX);
    tokio::spawn(async move {
        loop {
            let env = match rx.recv().await {
                Ok(env) => env,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            if let ServerEvent::ApprovalRequested { session, request_id, actor, .. } = env.event {
                if hub::worker_approval(&request_id, &actor) || request_id.starts_with(fuzukue_proto::GIT_APPROVAL)
                    || request_id.starts_with(fuzukue_proto::RESEARCH_ISSUE_APPROVAL) || request_id.starts_with(issue_threads::APPROVAL)
                    || request_id.starts_with(fuzukue_proto::FOLDER_ACCESS_REQUEST)
                {
                    continue;
                }
                let app = app.clone();
                tokio::spawn(async move {
                    let result = if matches!(actor, fuzukue_proto::Actor::Claude) {
                        let target = app.sessions.lock().await.get(&session).cloned();
                        if let Some(target) = target {
                            target.resolve_auto(&request_id).await.map(|resolved| {
                                if resolved {
                                    app.hub.publish(ServerEvent::ApprovalResolved {
                                        request_id, decision: "accept".into(), automatic: true,
                                    });
                                }
                            })
                        } else {
                            Ok(())
                        }
                    } else {
                        resolve_approval(&app, request_id, "accept".into(), true).await
                    };
                    if let Err(e) = result {
                        app.hub.publish(ServerEvent::Error { message: format!("auto approve: {e:#}") });
                    }
                });
            }
        }
    });
}

fn publish_prompt(hub: &Hub, session: &str, actor: fuzukue_proto::Actor, text: &str) {
    hub.publish(prompt_event(session, actor, text));
}

fn prompt_event(session: &str, actor: fuzukue_proto::Actor, text: &str) -> ServerEvent {
    ServerEvent::TextDelta { session: session.into(), item: user_item(), actor, text: text.into() }
}

fn user_item() -> String {
    // 履歴は再起動後も残るので、起動時刻を含める。
    format!("user:{}:{}", *SERVER_START, USER_ITEM.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// プロジェクトの外を読ませない。
fn safe_join(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let root = root.canonicalize()?;
    let full = root.join(rel).canonicalize()?;
    anyhow::ensure!(full.starts_with(&root), "path escapes project: {rel}");
    anyhow::ensure!(!mcp::result_spill::is_spill_path(&full), "Use the session-scoped MCP read tools for saved tool results");
    Ok(full)
}
