use super::*;
use fuzukue_proto::{ClientMsg, Envelope};
use std::time::Duration;

use crate::test_support::TestDir;

#[test]
fn parses_only_a_leading_slash_and_unescapes_once() {
    assert_eq!(parse(" /help"), Input::Prompt(" /help"));
    assert_eq!(parse("hello /help"), Input::Prompt("hello /help"));
    assert_eq!(parse("//compact"), Input::Prompt("/compact"));
    assert_eq!(parse("///compact"), Input::Prompt("//compact"));
    assert_eq!(parse("/"), Input::Command { name: "", args: "" });
    assert_eq!(
        parse("/compact   keep decisions\n and files  "),
        Input::Command {
            name: "compact",
            args: "keep decisions\n and files",
        }
    );
    assert_eq!(
        parse("/unknown"),
        Input::Command {
            name: "unknown",
            args: ""
        }
    );
}

#[test]
fn both_backends_have_the_same_localized_inventory() {
    let expected = [
        "compact", "clear", "new", "model", "effort", "escalate", "archive", "pin", "goal", "pace", "join",
        "stop", "help",
    ];
    assert_eq!(
        COMMANDS.iter().map(|spec| spec.name).collect::<Vec<_>>(),
        expected
    );
    for backend in AgentBackend::ALL {
        for locale in [
            fuzukue_i18n::Locale::JAPANESE,
            fuzukue_i18n::Locale::supported("en").unwrap(),
        ] {
            let help = crate::l10n::render(
                locale,
                &LocalizedMessage::new("command-help", [("backend", backend.usage_key().into())]),
            );
            assert_eq!(help.lines().count(), expected.len());
            for spec in COMMANDS {
                assert!(spec.available(backend));
                assert_ne!(locale.text(spec.description), spec.description);
                assert!(help.contains(&spec.usage()));
            }
            for key in [
                "command-cleared",
                "command-unknown",
                "command-busy",
                "chat-wait-compacting",
            ] {
                assert_ne!(locale.text(key), key);
            }
        }
    }
}

type Events = tokio::sync::broadcast::Receiver<Envelope>;

fn workspace() -> (TestDir, TestDir, Workspace) {
    let project = TestDir::new();
    let state = TestDir::new();
    let app = Workspace::new(
        project.path().into(),
        "127.0.0.1:7878".into(),
        state.path().into(),
        Arc::new(crate::hub::Hub::new()),
    );
    (project, state, app)
}

fn thread(app: &Workspace) -> Result<String> {
    app.hub.edit_threads(|store| store.create(None))
}

async fn wait_event(
    events: &mut Events,
    matches: impl Fn(&ServerEvent) -> bool,
) -> Result<ServerEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await?.event;
            if matches(&event) {
                return Ok(event);
            }
        }
    })
    .await?
}

async fn wait_notice(events: &mut Events, key: &str) -> Result<String> {
    let event = wait_event(events, |event| {
        matches!(event, ServerEvent::LocalNotice { localized: Some(message), .. } if message.key == key)
    }).await?;
    let ServerEvent::LocalNotice { text, .. } = event else {
        unreachable!()
    };
    Ok(text)
}

async fn send(app: &Arc<Workspace>, session: &str, text: &str) -> Result<()> {
    crate::handle_from_client(
        app,
        ClientMsg::Prompt {
            session: session.into(),
            text: text.into(),
        },
        None,
    )
    .await
}

