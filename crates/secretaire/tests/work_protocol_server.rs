use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use fuzukue_proto::{Actor, ChatSettings, ClientMsg, Envelope, PermissionMode, ServerEvent};
use secretaire::work_protocol::{Route, Stream};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const WAIT: Duration = Duration::from_secs(25);

const CLAUDE: &str = r#"#!/usr/bin/env python3
import json
import sys
import urllib.request

arguments = sys.argv
if "--mcp-config" not in arguments:
    sys.exit(0)
config = json.loads(arguments[arguments.index("--mcp-config") + 1])
mcp = next(iter(config["mcpServers"].values()))
for sequence, line in enumerate(sys.stdin, 1):
    message = json.loads(line)
    if message.get("type") != "user":
        continue
    content = message["message"]["content"]
    text = content if isinstance(content, str) else "\n".join(
        part.get("text", "") for part in content if part.get("type") == "text"
    )
    line = next((line for line in text.splitlines() if line.startswith("work-protocol-test ")), None)
    if line is None:
        continue
    args = json.loads(line.split(" ", 1)[1])
    request = urllib.request.Request(
        mcp["url"],
        headers={**mcp["headers"], "Content-Type": "application/json", "Accept": "application/json"},
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "watch", "arguments": args}}).encode(),
    )
    try:
        with urllib.request.urlopen(request, timeout=22) as response:
            result = json.load(response)["result"]
    except Exception as error:
        result = {"fixture_error": str(error)}
    print(json.dumps({"type": "assistant", "message": {"id": "fixture-" + str(sequence),
        "content": [{"type": "text", "text": "fixture-result " + json.dumps(result)}]}}), flush=True)
    print(json.dumps({"type": "result", "is_error": False,
        "session_id": "11111111-1111-4111-8111-111111111111"}), flush=True)
"#;

struct Workspace {
    project: PathBuf,
    state: PathBuf,
}

impl Workspace {
    fn new(root: &Path, name: &str) -> Result<Self> {
        let project = root.join(name);
        fs::create_dir(&project)?;
        git(&project, &["init", "--quiet"])?;
        let flat = project.to_string_lossy().trim_start_matches('/').replace('/', "%");
        let state = root.join("state").join("fuzukue").join(flat);
        fs::create_dir_all(&state)?;
        Ok(Self { project, state })
    }

    fn set_route(&self, route: &Route, writer: Stream, thread: &str) -> Result<()> {
        fs::write(self.state.join("work-protocol-v1.json"), serde_json::to_vec(&json!([
            {"name": "exchange", "route": route, "writer": writer, "thread": thread}
        ]))?)?;
        Ok(())
    }

    fn checkpoint(&self) -> PathBuf {
        self.state.join("work-protocol-v1/exchange/session.json")
    }
}

struct Server {
    child: Child,
    address: std::net::SocketAddr,
    log: PathBuf,
}

impl Server {
    fn start(root: &Path, workspace: &Workspace) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let log = root.join(format!("{}.log", workspace.project.file_name().unwrap().to_string_lossy()));
        let output = fs::File::create(&log)?;
        let inherited_path = std::env::var_os("PATH").context("PATH is missing")?;
        let path = std::env::join_paths(std::iter::once(root.to_owned()).chain(std::env::split_paths(&inherited_path)))?;
        let child = Command::new(env!("CARGO_BIN_EXE_secretaire"))
            .arg(&workspace.project)
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("PATH", path)
            .env("FUZUKUE_ADDR", address.to_string())
            .env("FUZUKUE_APPROVALS", "ask")
            .env("FUZUKUE_AUTO_BUILD", "0")
            .env_remove("FUZUKUE_SERVICE")
            .env_remove("FUZUKUE_SELF_UPDATE_READY")
            .env_remove("FUZUKUE_SELF_UPDATE_SOURCE")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(Self { child, address, log })
    }

    async fn connect(&mut self) -> Result<Client> {
        let url = format!("ws://{}/ws", self.address);
        let connection = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    anyhow::bail!("server exited with {status}\n{}", fs::read_to_string(&self.log)?);
                }
                if let Ok((socket, _)) = connect_async(&url).await {
                    return Ok(socket);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.context("connect to the server")??;
        let mut client = Client { socket: connection, workspace: String::new() };
        client.socket.send(Message::Text(json!({
            "type": "hello", "protocol": fuzukue_proto::PROTOCOL_VERSION, "thread_history": true,
            "selection_scoped_transcripts": true,
        }).to_string().into())).await?;
        client.workspace = client.until("workspace selection", |event| match event {
            ServerEvent::WorkspaceSelected { id, .. } => Some(id),
            _ => None,
        }).await?;
        Ok(client)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    workspace: String,
}

