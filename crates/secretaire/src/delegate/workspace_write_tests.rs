use std::fs;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::routing::post;
use serde_json::Value;
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn scoped_write_mcp_approves_only_outside_touches() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?.join("worktree");
    fs::create_dir(&root)?;
    let state = fixture.path().join("state");
    fs::create_dir(&state)?;
    let app = Arc::new(App::new(
        root.clone(),
        "127.0.0.1:0".into(),
        state.clone(),
        Arc::new(crate::Hub::new()),
    ));
    *app.worker_approval_policy.lock().unwrap() = fuzukue_proto::WorkerApprovalPolicy::Auto;
    let session = "scoped-write";
    let job = "delegate:scoped-write";
    let key = "test-worker-key";
    let writes = Arc::new(touches::WriteSet::new(
        &root,
        Some(vec!["in.txt".into()]),
        "",
        || None,
    )?);
    let scope = Arc::new(crate::mcp::workspace_write::Scope::new(
        root.clone(),
        writes,
        &state,
        job,
    )?);
    let tools = crate::mcp::worker_tool_names(&[]);
    let policy = crate::mcp::codex_tool_policy(Some(&tools));
    let worker = Arc::new(RunningJob {
        session: session.into(),
        key: key.into(),
        tools: tools.into(),
        approach: Default::default(),
        summary: Default::default(),
        retry: Default::default(),
        input: Default::default(),
        cancel: Default::default(),
        started: true.into(),
        drain_notice: Default::default(),
        capture_scope: Default::default(),
        workspace_read_scope: Default::default(),
        approval_scope: Default::default(),
        workspace_write_scope: Mutex::new(Some(scope)),
    });
    app.delegates
        .running
        .lock()
        .unwrap()
        .insert(job.into(), worker.clone());
    let router = Router::new()
        .route("/mcp", post(crate::mcp::handler))
        .with_state(app.clone());
    let request = |worker: bool, method: &str, params: Value| {
        let uri = if worker {
            format!("/mcp?session={session}&job={job}")
        } else {
            format!("/mcp?session={session}")
        };
        let request = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            ))
            .unwrap();
        let response = router.clone().oneshot(request);
        async move {
            let response = response.await?;
            assert_eq!(response.status(), StatusCode::OK);
            Ok::<_, anyhow::Error>(
                serde_json::from_slice::<Value>(
                    &to_bytes(response.into_body(), 4 * 1024 * 1024).await?,
                )?["result"]
                    .clone(),
            )
        }
    };
    let worker_list = request(true, "tools/list", json!({})).await?;
    let desk_list = request(false, "tools/list", json!({})).await?;
    for tool in crate::mcp::workspace_write::Tool::ALL {
        let name = tool.name();
        let schema = worker_list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .unwrap();
        let required = match name {
            "fs_write" => json!(["path", "content"]),
            "fs_edit" => json!(["path", "old", "new"]),
            "fs_apply_patch" => json!(["path", "patch"]),
            _ => unreachable!(),
        };
        assert_eq!(schema["inputSchema"]["required"], required);
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
        assert_eq!(policy[name]["approval_mode"], "approve");
        assert!(
            !desk_list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == name)
        );
        assert!(
            !crate::mcp::available_tool_names(false)
                .iter()
                .any(|tool| tool.ends_with(name))
        );
        let refused = request(false, "tools/call", json!({"name": name, "arguments": {}})).await?;
        assert_eq!(refused["isError"], true);
    }

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let write = |path: &str, content: &str| json!({"name":"fs_write","arguments":{"path":path,"content":content}});
    let result = request(true, "tools/call", write("in.txt", "hello\n")).await?;
    assert_eq!(result["isError"], false, "{result}");
    assert!(
        result["structuredContent"]["diff"]
            .as_str()
            .unwrap()
            .contains("+hello\n")
    );
    assert!(result["structuredContent"]["audit_id"].is_string());
    assert_eq!(fs::read_to_string(root.join("in.txt"))?, "hello\n");
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event.event, ServerEvent::ApprovalRequested { .. }))
    );

    let next_approval =
        async |events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let ServerEvent::ApprovalRequested {
                        request_id,
                        summary,
                        ..
                    } = events.recv().await?.event
                    {
                        return Ok::<_, anyhow::Error>((request_id, summary));
                    }
                }
            })
            .await?
        };
    let pending = tokio::spawn(request(true, "tools/call", write("out.txt", "accepted\n")));
    let (id, summary) = next_approval(&mut events).await?;
    assert!(summary.contains("out.txt"));
    assert!(!root.join("out.txt").exists());
    assert!(app.delegates.resolve_approval(&id, "accept", true)?);
    assert!(!pending.is_finished());
    assert!(app.delegates.resolve_approval(&id, "accept", false)?);
    assert_eq!(pending.await??["isError"], false);
    assert_eq!(fs::read_to_string(root.join("out.txt"))?, "accepted\n");

    let audit = crate::mcp::workspace_write::audit_page(&state, job, None, 1)?;
    assert_eq!(audit["writes"][0]["approved"], true);
    let next = audit["next_before"].as_str().unwrap();
    let earlier = crate::mcp::workspace_write::audit_page(&state, job, Some(next), 0)?;
    assert_eq!(earlier["writes"][0]["path"], "in.txt");
    assert_eq!(earlier["writes"][0]["approved"], false);

    let pending = tokio::spawn(request(true, "tools/call", write("out.txt", "stale\n")));
    let (id, _) = next_approval(&mut events).await?;
    fs::write(root.join("out.txt"), "concurrent edit\n")?;
    assert!(app.delegates.resolve_approval(&id, "accept", false)?);
    assert_eq!(pending.await??["isError"], true);
    assert_eq!(
        fs::read_to_string(root.join("out.txt"))?,
        "concurrent edit\n"
    );

    // Enable through the real authenticated settings router using the same app.
    use axum::{Extension, extract::ConnectInfo};
    use std::os::unix::fs::PermissionsExt;
    let token = crate::remote::ensure_token(&state, false)?;
    let access = Arc::new(crate::remote::Access::new(&state, Default::default())?);
    let settings = crate::router(crate::App::fixture(app.clone()), access)
        .layer(Extension(ConnectInfo("127.0.0.1:1234".parse::<std::net::SocketAddr>()?)));
    let route = |path: &str, credential: &str, change: Option<Value>| {
        let mut request = Request::builder().uri(format!("{path}?workspace={}", app.id));
        if !credential.is_empty() {
            request = request.header(header::AUTHORIZATION, format!("Bearer {credential}"));
        }
        let request = match change {
            Some(change) => request.method("POST").header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(change.to_string())),
            None => request.body(Body::empty()),
        }.unwrap();
        let settings = settings.clone();
        async move {
            let response = settings.oneshot(request).await?;
            let status = response.status();
            let body = to_bytes(response.into_body(), 64 * 1024).await?;
            Ok::<_, anyhow::Error>((status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null)))
        }
    };
    let path = crate::full_approval::SETTINGS_PATH;
    let enable = || Some(json!({"mode": "full"}));
    for credential in ["", key] {
        assert_eq!(route(path, credential, enable()).await?.0, StatusCode::FORBIDDEN);
    }
    let (status, body) = route(path, &token, enable()).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"]["duration"], "24h");
    let enabled = body["active"]["enabled_at"].as_i64().unwrap();
    assert_eq!(body["active"]["expires_at"], enabled + 24 * 60 * 60 * 1000);
    let settings_file = state.join("full-approval-mode.json");
    assert_eq!(fs::metadata(&settings_file)?.permissions().mode() & 0o777, 0o600);
    fs::write(&settings_file, "null")?;
    assert_eq!(route(path, &token, None).await?.1["active"]["duration"], "24h");

    let (status, result) = route(crate::full_approval::EXECUTION_PATH, &token, Some(json!({
        "sandbox": "full-access", "approval": "ask"
    }))).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["settings"]["sandbox"], "full-access");
    assert_eq!(result["settings"]["approval"], "ask");
    assert!(result["bubblewrap"]["checked_at"].is_i64());
    assert_eq!(route(crate::full_approval::EXECUTION_PATH, key, None).await?.0, StatusCode::FORBIDDEN);

    let (_, mut queue_updates) = app.hub.pending_approvals()?;
    let first = request(true, "tools/call", write("full.txt", "written once\n")).await?;
    assert_eq!(first["isError"], false, "{first}");
    let repeat = request(true, "tools/call", write("full.txt", "written once\n")).await?;
    assert_eq!(repeat["isError"], false, "{repeat}");
    assert_eq!(fs::read_to_string(root.join("full.txt"))?, "written once\n");
    let decision = queue_updates.try_recv()?;
    assert_eq!(decision.state, fuzukue_proto::ApprovalState::Granted);
    assert!(decision.consumed_at.is_some());
    assert!(queue_updates.try_recv().is_err());
    let audit = route(path, &token, None).await?.1;
    assert_eq!(audit["audit"].as_array().unwrap().len(), 1);
    assert_eq!(audit["audit"][0]["granted_by"], "full_mode");
    assert_eq!(audit["audit"][0]["action"]["args"], "[redacted]");
    assert_eq!(audit["audit"][0]["approval_id"], decision.approval_id);
    let lease = app.hub.full_approval_lease()?;
    assert!(route(path, &token, Some(json!({"mode": "off"}))).await?.1["active"].is_null());
    tokio::time::timeout(Duration::from_secs(1), crate::full_approval::wait_for_end(lease)).await?;

    let pending = tokio::spawn(request(
        true,
        "tools/call",
        write("cancelled.txt", "never written"),
    ));
    next_approval(&mut events).await?;
    worker.cancel.cancel();
    assert_eq!(pending.await??["isError"], true);
    assert!(!root.join("cancelled.txt").exists());
    Ok(())
}