#[tokio::test]
async fn local_commands_keep_history_and_settings_and_never_start_a_model() -> Result<()> {
    let (project, state, app) = workspace();
    let app = Arc::new(app);
    let id = thread(&app)?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);

    send(&app, &id, "/xyz").await?;
    assert_eq!(
        wait_notice(&mut events, "command-unknown").await?,
        "未対応のコマンドです（/help で一覧）"
    );
    send(&app, &id, "/help").await?;
    assert!(wait_notice(&mut events, "command-help")
        .await?
        .contains("/compact"));
    send(&app, &id, "/stop").await?;
    wait_notice(&mut events, "command-idle").await?;
    let empty = thread(&app)?;
    send(&app, &empty, "/goal").await?;
    wait_notice(&mut events, "command-no-goal").await?;

    planner::write_test_plan(&app, &id)?;
    send(&app, &id, "/goal").await?;
    assert!(wait_notice(&mut events, "command-plan")
        .await?
        .contains("Resume test goal"));
    send(&app, &id, "/pace economy").await?;
    assert_eq!(planner::pace(&app, &id), Some(GoalPace::Economy));
    send(&app, &id, "/pace").await?;
    assert!(wait_notice(&mut events, "command-parallel-setting")
        .await?
        .contains("節約"));
    send(&app, &id, "/model claude-sonnet-5-5").await?;
    send(&app, &id, "/effort").await?;
    let settings = app.chat_settings(&id);
    assert_eq!(settings.model.as_deref(), Some("claude-sonnet-5-5"));
    assert!(settings.effort.is_none());

    crate::publish_prompt(&app.hub, &id, Actor::User, "A previous conversation");
    app.hub.publish(ServerEvent::TurnEnded {
        session: id.clone(),
        actor: Actor::Claude,
    });
    app.record_session(&id, |record| {
        record.claude_session_id = Some("prior-id".into())
    });
    send(&app, &id, "/clear").await?;
    assert_eq!(
        wait_notice(&mut events, "command-cleared").await?,
        "新しいセッション"
    );
    assert!(app.hub.recovery_summary(&id).is_none());
    assert!(planner::current_plan(&app, &id)?.is_some());
    assert!(app.session_records.lock().unwrap()[&id]
        .claude_session_id
        .is_none());
    let new = dispatch(&app, &id, "/new").await.context("new thread")?;
    assert_ne!(new, id);
    send(&app, &id, "/pin").await?;
    send(&app, &id, "/archive").await?;
    let ServerEvent::Threads { threads } = app.hub.read_threads(|store| store.snapshot()) else {
        unreachable!()
    };
    let view = threads.iter().find(|view| view.id == id).unwrap();
    assert!(view.pinned && view.archived);
    assert!(app.sessions.lock().await.is_empty());
    assert!(app.codex.lock().await.is_none());

    let restored = Workspace::new(
        project.path().into(),
        app.addr.clone(),
        state.path().into(),
        Arc::new(crate::hub::Hub::new()),
    );
    let (history, _) = restored.hub.subscribe_client(0, true);
    assert!(history.iter().any(|env| matches!(&env.event,
        ServerEvent::TextDelta { text, .. } if text == "A previous conversation")));
    assert!(history.iter().any(|env| matches!(&env.event,
        ServerEvent::LocalNotice { localized: Some(message), .. } if message.key == "command-cleared")));
    Ok(())
}

