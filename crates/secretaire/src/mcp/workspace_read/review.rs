//! A one-run MCP credential for standalone review agents.

use super::{GUIDANCE, ReadScope, Tool};
use crate::{Workspace, delegate::WorkerLaunch};
use anyhow::Result;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const TOOLS: [Tool; 4] = [Tool::Read, Tool::List, Tool::Search, Tool::GitShow];
const PREFIX: &str = "workspace-review-";
static ACTIVE: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Mutex::default);

struct Entry {
    key: String,
    workspace: String,
    session: String,
    scope: ReadScope,
}

pub(crate) struct ReviewLease {
    id: String,
    pub(crate) launch: WorkerLaunch,
}

impl ReviewLease {
    pub(crate) fn open(app: &Workspace, session: &str) -> Result<Option<Self>> {
        if app.addr.is_empty() || crate::canary() {
            return Ok(None);
        }
        let id = format!("{PREFIX}{}", crate::mail::session_key());
        let key = crate::mail::session_key();
        let mut url = url::Url::parse(&format!("http://{}/mcp", app.addr))?;
        url.query_pairs_mut()
            .append_pair("workspace", &app.id)
            .append_pair("session", session)
            .append_pair("job", &id);
        let scope = ReadScope::new(&app.project_dir, &app.state_dir, None)?;
        ACTIVE.lock().unwrap().insert(
            id.clone(),
            Entry {
                key: key.clone(),
                workspace: app.id.clone(),
                session: session.to_owned(),
                scope,
            },
        );
        Ok(Some(Self {
            id,
            launch: WorkerLaunch {
                scope: None,
                instructions: GUIDANCE.into(),
                mcp_url: Some(url.to_string()),
                key,
                tools: TOOLS.map(|tool| tool.name().to_owned()).into(),
            },
        }))
    }
}

pub(crate) fn is_review_launch(launch: &WorkerLaunch) -> bool {
    !launch.tools.is_empty() && launch.tools.iter().all(|name| review_tool(name).is_some())
}

fn review_tool(name: &str) -> Option<Tool> {
    TOOLS.into_iter().find(|tool| tool.name() == name)
}

pub(crate) fn claude_config(launch: Option<&WorkerLaunch>) -> String {
    let servers = launch
        .and_then(|launch| launch.mcp_url.as_ref().map(|url| (launch, url)))
        .map_or_else(
            || json!({}),
            |(launch, url)| {
                json!({
                    (super::super::SERVER_NAME): {
                        "type": "http", "url": url,
                        "headers": {"Authorization": format!("Bearer {}", launch.key)},
                    },
                })
            },
        );
    json!({"mcpServers": servers}).to_string()
}

impl Drop for ReviewLease {
    fn drop(&mut self) {
        ACTIVE.lock().unwrap().remove(&self.id);
    }
}

pub(in crate::mcp) fn is_review(job: &str) -> bool {
    job.starts_with(PREFIX)
}

pub(in crate::mcp) async fn dispatch(
    app: &Workspace,
    job: &str,
    session: &str,
    key: Option<&str>,
    id: Value,
    req: &Value,
) -> Response {
    let scope = ACTIVE
        .lock()
        .unwrap()
        .get(job)
        .filter(|entry| {
            key == Some(entry.key.as_str()) && session == entry.session && app.id == entry.workspace
        })
        .map(|entry| entry.scope.clone());
    let Some(scope) = scope else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let params = &req["params"];
    let result = match req["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": super::super::SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
        }),
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools": TOOLS.map(Tool::definition)}),
        Some("tools/call") => {
            let tool = params["name"].as_str().and_then(review_tool);
            let result = match tool {
                Some(tool) => {
                    tool.call(scope, params.get("arguments").unwrap_or(&json!({})))
                        .await
                }
                None => Err(anyhow::anyhow!("Tool is not allowed for workspace review")),
            };
            match result {
                Ok(data) => super::super::data_result(data, false),
                Err(error) => super::super::text_result(format!("{error:#}"), true),
            }
        }
        _ => {
            return Json(json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": "Method not found",
            }}))
            .into_response();
        }
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}
