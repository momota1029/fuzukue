use super::*;
use fuzukue_proto::{AgentModelRole, AgentSelection, ChatSettings, RunDetails, Tier};
use std::{os::unix::fs::PermissionsExt, time::Duration};

fn workspace(project: &TempProject, state: &tempfile::TempDir) -> Workspace {
    Workspace::new(project.path().into(), "127.0.0.1:7878".into(), state.path().into(), Arc::new(Hub::new()))
}

async fn send(app: &Arc<Workspace>, session: &str, text: &str) -> Result<()> {
    handle(app, ClientMsg::Prompt { session: session.into(), text: text.into() }).await
}

async fn reply(
    events: &mut tokio::sync::broadcast::Receiver<Envelope>,
    session: &str,
) -> Result<RunDetails> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut details = None;
        loop {
            match events.recv().await?.event {
                ServerEvent::MessageDetails { session: id, details: runtime, .. } if id == session => {
                    details = Some(runtime);
                }
                ServerEvent::TurnEnded { session: id, actor } if id == session && crate::routing::is_partner(&actor) => {
                    return details.context("turn has no reply details");
                }
                ServerEvent::Error { message } => anyhow::bail!("{message}"),
                _ => {}
            }
        }
    }).await?
}

#[tokio::test]
async fn live_models_reconfigure_codex_and_revert_an_escalated_turn() -> Result<()> {
    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let app = Arc::new(workspace(&project, &state));
    let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Live models".into())))?;
    let script = state.path().join("fixture.py");
    let log = state.path().join("codex.jsonl");
    fs::write(&script, r#"import json, sys
class Server:
    def send(self, **value):
        print(json.dumps(value), flush=True)
    def notify(self, method, **params):
        self.send(method=method, params=dict(threadId="same-thread", **params))
server = Server()
with open(sys.argv[1], "w", buffering=1) as log:
    turn = 0
    effort = "medium"
    for line in sys.stdin:
        msg = json.loads(line)
        if "id" not in msg or "method" not in msg:
            continue
        log.write(line)
        method, params = msg["method"], msg.get("params") or {}
        result = {}
        if method == "model/list":
            result = {"data": [{"model": model, "defaultReasoningEffort": "medium",
                                "supportedReasoningEfforts": [{"reasoningEffort": e} for e in ["medium", "high", "xhigh"]]}
                               for model in ["gpt-model-a", "gpt-model-b"]]}
        elif method in ["thread/start", "thread/resume"]:
            result = {"thread": {"id": "same-thread"}, "model": params["model"], "reasoningEffort": effort}
        elif method == "turn/start":
            turn += 1
            effort = params.get("effort") or effort
            result = {"turn": {"id": str(turn)}}
        server.send(id=msg["id"], result=result)
        if method == "turn/start":
            server.notify("turn/started", turn={"id": str(turn)})
            item = {"type": "agentMessage", "phase": "final_answer", "id": f"reply-{turn}", "text": "Fixture reply"}
            server.notify("item/completed", item=item)
            server.notify("turn/completed", turn={"id": str(turn), "status": "completed"})
"#)?;
    let mut command = tokio::process::Command::new("/usr/bin/python3");
    command.arg(&script).arg(&log);
    let codex = Codex::spawn_command(command, &app.project_dir, app.hub.clone(), app.usage.clone(), app.session_stats.clone()).await?;
    *app.codex.lock().await = Some(codex);

    let model = |name: &str, effort: Option<&str>| {
        AgentSelection::from_model(Some(name.into()), effort.map(str::to_owned))
    };
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    app.set_partner_default(model("gpt-model-a", Some("high")).chat_settings()).await?;
    send(&app, &session, "Initial ordinary turn").await?;
    assert_eq!(reply(&mut events, &session).await?.selection, model("gpt-model-a", Some("high")));

    app.set_partner_default(model("gpt-model-b", None).chat_settings()).await?;
    send(&app, &session, "New workspace default").await?;
    assert_eq!(reply(&mut events, &session).await?.selection, model("gpt-model-b", Some("medium")));

    let explicit = model("gpt-model-a", None).chat_settings();
    handle(&app, ClientMsg::SetChatSettings { session: session.clone(), settings: explicit.clone() }).await?;
    app.set_agent_role_model(AgentModelRole::Escalation, Some(model("gpt-model-b", Some("xhigh")))).await?;
    send(&app, &session, "/escalate Think about this once").await?;
    let elevated = reply(&mut events, &session).await?;
    assert!(elevated.escalated);
    assert_eq!(elevated.selection, model("gpt-model-b", Some("xhigh")));
    assert_eq!(app.chat_settings(&session), explicit);

    send(&app, &session, "Ordinary follow-up").await?;
    let ordinary = reply(&mut events, &session).await?;
    assert!(!ordinary.escalated);
    assert_eq!(ordinary.selection, model("gpt-model-a", Some("medium")));

    let requests = fs::read_to_string(log)?;
    let requests: Vec<Value> = requests.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let turns: Vec<_> = requests.iter().filter(|call| call["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 4);
    assert!(turns.iter().all(|call| call["params"]["threadId"] == "same-thread"));
    assert_eq!(requests.iter().filter(|call| call["method"] == "thread/start").count(), 1);
    assert_eq!(turns[3]["params"]["effort"], "medium");

    app.agent_settings.lock().unwrap().escalation = Some(model("gpt-unavailable", None));
    send(&app, &session, "/escalate An unavailable role").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::LocalNotice { session: id, localized: Some(message), .. } = events.recv().await?.event {
                if id == session && message.key == "command-escalation-unavailable" {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    }).await??;
    assert!(!app.hub.thread_running(&session));
    Ok(())
}

#[tokio::test]
async fn live_models_resume_claude_and_reload_toml_for_existing_threads() -> Result<()> {
    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let mut app = workspace(&project, &state);
    let script = state.path().join("claude-fixture");
    let log = project.path().join("starts.jsonl");
    fs::write(&script, r#"#!/usr/bin/python3
import json, sys
def arg(flag):
    return sys.argv[sys.argv.index(flag)+1] if flag in sys.argv else None
def emit(kind, **fields):
    print(json.dumps(dict(type=kind, **fields)), flush=True)
model, resume, effort = arg("--model"), arg("--resume"), arg("--effort")
with open("starts.jsonl", "a") as log:
    log.write(json.dumps([model, resume, effort]) + "\n")
session = resume or "fixture-claude-session"
emit("system", subtype="init", session_id=session, model=model, effort=effort)
for turn, line in enumerate(sys.stdin):
    if json.loads(line).get("type") == "user":
        emit("assistant", message={"id": f"{model}-message-{turn}", "model": model,
            "content": [{"type": "text", "text": "Fixture reply"}]})
        emit("result", result="Fixture reply", session_id=session, is_error=False)
"#)?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    app.claude_program = Some(script);
    let app = Arc::new(app);
    let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Live Claude".into())))?;
    let config = state.path().join("fuzukue.toml");
    let first = "claude-sonnet-5-5";
    let second = fuzukue_proto::CLAUDE_OPUS;
    let set_default = |model| fs::write(&config, format!("[models.front_desk]\nmodel = '{model}'\n"));
    set_default(first)?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    send(&app, &session, "First Claude prompt").await?;
    assert_eq!(reply(&mut events, &session).await?.selection.model.as_deref(), Some(first));

    set_default(second)?;
    send(&app, &session, "Switch model without a new thread").await?;
    assert_eq!(reply(&mut events, &session).await?.selection.model.as_deref(), Some(second));
    let starts: Vec<Value> = fs::read_to_string(log)?.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    assert_eq!(starts, [json!([first, null, null]), json!([second, "fixture-claude-session", null])]);

    handle(&app, ClientMsg::SetChatSettings {
        session: session.clone(), settings: ChatSettings { model: Some(second.into()), ..Default::default() },
    }).await?;
    set_default(first)?;
    assert_eq!(app.partner_turn_settings(&session).model.as_deref(), Some(second));
    handle(&app, ClientMsg::UseDefaultThreadModel { session: session.clone() }).await?;
    assert_eq!(app.partner_turn_settings(&session).model.as_deref(), Some(first));
    Ok(())
}

#[tokio::test]
async fn live_models_resolve_delegate_override_agent_tier_and_default() -> Result<()> {
    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let app = Arc::new(workspace(&project, &state));
    stub_codex(&app, &state.path().join("worker.jsonl"), None).await?;
    let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Worker model routing".into())))?;
    let mut command = tokio::process::Command::new("cat");
    command.arg("/dev/stdin");
    let partner = crate::ClaudeSession::spawn_command(
        command, session.clone(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(session.clone(), partner);
    let call = |name, arguments| mcp_request_for(&app, &session, "tools/call", json!({"name": name, "arguments": arguments}));
    let worker_default = json!({"role":"worker","name":"gpt-6.2-sol","effort":"high"});
    let set = call("set_server_settings", json!({"model":worker_default})).await;
    assert_eq!(set["isError"], false, "{set}");
    let settings = call("get_server_settings", json!({"models":true})).await;
    assert_eq!(mcp_data(&settings)["models"]["worker"]["model"], "gpt-6.2-sol");

    let job_model = async |agent: Option<&str>, model: Option<&str>, effort: Option<&str>| {
        let result = call("delegate", json!({
            "prompt":"Verify the wording", "agent":agent, "model":model, "effort":effort, "tier":"standard", "touches":[]
        })).await;
        assert_eq!(result["isError"], false, "{result}");
        let id = mcp_data(&result)["job"].as_str().context("missing job ID")?;
        let job = call("job_status", json!({"job":id})).await;
        let job = mcp_data(&job);
        if effort.is_some() {
            assert_eq!(job["effort"].as_str(), effort);
        }
        let selected = job["model"].as_str().map(str::to_owned);
        app.delegates.interrupt_job(&session, id);
        Ok::<_, anyhow::Error>(selected)
    };
    assert_eq!(job_model(Some("prose"), None, None).await?.as_deref(), Some("gpt-6.2-sol"));
    let tier = AgentSelection::from_model(Some("claude-sonnet-5-5".into()), None);
    app.set_agent_role_model(AgentModelRole::WorkerTier(Tier::Standard), Some(tier)).await?;
    assert_eq!(job_model(Some("prose"), None, None).await?.as_deref(), Some("claude-sonnet-5-5"));
    app.set_agent_override(None, "prose".into(),
        Some(AgentSelection::from_model(Some(fuzukue_proto::CLAUDE_OPUS.into()), None))).await?;
    assert_eq!(job_model(Some("prose"), None, None).await?.as_deref(), Some(fuzukue_proto::CLAUDE_OPUS));
    assert_eq!(job_model(Some("prose"), Some("gpt-6.2-sol"), Some("high")).await?.as_deref(), Some("gpt-6.2-sol"));

    for arguments in [
        json!({"prompt":"Check text", "model":"gpt-unavailable"}),
        json!({"prompt":"Check text", "effort":"high"}),
        json!({"prompt":"Check text", "model":"gpt-6.2-sol", "effort":"not-supported"}),
    ] {
        assert_eq!(call("delegate", arguments).await["isError"], true);
    }
    Ok(())
}
