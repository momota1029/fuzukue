use super::*;
use crate::TurnKind;
use fuzukue_proto::{GitHubPolicy, TodoState, TrashAction, TrashTarget, TODO_PROTOCOL};
use std::time::Duration;

/// A new Claude front desk running a turn of this kind, after ending any
/// earlier turn. Its input is copied to the returned file.
async fn turn(app: &Arc<Workspace>, thread: &str, kind: TurnKind) -> Result<PathBuf> {
    app.hub.publish(ServerEvent::TurnActivity { session: thread.into(), running: false });
    let log = app.project_dir.join(format!("desk-{}.jsonl", crate::mail::session_key()));
    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", r#"while IFS= read -r line; do printf '%s\n' "$line" >> "$0"; done"#]).arg(&log);
    let session = crate::ClaudeSession::spawn_command(
        command, thread.into(), app.project_dir.clone(), app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(thread.into(), session.clone());
    assert!(session.try_background_prompt("synthetic turn".into(), kind, None).await?);
    Ok(log)
}

async fn desk(app: &Arc<Workspace>, thread: &str) -> Result<()> {
    turn(app, thread, TurnKind::Conversation).await.map(|_| ())
}

/// The first complete line of the log that contains the marker.
async fn received(log: &Path, marker: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = fs::read_to_string(log).unwrap_or_default();
            let line = text.split_inclusive('\n').find(|line| line.ends_with('\n') && line.contains(marker));
            if let Some(line) = line {
                return line.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("the front desk did not receive the input")
}

async fn call(app: &Arc<Workspace>, thread: &str, name: &str, args: Value) -> Value {
    mcp_request_for(app, thread, "tools/call", json!({"name": name, "arguments": args})).await
}

async fn listed(app: &Arc<Workspace>, thread: &str) -> Vec<String> {
    mcp_request_for(app, thread, "tools/list", json!({})).await["tools"].as_array().unwrap().iter()
        .filter_map(|tool| tool["name"].as_str().filter(|name| name.starts_with("todo_")).map(Into::into))
        .collect()
}

/// A trusted turn input that was submitted.
async fn input(app: &Arc<Workspace>, thread: &str) -> Result<String> {
    let (input, notices) = app.partner_input(thread, "synthetic request".into(), false).await?;
    crate::todo::delivered(app, thread, notices);
    Ok(input)
}

/// Adds three items without a front desk turn.
fn seed(app: &Workspace, thread: &str) -> Result<()> {
    crate::todo::update(app, thread, serde_json::from_value(three()["changes"].clone())?)?;
    Ok(())
}

async fn mark(app: &Arc<Workspace>, thread: &str, item: &str) -> Result<()> {
    handle(app, ClientMsg::SetTodoState { session: thread.into(), item: item.into(), done: true }).await
}

fn pending(app: &Workspace, thread: &str) -> bool {
    fs::read_to_string(list_file(app, thread)).is_ok_and(|text| text.contains("user_change"))
}

fn three() -> Value {
    json!({"changes": [
        {"op": "add", "text": "synthetic first"},
        {"op": "add", "text": "synthetic second"},
        {"op": "add", "text": "synthetic third", "state": "waiting", "wait": "action"},
    ]})
}

fn list_file(app: &Workspace, thread: &str) -> PathBuf {
    app.project_dir.join("threads").join(thread).join("todo.json")
}

fn snapshot(app: &Workspace, thread: &str) -> Option<(Vec<(String, TodoState)>, bool)> {
    crate::todo::snapshots(app).into_iter().find_map(|event| match event {
        ServerEvent::TodoChanged { session, items, unreadable, .. } if session == thread =>
            Some((items.into_iter().map(|item| (item.id, item.state)).collect(), unreadable)),
        _ => None,
    })
}

#[tokio::test]
async fn todo_tools_and_turn_input_belong_to_the_front_desk() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;

    // Without a front desk session the tools refuse.
    for name in ["todo_list", "todo_update"] {
        assert_eq!(call(&app, &thread, name, three()).await["isError"], true, "{name}");
        let explicit = [name.to_owned()];
        for allow in [&[][..], &explicit[..]] {
            assert!(!mcp::worker_tool_names(allow).iter().any(|tool| tool == name));
        }
        assert!(mcp::codex_tool_policy(None).get(name).is_none());
        assert!(mcp::codex_tool_policy(Some(&explicit)).get(name).is_none());
        // Adding items asks for no approval on either front desk backend.
        assert_eq!(mcp::codex_desk_tool_policy()[name]["approval_mode"], "approve");
        let claude = format!("mcp__fuzukue__{name}");
        assert!(mcp::claude_tool_names(false, true).any(|tool| tool == claude));
    }
    assert!(!list_file(&app, &thread).exists());

    desk(&app, &thread).await?;
    assert_eq!(listed(&app, &thread).await, ["todo_list", "todo_update"]);
    let before = input(&app, &thread).await?;
    assert!(!before.contains("T1"), "{before}");

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let result = call(&app, &thread, "todo_update", three()).await;
    assert_eq!(mcp_data(&result)["open"], 3);
    let changed = std::iter::from_fn(|| events.try_recv().ok()).map(|envelope| envelope.event)
        .find(|event| matches!(event, ServerEvent::TodoChanged { .. }));
    let Some(ServerEvent::TodoChanged { session, revision: 1, items, unreadable: false }) = changed else {
        anyhow::bail!("missing the TODO event");
    };
    assert_eq!((session.as_str(), items.len()), (thread.as_str(), 3));

    // A refused change reports itself and the others still apply.
    let result = call(&app, &thread, "todo_update", json!({"changes": [
        {"op": "set_state", "id": "T1", "state": "done"},
        {"op": "set_state", "id": "T2", "state": "doing"},
    ]})).await;
    assert_eq!(mcp_data(&result)["results"][0]["ok"], false);
    assert_eq!(mcp_data(&result)["results"][1]["ok"], true);
    let result = call(&app, &thread, "todo_list", json!({})).await;
    assert_eq!(mcp_data(&result)["items"][0]["id"], "T2");

    let turn = input(&app, &thread).await?;
    for text in ["T1 [", "T2 [", "T3 [", "synthetic first", "synthetic request"] {
        assert!(turn.contains(text), "{text}: {turn}");
    }

    // Automatic GitHub turns see neither the tools nor the list.
    app.issue_threads.test_start_automatic_turn(&thread, GitHubPolicy::Allow);
    assert!(listed(&app, &thread).await.is_empty());
    for name in ["todo_list", "todo_update"] {
        assert_eq!(call(&app, &thread, name, json!({})).await["isError"], true, "{name}");
    }
    let automatic = input(&app, &thread).await?;
    assert!(!automatic.contains("synthetic first") && !automatic.contains("TODO"), "{automatic}");
    app.issue_threads.finish(&thread);
    assert!(input(&app, &thread).await?.contains("synthetic first"));
    Ok(())
}

#[tokio::test]
async fn todo_lists_survive_restarts_and_screen_changes_reach_the_desk_once() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    desk(&app, &thread).await?;
    mcp_data(&call(&app, &thread, "todo_update", three()).await);

    // A change on screen and a front desk change both stay.
    handle(&app, ClientMsg::SetTodoState { session: thread.clone(), item: "T1".into(), done: true }).await?;
    mcp_data(&call(&app, &thread, "todo_update",
        json!({"changes": [{"op": "set_state", "id": "T2", "state": "doing"}]})).await);
    assert_eq!(snapshot(&app, &thread).unwrap().0, [
        ("T1".into(), TodoState::Done), ("T2".into(), TodoState::Doing), ("T3".into(), TodoState::Waiting),
    ]);
    let first = input(&app, &thread).await?;
    assert!(!first.contains("T1 [") && first.contains("T1"), "{first}");
    let second = input(&app, &thread).await?;
    assert!(!second.contains("T1") && second.contains("T2 ["), "{second}");

    handle(&app, ClientMsg::SetTodoState { session: thread.clone(), item: "T1".into(), done: false }).await?;
    assert!(handle(&app, ClientMsg::SetTodoState { session: thread.clone(), item: "T9".into(), done: true }).await.is_err());

    // A new server reads the same list, so no conversation state is needed.
    drop(app);
    let app = mcp_app(&project);
    assert_eq!(snapshot(&app, &thread).unwrap().0.len(), 3);
    let restarted = input(&app, &thread).await?;
    assert!(restarted.contains("T1 [") && restarted.contains("T3 ["), "{restarted}");

    // A damaged list is reported and left alone.
    fs::write(list_file(&app, &thread), "{")?;
    assert_eq!(snapshot(&app, &thread), Some((Vec::new(), true)));
    let damaged = input(&app, &thread).await?;
    assert!(damaged.contains("todo.json"), "{damaged}");
    desk(&app, &thread).await?;
    assert_eq!(call(&app, &thread, "todo_update", three()).await["isError"], true);
    assert!(handle(&app, ClientMsg::SetTodoState { session: thread.clone(), item: "T1".into(), done: true }).await.is_err());
    assert_eq!(fs::read_to_string(list_file(&app, &thread))?, "{");
    Ok(())
}