impl Client {
    async fn send(&mut self, request: ClientMsg) -> Result<()> {
        self.socket.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
        Ok(())
    }

    async fn until<T>(&mut self, label: &str, find: impl Fn(ServerEvent) -> Option<T>) -> Result<T> {
        tokio::time::timeout(WAIT, async {
            while let Some(message) = self.socket.next().await {
                let Message::Text(text) = message? else { continue };
                let event = serde_json::from_str::<Envelope>(&text)?.event;
                if let Some(found) = find(event) {
                    return Ok(found);
                }
            }
            anyhow::bail!("WebSocket closed");
        }).await.with_context(|| format!("waiting for {label}"))?
    }

    async fn new_thread(&mut self) -> Result<String> {
        self.send(ClientMsg::NewThread).await?;
        self.until("new conversation", |event| match event {
            ServerEvent::ThreadCreated { id } => Some(id),
            _ => None,
        }).await
    }

    async fn prompt(&mut self, session: &str, args: &Value) -> Result<()> {
        self.send(ClientMsg::Prompt { session: session.into(), text: format!("work-protocol-test {args}") }).await
    }

    async fn approval(&mut self, session: &str) -> Result<(String, String)> {
        self.until("publication approval", |event| match event {
            ServerEvent::ApprovalRequested { session: owner, request_id, summary, .. } if owner == session =>
                Some(Ok((request_id, summary))),
            ServerEvent::TextDelta { text, .. } if text.starts_with("fixture-result ") =>
                Some(Err(anyhow::anyhow!("publication returned before approval: {text}"))),
            _ => None,
        }).await?
    }

    async fn result(&mut self, session: &str) -> Result<Value> {
        let text = self.until("front desk MCP result", |event| match event {
            ServerEvent::TextDelta { session: owner, text, .. } if owner == session =>
                text.strip_prefix("fixture-result ").map(str::to_owned),
            _ => None,
        }).await?;
        self.until("front desk turn to finish", |event| match event {
            ServerEvent::TurnEnded { session: owner, .. } if owner == session => Some(()),
            _ => None,
        }).await?;
        Ok(serde_json::from_str(&text)?)
    }