#[tokio::test]
async fn join_rewrites_only_a_segmented_thread() -> Result<()> {
    let project = TestDir::new();
    let shared = TestDir::new();
    // The segmented thread list sits above each workspace state directory.
    let state = shared.path().join("workspace");
    let app = Arc::new(Workspace::new(
        project.path().into(),
        "127.0.0.1:7878".into(),
        state.clone(),
        Arc::new(crate::hub::Hub::new()),
    ));
    let single = thread(&app)?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    send(&app, &single, "/join").await?;
    wait_notice(&mut events, "command-not-segmented").await?;

    let id = crate::threads::segment_tests::segmented_workspace(project.path(), &app.hub, &state)?;
    let path = app.hub.thread_path(&id)?;
    assert!(std::fs::read_to_string(&path)?.starts_with("---\nformat: segmented-1\n"));
    let root = crate::threads::rollback_marker::root(&state);
    assert_eq!(crate::threads::rollback_marker::listed(&root)?.len(), 1);
    send(&app, &id, "/join").await?;
    wait_notice(&mut events, "command-joined").await?;
    let joined = std::fs::read_to_string(&path)?;
    assert!(joined.contains(&format!("id: {id}\n")) && !joined.contains("segmented-1"));
    assert!(joined.contains("Synthetic") && joined.contains("/join"));
    assert!(crate::threads::rollback_marker::listed(&root)?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn active_turn_queues_compaction_and_allows_help_and_stop() -> Result<()> {
    use tokio::process::Command;
    let (project, _state, mut app) = workspace();
    app.front_desk_nudge = false;
    let app = Arc::new(app);
    let id = thread(&app)?;
    app.record_session(&id, |record| {
        record.claude_session_id = Some("fake-session".into())
    });
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(r#"
while IFS= read -r input; do
    case "$input" in
        *'"content":"/compact Keep files"'*)
            while [ ! -f release-compaction ]; do sleep 0.01; done
            echo '{"type":"system","subtype":"compact_boundary","compact_metadata":{"pre_tokens":123,"post_tokens":41}}'
            echo '{"type":"result","is_error":false}' ;;
        *'"subtype":"interrupt"'*)
            echo '{"type":"result","is_error":true,"result":"Interrupted"}' ;;
        *'\n/literal"'*)
            echo '{"type":"assistant","message":{"id":"literal","content":[{"type":"text","text":"literal received"}]}}' ;;
        *'next question'*)
            echo '{"type":"assistant","message":{"id":"next","content":[{"type":"text","text":"Prompt replayed"}]}}'
            echo '{"type":"result","is_error":false}' ;;
        *) echo '{"type":"result","is_error":true,"result":"Unexpected test input"}' ;;
    esac
