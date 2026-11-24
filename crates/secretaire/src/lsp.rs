//! Shares language-server processes between editor connections and workspace tools.

mod actor;
mod mcp;
mod project;
mod rpc;
#[cfg(test)]
mod tests;

pub(crate) use mcp::Tool;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{LspBuffer, LspOutcome, LspPosition, LspQuery, LspStatus, LspUpdate, ServerEvent};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

use actor::{Command, Edit, Endpoint};
use project::Project;

type Events = mpsc::UnboundedSender<ServerEvent>;
type BufferId = (u64, u64);
const IDLE: Duration = Duration::from_secs(300);

pub(super) struct Manager {
    workspace: PathBuf,
    servers: Mutex<HashMap<(String, PathBuf), Endpoint>>,
    idle: Duration,
}

impl Manager {
    pub(super) fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.into()),
            servers: Mutex::default(),
            idle: IDLE,
        }
    }

    fn server(&self, project: Project) -> Endpoint {
        let key = (project.language.clone(), project.root.clone());
        let mut servers = self.servers.lock().unwrap();
        servers
            .entry(key)
            .or_insert_with(|| actor::spawn(project, self.workspace.clone(), self.idle))
            .clone()
    }

    fn document(&self, path: &str, text: &str) -> Result<Option<(Endpoint, PathBuf)>> {
        let file = project::scoped(&self.workspace, path)?;
        ensure!(file.is_file(), "source file required");
        let language = fuzukue_view::source_language_with_text(path, text);
        let Some(project) = project::discover(
            &self.workspace,
            file.parent().context("missing parent")?,
            language,
        )?
        else {
            return Ok(None);
        };
        Ok(Some((self.server(project), file)))
    }

    pub(super) fn connection(&self, events: Events) -> Connection<'_> {
        static CONNECTION: AtomicU64 = AtomicU64::new(0);
        Connection {
            manager: self,
            owner: CONNECTION.fetch_add(1, Ordering::Relaxed),
            events,
            documents: HashMap::new(),
        }
    }
}

pub(super) struct Connection<'a> {
    manager: &'a Manager,
    owner: u64,
    events: Events,
    documents: HashMap<u64, (Endpoint, PathBuf)>,
}

impl Connection<'_> {
    pub(super) fn update(&mut self, document: u64, update: LspBuffer) {
        let id = (self.owner, document);
        let save = matches!(update, LspBuffer::Save { .. });
        match update {
            LspBuffer::Open {
                path,
                version,
                text,
            } => {
                self.close(document);
                match self.manager.document(&path, &text) {
                    Ok(Some((server, file))) => {
                        server.send(Command::Open {
                            id,
                            file: file.clone(),
                            version,
                            text: text.into(),
                            events: self.events.clone(),
                        });
                        self.documents.insert(document, (server, file));
                    }
                    other => {
                        let status = match other {
                            Err(error) => {
                                tracing::warn!("LSP document {path}: {error:#}");
                                LspStatus::Failed
                            }
                            Ok(_) => LspStatus::Unavailable,
                        };
                        let _ = self.events.send(ServerEvent::LspDocument {
                            document,
                            update: LspUpdate::Status { status },
                        });
                    }
                }
            }
            LspBuffer::Change { version, text } | LspBuffer::Save { version, text } => {
                if let Some((server, _)) = self.documents.get(&document) {
                    server.send(Command::Edit {
                        id,
                        edit: Edit {
                            version,
                            text: text.into(),
                            save,
                        },
                    });
                }
            }
            LspBuffer::Close => self.close(document),
        }
    }

    pub(super) fn request(&self, request: u64, document: u64, query: LspQuery) {
        let events = self.events.clone();
        let Some((server, path)) = self.documents.get(&document) else {
            let _ = events.send(ServerEvent::LspResult {
                request,
                outcome: LspOutcome::Error {
                    message: "language server unavailable".into(),
                },
            });
            return;
        };
        let workspace = self.manager.workspace.clone();
        let (server, path) = (server.clone(), path.clone());
        let id = Some((self.owner, document));
        // Queue the first operation before reading the next editor message.
        let action = Request::start(&server, Some(&path), id, query);
        tokio::spawn(async move {
            let result = match action {
                Ok(action) => action.finish(&server, &workspace, id).await,
                Err(error) => Err(error),
            };
            let outcome = match result {
                Ok(value) => LspOutcome::Value { value },
                Err(error) => LspOutcome::Error {
                    message: format!("{error:#}"),
                },
            };
            let _ = events.send(ServerEvent::LspResult { request, outcome });
        });
    }

    fn close(&mut self, document: u64) {
        if let Some((server, _)) = self.documents.remove(&document) {
            server.send(Command::Close {
                id: (self.owner, document),
            });
        }
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        for (document, (server, _)) in self.documents.drain() {
            server.send(Command::Close {
                id: (self.owner, document),
            });
        }
    }
}

