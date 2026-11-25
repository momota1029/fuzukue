use std::{io, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Extension, extract::ConnectInfo};
use futures_util::{SinkExt, StreamExt};
use fuzukue_proto::{
    ApprovalDecision, ApprovalGrantScope, ApprovalOwner, ApprovalRisk, ApprovalState,
    ApprovalUnattended, ClientMsg, Envelope, PROTOCOL_VERSION, ServerEvent, WelcomeCapabilities,
};
use serde_json::{Value, json};
use tokio::{io::DuplexStream, sync::mpsc};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};

use super::super::approval_store::{Action, Registration};
use crate::{App, Hub, Workspace, remote, test_support::TestDir};

struct Listener(mpsc::Receiver<DuplexStream>);

impl axum::serve::Listener for Listener {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.recv().await {
            Some(stream) => (stream, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(())
    }
}

async fn connect(incoming: &mpsc::Sender<DuplexStream>) -> Result<WebSocketStream<DuplexStream>> {
    let (client, server) = tokio::io::duplex(128 * 1024);
    incoming.send(server).await?;
    let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
    // Omitting capability fields is wire-compatible with an existing Hello.
    socket
        .send(Message::Text(
            json!({
                "type": "hello", "protocol": PROTOCOL_VERSION, "resume_from": 0,
            })
            .to_string()
            .into(),
        ))
        .await?;
    let frame = socket.next().await.context("missing Welcome")??;
    let raw: Value = serde_json::from_str(frame.to_text()?)?;
    let capabilities: WelcomeCapabilities = serde_json::from_value(raw["event"].clone())?;
    assert!(capabilities.approval_queue);
    let welcome: Envelope = serde_json::from_value(raw)?;
    assert!(matches!(
        welcome.event,
        ServerEvent::Welcome {
            protocol: PROTOCOL_VERSION,
            ..
        }
    ));
    Ok(socket)
}

async fn send(socket: &mut WebSocketStream<DuplexStream>, msg: ClientMsg) -> Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(&msg)?.into()))
        .await?;
    Ok(())
}

async fn receive(
    socket: &mut WebSocketStream<DuplexStream>,
    filter: impl Fn(&ServerEvent) -> bool,
) -> Result<Envelope> {
    loop {
        let frame = socket.next().await.context("missing approval event")??;
        let env: Envelope = serde_json::from_str(frame.to_text()?)?;
        if filter(&env.event) {
            return Ok(env);
        }
    }
}

fn request(thread: &str, job: &str) -> Registration {
    Registration {
        thread_auto: false,
        owner: ApprovalOwner {
            thread: thread.into(),
            job: job.into(),
            requester: "worker".into(),
        },
        risk: ApprovalRisk::Low,
        unattended: ApprovalUnattended::DeferAndContinue,
        reason: "Fixture request".into(),
        expires_at: chrono::Utc::now().timestamp_millis() + 60_000,
        action: Action {
            tool: "fixture".into(),
            args: json!({"key": "raw-secret"}),
            scope: json!({}),
        },
        display_args: json!({"key": "[redacted]"}),
        display_scope: json!({}),
    }
}

#[tokio::test]
async fn pending_approval_queue_reconciles_on_websocket_reconnect() -> Result<()> {
    let project = TestDir::new();
    let state = TestDir::new();
    let app = Arc::new(Workspace::new(
        project.path().into(),
        "127.0.0.1:0".into(),
        state.path().into(),
        Arc::new(Hub::new()),
    ));
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let first = app.hub.register_approval(request(&thread, "job-1"))?;

    // Exercise the same HTTP upgrade and WebSocket frames without a loopback socket.
    let (incoming, connections) = mpsc::channel(1);
    let access = remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(App::fixture(app.clone()), Arc::new(access))
        .layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(Listener(connections), router).await });

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut legacy = connect(&incoming).await?;
        let mut client = connect(&incoming).await?;
        send(&mut client, ClientMsg::ListPendingApprovals { request: 1 }).await?;
        let env = receive(&mut client, |event| {
            matches!(event, ServerEvent::ApprovalQueue { request: 1, .. })
        })
        .await?;
        assert_eq!(env.workspace.as_deref(), Some(app.id.as_str()));
        let ServerEvent::ApprovalQueue {
            result: Ok(items), ..
        } = env.event
        else {
            anyhow::bail!("expected initial queue");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].approval_id, first);
        assert_eq!(items[0].action.args["key"], "[redacted]");
        drop(client);

        app.hub
            .decide_approval(&first, ApprovalDecision::Deny, "other-device", None)?;
        let second = app.hub.register_approval(request(&thread, "job-2"))?;
        let mut client = connect(&incoming).await?;
        send(&mut client, ClientMsg::ListPendingApprovals { request: 2 }).await?;
        let env = receive(&mut client, |event| {
            matches!(event, ServerEvent::ApprovalQueue { request: 2, .. })
        })
        .await?;
        let ServerEvent::ApprovalQueue {
            result: Ok(items), ..
        } = env.event
        else {
            anyhow::bail!("expected reconciled queue");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].approval_id, second);

        let third = app.hub.register_approval(request(&thread, "job-3"))?;
        let env = receive(&mut client, |event| {
            matches!(event, ServerEvent::ApprovalQueueChanged { .. })
        })
        .await?;
        assert!(
            matches!(env.event, ServerEvent::ApprovalQueueChanged { approval }
            if approval.approval_id == third && approval.state == ApprovalState::Pending)
        );

        send(
            &mut client,
            ClientMsg::DecideApproval {
                request: 3,
                approval_id: second.clone(),
                decision: ApprovalDecision::Grant {
                    scope: ApprovalGrantScope::Once,
                },
                reason: Some("verified".into()),
            },
        )
        .await?;
        let env = receive(&mut client, |event| {
            matches!(
                event,
                ServerEvent::ApprovalDecisionResult { request: 3, .. }
            )
        })
        .await?;
        assert!(
            matches!(env.event, ServerEvent::ApprovalDecisionResult { result: Ok(approval), .. }
            if approval.approval_id == second && approval.state == ApprovalState::Granted)
        );
        let env = receive(&mut client, |event| {
            matches!(event, ServerEvent::ApprovalQueueChanged { .. })
        })
        .await?;
        assert!(
            matches!(env.event, ServerEvent::ApprovalQueueChanged { approval }
            if approval.approval_id == second && approval.state == ApprovalState::Granted)
        );

        // A client which never requests the queue receives the regular feed without queue events.
        app.hub.publish(ServerEvent::FileChanged {
            path: "approval-test-fence".into(),
        });
        let env = receive(&mut legacy, |event| {
            matches!(
                event,
                ServerEvent::ApprovalQueue { .. }
                    | ServerEvent::ApprovalQueueChanged { .. }
                    | ServerEvent::ApprovalDecisionResult { .. }
            ) || matches!(event, ServerEvent::FileChanged { path } if path == "approval-test-fence")
        })
        .await?;
        assert!(matches!(env.event, ServerEvent::FileChanged { .. }));
        Ok::<_, anyhow::Error>(())
    })
    .await;
    server.abort();
    result?
}