done
"#);
    let claude = crate::ClaudeSession::spawn_command(
        cmd,
        id.clone(),
        project.path().into(),
        &app,
        Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(id.clone(), claude);
    let (_, mut events) = app.hub.subscribe(u64::MAX);

    send(&app, &id, "//literal").await?;
    wait_event(&mut events, |event| matches!(event, ServerEvent::TextDelta { actor: Actor::Claude, text, .. } if text == "literal received")).await?;
    for command in [
        "/archive", "/clear", "/new", "/pin", "/goal", "/pace",
    ] {
        send(&app, &id, command).await?;
        wait_notice(&mut events, "command-busy").await?;
    }
    for command in ["/model", "/effort"] {
        send(&app, &id, command).await?;
        wait_notice(&mut events, "command-value").await?;
    }
    send(&app, &id, "/help").await?;
    wait_notice(&mut events, "command-help").await?;
    send(&app, &id, "/compact Keep files").await?;
    wait_notice(&mut events, "command-queued").await?;
    assert!(app.hub.compaction(&id).is_none());
    crate::replay_prompts(&app).await;
    assert!(app.hub.compaction(&id).is_none());
    send(&app, &id, "/stop").await?;
    wait_notice(&mut events, "command-stopped").await?;
    wait_event(
        &mut events,
        |event| matches!(event, ServerEvent::TurnEnded { session, .. } if session == &id),
    )
    .await?;
    crate::replay_prompts(&app).await;
    assert!(app.hub.compaction(&id).is_some());
    send(&app, &id, "next question").await?;
    assert!(crate::has_held_prompt(&app, &id)?);
    std::fs::write(project.path().join("release-compaction"), "")?;
    assert_eq!(
        wait_notice(&mut events, "command-compacted-counts").await?,
        "文脈を圧縮しました（前 123 → 後 41 トークン）"
    );
    crate::replay_prompts(&app).await;
    wait_event(
        &mut events,
        |event| matches!(event, ServerEvent::TextDelta { text, .. } if text == "Prompt replayed"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn high_context_notice_is_once_per_backend_session_even_after_reload() -> Result<()> {
    let (project, state, app) = workspace();
    let id = thread(&app)?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let high = |app: &Workspace, used| {
        app.session_stats.update(&app.hub, &id, "claude", |stats| {
            stats.context_tokens = Some(used);
            stats.context_window = Some(100);
        });
    };
    app.session_stats
        .identify(&id, AgentBackend::Claude, "initial");
    high(&app, 80);
    assert!(app
        .hub
        .read_threads(|store| !store.has_item(&id, "context-notice:claude:initial")));
    high(&app, 81);
    assert_eq!(
        wait_notice(&mut events, "command-context-high").await?,
        "文脈が 80% を超えました。/compact で圧縮できます"
    );
    high(&app, 79);
    high(&app, 95);
    assert!(!matches!(
        events.try_recv()?.event,
        ServerEvent::LocalNotice { .. }
    ));
    assert!(!matches!(
        events.try_recv()?.event,
        ServerEvent::LocalNotice { .. }
    ));
    assert!(events.try_recv().is_err());

    let restored = Workspace::new(
        project.path().into(),
        app.addr.clone(),
        state.path().into(),
        Arc::new(crate::hub::Hub::new()),
    );
    let (_, mut live) = restored.hub.subscribe(u64::MAX);
    restored
        .session_stats
        .identify(&id, AgentBackend::Claude, "initial");
    high(&restored, 90);
    assert!(matches!(
        live.try_recv()?.event,
        ServerEvent::SessionStats { .. }
    ));
    assert!(live.try_recv().is_err());
    restored
        .session_stats
        .identify(&id, AgentBackend::Claude, "replacement");
    high(&restored, 85);
    wait_notice(&mut live, "command-context-high").await?;
    Ok(())
}

#[cfg(unix)]
async fn codex_fixture(app: &Workspace, supports_compaction: bool) -> Result<()> {
    let mut process = tokio::process::Command::new("python3");
    process.arg("-u").arg("-c").arg(r#"
import json, sys
def emit(**args):
    print(json.dumps(args))
def event(method, **params):
    emit(method=method, params=params)
def turn(thread, **fields):
    return dict(threadId=thread, turn=fields)
for line in sys.stdin:
    req = json.loads(line)
    if "id" not in req:
        continue
    request = req["id"]
    method = req["method"]
    params = req.get("params") or {}
    result = {}
    if method == "model/list":
        result = {"data":[{"id":"gpt-slash-fixture","model":"gpt-slash-fixture","displayName":"Fixture","supportedReasoningEfforts":[{"reasoningEffort":"high","description":"High"}]}]}
    if method in ("thread/start", "thread/resume"):
        thread = params.get("threadId") or "thread-" + str(request)
        result = {"thread": {"id": thread}}
    if method == "thread/compact/start":
        assert set(params) == {"threadId"}
        thread = params["threadId"]
        assert thread == "saved-codex"
        if sys.argv[1] == "false":
            emit(id=request, error={"code":-32601,"message":"Method not found"})
            continue
        emit(id=request, result={})
        event("turn/started", **turn(thread, id="compact", status="inProgress"))
        event("item/completed", threadId=thread, turnId="compact", item={"id":"compaction","type":"contextCompaction"})
        event("thread/tokenUsage/updated", threadId=thread, turnId="compact",
              tokenUsage={"last":{"inputTokens":40},"modelContextWindow":200})
        event("turn/completed", **turn(thread, id="compact", status="completed"))
        continue
    if method == "turn/start":
        thread = params["threadId"]
        text = params["input"][-1]["text"]
        if "outputSchema" in params:
            assert req["params"]["outputSchema"]["required"] == ["summary"]
            assert "Previous conversation text" in text
            text = json.dumps({"summary":"Decisions\nPreserve the decision to use a fixture."})
        else:
            assert "/compact" not in text
            assert "Preserve the decision to use a fixture." in text
            text = "New session received the summary."
        emit(id=request, result={"turn": {"id": "user-turn"}})
        event("turn/started", **turn(thread, id="user-turn", status="inProgress"))
        event("item/completed", threadId=thread, turnId="user-turn",
              item={"id":"answer","type":"agentMessage","text":text,"phase":"final_answer"})
        event("turn/completed", **turn(thread, id="user-turn", status="completed"))
        continue
    emit(id=request, result=result)
"#).arg(supports_compaction.to_string());
    let codex = crate::Codex::spawn_command(
        process,
        &app.project_dir,
        app.hub.clone(),
        app.usage.clone(),
        app.session_stats.clone(),
    )
    .await?;
    app.codex.lock().await.replace(codex);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn codex_native_compaction_and_summary_fallback() -> Result<()> {
    for native in [true, false] {
        let (_project, _state, app) = workspace();
        codex_fixture(&app, native).await?;
        let app = Arc::new(app);
        let id = thread(&app)?;
        let (_, mut events) = app.hub.subscribe(u64::MAX);
        send(&app, &id, "/model gpt-slash-fixture").await?;
        send(&app, &id, "/effort high").await?;
        assert_eq!(app.chat_settings(&id).effort.as_deref(), Some("high"));
        crate::publish_prompt(&app.hub, &id, Actor::User, "Previous conversation text");
        app.hub.publish(ServerEvent::TurnEnded {
            session: id.clone(),
            actor: Actor::Claude,
        });
        app.record_session(&id, |record| {
            record.codex_thread_id = Some("saved-codex".into());
            record.last_partner_codex = true;
        });
        app.session_stats.update(&app.hub, &id, "codex", |stats| {
            stats.context_tokens = Some(123)
        });

        send(&app, &id, "/compact").await?;
        if native {
            assert_eq!(
                wait_notice(&mut events, "command-compacted-counts").await?,
                "文脈を圧縮しました（前 123 → 後 40 トークン）"
            );
            assert_eq!(
                app.session_records.lock().unwrap()[&id]
                    .codex_thread_id
                    .as_deref(),
                Some("saved-codex")
            );
        } else {
            wait_notice(&mut events, "command-compacted").await?;
            let record = app.session_records.lock().unwrap()[&id].clone();
            assert!(record.codex_thread_id.is_none() && record.fresh_context);
            assert_eq!(record.previous_sessions[0].id, "saved-codex");
            assert_eq!(record.previous_sessions[0].backend, AgentBackend::Codex);
            assert!(record
                .context_seed
                .unwrap()
                .contains("Preserve the decision"));
            // The completion notice and admission state are published together by the job.
            while app.hub.compaction(&id).is_some() {
                tokio::task::yield_now().await;
            }
            send(&app, &id, "A fresh question").await?;
            wait_event(&mut events, |event| {
                matches!(event, ServerEvent::TextDelta { text, .. }
                if text == "New session received the summary.")
            })
            .await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn goal_text_uses_the_planner() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let (project, _state, mut app) = workspace();
    let program = project.path().join("claude-planner");
    std::fs::write(
        &program,
        r#"#!/usr/bin/python3
import json, sys
evidence = json.loads(sys.argv[-1])
assert evidence["plan"]["goal"] == "Resume test goal"
assert evidence["reason"] == "Check the fixtures first"
print(json.dumps({"status":"done","done_condition":"Fixtures checked","verification":"Read the fixture report"}))
"#,
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    app.claude_program = Some(program);
    app.agent_settings.lock().unwrap().overrides.insert(
        "planner".into(),
        fuzukue_proto::AgentSelection::from_model(Some("claude-sonnet-5-5".into()), None),
    );
    let app = Arc::new(app);
    let id = thread(&app)?;
    planner::write_test_plan(&app, &id)?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    send(&app, &id, "/goal Check the fixtures first").await?;
    assert!(wait_notice(&mut events, "command-plan")
        .await?
        .contains("Fixtures checked"));
    Ok(())
}

#[test]
fn old_clients_receive_notices_as_system_text_on_live_and_restore() -> Result<()> {
    let local = ServerEvent::LocalNotice {
        session: "thread-fixture".into(),
        item: "notice".into(),
        text: "新しいセッション".into(),
        localized: Some(LocalizedMessage::new("command-cleared", [])),
    };
    for protocol in [
        fuzukue_proto::SLASH_COMMAND_PROTOCOL - 1,
        fuzukue_proto::SLASH_COMMAND_PROTOCOL,
    ] {
        let encode = |event| -> Result<ServerEvent> {
            let env = Envelope {
                event,
                workspace: None,
                seq: 1,
            };
            let wire = crate::wire_event(
                &env,
                protocol,
                None,
                fuzukue_i18n::Locale::supported("en").unwrap(),
            );
            let axum::extract::ws::Message::Text(wire) = wire else {
                unreachable!()
            };
            Ok(serde_json::from_str::<Envelope>(&wire)?.event)
        };
        let assert_notice = |event: ServerEvent| {
            if protocol == fuzukue_proto::SLASH_COMMAND_PROTOCOL {
                assert!(
                    matches!(event, ServerEvent::LocalNotice { text, .. } if text == "New session")
                );
            } else {
                assert!(
                    matches!(event, ServerEvent::TextDelta { actor: Actor::System, text, .. } if text == "New session")
                );
            }
        };
        assert_notice(encode(local.clone())?);
        let ServerEvent::ThreadRestored { mut events, .. } = encode(ServerEvent::ThreadRestored {
            session: "thread-fixture".into(),
            events: vec![local.clone()],
        })?
        else {
            unreachable!()
        };
        assert_notice(events.pop().unwrap());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn claude_without_native_compaction_uses_the_read_only_summary() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let (project, state, mut app) = workspace();
    let summary = project.path().join("claude-summary");
    std::fs::write(
        &summary,
        r#"#!/bin/sh
IFS= read -r input
case "$input" in
    *'Keep the checklist'*'Earlier Claude conversation'*)
        echo '{"type":"result","is_error":false,"result":"Decisions: keep the checklist."}' ;;
    *) echo '{"type":"result","is_error":true,"result":"unexpected input"}' ;;
esac
"#,
    )?;
    std::fs::set_permissions(&summary, std::fs::Permissions::from_mode(0o755))?;
    app.claude_program = Some(summary);
    let app = Arc::new(app);
    let id = thread(&app)?;
    crate::publish_prompt(&app.hub, &id, Actor::User, "Earlier Claude conversation");
    app.hub.publish(ServerEvent::TurnEnded {
        session: id.clone(),
        actor: Actor::Claude,
    });
    app.record_session(&id, |record| {
        record.claude_session_id = Some("older-claude".into())
    });

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let mut cli = tokio::process::Command::new("sh");
    cli.arg("-c").arg(
        r#"
echo '{"type":"system","subtype":"init","session_id":"older-claude","slash_commands":[]}'
if IFS= read -r input; then
    printf '%s\n' "$input" > unexpected-input
    echo '{"type":"result","is_error":true,"result":"native compact was called"}'
fi
"#,
    );
    let session = crate::ClaudeSession::spawn_command(
        cli,
        id.clone(),
        project.path().into(),
        &app,
        Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(id.clone(), session);
    wait_event(&mut events, |event| {
        matches!(
            event,
            ServerEvent::SessionStats {
                connected: Some(true),
                ..
            }
        )
    })
    .await?;
    send(&app, &id, "/compact Keep the checklist").await?;
    wait_notice(&mut events, "command-compacted").await?;
    assert!(!project.path().join("unexpected-input").exists());

    let saved = crate::state::load_sessions(state.path());
    let record = &saved[&id];
    assert!(record.fresh_context && record.claude_session_id.is_none());
    assert_eq!(record.previous_sessions[0].id, "older-claude");
    assert_eq!(
        record.context_seed.as_deref(),
        Some("Decisions: keep the checklist.")
    );
    Ok(())
}