#[tokio::test]
async fn todo_lists_follow_their_thread_through_archive_trash_and_purge() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let other = app.hub.edit_threads(|store| store.create(None))?;
    seed(&app, &thread)?;
    // Another thread starts empty.
    assert!(snapshot(&app, &other).is_none());
    app.hub.set_thread_archived(&thread, true)?;
    assert_eq!(snapshot(&app, &thread).unwrap().0.len(), 3);
    app.hub.set_thread_archived(&thread, false)?;

    let trash = async |action| -> Result<Vec<fuzukue_proto::TrashItem>> {
        match crate::trash::request(&app, action, fuzukue_i18n::Locale::JAPANESE).await? {
            ServerEvent::TrashItems { items } => Ok(items),
            event => anyhow::bail!("unexpected trash result: {event:?}"),
        }
    };
    let delete = || TrashAction::Delete { target: TrashTarget::Thread { id: thread.clone() } };
    let items = trash(delete()).await?;
    assert!(list_file(&app, &thread).is_file());
    assert!(snapshot(&app, &thread).is_none());

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    trash(TrashAction::Restore { id: items[0].id.clone() }).await?;
    let mut republished = false;
    while let Ok(envelope) = events.try_recv() {
        republished |= matches!(envelope.event,
            ServerEvent::TodoChanged { ref session, ref items, .. } if *session == thread && items.len() == 3);
    }
    assert!(republished);

    let items = trash(delete()).await?;
    trash(TrashAction::Purge { id: items[0].id.clone() }).await?;
    assert!(!list_file(&app, &thread).exists());
    Ok(())
}