struct Request {
    result: oneshot::Receiver<Result<Value>>,
    extra: Extra,
}

enum Extra {
    None,
    Calls,
    TermGoal(oneshot::Receiver<Result<Value>>),
}

impl Request {
    fn start(
        server: &Endpoint,
        path: Option<&Path>,
        buffer: Option<BufferId>,
        query: LspQuery,
    ) -> Result<Self> {
        let document = path
            .map(|path| Ok::<_, anyhow::Error>(json!({"uri":project::uri(path)?})))
            .transpose()?;
        let position = |position: LspPosition| json!({"textDocument":document,"position":position});
        let (method, params) = match &query {
            LspQuery::Hover { position: point } => ("textDocument/hover", position(*point)),
            LspQuery::Completion {
                position: point,
                context,
            } => {
                let mut params = position(*point);
                params["context"] = context.clone();
                ("textDocument/completion", params)
            }
            LspQuery::Definition { position: point } => {
                ("textDocument/definition", position(*point))
            }
            LspQuery::References { position: point } => {
                let mut params = position(*point);
                params["context"] = json!({"includeDeclaration": true});
                ("textDocument/references", params)
            }
            LspQuery::DocumentSymbols => (
                "textDocument/documentSymbol",
                json!({"textDocument": document}),
            ),
            LspQuery::WorkspaceSymbols { query } => ("workspace/symbol", json!({"query":query})),
            LspQuery::CallHierarchy { position: point } => {
                ("textDocument/prepareCallHierarchy", position(*point))
            }
            LspQuery::LeanGoal { position: point } => ("$/lean/plainGoal", position(*point)),
            LspQuery::Diagnostics => ("textDocument/publishDiagnostics", Value::Null),
        };
        let file = match query {
            LspQuery::WorkspaceSymbols { .. } => None,
            _ => Some(path.context("source path required")?.to_owned()),
        };
        let result = server.request(file.clone(), buffer, method, params);
        let extra = match query {
            LspQuery::CallHierarchy { .. } => Extra::Calls,
            LspQuery::LeanGoal { position: point } => Extra::TermGoal(server.request(
                file,
                buffer,
                "$/lean/plainTermGoal",
                position(point),
            )),
            _ => Extra::None,
        };
        Ok(Self { result, extra })
    }

    async fn finish(
        self,
        server: &Endpoint,
        workspace: &Path,
        buffer: Option<BufferId>,
    ) -> Result<Value> {
        let result = receive(self.result).await;
        let value = match self.extra {
            Extra::None => result?,
            Extra::TermGoal(term) => {
                let term = receive(term).await;
                match (result, term) {
                    (Err(error), Err(_)) => return Err(error),
                    (goal, term) => json!({"goal": goal.ok(), "term_goal": term.ok()}),
                }
            }
            Extra::Calls => {
                let items = result?;
                let mut calls = Vec::new();
                for item in items.as_array().into_iter().flatten() {
                    if item["uri"]
                        .as_str()
                        .and_then(|uri| project::relative_uri(workspace, uri))
                        .is_none()
                    {
                        continue;
                    }
                    let params = json!({"item":item});
                    let incoming =
                        server.request(None, buffer, "callHierarchy/incomingCalls", params.clone());
                    let outgoing =
                        server.request(None, buffer, "callHierarchy/outgoingCalls", params);
                    let (incoming, outgoing) =
                        tokio::try_join!(receive(incoming), receive(outgoing))?;
                    calls.push(json!({"item": item, "incoming": incoming, "outgoing": outgoing}));
                }
                json!(calls)
            }
        };
        Ok(project::scope_locations(workspace, value).unwrap_or(Value::Null))
    }
}

async fn receive(rx: oneshot::Receiver<Result<Value>>) -> Result<Value> {
    tokio::time::timeout(rpc::WAIT + Duration::from_secs(5), rx)
        .await
        .context("language server timed out")??
}