#[test]
fn post_job_quarantine_copies_changes_and_preserves_ambiguous_user_data() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("worktree");
    let state = fixture.path().join("state");
    fs::create_dir(&root)?;
    fs::create_dir(&state)?;
    fs::write(root.join("old.txt"), "before\n")?;
    fs::write(root.join("removed.txt"), "before\n")?;
    let before = Files::read(&root);
    fs::write(root.join("old.txt"), "after\n")?;
    fs::write(root.join("in.txt"), "allowed\n")?;
    fs::write(root.join("user-new.txt"), "preserve this\n")?;
    fs::remove_file(root.join("removed.txt"))?;
    std::os::unix::fs::symlink(fixture.path(), root.join("escape"))?;
    let after = Files::read(&root);
    let writes = touches::WriteSet::new(&root, Some(vec!["in.txt".into()]), "", || None)?;
    let changes = before.changes(&after);
    let recovery = crate::mcp::workspace_write::recovery_dir(&state, "delegate:quarantine-test")?;
    let review = quarantine::review(
        &root,
        &recovery,
        &writes,
        &changes.paths,
        &after.entries,
        before.incomplete || after.incomplete,
    );
    let text = review.render(false);
    assert!(text.contains("## Out-of-scope writes"));
    assert!(text.contains("removed.txt") && text.contains("missing"));
    assert!(text.contains("escape") && text.contains("not a regular file"));
    assert!(!text.contains("in.txt"));
    let dir = recovery.join("quarantine");
    let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    let entries = manifest["entries"].as_array().unwrap();
    let old = entries
        .iter()
        .find(|entry| entry["path"] == "old.txt")
        .unwrap();
    assert_eq!(old["status"], "copied");
    assert_eq!(
        fs::read_to_string(dir.join(old["backup"].as_str().unwrap()))?,
        "after\n"
    );
    assert_eq!(fs::read_to_string(root.join("old.txt"))?, "after\n");
    assert_eq!(
        fs::read_to_string(root.join("user-new.txt"))?,
        "preserve this\n"
    );
    Ok(())
}
