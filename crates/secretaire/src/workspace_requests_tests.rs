use std::{fs, os::unix::fs::PermissionsExt};

use fuzukue_proto::{Actor, FullApprovalChange, FullApprovalDuration, WORKSPACE_REQUESTS_PROTOCOL};

use super::*;
use crate::{
    remote::SettingsPrincipal,
    test_support::{TestDir, wait_for_path},
};

struct Fixture {
    dir: TestDir,
    source: PathBuf,
    receiver: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = TestDir::new();
        let source = dir.path().join("research");
        let receiver = dir.path().join("fuzukue");
        let state = dir.path().join("server-state");
        fs::create_dir(&source)?;
        fs::create_dir(&receiver)?;
        Ok(Self {
            dir,
            source,
            receiver,
            state,
        })
    }

    fn open(&self) -> Result<Arc<App>> {
        let app = App::new(
            self.dir.path().into(),
            self.source.clone(),
            self.state.clone(),
            "127.0.0.1:1".into(),
            None,
        )?;
        app.register(self.receiver.to_str().unwrap())?;
        Ok(app)
    }

    fn receiver(&self, app: &Arc<App>) -> Result<Arc<Workspace>> {
        app.known_workspace(&crate::workspaces::id_for(&self.receiver))
    }

    fn install_desk(&self, app: &App) {
        self.dir.write(
            "mock-claude",
            r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","session_id":"workspace-request-fixture"}'
while IFS= read -r input; do
    case "$input" in *'"type":"user"'*)
        printf '%s\n' "$input" > prompt-fixture.next
        mv prompt-fixture.next prompt-fixture
        case "$input" in *'Report this bug to fuzukue'*)
            for attempt in $(seq 1 200); do
                [ ! -e draft-test-done ] || break
                sleep 0.05
            done
            ;; esac
        printf '%s\n' '{"type":"result","is_error":false,"result":"started work"}'
        ;; esac
done
"#,
        );
        let program = self.dir.path().join("mock-claude");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        app.test_claude_program.set(program).unwrap();
    }
}

fn send_draft(receiver: &Workspace, body: &str) -> Value {
    json!({
        "action":"send", "destination": {"kind":"workspace","id":receiver.id},
        "title":"Selection disappears", "body":body,
    })
}

fn action(registry: &App, workspace: &Workspace, action: Action) -> Result<()> {
    client_action(
        registry,
        workspace,
        Some(&SettingsPrincipal::paired_device_fixture()),
        action,
    )
}

fn review(registry: &App, workspace: &Workspace, draft: &Preview, send: bool) -> Result<()> {
    action(
        registry,
        workspace,
        Action::Decide {
            revision: draft.revision.clone(),
            send,
        },
    )
}

fn draft(app: &Workspace, thread: &str, args: &Value) -> Result<Preview> {
    assert_eq!(
        Tool::Draft.call(app, thread, args)?["state"],
        "awaiting_desktop_confirmation"
    );
    let registry = app.registry.upgrade().unwrap();
    Ok(registry
        .workspace_requests
        .view(&app.id)
        .drafts
        .into_iter()
        .find(|draft| draft.session == thread)
        .unwrap())
}

