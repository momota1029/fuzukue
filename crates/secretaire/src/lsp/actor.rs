use anyhow::{ensure, Context, Result};
use fuzukue_proto::{LspStatus, LspUpdate, ServerEvent};
use lsp_types::{
    InitializeResult, Position, PublishDiagnosticsParams, TextDocumentSyncCapability,
    TextDocumentSyncKind,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{sleep_until, Instant},
};

use super::{
    project,
    rpc::{self, Process},
    BufferId, Events, Project,
};

type Reply = oneshot::Sender<Result<Value>>;
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(10);
pub(super) const MAX_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Endpoint(mpsc::UnboundedSender<Command>);

impl Endpoint {
    pub fn send(&self, command: Command) {
        let _ = self.0.send(command);
    }

    pub fn request(
        &self,
        file: Option<PathBuf>,
        buffer: Option<BufferId>,
        method: &'static str,
        params: Value,
    ) -> oneshot::Receiver<Result<Value>> {
        let (reply, result) = oneshot::channel();
        self.send(Command::Query {
            file,
            buffer,
            method,
            params,
            reply,
        });
        result
    }
}

pub(super) struct Edit {
    pub version: u64,
    pub text: Arc<str>,
    pub save: bool,
}

pub(super) enum Command {
    Open {
        id: BufferId,
        file: PathBuf,
        version: u64,
        text: Arc<str>,
        events: Events,
    },
    Edit {
        id: BufferId,
        edit: Edit,
    },
    Close {
        id: BufferId,
    },
    Query {
        file: Option<PathBuf>,
        buffer: Option<BufferId>,
        method: &'static str,
        params: Value,
        reply: Reply,
    },
}

struct Buffer {
    file: PathBuf,
    text: Arc<str>,
    version: u64,
    order: u64,
    events: Events,
}

struct Published {
    version: i32,
    text: Arc<str>,
    diagnostics: Option<Vec<Value>>,
}

struct Pending {
    reply: Reply,
    deadline: Instant,
}

struct SyncOptions {
    change: TextDocumentSyncKind,
    save_text: Option<bool>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            change: TextDocumentSyncKind::NONE,
            save_text: None,
        }
    }
}

impl SyncOptions {
    fn from(result: InitializeResult) -> Self {
        use lsp_types::TextDocumentSyncSaveOptions;
        match result.capabilities.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(change)) => Self {
                change,
                save_text: Some(false),
            },
            Some(TextDocumentSyncCapability::Options(options)) => Self {
                change: options.change.unwrap_or(TextDocumentSyncKind::NONE),
                save_text: match options.save {
                    Some(TextDocumentSyncSaveOptions::Supported(true)) => Some(false),
                    Some(TextDocumentSyncSaveOptions::SaveOptions(options)) => {
                        Some(options.include_text.unwrap_or(false))
                    }
                    _ => None,
                },
            },
            None => Self::default(),
        }
    }
}

struct Actor {
    workspace: PathBuf,
    project: Project,
    folder: Value,
    process: Option<Process>,
    sync: SyncOptions,
    status: LspStatus,
    failures: u32,
    retry: Instant,
    started: Instant,
    last_use: Instant,
    next: u64,
    version: i32,
    buffers: HashMap<BufferId, Buffer>,
    disk: HashMap<PathBuf, Arc<str>>,
    published: HashMap<PathBuf, Published>,
    pending: HashMap<u64, Pending>,
    diagnostic_waiters: HashMap<PathBuf, Vec<Pending>>,
}

pub(super) fn spawn(project: Project, workspace: PathBuf, idle: Duration) -> Endpoint {
    let (tx, rx) = mpsc::unbounded_channel();
    let folder = json!({"uri": project::uri(&project.root).unwrap_or_default(),
        "name": project.root.file_name().unwrap_or_default().to_string_lossy()});
    let actor = Actor {
        workspace,
        project,
        folder,
        process: None,
        sync: SyncOptions::default(),
        status: LspStatus::Idle,
        failures: 0,
        retry: Instant::now(),
        started: Instant::now(),
        last_use: Instant::now(),
        next: 1,
        version: 0,
        buffers: HashMap::new(),
        disk: HashMap::new(),
        published: HashMap::new(),
        pending: HashMap::new(),
        diagnostic_waiters: HashMap::new(),
    };
    tokio::spawn(actor.run(rx, idle));
    Endpoint(tx)
}