    async fn history(&mut self, session: &str) -> Result<Vec<ServerEvent>> {
        self.send(ClientMsg::InWorkspace {
            id: self.workspace.clone(),
            request: Box::new(ClientMsg::GetThreadTail { session: session.into(), request: 81 }),
        }).await?;
        self.until("front desk transcript", |event| match event {
            ServerEvent::ThreadTail { request: 81, events, .. } => Some(Ok(events)),
            ServerEvent::ThreadHistoryFailed { request: 81, reason, .. } =>
                Some(Err(anyhow::anyhow!("transcript request failed: {reason:?}"))),
            _ => None,
        }).await?
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("git").current_dir(dir).args(args).output()?;
    ensure!(result.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&result.stderr));
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn tip(remote: &Path, branch: &str) -> Result<String> {
    git(remote, &["for-each-ref", "--format=%(objectname)", branch])
}

async fn wait_for(path: &Path, predicate: impl Fn(&Value) -> bool) -> Result<Value> {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(bytes) = fs::read(path) {
                let state: Value = serde_json::from_slice(&bytes)?;
                if predicate(&state) {
                    return Ok(state);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.with_context(|| format!("waiting for {}", path.display()))?
}

async fn boot_thread(root: &Path, workspace: &Workspace) -> Result<String> {
    let mut server = Server::start(root, workspace)?;
    let mut client = server.connect().await?;
    let session = client.new_thread().await?;
    client.prompt(&session, &json!({ "action": "protocol_routes" })).await?;
    assert_eq!(client.result(&session).await?["structuredContent"]["routes"], json!([]));
    assert!(!workspace.state.join("work-protocol-v1").exists());
    Ok(session)
}

fn publish_args(operation: &str, kind: &str, payload: Value) -> Value {
    json!({"action": "protocol_publish", "route": "exchange", "operation_id": operation,
        "event": {"type": kind, "payload": payload}})
}

async fn publish(
    client: &mut Client, session: &str, remote: &Path, branch: &str, args: &Value, decision: &str,
) -> Result<Value> {
    let before = tip(remote, branch)?;
    client.prompt(session, args).await?;
    let (request_id, summary) = client.approval(session).await?;
    assert!(summary.contains(remote.to_str().unwrap()) && summary.contains(branch));
    assert!(summary.contains(if before.is_empty() { "(new ref)" } else { &before }));
    assert_eq!(tip(remote, branch)?, before);
    client.send(ClientMsg::Resolve { request_id, decision: decision.into() }).await?;
    let result = client.result(session).await?;
    let after = tip(remote, branch)?;
    if decision == "accept" {
        assert_eq!(result["structuredContent"]["published"], true, "{result}");
        assert_ne!(after, before);
        assert!(summary.contains(&after));
    } else {
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(after, before);
    }
    Ok(result)
}

fn notes(events: &[ServerEvent], id: &str, body: &str) -> usize {
    events.iter().filter(|event| matches!(
        event, ServerEvent::TextDelta { actor: Actor::System, item, text, .. }
        if item.starts_with("watch:external:") && text.contains(id) && text.contains(body)
    )).count()
}

async fn wait_for_notes(client: &mut Client, session: &str, expected: &[(&str, &str)]) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            let events = client.history(session).await?;
            let counts: Vec<_> = expected.iter().map(|(id, body)| notes(&events, id, body)).collect();
            assert!(counts.iter().all(|count| *count <= 1), "duplicate notes: {counts:?}");
            if counts.iter().all(|count| *count == 1) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.with_context(|| format!("waiting for notes {expected:?}"))?
}

#[tokio::test]
async fn work_protocol_server_round_trip_replays_retained_receipts() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let claude = root.join("claude");
    fs::write(&claude, CLAUDE)?;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755))?;
    let remote = root.join("communication.git");
    git(&root, &["init", "--bare", "--quiet", remote.to_str().unwrap()])?;
    let desk = Workspace::new(&root, "desk")?;
    let worker = Workspace::new(&root, "worker")?;
    let desk_thread = boot_thread(&root, &desk).await?;
    let worker_thread = boot_thread(&root, &worker).await?;

    let route = Route::new("shared", remote.to_str().unwrap(), "general", "run-a", true)?;
    let desk_branch = route.branch(Stream::Desk);
    let worker_branch = route.branch(Stream::Worker);
    desk.set_route(&route, Stream::Desk, &desk_thread)?;
    worker.set_route(&route, Stream::Worker, &worker_thread)?;
    let mut desk_server = Server::start(&root, &desk)?;
    let mut worker_server = Server::start(&root, &worker)?;

    // These checkpoints require a production startup with no connected client.
    wait_for(&desk.checkpoint(), |state| state["outgoing"]["observed"] == true).await?;
    wait_for(&worker.checkpoint(), |state| state["outgoing"]["observed"] == true).await?;
    let mut desk_client = desk_server.connect().await?;
    let mut worker_client = worker_server.connect().await?;

    let assignment = publish_args("assign", "assignment", json!({
        "task_id": "sample", "repositories": ["worker"], "scope": "generic",
        "acceptance": ["plain Git"], "heartbeat_seconds": 30, "verification_policy": "local"
    }));
    desk_client.send(ClientMsg::SetChatSettings {
        session: desk_thread.clone(),
        settings: ChatSettings { permission_mode: PermissionMode::Plan, ..Default::default() },
    }).await?;
    desk_client.prompt(&desk_thread, &json!({"action": "protocol_routes"})).await?;
    let routes = desk_client.result(&desk_thread).await?;
    assert_eq!(routes["isError"], false, "{routes}");
    let before = tip(&remote, &desk_branch)?;
    desk_client.prompt(&desk_thread, &assignment).await?;
    let denied = desk_client.result(&desk_thread).await?;
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(denied["content"][0]["text"].as_str().unwrap().contains("active front desk turn with write access"));
    assert_eq!(tip(&remote, &desk_branch)?, before);
    desk_client.send(ClientMsg::SetChatSettings {
        session: desk_thread.clone(), settings: ChatSettings::default(),
    }).await?;
    publish(&mut desk_client, &desk_thread, &remote, &desk_branch, &assignment, "decline").await?;
    publish(&mut desk_client, &desk_thread, &remote, &desk_branch, &assignment, "accept").await?;
    wait_for_notes(&mut worker_client, &worker_thread, &[("run-a:desk:1", "plain Git")]).await?;

    let report = |id: &str, text: &str| publish_args(id, "report", json!({"summary": text}));
    let first = report("progress-1", "First progress update");
    publish(&mut worker_client, &worker_thread, &remote, &worker_branch, &first, "accept").await?;
    wait_for_notes(&mut desk_client, &desk_thread, &[("run-a:worker:1", "First progress update")]).await?;

    // Fail the front desk's durable acceptance while Git still has more updates.
    let watch = desk.state.join("watch-v1/watch.json");
    let saved = watch.with_extension("saved");
    fs::rename(&watch, &saved)?;
    fs::create_dir(&watch)?;
    let second = report("progress-2", "Second progress update");
    publish(&mut worker_client, &worker_thread, &remote, &worker_branch, &second, "accept").await?;
    let pending = wait_for(&desk.checkpoint(), |state| state["incoming"]["count"] == 3).await?;
    assert_eq!(pending["delivered_through"], 1);

    let third = report("progress-3", "Third progress update");
    publish(&mut worker_client, &worker_thread, &remote, &worker_branch, &third, "accept").await?;
    let pending = wait_for(&desk.checkpoint(), |state| state["incoming"]["count"] == 4).await?;
    assert_eq!(pending["delivered_through"], 1);
    assert_eq!(notes(&desk_client.history(&desk_thread).await?, "run-a:worker:3", "Second"), 0);

    drop(desk_client);
    drop(desk_server);
    drop(worker_client);
    drop(worker_server);
    fs::remove_dir(&watch)?;
    fs::rename(saved, &watch)?;
    let offline = root.join("offline.git");
    fs::rename(&remote, &offline)?;
    let expected = [
        ("run-a:worker:1", "First progress update"),
        ("run-a:worker:3", "Second progress update"),
        ("run-a:worker:4", "Third progress update"),
    ];

    let mut desk_server = Server::start(&root, &desk)?;
    wait_for(&desk.checkpoint(), |state| state["delivered_through"] == 4).await?;
    let mut desk_client = desk_server.connect().await?;
    wait_for_notes(&mut desk_client, &desk_thread, &expected).await?;
    drop(desk_client);
    drop(desk_server);

    // Recreate the crash window after 1a accepted the ID and before v1 saved
    // the delivery cursor. The next receipt handoff must be acknowledged once.
    let path = desk.checkpoint();
    let mut state: Value = serde_json::from_slice(&fs::read(&path)?)?;
    state["delivered_through"] = json!(1);
    fs::write(&path, serde_json::to_vec(&state)?)?;
    let mut desk_server = Server::start(&root, &desk)?;
    wait_for(&path, |state| state["delivered_through"] == 4).await?;
    let mut desk_client = desk_server.connect().await?;
    wait_for_notes(&mut desk_client, &desk_thread, &expected).await?;
    Ok(())
}
