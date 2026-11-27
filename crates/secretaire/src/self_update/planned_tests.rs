use super::super::{drain, drain_tests::load, *};
use crate::{App, Workspace};
use fuzukue_proto::{Actor, GitHubPolicy, ServerEvent};

fn set_running(workspace: &Workspace, session: &str, running: bool) {
    workspace.hub.publish(ServerEvent::TurnActivity {
        session: session.into(),
        running,
    });
}

fn start_requester(workspace: &Workspace, session: &str) -> RequestingTurn {
    let proof = crate::mail::session_key();
    workspace.hub.publish_partner_turn_started(session, None, false, Some(proof.clone()));
    RequestingTurn::capture(workspace, session, &proof).unwrap()
}

#[tokio::test]
async fn a_lean_sweep_yields_to_a_planned_shutdown_and_resumes_after_cancel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let package = desk.project_dir.join("lean/A");
    fs::create_dir_all(desk.project_dir.join("cards"))?;
    fs::create_dir_all(&package)?;
    fs::write(desk.project_dir.join("deck.toml"), "title = 'Synthetic deck'\n")?;
    fs::write(desk.project_dir.join("cards/example.md"),
        "---\nid: example\nkind: definition\nlean: [A.x]\n---\n")?;
    fs::write(package.join("lakefile.toml"), "name = 'A'\n")?;
    // The first audit hangs. Later audits finish at once.
    let tools = tempfile::tempdir()?;
    let lake = tools.path().join("lake");
    fs::write(&lake, r#"ctl=$(dirname "$0")
[ "$1" = build ] && exit 0
cat >/dev/null
if [ ! -f "$ctl/child" ]; then
  sleep 300 & echo $! > "$ctl/pid"; mv "$ctl/pid" "$ctl/child"; wait
fi
echo "'A.x' does not depend on any axioms"
echo 'fuzukue audit complete'
"#)?;
    assert!(desk.lean.lake.set(vec!["/bin/sh".into(), lake.into()]).is_ok());

    let sweep = desk.check_lean(crate::lean::Run::Open);
    let child = tools.path().join("child");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !child.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await?;
    update.check_planned(&app, None).map(drop).expect("a Lean sweep must not count as active work");
    update.begin_planned_wait(&app, DrainReason::Shutdown, None)?;
    let decls = tokio::time::timeout(Duration::from_secs(10), sweep).await???;
    assert!(decls.iter().any(|(name, _)| name == "(lean)"), "{decls:?}");
    let pid = fs::read_to_string(&child)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
            .is_ok_and(|stat| stat.rsplit_once(") ").is_some_and(|(_, tail)| !tail.starts_with(['Z', 'X'])))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await?;

    // Reopen admission, as a cancelled drain does.
    *update.wait_control.lock().unwrap() = WaitControl::Closed;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !desk.lean_status.lock().unwrap().contains(&("A.x".into(), "proved".into())) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await?;
    Ok(())
}

#[tokio::test]
async fn preflight_exempts_only_the_authenticated_requesting_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let other_session = desk.hub.edit_threads(|store| store.create(None))?;
    let other_path = dir.path().join("other");
    fs::create_dir(&other_path)?;
    let other = app.select(Some(&app.register(other_path.to_str().unwrap())?))?;
    let other_workspace_session = other.hub.edit_threads(|store| store.create(None))?;

    // Simulate a server turn registered before the backend acknowledges start.
    let requester = start_requester(&desk, &session);
    assert!(update
        .check_planned(&app, None)
        .unwrap_err()
        .to_string()
        .contains("1 active turns"));
    assert!(update.check_planned(&app, Some(&requester)).is_ok());

    set_running(&desk, &other_session, true);
    set_running(&other, &other_workspace_session, true);
    let error = update
        .start(app.clone(), false, None, Some(requester.clone()))
        .unwrap_err();
    assert!(error.to_string().contains("2 active turns"), "{error:#}");
    assert!(!update.root.join("lock").exists());
    assert!(update.subscribe_drain().borrow().is_none());

    set_running(&desk, &other_session, false);
    set_running(&other, &other_workspace_session, false);
    let work = desk.admit_work()?;
    assert!(update
        .check_planned(&app, Some(&requester))
        .unwrap_err()
        .to_string()
        .contains("admission held: true"));
    drop(work);

    // A new turn in the same thread cannot inherit the earlier request.
    set_running(&desk, &session, false);
    set_running(&desk, &session, true);
    assert!(update
        .check_planned(&app, Some(&requester))
        .unwrap_err()
        .to_string()
        .contains("1 active turns"));
    Ok(())
}