#[tokio::test]
async fn claude_turns_with_external_input_neither_read_nor_change_todos() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    seed(&app, &thread)?;
    mark(&app, &thread, "T1").await?;
    let refused = async || {
        for name in ["todo_list", "todo_update"] {
            let args = if name == "todo_list" { json!({}) } else { three() };
            assert_eq!(call(&app, &thread, name, args).await["isError"], true, "{name}");
        }
    };

    // A thread's first turn can start on external input, as for a workspace
    // request, work on an Issue, or an update result.
    let log = turn(&app, &thread, TurnKind::ExternalInput).await?;
    let sent = received(&log, "synthetic turn").await?;
    assert!(!sent.contains("synthetic second") && !sent.contains(": T1"), "{sent}");
    assert!(pending(&app, &thread));
    refused().await;

    // The next trusted turn reports the change once it is submitted.
    let log = turn(&app, &thread, TurnKind::Conversation).await?;
    let sent = received(&log, "synthetic turn").await?;
    assert!(sent.contains("synthetic second") && sent.contains(": T1"), "{sent}");
    assert!(!pending(&app, &thread));

    // A change made on screen during the turn wins until it has been reported.
    mark(&app, &thread, "T2").await?;
    let doing = json!({"changes": [{"op": "set_state", "id": "T2", "state": "doing"}]});
    let result = call(&app, &thread, "todo_update", doing.clone()).await;
    assert_eq!(mcp_data(&result)["results"][0]["ok"], false);
    assert_eq!(mcp_data(&result)["results"][0]["user_change"], "done");
    let removal = json!({"changes": [{"op": "remove", "id": "T2"}]});
    let result = call(&app, &thread, "todo_update", removal).await;
    assert_eq!(mcp_data(&result)["results"][0]["ok"], false);
    assert_eq!(snapshot(&app, &thread).unwrap().0[1], ("T2".into(), TodoState::Done));
    assert!(pending(&app, &thread));
    input(&app, &thread).await?;
    let result = call(&app, &thread, "todo_update", doing).await;
    assert_eq!(mcp_data(&result)["results"][0]["ok"], true);

    // External content arrives partway through the turn.
    mark(&app, &thread, "T3").await?;
    app.hub.mark_external_approval_input(&thread);
    refused().await;
    let (steering, notices) = app.partner_input(&thread, "synthetic request".into(), false).await?;
    assert!(!steering.contains("synthetic second") && !steering.contains("T3"), "{steering}");
    crate::todo::delivered(&app, &thread, notices);
    assert!(pending(&app, &thread));
    Ok(())
}