#[tokio::test]
async fn full_approval_round_trip_pins_the_reviewed_payload_and_keeps_routes_private() -> Result<()>
{
    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let thread = source.hub.edit_threads(|threads| threads.create(None))?;
    let marker = "private-research-source-marker-92a4";
    source.hub.publish(ServerEvent::TextDelta {
        session: thread.clone(),
        item: "private".into(),
        actor: Actor::User,
        text: marker.into(),
    });
    source.hub.set_full_approval(
        FullApprovalChange::Full {
            duration: FullApprovalDuration::UntilDisabled,
        },
        &SettingsPrincipal::paired_device_fixture(),
    )?;
    assert!(source.hub.full_approval_settings(None)?.active.is_some());

    let body = "Observed: opening an empty list clears the selection.\nExpected: keep the current selection.";
    let original = draft(&source, &thread, &send_draft(&receiver, "Old report"))?;
    assert_eq!(
        draft(&source, &thread, &send_draft(&receiver, "Old report"))?.revision,
        original.revision
    );
    let revised = draft(&source, &thread, &send_draft(&receiver, body))?;
    assert_ne!(original.revision, revised.revision);
    assert!(review(&registry, &source, &original, true).is_err());
    assert!(review(&registry, &receiver, &revised, true).is_err());
    for principal in [None, Some(SettingsPrincipal::fixture())] {
        assert!(
            client_action(
                &registry,
                &source,
                principal.as_ref(),
                Action::Decide {
                    revision: revised.revision.clone(),
                    send: true
                }
            )
            .is_err()
        );
    }
    let mut self_confirm = send_draft(&receiver, body);
    self_confirm["confirm"] = json!(true);
    assert!(Tool::Draft.call(&source, &thread, &self_confirm).is_err());
    assert!(
        registry
            .workspace_requests
            .view(&receiver.id)
            .inbox
            .is_empty()
    );
    review(&registry, &source, &revised, false)?;
    assert!(review(&registry, &source, &revised, true).is_err());

    let preview = draft(&source, &thread, &send_draft(&receiver, body))?;
    drop((source, receiver, registry));
    let registry = fixture.open()?;
    fixture.install_desk(&registry);
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let store = &registry.workspace_requests;
    assert_eq!(store.view(&source.id).drafts[0].revision, preview.revision);
    review(&registry, &source, &preview, true)?;
    review(&registry, &source, &preview, true)?;
    assert!(store.view(&source.id).drafts.is_empty());
    assert!(store.ready().is_empty());
    let view = store.view(&receiver.id);
    assert_eq!(view.inbox.len(), 1);
    let inbox = &view.inbox[0];
    assert_eq!(inbox.body, body);
    assert!(inbox.session.is_none());
    assert!(matches!(
        store.view(&source.id).updates[0].kind,
        UpdateKind::Received
    ));
    let serialized = serde_json::to_string(&view)?;
    assert!(!serialized.contains(marker) && !serialized.contains(&thread));
    assert!(!serialized.contains(&preview.revision));
    assert!(!receiver.hub.has_thread(&thread));
    assert!(receiver.hub.full_approval_settings(None)?.active.is_none());

    let start = Action::Start {
        id: inbox.id.clone(),
    };
    assert!(action(&registry, &source, start.clone()).is_err());
    action(&registry, &receiver, start.clone())?;
    action(&registry, &receiver, start)?;
    let [route] = store.ready().try_into().expect("one queued start");
    start_one(&registry, &route).await?;
    assert!(store.ready().is_empty());
    let started = store.view(&receiver.id).inbox[0].session.clone().unwrap();
    assert_eq!(started, route.thread);
    let prompt = fixture.receiver.join("prompt-fixture");
    wait_for_path(&prompt).await?;
    let context = fs::read_to_string(prompt)?;
    assert!(context.contains("opening an empty list clears the selection"));
    assert!(context.contains("quoted external data") || context.contains("引用した外部資料"));
    assert!(!context.contains(marker) && !context.contains(&thread));
    assert!(matches!(
        store.view(&source.id).updates[1].kind,
        UpdateKind::Started
    ));

    let source_id = Tool::List.call(&source, &thread, &json!({}))?["requests"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(source_id, inbox.id);
    assert_eq!(
        Tool::List.call(&receiver, &started, &json!({}))?["requests"][0]["id"],
        inbox.id
    );
    let unrelated = receiver.hub.edit_threads(|threads| threads.create(None))?;
    assert!(
        Tool::Draft
            .call(
                &receiver,
                &unrelated,
                &json!({
                    "action":"reply","id":inbox.id,"kind":"question","body":"Which version?",
                })
            )
            .is_err()
    );
    for (app, session, id, kind, text) in [
        (
            &receiver,
            &started,
            &inbox.id,
            "question",
            "Which version is affected?",
        ),
        (&source, &thread, &source_id, "answer", "Version 4."),
        (
            &receiver,
            &started,
            &inbox.id,
            "result",
            "Fixed and verified with an empty-list case.",
        ),
    ] {
        let before = store.view(&source.id).updates.len();
        let reply = draft(
            app,
            session,
            &json!({"action":"reply","id":id,"kind":kind,"body":text}),
        )?;
        assert_eq!(store.view(&source.id).updates.len(), before);
        review(&registry, app, &reply, true)?;
        review(&registry, app, &reply, true)?;
        let view = store.view(&source.id);
        assert_eq!(view.updates.len(), before + 1);
        assert_eq!(view.updates.last().unwrap().session, thread);
        assert_eq!(view.updates.last().unwrap().body.as_deref(), Some(text));
    }
    assert!(store.view(&receiver.id).inbox[0].complete);
    assert!(fixture.state.join("workspace-requests.json").is_file());
    assert!(!fixture.source.join("workspace-requests.json").exists());
    assert!(!fixture.receiver.join("workspace-requests.json").exists());
    let recovered = Store::open(&fixture.state)?;
    recovered.decide(&registry, &source, &preview.revision, true)?;
    assert_eq!(recovered.view(&source.id).updates.len(), 5);
    assert_eq!(recovered.view(&receiver.id).inbox.len(), 1);
    assert!(recovered.view(&receiver.id).inbox[0].complete);
    Ok(())
}

#[tokio::test]
async fn receiver_policy_and_interrupted_start_survive_restart() -> Result<()> {
    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let thread = source.hub.edit_threads(|threads| threads.create(None))?;
    action(&registry, &receiver, Action::SetAutoStart { enabled: true })?;
    let preview = draft(
        &source,
        &thread,
        &send_draft(&receiver, "A synthetic report"),
    )?;
    review(&registry, &source, &preview, true)?;
    let store = Store::open(&fixture.state)?;
    assert!(store.view(&receiver.id).auto_start);
    assert!(!store.view(&source.id).auto_start);
    let [route] = store.ready().try_into().expect("one automatic start");
    store.intake(&route, Intake::Ready, Intake::Launching)?;
    drop(store);
    let store = Store::open(&fixture.state)?;
    assert!(store.ready().is_empty());
    let task = &store.view(&receiver.id).inbox[0];
    assert!(task.failed);
    assert_eq!(task.session.as_deref(), Some(route.thread.as_str()));
    Ok(())
}

#[test]
fn protocol_excludes_android_browsers_and_older_clients() {
    let current = fuzukue_proto::WORKSPACE_REQUEST_RECOVERY_PROTOCOL;
    for (protocol, platform, native, paired, expected) in [
        (WORKSPACE_REQUESTS_PROTOCOL - 1, "windows-x86_64", true, true, ClientSupport::OldProtocol),
        (current, "android", true, true, ClientSupport::Android),
        (current, "linux-x86_64", false, true, ClientSupport::Browser),
        (current, "linux-x86_64", true, false, ClientSupport::UnpairedDesktop),
        (WORKSPACE_REQUESTS_PROTOCOL, "linux-x86_64", true, true, ClientSupport::LegacyDesktop),
        (current, "linux-x86_64", true, true, ClientSupport::Desktop),
    ] {
        assert!(ClientSupport::from_connection(protocol, Some(platform), native, paired) == expected);
    }
}

#[tokio::test]
async fn websocket_draft_discovery_reports_the_open_surface_and_survives_restart() -> Result<()> {
    use axum::{Extension, extract::ConnectInfo};
    use fuzukue_proto::{ClientMsg, Envelope, WORKSPACE_REQUEST_RECOVERY_PROTOCOL};
    use futures_util::{SinkExt, StreamExt};
    use tokio::{io::DuplexStream, sync::mpsc, time::timeout};
    use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::{Message, client::IntoClientRequest}};

    type Socket = WebSocketStream<DuplexStream>;
    async fn next_event(socket: &mut Socket, wanted: fn(&ServerEvent) -> bool) -> Result<Envelope> {
        timeout(Duration::from_secs(10), async {
            while let Some(message) = socket.next().await {
                let message = message?;
                if message.is_text() {
                    let env: Envelope = serde_json::from_str(message.to_text()?)?;
                    if wanted(&env.event) {
                        return Ok(env);
                    }
                }
            }
            bail!("The test socket closed before the expected event")
        }).await?
    }
    async fn send(socket: &mut Socket, msg: &ClientMsg) -> Result<()> {
        socket.send(Message::Text(serde_json::to_string(msg)?.into())).await?;
        Ok(())
    }
    async fn connect(
        incoming: &mpsc::Sender<DuplexStream>, token: Option<&str>,
        protocol: u32, platform: &str, workspace: &str,
    ) -> Result<Socket> {
        let (client, server) = tokio::io::duplex(128 * 1024);
        incoming.send(server).await?;
        let mut request = "ws://localhost/ws".into_client_request()?;
        if let Some(token) = token {
            request.headers_mut().insert("Authorization", format!("Bearer {token}").parse()?);
        }
        let (mut socket, _) = client_async(request, client).await?;
        let hello = json!({"type":"hello", "protocol":protocol, "platform":platform, "workspace":workspace});
        socket.send(Message::Text(hello.to_string().into())).await?;
        next_event(&mut socket, |event| matches!(event, ServerEvent::Workspaces { .. })).await?;
        Ok(socket)
    }
    fn serve(registry: Arc<App>, state: &Path) -> Result<(mpsc::Sender<DuplexStream>, tokio::task::JoinHandle<()>)> {
        let access = Arc::new(crate::remote::Access::new(state, Default::default())?);
        let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
        let router = crate::router(registry, access).layer(Extension(ConnectInfo(peer)));
        let (incoming, streams) = mpsc::channel(4);
        let server = tokio::spawn(async move {
            axum::serve(crate::integration_tests::MemoryListener(streams), router).await.unwrap();
        });
        Ok((incoming, server))
    }

    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let thread = source.hub.edit_threads(|threads| threads.create(None))?;
    let other_thread = source.hub.edit_threads(|threads| threads.create(None))?;
    let args = send_draft(&receiver, "Keep the draft across reconnects.");
    let draft_result = || Tool::Draft.call(&source, &thread, &args);
    let pending = || Ok::<_, anyhow::Error>(Tool::List.call(&source, &thread, &json!({}))?["pending_draft"].clone());
    let check = |confirmation: &Value, ready: bool, reasons: &[&str]| {
        assert_eq!(confirmation["can_show_confirmation_now"], ready);
        assert_eq!(confirmation["reasons"], json!(reasons));
        assert_eq!(confirmation["workspace"], source.id);
        assert_eq!(confirmation["thread"], thread);
        assert!(!confirmation["message"].as_str().unwrap().is_empty());
    };
    check(&draft_result()?["confirmation"], false, &["no_client_connected"]);
    assert_eq!(pending()?["body"], args["body"]);
    assert!(pending()?.get("revision").is_none());
    assert_eq!(Tool::List.call(&source, &other_thread, &json!({}))?["pending_draft"], Value::Null);
    assert_eq!(Tool::List.call(&source, &thread, &json!({}))?["requests"], json!([]));

    let token = crate::device_tokens::Store::new(&fixture.state)
        .add("review desktop", None, chrono::Utc::now().timestamp())?;
    let (incoming, server) = serve(registry.clone(), &fixture.state)?;
    let mut old = connect(&incoming, Some(&token), WORKSPACE_REQUESTS_PROTOCOL - 1, "windows-x86_64", &source.id).await?;
    let mut android = connect(&incoming, Some(&token), WORKSPACE_REQUEST_RECOVERY_PROTOCOL, "android", &source.id).await?;
    let mut unpaired = connect(&incoming, None, WORKSPACE_REQUEST_RECOVERY_PROTOCOL, "windows-x86_64", &source.id).await?;
    for socket in [&mut old, &mut android, &mut unpaired] {
        // A round trip proves these clients stayed connected. Inspect all startup events for leaks.
        send(socket, &ClientMsg::ListWorkspaces).await?;
        next_event(socket, |event| {
            assert!(!matches!(event, ServerEvent::WorkspaceRequestDrafts { .. } | ServerEvent::WorkspaceRequests { view: Some(_) }));
            matches!(event, ServerEvent::Workspaces { .. })
        }).await?;
    }
    check(&draft_result()?["confirmation"], false, &["android", "old_protocol", "pairing_required"]);
    drop((old, android, unpaired));
    timeout(Duration::from_secs(5), async {
        while !registry.workspace_requests.clients.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    }).await?;

    let mut legacy = connect(&incoming, Some(&token), WORKSPACE_REQUESTS_PROTOCOL, "windows-x86_64", &source.id).await?;
    let event = next_event(&mut legacy, |event| {
        assert!(!matches!(event, ServerEvent::WorkspaceRequestDrafts { .. }));
        matches!(event, ServerEvent::WorkspaceRequests { view: Some(_) })
    }).await?;
    let ServerEvent::WorkspaceRequests { view: Some(view) } = event.event else { unreachable!() };
    let revision = view.drafts[0].revision.clone();
    check(&pending()?["confirmation"], false, &["legacy_desktop"]);
    drop(legacy);
    timeout(Duration::from_secs(5), async {
        while !registry.workspace_requests.clients.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    }).await?;

    let mut desktop = connect(&incoming, Some(&token), WORKSPACE_REQUEST_RECOVERY_PROTOCOL, "windows-x86_64", &receiver.id).await?;
    let event = next_event(&mut desktop, |event| matches!(event, ServerEvent::WorkspaceRequestDrafts { complete: true, .. })).await?;
    assert!(matches!(event.event, ServerEvent::WorkspaceRequestDrafts { drafts, other_workspaces, .. }
        if drafts.is_empty() && other_workspaces.len() == 1 && other_workspaces[0].id == source.id));
    check(&pending()?["confirmation"], false, &["different_workspace"]);
    send(&mut desktop, &ClientMsg::SwitchWorkspace { id: source.id.clone() }).await?;
    let event = next_event(&mut desktop, |event| matches!(event, ServerEvent::WorkspaceRequestDrafts { complete: true, .. })).await?;
    assert_eq!(event.workspace.as_deref(), Some(source.id.as_str()));
    assert!(matches!(event.event, ServerEvent::WorkspaceRequestDrafts { drafts, .. }
        if drafts.len() == 1 && drafts[0].revision == revision));
    check(&pending()?["confirmation"], false, &["view_unreported"]);

    for (session, inbox_revision, ready, reasons) in [
        (Some(other_thread.as_str()), None, false, &["different_thread"][..]),
        (None, None, false, &["thread_view_hidden"][..]),
        (Some(thread.as_str()), None, true, &[][..]),
        (Some(other_thread.as_str()), Some(revision.as_str()), true, &[][..]),
    ] {
        send(&mut desktop, &ClientMsg::InWorkspace {
            id: source.id.clone(), request: Box::new(ClientMsg::WorkspaceRequestSurface {
                session: session.map(str::to_owned), inbox_revision: inbox_revision.map(str::to_owned),
            }),
        }).await?;
        send(&mut desktop, &ClientMsg::ListWorkspaces).await?;
        next_event(&mut desktop, |event| matches!(event, ServerEvent::Workspaces { .. })).await?;
        check(&pending()?["confirmation"], ready, reasons);
    }

    drop(desktop);
    timeout(Duration::from_secs(5), async {
        while !registry.workspace_requests.clients.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    }).await?;
    check(&pending()?["confirmation"], false, &["no_client_connected"]);
    let mut desktop = connect(&incoming, Some(&token), WORKSPACE_REQUEST_RECOVERY_PROTOCOL, "windows-x86_64", &source.id).await?;
    let event = next_event(&mut desktop, |event| matches!(event, ServerEvent::WorkspaceRequestDrafts { complete: true, .. })).await?;
    assert!(matches!(event.event, ServerEvent::WorkspaceRequestDrafts { drafts, .. }
        if drafts.len() == 1 && drafts[0].revision == revision));
    drop(desktop);
    server.abort();

    let restarted = fixture.open()?;
    let restored_source = restarted.select(Some(&source.id))?;
    let (incoming, server) = serve(restarted.clone(), &fixture.state)?;
    let mut desktop = connect(&incoming, Some(&token), WORKSPACE_REQUEST_RECOVERY_PROTOCOL, "windows-x86_64", &source.id).await?;
    let event = next_event(&mut desktop, |event| matches!(event, ServerEvent::WorkspaceRequestDrafts { complete: true, .. })).await?;
    assert!(matches!(event.event, ServerEvent::WorkspaceRequestDrafts { drafts, .. }
        if drafts.len() == 1 && drafts[0].revision == revision));
    send(&mut desktop, &ClientMsg::InWorkspace {
        id: source.id.clone(), request: Box::new(ClientMsg::WorkspaceRequest {
            action: Action::Decide { revision, send: true },
        }),
    }).await?;
    let event = next_event(&mut desktop, |event| matches!(event, ServerEvent::WorkspaceRequestDrafts { complete: true, .. })).await?;
    assert!(matches!(event.event, ServerEvent::WorkspaceRequestDrafts { drafts, .. } if drafts.is_empty()));
    assert_eq!(Tool::List.call(&restored_source, &thread, &json!({}))?["pending_draft"], Value::Null);
    assert_eq!(restarted.workspace_requests.view(&receiver.id).inbox.len(), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn normal_prompt_exposes_drafting_only_to_the_authenticated_front_desk() -> Result<()> {
    use axum::{
        Json,
        body::to_bytes,
        extract::{Query, State},
        http::{HeaderMap, StatusCode, header},
    };
    use fuzukue_proto::ClientMsg;

    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    fixture.install_desk(&registry);
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let thread = source.hub.edit_threads(|threads| threads.create(None))?;
    crate::handle_from_client(
        &source,
        ClientMsg::Prompt {
            session: thread.clone(),
            text: "Report this bug to fuzukue. An empty list clears the selection.".into(),
        },
        None,
    )
    .await?;
    wait_for_path(&fixture.source.join("prompt-fixture")).await?;
    let token = source.sessions.lock().await[&thread]
        .mcp_key()
        .unwrap()
        .to_owned();
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    let call = async |headers, name: &str, arguments| {
        crate::mcp::route(
            State(registry.clone()),
            Query(crate::mcp::McpQuery {
                session: thread.clone(),
                backend: None,
                workspace: Some(source.id.clone()),
                job: None,
            }),
            headers,
            Json(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":name, "arguments":arguments},
            })),
        )
        .await
    };
    let args = send_draft(
        &receiver,
        "Keep the current selection when opening an empty list.",
    );
    let response = call(HeaderMap::new(), "draft_workspace_request", args.clone()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = call(headers.clone(), "draft_workspace_request", args).await;
    let draft: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await?)?;
    assert_eq!(
        draft["result"]["structuredContent"]["state"],
        "awaiting_desktop_confirmation"
    );
    assert_eq!(
        draft["result"]["structuredContent"]["confirmation"]["reasons"],
        json!(["no_client_connected"])
    );
    assert!(
        draft["result"]["structuredContent"]
            .get("revision")
            .is_none()
    );
    let response = call(
        headers,
        "draft_workspace_request",
        json!({"action":"decide","send":true}),
    )
    .await;
    let denied: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await?)?;
    assert_eq!(denied["result"]["isError"], true);
    fs::write(fixture.source.join("draft-test-done"), "")?;
    assert!(
        registry
            .workspace_requests
            .view(&receiver.id)
            .inbox
            .is_empty()
    );
    assert_eq!(registry.workspace_requests.view(&source.id).drafts.len(), 1);
    Ok(())
}

