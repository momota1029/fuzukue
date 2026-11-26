#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use axum::{body::to_bytes, extract::{Query, State}, http::{header, HeaderMap, StatusCode}, Json};
use serde_json::Value;

use super::*;
use crate::tool_guard::{Limits, WorkerStopped};

const PARTIAL: &str = "The fixture wrote draft.txt.";

#[tokio::test]
async fn nested_mcp_burst_is_refused_and_the_worker_stops_with_partial_work() -> Result<()> {
    exercise_worker(false).await
}

#[tokio::test]
async fn native_claude_burst_stops_the_worker_without_any_mcp_calls() -> Result<()> {
    exercise_worker(true).await
}

async fn exercise_worker(native: bool) -> Result<()> {
    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = project.path().canonicalize()?;
    let worktree = root.join("checkout");
    fs::create_dir(&worktree)?;
    fs::write(worktree.join("input.json"), "{\"value\": 42}")?;
    let fixture = root.join("fake-claude");
    fs::write(&fixture, r#"#!/usr/bin/python3
import json, sys, time
from pathlib import Path
def emit(frame):
    print(json.dumps(frame), flush=True)

prompt = json.loads(sys.stdin.readline())["message"]["content"][0]["text"]
Path("draft.txt").write_text("partial work")
emit({"type":"assistant", "message":{"id":"fixture", "content":[
    {"type":"text", "text":"The fixture wrote draft.txt."}
]}})
if "native burst" in prompt:
    for i in range(11):
        emit({"type":"assistant", "message":{"id":f"msg-{i}", "content":[
            {"type":"tool_use", "id":f"tool-{i}", "name":"Read", "input":{"file_path":"input.json"}}
        ]}})
time.sleep(60)
"#)?;
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700))?;
    let hub = Arc::new(crate::Hub::with_tool_limits(Limits { same_tool_per_minute: 2, total_calls: 30 }));
    let mut app = App::new(root, "127.0.0.1:0".into(), state.path().into(), hub.clone());
    app.claude_program = Some(fixture);
    let app = Arc::new(app);
    let session = hub.edit_threads(|threads| threads.create(None))?;
    let (_, mut events) = hub.subscribe(u64::MAX);
    let selection = AgentSelection { backend: AgentBackend::Claude, model: None, effort: None };
    let prompt = if native { "Write draft.txt, then native burst" } else { "Write draft.txt" };
    let (job, run) = launch(&app, &session, prompt.into(), None, Some(worktree.clone()), None,
        Some(selection), None, None, Default::default(), Some(vec!["draft.txt".into()])).await?;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::TextDelta { text, .. } if text == PARTIAL) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    }).await??;

    if !native {
        let key = app.delegates.running.lock().unwrap()[&job].key.clone();
        let request = async |key: &str, name: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, format!("Bearer {key}").parse()?);
            let response = crate::mcp::handler(
                State(app.clone()),
                Query(crate::mcp::McpQuery {
                    session: session.clone(), job: Some(job.clone()), workspace: None, backend: None,
                }),
                headers,
                Json(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
                    "name": name, "arguments": {"path": "input.json", "filter": ".value"}
                }})),
            ).await;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 64 * 1024).await?;
            let result = if bytes.is_empty() { Value::Null } else { serde_json::from_slice::<Value>(&bytes)? };
            Ok::<_, anyhow::Error>((status, result))
        };
        assert_eq!(request("wrong key", "jq").await?.0, StatusCode::UNAUTHORIZED);
        for _ in 0..2 {
            let (status, response) = request(&key, "jq").await?;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(response["result"]["isError"], false, "{response}");
        }
        let (_, refused) = request(&key, "jq").await?;
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused.to_string().contains("more than 2 calls to fuzukue.jq in 60 seconds"));
        assert!(!run.is_finished());

        let (_, other) = request(&key, "fs_read").await?;
        assert!(other.to_string().contains("Tool call limit exceeded"));
        assert_eq!(app.delegates.job_status(&app, &job, JobView::Summary)?["tool_count"], 4);
        for _ in 0..7 {
            assert_eq!(request(&key, "jq").await?.1["result"]["isError"], true);
        }
    }

    let failure = tokio::time::timeout(Duration::from_secs(10), run).await??.unwrap_err();
    let stop = failure.downcast_ref::<WorkerStopped>().context("missing guard stop")?;
    assert_eq!(stop.partial.trim(), PARTIAL);
    assert_eq!(fs::read_to_string(worktree.join("draft.txt"))?, "partial work");
    let mut notices = 0;
    let mut report = None;
    while let Ok(event) = events.try_recv() {
        match event.event {
            ServerEvent::LocalNotice { session: owner, localized: Some(message), .. }
                if owner == session && message.key == "server-tool-guard-worker" => notices += 1,
            ServerEvent::ToolResult { item, ok: false, output, .. } if item == job => {
                report = Some(serde_json::from_str::<DelegateReport>(&output)?);
            }
            _ => {}
        }
    }
    assert_eq!(notices, 1);
    let report = report.context("missing worker report")?;
    assert!(report.text.contains(PARTIAL));
    assert!(report.changes.contains(&worktree.display().to_string()));
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.tool_activity.context("missing tool count")?.count, 11);
    Ok(())
}