#[tokio::test]
async fn codex_turns_with_external_input_get_no_todos_even_when_steered() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    seed(&app, &thread)?;
    mark(&app, &thread, "T1").await?;
    let log = project.path().join("fixture-rpc.jsonl");
    stub_codex(&app, &log, None).await?;
    app.hub.edit_threads(|store| store.set_roles(&thread, Some(fuzukue_proto::ChatSettings {
        model: Some("gpt-6.2-sol".into()), ..Default::default()
    }), None))?;

    // The fixture's question keeps the external turn running.
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let prompt = "codex-question fixture dismissal external".to_owned();
    assert!(crate::delegate_background_turn(&app, &thread, prompt, TurnKind::ExternalInput).await?);
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::QuestionAsked { request_id, .. } = live.recv().await?.event {
                return Ok::<_, anyhow::Error>(request_id);
            }
        }
    }).await??;
    handle(&app, ClientMsg::Prompt { session: thread.clone(), text: "synthetic steering".into() }).await?;
    let steering = received(&log, "synthetic steering").await?;
    assert!(steering.contains("turn/steer"), "{steering}");
    let sent = fs::read_to_string(&log)?;
    assert!(!sent.contains("synthetic second"), "{sent}");
    assert!(pending(&app, &thread));

    handle(&app, ClientMsg::AnswerQuestion { request_id: question, answers: Default::default(), dismiss: true }).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.hub.thread_running(&thread) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    let prompt = "synthetic trusted request".to_owned();
    assert!(crate::delegate_background_turn(&app, &thread, prompt, TurnKind::Conversation).await?);
    let trusted = received(&log, "synthetic trusted request").await?;
    assert!(trusted.contains("synthetic second") && trusted.contains(": T1"), "{trusted}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while pending(&app, &thread) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    Ok(())
}

#[tokio::test]
async fn todo_events_and_changes_need_a_client_that_negotiates_them() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    seed(&app, &thread)?;
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access))
        .layer(Extension(ConnectInfo(peer)));
    let (incoming, sockets) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move { axum::serve(MemoryListener(sockets), router).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for protocol in [TODO_PROTOCOL - 1, TODO_PROTOCOL] {
            let current = protocol >= TODO_PROTOCOL;
            let (local, remote) = tokio::io::duplex(128 * 1024);
            incoming.send(remote).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", local).await?;
            let hello = ClientMsg::Hello { protocol, locale: None, resume_from: Some(0),
                workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            let change = ClientMsg::SetTodoState { session: thread.clone(), item: "T1".into(), done: true };
            socket.send(Message::Text(serde_json::to_string(&change)?.into())).await?;
            // The rename is handled after the change, so it marks the end.
            let title = format!("protocol {protocol}");
            let rename = ClientMsg::RenameThread { id: thread.clone(), title: title.clone() };
            socket.send(Message::Text(serde_json::to_string(&rename)?.into())).await?;
            let mut lists = Vec::new();
            loop {
                let frame = socket.next().await.context("Socket closed early")??;
                let envelope: Envelope = serde_json::from_str(frame.to_text()?)?;
                match envelope.event {
                    ServerEvent::TodoChanged { items, .. } => lists.push(items[0].state),
                    ServerEvent::Threads { threads }
                        if threads.iter().any(|thread| thread.title == title) => break,
                    _ => {}
                }
            }
            if current {
                assert_eq!(lists, [TodoState::Todo, TodoState::Done]);
            } else {
                assert!(lists.is_empty());
                assert_eq!(snapshot(&app, &thread).unwrap().0[0].1, TodoState::Todo);
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}