#[tokio::test]
async fn planned_update_waits_for_the_requester_and_closes_admission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let requester = start_requester(&desk, &session);
    let wait = begin(&app, &update, &requester)?;
    assert!(desk.admit_work().is_err());
    assert!(update
        .begin_planned_wait(&app, DrainReason::Update, Some(&requester))
        .is_err());

    let mut status = update.subscribe_drain();
    let finish = async {
        status
            .wait_for(|status| {
                status
                    .as_ref()
                    .is_some_and(|status| status.jobs_running == 1)
            })
            .await?;
        assert!(!*update.swapping.borrow());
        // File changes and editor-close consistency checks can leave durable
        // work while the requester finishes and replay is suspended.
        desk.schedule_build("deck".into());
        desk.drain.enqueue(
            &desk.state_dir,
            "deck",
            "review".into(),
            drain::Work::HumanReview {
                root: desk.project_dir.join("deck"),
                baseline: Default::default(),
            },
        )?;
        set_running(&desk, &session, false);
        Ok::<_, anyhow::Error>(())
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (result, ()) = tokio::try_join!(
        update.wait_for_planned(&app, &wait, Some(&requester), deadline),
        finish
    )?;
    assert!(matches!(result, WaitOutcome::Ready(_)));
    let records = desk.drain.records(&desk.state_dir)?;
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| !record.claimed));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn planned_update_aborts_at_the_deadline_without_interrupting_the_requester() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let requester = start_requester(&desk, &session);
    let wait = begin(&app, &update, &requester)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let Err(error) = update
        .wait_for_planned(&app, &wait, Some(&requester), deadline)
        .await
    else {
        panic!("the requester was still running at the deadline");
    };
    assert!(error
        .to_string()
        .contains("requesting front-desk turn is still active"));
    assert!(desk.hub.thread_running(&session));
    assert_eq!(
        update.subscribe_drain().borrow().as_ref().unwrap().progress,
        DrainProgress::Draining
    );
    assert!(!*update.swapping.borrow());
    drop(wait);
    assert!(desk.admit_work().is_ok());
    Ok(())
}

fn begin<'a>(app: &App, update: &'a Update, requester: &RequestingTurn) -> Result<WaitGuard<'a>> {
    let reason = DrainReason::Update;
    let id = update.begin_planned_wait(app, reason, Some(requester))?;
    Ok(WaitGuard {
        control: &update.wait_control,
        id,
        reason,
    })
}