#[test]
fn draft_snapshots_are_bounded_independently_of_request_history() -> Result<()> {
    use fuzukue_proto::{Envelope, WORKSPACE_REQUEST_RECOVERY_PROTOCOL};

    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    let source = registry.select(None)?;
    let receiver = fixture.receiver(&registry)?;
    let thread = source.hub.edit_threads(|threads| threads.create(None))?;
    let initial = draft(&source, &thread, &send_draft(&receiver, "Original request."))?;
    review(&registry, &source, &initial, true)?;
    let pending = draft(&source, &thread, &send_draft(&receiver, "Review this later."))?;
    let store = &registry.workspace_requests;
    {
        // Model many valid replies, enough to overflow the 12 MiB history frame.
        let mut state = store.state.lock().unwrap();
        let old_body = "💬".repeat(1200);
        state.requests[0].replies = (0..2800).map(|sequence| Reply {
            revision: sequence.to_string(), kind: ReplyKind::Answer, body: old_body.clone(), sequence,
        }).collect();
        let other_drafts = (0..32).map(|index| {
            let mut draft = state.drafts[0].clone();
            draft.owner.thread = format!("another-thread-{index}");
            draft.preview.session = draft.owner.thread.clone();
            draft.preview.revision = index.to_string();
            draft.preview.body = "💬".repeat(4000);
            draft
        }).collect::<Vec<_>>();
        state.drafts.extend(other_drafts);
    }
    let on_wire = |event| -> Result<ServerEvent> {
        let frame = crate::wire_event(
            &crate::snapshot(event), WORKSPACE_REQUEST_RECOVERY_PROTOCOL,
            Some(&source.id), fuzukue_i18n::Locale::JAPANESE,
        );
        let axum::extract::ws::Message::Text(frame) = frame else { unreachable!() };
        Ok(serde_json::from_str::<Envelope>(frame.as_str())?.event)
    };
    assert!(matches!(on_wire(store.event(&source.id, true, false))?, ServerEvent::ResponseTooLarge { .. }));
    let [first, last] = store.draft_events(&registry, &source.id)
        .try_into().expect("33 pending drafts in two chunks");
    assert!(matches!(on_wire(first)?, ServerEvent::WorkspaceRequestDrafts { drafts, complete: false, .. }
        if drafts.len() == 32 && drafts[0].revision == pending.revision));
    assert!(matches!(on_wire(last)?, ServerEvent::WorkspaceRequestDrafts { drafts, complete: true, .. }
        if drafts.len() == 1));
    Ok(())
}