impl Actor {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>, idle: Duration) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            let automatic_retry = self.process.is_none()
                && self.failures > 0
                && !self.buffers.is_empty()
                && self.retry < self.last_use + idle;
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if let Err(error) = self.command(command).await {
                        self.failed(error);
                    }
                }
                message = async { self.process.as_mut().unwrap().recv().await }, if self.process.is_some() => {
                    let result = match message {
                        Ok(message) => self.message(message).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result { self.failed(error); }
                }
                _ = sleep_until(self.retry), if automatic_retry => {
                    let _ = self.start().await;
                }
                _ = sleep_until(self.last_use + idle), if self.process.is_some() && self.pending.is_empty() && self.diagnostic_waiters.is_empty() => {
                    self.stop().await;
                }
                _ = tick.tick(), if !self.pending.is_empty() || !self.diagnostic_waiters.is_empty() => {
                    self.expire();
                }
            }
        }
        self.stop().await;
    }

    fn status(&mut self, status: LspStatus) {
        self.status = status;
        for (id, buffer) in &self.buffers {
            Self::update(*id, buffer, LspUpdate::Status { status });
        }
    }

    fn update(id: BufferId, buffer: &Buffer, update: LspUpdate) {
        let _ = buffer.events.send(ServerEvent::LspDocument {
            document: id.1,
            update,
        });
    }

    async fn start(&mut self) -> Result<()> {
        if self.process.is_some() {
            return Ok(());
        }
        ensure!(
            Instant::now() >= self.retry,
            "language server is waiting to restart"
        );
        self.status(LspStatus::Starting);
        match self.initialize().await {
            Ok(()) => {
                self.started = Instant::now();
                self.status(LspStatus::Running);
                Ok(())
            }
            Err(error) => {
                let detail = format!("{error:#}");
                self.failed(error);
                anyhow::bail!("language server failed to start: {detail}")
            }
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        let mut process = Process::spawn(&self.project).await?;
        process
            .request(
                1,
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": self.folder["uri"],
                    "workspaceFolders": [&self.folder],
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-16"] },
                        "workspace": { "configuration": true, "workspaceFolders": true },
                        "textDocument": {
                            "hover": { "contentFormat": ["markdown", "plaintext"] },
                            "completion": { "completionItem": { "snippetSupport": false } },
                            "definition": { "linkSupport": true },
                            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                            "publishDiagnostics": { "versionSupport": true }
                        }
                    }
                }),
            )
            .await?;
        let response: InitializeResult =
            serde_json::from_value(process.response(1, &self.folder, rpc::WAIT).await?)?;
        ensure!(
            response
                .capabilities
                .position_encoding
                .as_ref()
                .is_none_or(|encoding| encoding.as_str() == "utf-16"),
            "language server must use UTF-16 positions"
        );
        self.sync = SyncOptions::from(response);
        process.notify("initialized", json!({})).await?;
        self.process = Some(process);
        let files: Vec<_> = self.buffers.values().map(|b| b.file.clone()).collect();
        for file in files {
            self.sync_file(&file, false).await?;
        }
        Ok(())
    }

    async fn stop(&mut self) {
        if let Some(process) = self.process.take() {
            self.reset("language server stopped");
            self.status(LspStatus::Idle);
            process.shutdown(&self.folder).await;
        }
        self.failures = 0;
        self.retry = Instant::now();
    }

    fn failed(&mut self, error: anyhow::Error) {
        tracing::warn!(language = %self.project.language, root = %self.project.root.display(), "language server: {error:#}");
        if self.status == LspStatus::Running && self.started.elapsed() >= Duration::from_secs(60) {
            self.failures = 0;
        }
        self.process.take();
        self.reset("language server disconnected");
        self.failures = self.failures.saturating_add(1);
        self.retry = Instant::now() + Duration::from_millis(250 << (self.failures - 1).min(7));
        self.status(LspStatus::Failed);
    }

    fn reset(&mut self, error: &str) {
        for pending in self.pending.drain().map(|(_, p)| p).chain(
            self.diagnostic_waiters
                .drain()
                .flat_map(|(_, waiters)| waiters),
        ) {
            let _ = pending.reply.send(Err(anyhow::anyhow!("{error}")));
        }
        self.published.clear();
        self.disk.clear();
        for (id, buffer) in &self.buffers {
            Self::update(
                *id,
                buffer,
                LspUpdate::Diagnostics {
                    version: buffer.version,
                    diagnostics: vec![],
                },
            );
        }
    }

    fn active_text(&self, file: &Path) -> Option<Arc<str>> {
        self.buffers
            .values()
            .filter(|buffer| buffer.file == file)
            .max_by_key(|buffer| buffer.order)
            .map(|buffer| buffer.text.clone())
            .or_else(|| self.disk.get(file).cloned())
    }

    async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Open {
                id,
                file,
                text,
                version,
                events,
            } => {
                if text.len() > MAX_BUFFER || text.contains('\0') {
                    let _ = events.send(ServerEvent::LspDocument {
                        document: id.1,
                        update: LspUpdate::Status {
                            status: LspStatus::Unavailable,
                        },
                    });
                    return Ok(());
                }
                self.disk.remove(&file);
                self.next += 1;
                self.last_use = Instant::now();
                self.buffers.insert(
                    id,
                    Buffer {
                        file: file.clone(),
                        text,
                        version,
                        events,
                        order: self.next,
                    },
                );
                Self::update(
                    id,
                    &self.buffers[&id],
                    LspUpdate::Status {
                        status: self.status,
                    },
                );
                if self.start().await.is_ok() {
                    self.sync_file(&file, false).await?;
                    self.fan_out(&file);
                }
            }
            Command::Edit { id, edit } => {
                let Some(buffer) = self.buffers.get_mut(&id) else {
                    return Ok(());
                };
                if edit.version < buffer.version
                    || edit.text.len() > MAX_BUFFER
                    || edit.text.contains('\0')
                {
                    return Ok(());
                }
                self.last_use = Instant::now();
                self.next += 1;
                buffer.version = edit.version;
                buffer.text = edit.text;
                buffer.order = self.next;
                let file = buffer.file.clone();
                if self.start().await.is_ok() {
                    self.sync_file(&file, edit.save).await?;
                }
            }
            Command::Close { id } => {
                if let Some(buffer) = self.buffers.remove(&id) {
                    self.sync_file(&buffer.file, false).await?;
                }
            }
            Command::Query {
                file,
                buffer,
                method,
                params,
                reply,
            } => {
                if reply.is_closed() {
                    return Ok(());
                }
                if let Err(error) = self.prepare_query(file.as_deref(), buffer, method).await {
                    let _ = reply.send(Err(error));
                    return Ok(());
                }
                if method == "textDocument/publishDiagnostics" {
                    let file = file.context("source path required")?;
                    if let Some(items) = self
                        .published
                        .get(&file)
                        .and_then(|doc| doc.diagnostics.as_ref())
                    {
                        let _ = reply.send(Ok(json!(items)));
                    } else {
                        self.diagnostic_waiters
                            .entry(file)
                            .or_default()
                            .push(Pending {
                                reply,
                                deadline: Instant::now() + DIAGNOSTICS_WAIT,
                            });
                    }
                } else {
                    self.next += 1;
                    self.pending.insert(
                        self.next,
                        Pending {
                            reply,
                            deadline: Instant::now() + rpc::WAIT,
                        },
                    );
                    self.process
                        .as_mut()
                        .unwrap()
                        .request(self.next, method, params)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn prepare_query(
        &mut self,
        file: Option<&Path>,
        buffer: Option<BufferId>,
        method: &str,
    ) -> Result<()> {
        if method.starts_with("$/lean/") {
            ensure!(self.project.language == "lean", "Lean source required");
        }
        if let Some(id) = buffer {
            let buffer = self
                .buffers
                .get_mut(&id)
                .context("editor buffer is closed")?;
            self.next += 1;
            buffer.order = self.next;
        }
        self.last_use = Instant::now();
        self.start().await?;
        if let Some(file) = file {
            if buffer.is_none() && !self.buffers.values().any(|buffer| buffer.file == file) {
                self.disk
                    .insert(file.to_owned(), read_source(file).await?.into());
            }
            if let Err(error) = self.sync_file(file, false).await {
                self.failed(error);
                anyhow::bail!("language server disconnected");
            }
        }
        Ok(())
    }

    async fn sync_file(&mut self, file: &Path, save: bool) -> Result<()> {
        let Some(_) = self.process else { return Ok(()) };
        let active = self.active_text(file);
        let uri = project::uri(file)?;
        let process = self.process.as_mut().unwrap();
        match (self.published.get(file), active) {
            (None, None) => return Ok(()),
            (Some(_), None) => {
                process
                    .notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
                    .await?;
                self.published.remove(file);
                return Ok(());
            }
            (None, Some(text)) => {
                self.version += 1;
                process.notify("textDocument/didOpen", json!({"textDocument":{
                    "uri":uri,"languageId":self.project.language,"version":self.version,"text": &*text
                }})).await?;
                self.published.insert(
                    file.to_owned(),
                    Published {
                        text,
                        version: self.version,
                        diagnostics: None,
                    },
                );
            }
            (Some(old), Some(text))
                if old.text != text && self.sync.change != TextDocumentSyncKind::NONE =>
            {
                self.version += 1;
                let mut change = json!({"text": &*text});
                if self.sync.change == TextDocumentSyncKind::INCREMENTAL {
                    change["range"] =
                        json!({"start":Position::new(0,0),"end":end_position(&old.text)});
                }
                process.notify("textDocument/didChange", json!({
                    "textDocument":{"uri":uri,"version":self.version}, "contentChanges":[change]
                })).await?;
                self.published.insert(
                    file.to_owned(),
                    Published {
                        text,
                        version: self.version,
                        diagnostics: None,
                    },
                );
                self.clear_diagnostics(file);
            }
            _ => {}
        }
        if save {
            if let Some(include_text) = self.sync.save_text {
                let mut params = json!({"textDocument":{"uri":uri}});
                if include_text {
                    params["text"] = json!(&*self.published[file].text);
                }
                self.process
                    .as_mut()
                    .unwrap()
                    .notify("textDocument/didSave", params)
                    .await?;
            }
        }
        Ok(())
    }

    fn clear_diagnostics(&self, file: &Path) {
        for (id, buffer) in self
            .buffers
            .iter()
            .filter(|(_, buffer)| buffer.file == file)
        {
            Self::update(
                *id,
                buffer,
                LspUpdate::Diagnostics {
                    version: buffer.version,
                    diagnostics: vec![],
                },
            );
        }
    }

    fn fan_out(&self, file: &Path) {
        let Some(doc) = self.published.get(file) else {
            return;
        };
        let Some(diagnostics) = &doc.diagnostics else {
            return;
        };
        let mut diagnostics = diagnostics.clone();
        // The editor renders the primary range and message. Related locations
        // have their own file coordinates and are exposed through the tool.
        for item in &mut diagnostics {
            if let Some(fields) = item.as_object_mut() {
                fields.remove("relatedInformation");
            }
        }
        for (id, buffer) in self
            .buffers
            .iter()
            .filter(|(_, buffer)| buffer.file == file && buffer.text == doc.text)
        {
            Self::update(
                *id,
                buffer,
                LspUpdate::Diagnostics {
                    version: buffer.version,
                    diagnostics: diagnostics.clone(),
                },
            );
        }
    }

    async fn message(&mut self, mut message: Value) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            if message.get("id").is_some() {
                self.process
                    .as_mut()
                    .unwrap()
                    .answer(&message, &self.folder)
                    .await?;
            } else if method == "textDocument/publishDiagnostics" {
                if let Ok(params) = serde_json::from_value(message["params"].take()) {
                    self.diagnostics(params);
                }
            }
        } else if let Some(id) = message["id"].as_u64() {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = pending.reply.send(rpc::result(message));
            }
        }
        Ok(())
    }

    fn diagnostics(&mut self, params: PublishDiagnosticsParams) {
        let Some(path) = project::relative_uri(&self.workspace, params.uri.as_str()) else {
            return;
        };
        let file = self.workspace.join(path);
        let Some(doc) = self.published.get_mut(&file) else {
            return;
        };
        if params.version.is_some_and(|version| version != doc.version) {
            return;
        }
        let diagnostics = params
            .diagnostics
            .into_iter()
            .filter_map(|d| serde_json::to_value(d).ok())
            .collect::<Vec<_>>();
        doc.diagnostics = Some(diagnostics.clone());
        self.fan_out(&file);
        for pending in self.diagnostic_waiters.remove(&file).into_iter().flatten() {
            let _ = pending.reply.send(Ok(json!(diagnostics)));
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let keep = |request: &Pending| request.deadline > now && !request.reply.is_closed();
        self.pending.retain(|_, request| keep(request));
        self.diagnostic_waiters.retain(|_, waiters| {
            waiters.retain(keep);
            !waiters.is_empty()
        });
    }
}

fn end_position(text: &str) -> Position {
    let row = text.bytes().filter(|byte| *byte == b'\n').count();
    let last = text.rsplit('\n').next().unwrap_or_default();
    Position::new(row as u32, last.encode_utf16().count() as u32)
}

pub(super) async fn read_source(file: &Path) -> Result<String> {
    let metadata = tokio::fs::metadata(file).await?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_BUFFER as u64,
        "regular source file within size limit required"
    );
    let text = tokio::fs::read_to_string(file).await?;
    ensure!(
        text.len() <= MAX_BUFFER && !text.contains('\0'),
        "UTF-8 source text required"
    );
    Ok(text)
}