#[tokio::test]
async fn active_queue_refuses_close_and_archived_github_and_held_input_do_not() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let (session, stale) = desk
        .hub
        .edit_threads(|store| Ok((store.create(None)?, store.create(None)?)))?;
    desk.hub.edit_threads(|store| {
        store.restore_github_after_rollback(
            &stale,
            &drain::GitHubHold {
                policy: GitHubPolicy::Allow,
                classes: None,
                pending: vec![],
                responses: vec![vec!["missing-github-event".into()]],
            },
        )
    })?;
    assert!(crate::issue_threads::queued_for_dispatch(&desk).is_empty());

    let event = fuzukue_proto::GitHubActivity {
        id: "comment".into(),
        link: fuzukue_proto::GitHubLink {
            repo: "owner/repo".into(),
            number: 1,
        },
        actor: "owner".into(),
        author: fuzukue_proto::GitHubAuthor::Owner,
        kind: fuzukue_proto::GitHubActivityKind::Comment,
        excerpt: "Queued comment".into(),
        body: None,
        url: "https://github.com/owner/repo/issues/1".into(),
        at: chrono::Utc::now().to_rfc3339(),
        review_comment: None,
        intake: false,
    };
    desk.hub.edit_threads(|store| {
        store.github_set_intake(&session, &event.link.repo, None)?;
        assert!(store
            .github_deliver(&session, &event.link, std::slice::from_ref(&event), true)?
            .is_some());
        store.github_request(&session, &[event.id], &[])
    })?;
    assert_eq!(
        crate::issue_threads::queued_for_dispatch(&desk),
        [session.clone()]
    );
    desk.drain.enqueue(
        &desk.state_dir,
        &session,
        "held".into(),
        drain::Work::Prompt {
            text: "queued input".into(),
            paths: vec![],
        },
    )?;
    assert!(update
        .request_shutdown(app.clone(), Duration::from_secs(5), false)
        .unwrap_err()
        .to_string()
        .contains("1 queued work items or paused threads"));

    desk.hub.set_thread_archived(&session, true)?;
    assert!(crate::issue_threads::queued_for_dispatch(&desk).is_empty());
    assert!(desk
        .hub
        .read_threads(|store| store.github_response(&session))
        .is_some());
    assert_eq!(desk.drain.records(&desk.state_dir)?.len(), 1);
    update.request_shutdown(app, Duration::from_secs(5), false)?;
    let mut ready = update.shutdown_ready.subscribe();
    tokio::time::timeout(Duration::from_secs(5), ready.wait_for(|ready| *ready)).await??;
    Ok(())
}

#[tokio::test]
async fn archived_in_memory_desk_queue_refuses_planned_shutdown() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    crate::delegate::wake_for_update_result(&desk, &session, "fixture result".into());
    desk.hub.set_thread_archived(&session, true)?;
    assert!(desk.drain.records(&desk.state_dir)?.is_empty());
    let error = update.request_shutdown(app, Duration::from_secs(5), false).unwrap_err();
    assert!(error.to_string().contains("1 queued work items or paused threads"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn a_scheduled_deck_build_is_busy_without_a_thread() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    desk.drain.enqueue(
        &desk.state_dir,
        "deck",
        "build".into(),
        drain::Work::Build {
            deck: "deck".into(),
        },
    )?;
    let error = update
        .request_shutdown(app, Duration::from_secs(5), false)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("1 queued work items or paused threads"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn approval_holding_a_mutation_guard_blocks_even_the_requesting_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let requester = start_requester(&desk, &session);
    let (_, mut events) = desk.hub.subscribe(u64::MAX);
    let pending = tokio::spawn({
        let desk = desk.clone();
        let session = session.clone();
        async move {
            let change = crate::mutations::Mutation::new(&desk, &session, "pending write".into())?;
            change
                .approve(
                    "planned-close-test",
                    Actor::Claude,
                    fuzukue_proto::LocalizedMessage::new("planned_close_test", []),
                )
                .await
        }
    });
    let request_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::ApprovalRequested { request_id, .. } = events.recv().await?.event {
                return Ok::<_, anyhow::Error>(request_id);
            }
        }
    })
    .await??;
    let error = update
        .start(app.clone(), false, None, Some(requester.clone()))
        .unwrap_err();
    assert!(error.to_string().contains("1 approvals"), "{error:#}");
    assert!(error.to_string().contains("1 mutations"));
    assert!(desk.hub.live_approval_threads().contains(&session));
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(desk.hub.has_seen_approval(&request_id));
    assert!(update.check_planned(&app, Some(&requester)).is_ok());
    Ok(())
}

#[tokio::test]
async fn archived_claude_usage_pause_is_busy_after_turn_ended() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let mut command = Command::new("sh");
    command.env("RESET_AT", (crate::usage::now() + 3600).to_string()).arg("-c").arg(r#"
IFS= read -r prompt || exit 1
printf '{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"status":"rejected","resetsAt":%s}}}}\n' "$RESET_AT"
IFS= read -r interrupt || exit 1
printf '%s\n' '{"type":"result","is_error":false}'
while IFS= read -r _; do :; done
"#);
    let (_, mut events) = desk.hub.subscribe(u64::MAX);
    let claude = crate::ClaudeSession::spawn_command(
        command,
        session.clone(),
        desk.project_dir.clone(),
        &desk,
        Box::new(|_| {}),
    )?;
    desk.sessions
        .lock()
        .await
        .insert(session.clone(), claude.clone());
    claude.prompt("start fixture turn").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::TurnEnded { session: ended, .. } = events.recv().await?.event {
                if ended == session {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    })
    .await??;
    assert!(!desk.hub.thread_running(&session));
    tokio::time::timeout(Duration::from_secs(5), async {
        while claude.swap_activity() != Some((false, false, true)) {
            tokio::task::yield_now().await;
        }
    }).await?;
    desk.hub.set_thread_archived(&session, true)?;
    let error = update
        .request_shutdown(app, Duration::from_secs(5), false)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("1 queued work items or paused threads"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn tracked_background_command_remains_busy_after_its_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let mut command = Command::new("sh");
    command.arg("-c").arg(r#"
IFS= read -r prompt || exit 1
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bg","name":"Bash","input":{"command":"true","run_in_background":true}}]}}'
printf '%s\n' '{"type":"system","subtype":"task_started","task_type":"local_bash","task_id":"bg","tool_use_id":"toolu_bg"}'
printf '%s\n' '{"type":"result","is_error":false}'
for i in $(seq 1 100); do [ -f finish ] && break; sleep .05; done
printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"bg","status":"completed"}'
while IFS= read -r _; do :; done
"#);
    let (_, mut events) = desk.hub.subscribe(u64::MAX);
    let claude = crate::ClaudeSession::spawn_command(
        command,
        session.clone(),
        desk.project_dir.clone(),
        &desk,
        Box::new(|_| {}),
    )?;
    desk.sessions
        .lock()
        .await
        .insert(session.clone(), claude.clone());
    claude.prompt("start fixture command").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::TurnEnded { session: ended, .. } = events.recv().await?.event {
                if ended == session {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    })
    .await??;
    assert!(!desk.hub.thread_running(&session));
    assert_eq!(claude.background_commands().len(), 1);
    let error = update.start(app.clone(), false, None, None).unwrap_err();
    assert!(
        error.to_string().contains("1 background commands"),
        "{error:#}"
    );
    fs::write(desk.project_dir.join("finish"), "")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !claude.background_commands().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(update.check_planned(&app, None).is_ok());
    Ok(())
}

async fn blocked_claude(dir: &Path, desk: &Arc<Workspace>, session: &str) -> Result<Arc<crate::ClaudeSession>> {
    fs::write(dir.join("mock-claude"), r#"#!/bin/sh
turn=0
while IFS= read -r prompt; do
    turn=$((turn + 1))
    printf '%s\n' "$prompt" > "input-$turn"
    for i in $(seq 1 200); do [ -f "finish-$turn" ] && break; sleep .05; done
    echo '{"type":"result","is_error":false,"result":"done"}'
done
"#)?;
    let claude = crate::ClaudeSession::spawn(
        session.into(), desk.project_dir.clone(), "http://127.0.0.1:1/mcp",
        None, None, fuzukue_proto::ChatSettings::default(), Box::new(|_| {}), desk,
    ).await?;
    desk.sessions.lock().await.insert(session.into(), claude.clone());
    Ok(claude)
}

async fn fixture_proof(desk: &Workspace, turn: u8) -> Result<String> {
    let path = desk.project_dir.join(format!("input-{turn}"));
    let input = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(input) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    break input;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    let marker = RequestingTurn::proof_instruction("PROOF_PLACEHOLDER");
    let (prefix, suffix) = marker.split_once("PROOF_PLACEHOLDER").unwrap();
    input["message"]["content"].as_array().unwrap().iter()
        .filter_map(|block| block["text"].as_str())
        .find_map(|text| text.split_once(prefix)?.1.strip_suffix(suffix))
        .map(str::to_owned)
        .context("fixture did not receive an update proof")
}

#[tokio::test]
async fn delayed_mcp_request_from_turn_a_is_rejected_during_turn_b() -> Result<()> {
    use axum::{extract::{Query, State}, http::{header, HeaderMap}, Json};
    use serde_json::{json, Value};

    let dir = tempfile::tempdir()?;
    let (_app, _update, desk) = load(dir.path())?;
    let session = desk.hub.edit_threads(|store| store.create(None))?;
    let claude = blocked_claude(dir.path(), &desk, &session).await?;
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, format!("Bearer {}", claude.mcp_key().unwrap()).parse()?);
    let request = |proof: Option<&str>| {
        let mut args = json!({"action": "start", "confirm": false});
        if let Some(proof) = proof {
            args["turn_proof"] = json!(proof);
        }
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "self_update", "arguments": args}})
    };
    let call = |request| async {
        let response = crate::mcp::handler(
            State(desk.clone()),
            Query(crate::mcp::McpQuery { session: session.clone(), backend: None, workspace: None, job: None }),
            headers.clone(),
            Json(request),
        ).await;
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await?)?;
        Ok::<_, anyhow::Error>(body.to_string())
    };

    claude.prompt("A").await?;
    let proof_a = fixture_proof(&desk, 1).await?;
    let delayed = request(Some(&proof_a));
    assert!(!call(delayed.clone()).await?.contains("could not be verified"));
    let (_, mut events) = desk.hub.subscribe(u64::MAX);
    fs::write(desk.project_dir.join("finish-1"), "")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: ref ended, .. } if ended == &session) {}
        Ok::<_, anyhow::Error>(())
    }).await??;
    claude.prompt("B").await?;
    let proof_b = fixture_proof(&desk, 2).await?;
    assert_ne!(proof_a, proof_b);
    assert!(call(delayed).await?.contains("could not be verified"));
    assert!(call(request(None)).await?.contains("could not be verified"));
    let valid = call(request(Some(&proof_b))).await?;
    assert!(valid.contains("confirm=true"), "{valid}");
    fs::write(desk.project_dir.join("finish-2"), "")?;
    Ok(())
}

#[tokio::test]
async fn finishing_requester_only_defers_real_card_reviews() -> Result<()> {
    for changed in [false, true] {
        let dir = tempfile::tempdir()?;
        let (app, update, desk) = load(dir.path())?;
        let session = desk.hub.edit_threads(|store| store.create(None))?;
        assert!(Command::new("git").args(["init", "--quiet", "--template="])
            .current_dir(&desk.project_dir).status().await?.success());
        let deck = desk.project_dir.join("paper");
        fs::create_dir_all(deck.join("cards"))?;
        fs::write(deck.join("deck.toml"), "[review]\nauto = true\n")?;
        let card = deck.join("cards/main.md");
        fs::write(&card, "---\nid: main\n---\nBefore.\n")?;
        let expected = crate::auto_review::Snapshot::take(&desk.project_dir);
        let claude = blocked_claude(dir.path(), &desk, &session).await?;
        claude.prompt("start update").await?;
        let proof = fixture_proof(&desk, 1).await?;
        let requester = RequestingTurn::capture(&desk, &session, &proof).unwrap();
        let wait = begin(&app, &update, &requester)?;
        if changed {
            fs::write(&card, "---\nid: main\n---\nAfter.\n")?;
        }
        fs::write(desk.project_dir.join("finish-1"), "")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let outcome = update.wait_for_planned(&app, &wait, Some(&requester), deadline).await?;
        assert!(matches!(outcome, WaitOutcome::Ready(_)));
        let records = drain::Journal::default().records(&desk.state_dir)?;
        assert_eq!(records.len(), usize::from(changed));
        if changed {
            let drain::Work::AutomaticReview { project, before, .. } = &records[0].work else {
                panic!("expected a saved automatic review");
            };
            assert_eq!(project, &desk.project_dir);
            assert_eq!(serde_json::to_value(before)?, serde_json::to_value(&expected)?);
        }
        assert_eq!(desk.running_review_count(), 0);
    }
    Ok(())
}
