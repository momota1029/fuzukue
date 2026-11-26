use super::*;
use crate::{delegate, Workspace};
use fuzukue_proto::{Actor, AgentCaller, ClientMsg, DelegateSettings, PlanStep, SelfUpdateBlockerKind, ServerEvent};

async fn until(test: impl Fn() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !test() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("fixture did not reach expected state")
}

fn install_desk_fixture(app: &crate::App, dir: &Path) -> Result<()> {
    let program = dir.join("mock-claude");
    fs::write(
        &program,
        r#"#!/bin/sh
case "$1" in --help) echo 'Claude'; exit 0;; esac
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fixture"}'
while IFS= read -r input; do
    case "$input" in *'"type":"user"'*)
        printf '%s\n' "$input" >> prompts
        printf '%s\n' '{"type":"result","is_error":false,"result":"saved result"}'
        ;; esac
done
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
    assert!(app.test_claude_program.set(program).is_ok());
    Ok(())
}

pub(super) fn load(dir: &Path) -> Result<(Arc<crate::App>, Arc<Update>, Arc<Workspace>)> {
    let (app, update) = tests::waiting_fixture(dir)?;
    let update = Arc::new(update);
    assert!(app.self_update.set(update.clone()).is_ok());
    install_desk_fixture(&app, dir)?;
    let workspace = app.select(None)?;
    Ok((app, update, workspace))
}

fn begin(update: &Update) -> Result<WaitGuard<'_>> {
    let reason = DrainReason::Update;
    let id = update
        .begin_wait(reason, false)?
        .context("drain did not open")?;
    Ok(WaitGuard {
        control: &update.wait_control,
        id,
        reason,
    })
}

impl Update {
    pub(crate) fn test_update_drain(
        self: &Arc<Self>, app: Arc<crate::App>,
    ) -> Result<(i64, tokio::task::JoinHandle<Result<()>>)> {
        fs::create_dir_all(&self.root)?;
        let reason = DrainReason::Update;
        let id = self.begin_wait(reason, false)?.context("drain did not open")?;
        let update = self.clone();
        let task = tokio::spawn(async move {
            let wait = WaitGuard { control: &update.wait_control, id, reason };
            let WaitOutcome::Ready(_permit) = update.wait_for_idle(&app, &wait, None).await? else {
                anyhow::bail!("drain did not reach the swap gate");
            };
            update.set_drain_progress(id, DrainProgress::Ready, None);
            Ok(())
        });
        Ok((id, task))
    }
}

fn prompt_count(workspace: &Workspace, text: &str) -> usize {
    fs::read_to_string(workspace.project_dir.join("prompts"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(text))
        .count()
}

fn planner_thread(workspace: &Workspace, task: &str) -> Result<String> {
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.edit_threads(|threads| threads.set_roles(&session, None, Some(DelegateSettings {
        model: fuzukue_proto::CLAUDE_OPUS.into(), effort: "high".into(),
    })))?;
    crate::planner::write_test_plan_steps(workspace, &session, vec![PlanStep {
        task: task.into(), agent: None, reason: String::new(),
        routing: Default::default(), tier_reason: None, blocker: None,
    }])?;
    Ok(session)
}

async fn assign(workspace: &Arc<Workspace>, session: &str, task: &str, caller: AgentCaller) -> Result<String> {
    delegate::start(workspace, session, task.into(), None, None, caller, Default::default(), None).await
}

async fn assert_assignment_replayed(workspace: &Workspace, job: &str, task: &str) -> Result<()> {
    until(|| prompt_count(workspace, task) == 1).await
        .with_context(|| format!("waiting for assignment {job} to run: {task}"))?;
    until(|| workspace.drain.records(&workspace.state_dir)
        .is_ok_and(|records| records.iter().all(|record| record.id != job))).await
        .with_context(|| format!("waiting for assignment {job} to leave the journal"))?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompt_count(workspace, task), 1);
    Ok(())
}

#[tokio::test]
async fn full_attach_and_drain_follow_the_worker_approval_waiter() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let (_, mut live) = workspace.hub.subscribe(u64::MAX);
    let worker = workspace.clone();
    let thread = session.clone();
    *workspace.worker_approval_policy.lock().unwrap() = fuzukue_proto::WorkerApprovalPolicy::Advisor;
    let pending = tokio::spawn(async move {
        delegate::request_worker_approval(&worker, &thread, "delegate:fixture", "fixture-rpc", "Bash",
            &serde_json::json!({"command":"cargo test"}), false, None).await
    });
    let request_id = match tokio::time::timeout(Duration::from_secs(5), live.recv()).await??.event {
        ServerEvent::ApprovalRequested { request_id, .. } => request_id,
        event => anyhow::bail!("expected worker approval, got {event:?}"),
    };
    assert!(!workspace.hub.thread_active(&session));
    let replay_has_request = || workspace.hub.subscribe_client(0, true).0.iter().any(|env| {
        matches!(&env.event, ServerEvent::ApprovalRequested { request_id: id, .. } if id == &request_id)
    });
    assert!(replay_has_request());
    assert!(matches!(&app.swap_blockers().blockers[..], [blocker]
        if blocker.kind == SelfUpdateBlockerKind::Approval && blocker.workspace == workspace.id
            && blocker.session.as_deref() == Some(session.as_str()) && blocker.id == request_id
            && blocker.started_at > 0));
    workspace.hub.expire_orphaned_approvals(Duration::ZERO);
    assert!(replay_has_request());
    let wait = begin(&update)?;
    let drain = update.wait_for_idle(&app, &wait, None);
    tokio::pin!(drain);
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut drain).await.is_err());

    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(matches!(live.recv().await?.event, ServerEvent::ApprovalResolved {
        request_id: id, decision, automatic: false,
    } if id == request_id && decision == "cancelled"));
    assert!(!replay_has_request());
    assert!(app.swap_blockers().blockers.is_empty());
    assert!(matches!(tokio::time::timeout(Duration::from_secs(5), &mut drain).await??, WaitOutcome::Ready(_)));
    Ok(())
}

#[tokio::test]
async fn drain_uses_the_worker_title_or_a_short_assignment_summary() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, _, workspace) = load(dir.path())?;
    let program = dir.path().join("mock-claude");
    let script = fs::read_to_string(&program)?.replace(
        "printf '%s\\n' \"$input\" >> prompts",
        "while [ ! -f finish-worker ]; do sleep 0.05; done",
    );
    fs::write(program, script)?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.edit_threads(|threads| threads.set_roles(&session, None, Some(DelegateSettings {
        model: fuzukue_proto::CLAUDE_OPUS.into(), effort: "high".into(),
    })))?;
    let done = workspace.project_dir.join("finish-worker");
    let long = "検".repeat(45);
    let short = format!("サーバーテスト {}…", "検".repeat(32));
    let prompt = format!("サーバーテスト\n  {long}");
    for (title, expected) in [(Some("Named job"), "Named job"), (None, short.as_str())] {
        let job = delegate::start(&workspace, &session, prompt.clone(), title.map(str::to_owned), None,
            AgentCaller::FrontDesk, Default::default(), None).await?;
        until(|| app.swap_blockers().blockers.iter().any(|b| b.id == job)).await?;
        let blocker = app.swap_blockers().blockers.into_iter().find(|b| b.id == job).unwrap();
        assert_eq!(blocker.kind, SelfUpdateBlockerKind::Worker);
        assert_eq!(blocker.title.as_deref(), Some(expected));
        assert!(blocker.started_at > 0);
        fs::write(&done, "")?;
        until(|| !workspace.delegates.is_running(&session)).await?;
        fs::remove_file(&done)?;
    }
    Ok(())
}

#[tokio::test]
async fn restart_closes_orphaned_worker_approvals_and_expires_other_orphans() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_app, _update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let requests = [
        ("delegate:fixture:lost".into(), Actor::Claude),
        ("codex-1".into(), Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some("delegate:fixture".into()) }),
        (format!("{}orphan", fuzukue_proto::GIT_APPROVAL), Actor::System),
        (format!("{}:orphan", fuzukue_proto::RESEARCH_ISSUE_APPROVAL), Actor::System),
        (format!("{}:orphan", crate::issue_threads::APPROVAL), Actor::System),
    ];
    for (request_id, actor) in &requests {
        workspace.hub.publish(ServerEvent::ApprovalRequested {
            session: session.clone(), request_id: request_id.clone(), actor: actor.clone(),
            summary: "fixture".into(), localized: None, shell_directory: None, shell_prefix: None,
        });
    }
    let cursor = workspace.hub.subscribe(0).0.last().unwrap().seq;
    let hub = crate::Hub::with_timeline(&workspace.state_dir);
    assert!(hub.live_approval_threads().is_empty());
    let (resumed, _) = hub.subscribe_client(cursor, true);
    for (request_id, _) in requests {
        assert!(resumed.iter().any(|env| matches!(&env.event,
            ServerEvent::ApprovalResolved { request_id: id, decision, automatic: false }
            if id == &request_id && decision == "cancelled")));
    }
    let (_, mut live) = hub.subscribe(u64::MAX);
    hub.expire_orphaned_approvals(Duration::ZERO);
    assert!(live.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn claude_cancelled_tool_request_releases_approval_while_turn_stays_live() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, _update, workspace) = load(dir.path())?;
    let id = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let mut command = tokio::process::Command::new("/usr/bin/python3");
    command.arg("-c").arg(r#"
import json, sys, time
from pathlib import Path
json.loads(sys.stdin.readline())
print(json.dumps({"type":"control_request", "request_id":"tool", "request":{
    "subtype":"can_use_tool", "tool_name":"Bash", "input":{"command":"git status"},
}}), flush=True)
while not Path("cancel-tool").exists():
    time.sleep(0.01)
print(json.dumps({"type":"control_cancel_request", "request_id":"tool"}), flush=True)
sys.stdin.readline()
"#);
    let claude = crate::ClaudeSession::spawn_command(
        command, id.clone(), workspace.project_dir.clone(), &workspace, Box::new(|_| {}),
    )?;
    workspace.sessions.lock().await.insert(id.clone(), claude.clone());
    let (_, mut live) = workspace.hub.subscribe(u64::MAX);
    claude.prompt("Request a tool approval.").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let request_id = loop {
            if let ServerEvent::ApprovalRequested { request_id, .. } = live.recv().await?.event {
                break request_id;
            }
        };
        let reason = app.swap_blockers();
        assert!(reason.blockers.iter().any(|blocker| blocker.kind == SelfUpdateBlockerKind::Approval && blocker.id == request_id));
        assert_eq!(app.drain_job_count(&reason), 1);
        fs::write(workspace.project_dir.join("cancel-tool"), "")?;
        loop {
            if let ServerEvent::ApprovalResolved { request_id: resolved, decision, automatic } = live.recv().await?.event {
                assert_eq!(resolved, request_id);
                assert_eq!(decision, "cancelled");
                assert!(!automatic);
                break;
            }
        }
        let reason = app.swap_blockers();
        assert!(matches!(&reason.blockers[..], [blocker] if blocker.kind == SelfUpdateBlockerKind::Turn && blocker.id == id));
        assert_eq!(app.drain_job_count(&reason), 1);
        assert!(!claude.resolve(&request_id, "accept").await?);
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn planner_assignments_queue_during_drain_and_retry_after_startup_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_app, update, workspace) = load(dir.path())?;
    let prompt = "Read the saved test assignment";
    let session = planner_thread(&workspace, prompt)?;
    let wait = begin(&update)?;
    let job = delegate::start(
        &workspace, &session, prompt.into(), None, None, AgentCaller::Planner, Default::default(), None,
    ).await?;
    assert!(delegate::start(
        &workspace, &session, prompt.into(), None, None, AgentCaller::FrontDesk, Default::default(), None,
    ).await.is_err());
    let held = workspace.drain.records(&workspace.state_dir)?;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, job);
    assert!(!held[0].claimed);
    let drain::Work::Delegate(assignment) = held[0].work.clone() else {
        anyhow::bail!("expected a held delegation");
    };
    delegate::restore_assignment(&workspace, session.clone(), job.clone(), assignment).await;
    assert!(!workspace.delegates.is_running(&session));
    drop(wait);

    // Simulate an older server that lost the worker after claiming the assignment.
    assert!(workspace.drain.claim(&workspace.state_dir, &session, &job)?);
    workspace.drain.finish(&workspace.state_dir, &session, &job, Some(
        "The previous server stopped after claiming this work. Check the thread and worktree before retrying.".into(),
    ));
    let new_app = crate::App::new(
        dir.path().into(), workspace.project_dir.clone(), workspace.state_dir.clone(),
        "127.0.0.1:0".into(), None,
    )?;
    install_desk_fixture(&new_app, dir.path())?;
    let next = Arc::new(Update::with_paths(
        Config::default(), None, workspace.project_dir.clone(), dir.path().join("next-builds"), PathBuf::new(),
    ));
    next.trial_pending.store(true, std::sync::atomic::Ordering::Release);
    assert!(new_app.self_update.set(next.clone()).is_ok());
    let restarted = new_app.select(None)?;
    drain::start(&restarted);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(restarted.drain.records(&restarted.state_dir)?[0].claimed);
    assert!(!restarted.delegates.is_running(&session));
    next.trial_pending.store(false, std::sync::atomic::Ordering::Release);
    until(|| prompt_count(&restarted, prompt) == 1).await?;
    until(|| !restarted.delegates.is_running(&session)).await?;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(prompt_count(&restarted, prompt), 1);
    assert!(!restarted.drain.records(&restarted.state_dir)?.iter().any(|record| record.id == job));
    Ok(())
}

#[tokio::test]
async fn planner_work_held_during_build_and_test_replays_after_startup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    drain::start(&workspace);
    let task = "Planner assignment submitted during build";
    let session = planner_thread(&workspace, task)?;
    let review = planner_thread(&workspace, "Review the saved goal")?;
    crate::planner::pause_goal(&workspace, &review)?;
    let usage = |utilization, resets_at| serde_json::json!({
        "unifiedWindows": { "five_hour": { "utilization": utilization, "resetsAt": resets_at } }
    });
    workspace.usage.update_claude(&usage(1.0, crate::usage::now() + 60));
    assert!(crate::planner::wake_goal(&workspace, &review)?);
    assert_eq!(crate::planner::resume_eligible(&workspace, &review), Some(false));

    update.step(Step::Build, State::Running);
    let job = assign(&workspace, &session, task, AgentCaller::Planner).await?;
    workspace.usage.update_claude(&usage(0.0, crate::usage::now()));
    until(|| workspace.drain.records(&workspace.state_dir).unwrap().iter().any(|record|
        record.session == review
            && matches!(record.work, drain::Work::Desk(delegate::DeskTurn::Plan { resumed: true }))))
        .await.context("waiting for the usage monitor to journal the resume")?;
    update.step(Step::Build, State::Passed);
    update.step(Step::Test, State::Running);
    delegate::plan_follow_up(&workspace, &review);

    let user = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.edit_threads(|threads| threads.set_roles(&user, None, Some(DelegateSettings {
        model: fuzukue_proto::CLAUDE_OPUS.into(), effort: "high".into(),
    })))?;
    let manual = "Explicit request sent by the front desk";
    let manual_job = assign(&workspace, &user, manual, AgentCaller::FrontDesk).await?;
    assert_assignment_replayed(&workspace, &manual_job, manual).await?;
    assert!(!workspace.delegates.is_running(&session));
    assert_eq!(prompt_count(&workspace, task), 0);
    assert_eq!(prompt_count(&workspace, "Resume test goal"), 0);
    let records = workspace.drain.records(&workspace.state_dir)?;
    assert_eq!(records.iter().filter(|record| !record.claimed && record.work.is_planner()).count(), 2);
    assert!(records.iter().any(|record| record.session == review
        && matches!(record.work, drain::Work::Desk(delegate::DeskTurn::Plan { resumed: true }))));

    update.step(Step::Test, State::Passed);
    update.status.send_modify(|status| status.active = false);
    let error = update.check_planned(&app, None).unwrap_err();
    assert!(error.to_string().contains("2 queued work items"), "{error:#}");
    update.status.send_modify(|status| status.active = true);
    let reason = DrainReason::Update;
    let id = update.begin_planned_wait(&app, reason, None)?;
    let wait = WaitGuard { control: &update.wait_control, id, reason };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let idle = update.wait_for_planned(&app, &wait, None, deadline).await?;
    assert!(matches!(idle, WaitOutcome::Ready(_)));
    assert_eq!(workspace.drain.queued_planner_count(&workspace.state_dir), 2);
    assert_eq!(app.drain_job_count(&app.swap_blockers()), 0);

    let new_app = crate::App::new(
        dir.path().into(), workspace.project_dir.clone(), workspace.state_dir.clone(),
        "127.0.0.1:0".into(), None,
    )?;
    install_desk_fixture(&new_app, dir.path())?;
    let next = Arc::new(Update::with_paths(
        Config::default(), None, workspace.project_dir.clone(), dir.path().join("next-builds"), PathBuf::new(),
    ));
    next.trial_pending.store(true, std::sync::atomic::Ordering::Release);
    assert!(new_app.self_update.set(next.clone()).is_ok());
    let restarted = new_app.select(None)?;
    drain::start(&restarted);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompt_count(&restarted, task), 0);
    assert_eq!(prompt_count(&restarted, "Resume test goal"), 0);
    assert!(!restarted.delegates.is_running(&session));

    next.trial_pending.store(false, std::sync::atomic::Ordering::Release);
    until(|| prompt_count(&restarted, "Resume test goal") == 1).await
        .context("waiting for the saved review after the startup check")?;
    assert_assignment_replayed(&restarted, &job, task).await?;
    assert_eq!(prompt_count(&restarted, "Resume test goal"), 1);
    Ok(())
}

#[tokio::test]
async fn update_holds_planner_assignment_already_waiting_for_a_worker_slot() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    update.status.send_modify(|status| status.active = false);
    let slots = tokio::time::timeout(Duration::from_secs(20), delegate::hold_all_slots(&workspace)).await?;
    let task = "Planner assignment waiting for a slot";
    let session = planner_thread(&workspace, task)?;
    let job = assign(&workspace, &session, task, AgentCaller::Planner).await?;
    update.status.send_modify(|status| status.active = true);
    update.step(Step::Build, State::Running);
    drop(slots);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompt_count(&workspace, task), 0);
    assert!(workspace.drain.delegation_queued(&workspace.state_dir, &job)?);
    let wait = begin(&update)?;
    let idle = tokio::time::timeout(Duration::from_secs(5), update.wait_for_idle(&app, &wait, None)).await??;
    assert!(matches!(idle, WaitOutcome::Ready(_)));
    Ok(())
}

#[tokio::test]
async fn planner_work_replays_on_update_failure_or_cancel() -> Result<()> {
    for cancel in [false, true] {
        let dir = tempfile::tempdir()?;
        let (app, update, workspace) = load(dir.path())?;
        drain::start(&workspace);
        update.step(Step::Test, State::Running);
        let task = "Planner assignment submitted during test";
        let session = planner_thread(&workspace, task)?;
        let job = assign(&workspace, &session, task, AgentCaller::Planner).await?;
        let review = workspace.hub.edit_threads(|threads| threads.create(None))?;
        crate::planner::write_test_plan(&workspace, &review)?;
        delegate::plan_follow_up(&workspace, &review);
        delegate::plan_follow_up(&workspace, &review);
        update.report_planner_queue(&app);
        assert_eq!(update.subscribe().borrow().planner_work_held, Some(2));

        if cancel {
            let wait = begin(&update)?;
            update.control_wait(wait.id, WaitAction::Cancel)?;
            assert!(matches!(update.wait_for_idle(&app, &wait, None).await?, WaitOutcome::Cancelled));
            assert!(update.subscribe().borrow().cancelled);
        } else {
            // Simulate an accepted build failing after planner work arrived.
            update.fail("fixture build failed".into());
        }
        assert!(update.subscribe().borrow().planner_work_held.is_none());
        until(|| prompt_count(&workspace, "Resume test goal") == 1).await?;
        assert_assignment_replayed(&workspace, &job, task).await?;
        assert_eq!(prompt_count(&workspace, "Resume test goal"), 1);
    }
    Ok(())
}

#[tokio::test]
async fn update_failure_before_swap_wakes_only_its_requesting_thread() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let other = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let (_, mut events) = workspace.hub.subscribe(u64::MAX);

    update.start(app, false, Some(Recipient::new(&workspace, &session)), None)?;
    let (item, text) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await?;
            if let ServerEvent::TextDelta { session: target, item, actor: Actor::System, text } = event.event {
                if item.starts_with("server-update:") {
                    assert_eq!(target, session);
                    return Ok::<_, anyhow::Error>((item, text));
                }
            }
        }
    }).await??;

    assert!(text.contains(r#""outcome":"failure""#));
    assert!(text.contains(r#""failed_step":"check""#));
    assert!(text.contains(BUILD_ID));
    until(|| prompt_count(&workspace, "running_revision") == 1).await?;
    assert!(!workspace.hub.read_threads(|threads| threads.has_item(&other, &item)));
    Ok(())
}

// These are the 668973e serde shapes for the desk records used in this fixture.
#[derive(serde::Deserialize)]
struct OldHeldRecord {
    id: String,
    session: String,
    work: OldHeldWork,
    claimed: bool,
}

#[derive(serde::Deserialize)]
enum OldHeldWork {
    Desk(OldDeskTurn),
}

#[derive(serde::Deserialize)]
enum OldDeskTurn {
    Report { job: String, text: String, plan_failed: bool },
}

#[tokio::test]
async fn update_result_is_saved_while_busy_and_quota_blocked_without_changing_held_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let held = drain::Work::Desk(delegate::DeskTurn::Report {
        job: "held-job".into(), text: "held report".into(), plan_failed: false,
    });
    workspace.drain.enqueue(&workspace.state_dir, &session, "held-report".into(), held)?;
    let journal = workspace.state_dir.join("held-work.json");
    let before = fs::read(&journal)?;

    let mut status = update.status.borrow().clone();
    status.active = true;
    update.begin_result(status, Some(Recipient::new(&workspace, &session)), false)?;
    let pending = fs::read(update.root.join(STATUS_FILE))?;
    // The 668973e status reader uses this unchanged protocol type directly.
    assert!(serde_json::from_slice::<fuzukue_proto::SelfUpdateStatus>(&pending)?.active);
    let request: serde_json::Value = serde_json::from_slice(&pending)?;
    let item = request["request"]["id"].as_str().context("missing result ID")?;
    let transcript = workspace.hub.thread_path(&session)?;

    workspace.hub.publish_partner_turn_started(&session, None, false, None);
    workspace.usage.restore(vec![fuzukue_proto::UsageWindow {
        agent: "claude".into(),
        name: "test".into(),
        used: 1.0,
        resets_at: Some(crate::usage::now() + 600),
        rejected: true,
    }]);
    assert!(workspace.hub.thread_active(&session));
    assert!(workspace.usage.blocked("claude").is_some());
    update.fail("candidate failed while the desk was busy".into());
    update.report_result(&app)?;

    let saved = fs::read_to_string(&transcript)?;
    assert_eq!(saved.matches(item).count(), 1);
    assert!(saved.contains("candidate failed while the desk was busy"));
    assert!(workspace.delegates.has_queued_desk_turn(&session));
    assert_eq!(prompt_count(&workspace, "candidate failed while the desk was busy"), 0);
    let after = fs::read(journal)?;
    let records: Vec<OldHeldRecord> = serde_json::from_slice(&after)?;
    let [record] = records.as_slice() else { anyhow::bail!("expected one old held record") };
    assert_eq!(record.id, "held-report");
    assert_eq!(record.session, session);
    assert!(!record.claimed);
    let OldHeldWork::Desk(OldDeskTurn::Report { job, text, plan_failed }) = &record.work;
    assert_eq!(job, "held-job");
    assert_eq!(text, "held report");
    assert!(!plan_failed);
    assert_eq!(before, after);
    let status: serde_json::Value = serde_json::from_slice(&fs::read(update.root.join(STATUS_FILE))?)?;
    assert!(status.get("request").is_none());
    Ok(())
}

#[tokio::test]
async fn update_result_retries_the_note_and_does_not_wake_an_archived_thread() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.edit_threads(|threads| threads.set_archived(&session, true))?;

    let mut status = update.status.borrow().clone();
    status.active = true;
    update.begin_result(status, Some(Recipient::new(&workspace, &session)), false)?;
    update.fail("fixture update failed".into());
    let status_path = update.root.join(STATUS_FILE);
    let pending = fs::read(&status_path)?;
    let path = workspace.hub.thread_path(&session)?;
    let moved = path.with_extension("moved");
    fs::rename(&path, &moved)?;
    assert!(update.report_result(&app).is_err());
    assert_eq!(fs::read(&status_path)?, pending);

    fs::rename(moved, &path)?;
    update.report_result(&app)?;
    assert!(fs::read_to_string(path)?.contains("fixture update failed"));
    assert!(!workspace.delegates.has_queued_desk_turn(&session));
    let completed: serde_json::Value = serde_json::from_slice(&fs::read(status_path)?)?;
    assert!(completed.get("request").is_none());
    Ok(())
}

#[tokio::test]
async fn update_result_survives_a_restart_and_a_replayed_request() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, default) = load(dir.path())?;
    let project = dir.path().join("other");
    fs::create_dir(&project)?;
    let workspace_id = app.register(project.to_str().unwrap())?;
    let workspace = app.select(Some(&workspace_id))?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let other = default.hub.edit_threads(|threads| threads.create(None))?;
    let status_path = update.root.join(STATUS_FILE);
    let has_request = || -> Result<bool> {
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&status_path)?)?;
        Ok(saved.get("request").is_some())
    };
    let mut status = update.status.borrow().clone();
    status.steps = steps(false);
    status.steps.iter_mut().find(|step| step.kind == Step::Swap).unwrap().state = State::Running;

    // A dashboard start stays quiet until a desk subscribes from its own thread.
    update.begin_result(status.clone(), None, false)?;
    assert!(workspace.drain.records(&workspace.state_dir)?.is_empty());
    let recipient = Recipient::new(&workspace, &session);
    update.watch_result(recipient.clone())?;
    update.watch_result(recipient)?;
    assert!(update.watch_result(Recipient::new(&default, &other)).is_err());
    assert!(workspace.drain.records(&workspace.state_dir)?.is_empty());
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&status_path)?)?;
    let item = saved["request"]["id"].as_str().context("missing update result id")?.to_owned();
    assert_eq!(saved["request"]["recipient"]["workspace"], workspace_id);
    assert_eq!(saved["request"]["recipient"]["thread"], session);

    // The supervisor saves the rollback after the restored server is healthy.
    fail(&mut status, "candidate failed its health check".into());
    status.recovered = true;
    write_status(&update.root, &status)?;
    let saved_request = fs::read(&status_path)?;
    drop((app, update, workspace, default));

    let reopen = || -> Result<_> {
        let app = crate::App::new(
            dir.path().into(), dir.path().join("project"), dir.path().join("state"), "127.0.0.1:0".into(), None,
        )?;
        install_desk_fixture(&app, dir.path())?;
        let update = Arc::new(Update::with_paths(
            Config::default(), None, dir.path().into(), dir.path().join("builds"), PathBuf::new(),
        ));
        assert!(app.self_update.set(update.clone()).is_ok());
        let workspace = app.known_workspace(&workspace_id)?;
        Ok((app, update, workspace))
    };

    let (app, update, workspace) = reopen()?;
    update.follow_handoff(None, app.clone());
    until(|| {
        !has_request().unwrap()
            && prompt_count(&workspace, "candidate failed its health check") == 1
            && workspace.drain.records(&workspace.state_dir).unwrap().is_empty()
    }).await?;
    assert!(workspace.hub.read_threads(|threads| threads.has_item(&session, &item)));
    drop((app, update, workspace));

    // Simulate losing power before the request removal finished syncing.
    fs::write(&status_path, saved_request)?;
    let (app, update, workspace) = reopen()?;
    update.report_result(&app)?;
    assert!(!has_request()?);
    assert!(workspace.drain.records(&workspace.state_dir)?.is_empty());
    assert!(!workspace.delegates.has_queued_desk_turn(&session));
    assert_eq!(prompt_count(&workspace, "candidate failed its health check"), 1);
    let transcript = fs::read_to_string(workspace.hub.thread_path(&session)?)?;
    assert_eq!(transcript.matches(&item).count(), 1);
    Ok(())
}

#[tokio::test]
async fn workspace_can_allow_planner_work_during_update_preparation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, default) = load(dir.path())?;
    let path = dir.path().join("opt-out");
    fs::create_dir(&path)?;
    fs::write(path.join("fuzukue.toml"), "[delegate]\nhold_planner_during_update = false\n")?;
    let workspace = app.select(Some(&app.register(path.to_str().unwrap())?))?;
    let task = "Planner assignment allowed during test";
    let session = planner_thread(&workspace, task)?;
    let review = workspace.hub.edit_threads(|threads| threads.create(None))?;
    crate::planner::write_test_plan(&workspace, &review)?;
    update.step(Step::Test, State::Running);
    assert!(default.planner_work_held());
    let job = assign(&workspace, &session, task, AgentCaller::Planner).await?;
    delegate::plan_follow_up(&workspace, &review);
    until(|| prompt_count(&workspace, "Resume test goal") == 1).await?;
    assert_assignment_replayed(&workspace, &job, task).await?;
    assert!(update.planner_work_held());
    update.report_planner_queue(&app);
    assert!(update.subscribe().borrow().planner_work_held.is_none());

    let _wait = begin(&update)?;
    let job = assign(&workspace, &session, task, AgentCaller::Planner).await?;
    assert!(workspace.drain.delegation_queued(&workspace.state_dir, &job)?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_closes_atomically_across_workspaces_and_cancel_reopens_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, first) = load(dir.path())?;
    let second = dir.path().join("second");
    fs::create_dir(&second)?;
    let id = app.register(second.to_str().unwrap())?;
    let second = app.select(Some(&id))?;
    let first_thread = first.hub.edit_threads(|threads| threads.create(None))?;
    let second_thread = second.hub.edit_threads(|threads| threads.create(None))?;
    let ready = Arc::new(tokio::sync::Barrier::new(3));
    let admitted = Arc::new(tokio::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for (workspace, session) in [
        (first.clone(), first_thread.clone()),
        (second.clone(), second_thread.clone()),
    ] {
        let (ready, admitted) = (ready.clone(), admitted.clone());
        jobs.push(tokio::spawn(async move {
            let _permit = workspace.admit_work().unwrap();
            workspace.hub.publish(ServerEvent::TurnActivity {
                session: session.clone(),
                running: true,
            });
            ready.wait().await;
            admitted.wait().await;
            assert!(workspace.admit_work().is_err());
            workspace.hub.publish(ServerEvent::TurnEnded {
                session,
                actor: Actor::Claude,
            });
        }));
    }
    ready.wait().await;
    let wait = begin(&update)?;
    let (_, reason) = update.swap_gate(&app);
    assert!(reason.gate_held);
    assert_eq!(reason.threads.len(), 2);
    assert!(reason
        .threads
        .iter()
        .any(|thread| thread.workspace == first.id && thread.id == first_thread && thread.running));
    assert!(reason
        .threads
        .iter()
        .any(|thread| thread.workspace == second.id
            && thread.id == second_thread
            && thread.running));
    assert!(update
        .control_wait(wait.id - 1, WaitAction::Cancel)
        .is_err());
    update.control_wait(wait.id, WaitAction::Cancel)?;
    assert!(matches!(
        update.wait_for_idle(&app, &wait, None).await?,
        WaitOutcome::Cancelled
    ));
    admitted.wait().await;
    for job in jobs {
        job.await?;
    }
    drop(wait);
    assert!(first.admit_work().is_ok() && second.admit_work().is_ok());
    Ok(())
}

#[tokio::test]
async fn save_failure_is_visible_and_keeps_the_gate_closed_until_retry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace
        .hub
        .edit_threads(|threads| threads.create_titled(None, Some("Saving thread".into())))?;
    let wait = begin(&update)?;
    fs::create_dir(workspace.state_dir.join("held-work.json"))?;
    let request = ClientMsg::Prompt {
        session: session.clone(),
        text: "try to hold this".into(),
    };
    assert!(crate::handle(&workspace, request.clone()).await.is_err());
    let mut statuses = update.subscribe();
    let action = async {
        {
            let status = statuses.wait_for(|status| status.wait.is_some()).await?;
            let threads = &status.wait.as_ref().unwrap().reason.threads;
            assert!(threads
                .iter()
                .any(|thread| thread.id == session && thread.checking));
            assert!(status
                .steps
                .iter()
                .any(|step| step.kind == Step::Wait && step.log.contains("saving work")));
        }
        fs::remove_dir(workspace.state_dir.join("held-work.json"))?;
        crate::handle(&workspace, request).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(update.wait_for_idle(&app, &wait, None), action)
    })
    .await??;
    assert!(matches!(outcome, WaitOutcome::Ready(_)));
    let records = workspace.drain.records(&workspace.state_dir)?;
    assert_eq!(records.iter().filter(|record| !record.claimed).count(), 1);
    Ok(())
}

#[tokio::test]
async fn queued_delegation_resumes_once_after_cancel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.edit_threads(|threads| {
        threads.set_roles(
            &session,
            None,
            Some(fuzukue_proto::DelegateSettings {
                model: "claude-sonnet-5-5".into(),
                effort: String::new(),
            }),
        )
    })?;
    let slots = tokio::time::timeout(Duration::from_secs(20), delegate::hold_all_slots(&workspace)).await?;
    let id = delegate::start(
        &workspace,
        &session,
        "manual task marker".into(),
        None,
        None,
        fuzukue_proto::AgentCaller::FrontDesk,
        Default::default(),
        None,
    )
    .await?;
    let wait = begin(&update)?;
    assert!(delegate::start(
        &workspace,
        &session,
        "recursive denied".into(),
        None,
        None,
        fuzukue_proto::AgentCaller::Worker,
        Default::default(),
        None,
    )
    .await
    .is_err());
    drop(slots);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompt_count(&workspace, "manual task marker"), 0);
    assert!(workspace
        .drain
        .records(&workspace.state_dir)?
        .iter()
        .any(|record| record.id == id && !record.claimed));
    update.control_wait(wait.id, WaitAction::Cancel)?;
    assert!(matches!(
        update.wait_for_idle(&app, &wait, None).await?,
        WaitOutcome::Cancelled
    ));
    drop(wait);
    until(|| prompt_count(&workspace, "manual task marker") == 1).await?;
    until(|| {
        workspace
            .drain
            .records(&workspace.state_dir)
            .is_ok_and(|records| records.iter().all(|record| record.id != id))
    })
    .await?;
    assert_eq!(prompt_count(&workspace, "manual task marker"), 1);
    Ok(())
}

#[tokio::test]
async fn shutdown_drain_checkpoints_a_memory_only_tool_result() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let item = "tool".to_owned();
    workspace.hub.publish(ServerEvent::ToolCall {
        session: session.clone(), item: item.clone(), actor: Actor::Claude,
        name: "Read".into(), input: serde_json::json!({}),
    });
    let path = workspace.hub.thread_path(&session)?;
    let payloads = path.parent().unwrap().join(fuzukue_proto::THREAD_SAVE_DIR).join("payloads");
    fs::write(&payloads, "temporarily obstruct sidecar writes")?;
    workspace.hub.disable_journal_writes(&workspace.state_dir)?;
    let output = "synthetic output ".repeat(700);
    workspace.hub.publish(ServerEvent::ToolResult {
        session: session.clone(), item: item.clone(), ok: false, output: output.clone(),
    });
    workspace.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: false });

    let lock = std::fs::File::open(payloads.parent().unwrap())?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    update.request_shutdown(app.clone(), Duration::from_secs(5), true)?;
    until(|| update.drain_status.borrow().as_ref().is_some_and(|status| {
        status.message.as_deref().is_some_and(|message| message.contains("could not save dirty transcripts"))
    })).await?;
    assert!(!*update.shutdown_ready.borrow());
    drop(lock);
    fs::remove_file(payloads)?;
    until(|| *update.shutdown_ready.borrow()).await?;

    let no_journal = tempfile::tempdir()?;
    let hub = crate::Hub::with_timeline(no_journal.path());
    hub.load_threads(&workspace.project_dir, no_journal.path());
    assert_eq!(hub.tool_detail(&session, &item).unwrap().output()?, Some(output));
    Ok(())
}

#[tokio::test]
async fn safe_close_reports_active_threads_and_force_commits_only_the_matching_shutdown() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });
    let work = app.shutdown_work();
    assert_eq!(work.jobs_running, 1);
    assert_eq!(work.threads[0].id, session);

    let job = workspace.admit_work()?;
    let error = update.request_shutdown(app.clone(), Duration::from_secs(60), false).unwrap_err();
    assert!(error.to_string().contains("1 active turns"));
    assert!(!*update.shutdown_ready.borrow());

    drop(job);
    workspace.hub.publish(ServerEvent::TurnEnded { session, actor: Actor::Claude });
    update.request_shutdown(app.clone(), Duration::from_secs(60), false)?;
    let id = update.drain_status.borrow().as_ref().unwrap().started_at;
    assert!(update.force_shutdown(&app, Some(id - 1)).await.is_err());
    assert!(!*update.shutdown_ready.borrow());
    update.force_shutdown(&app, Some(id)).await?;
    assert!(*update.shutdown_ready.borrow());
    assert!(workspace.admit_work().is_err());
    assert_eq!(update.drain_status.borrow().as_ref().unwrap().progress, DrainProgress::Ready);
    assert!(update.cancel_shutdown(id).is_err());
    tokio::task::yield_now().await;
    assert_eq!(update.drain_status.borrow().as_ref().unwrap().progress, DrainProgress::Ready);

    let other = tempfile::tempdir()?;
    let (app, update, workspace) = load(other.path())?;
    let _job = workspace.admit_work()?;
    let (entered, running) = tokio::sync::oneshot::channel();
    let mutation = tokio::spawn(async move {
        let mutation = crate::mutations::Mutation::new(&workspace, "", "Shutdown fixture".into())?;
        mutation.run(async {
            mutation.begin_write();
            let _ = entered.send(());
            std::future::pending::<Result<()>>().await
        }).await
    });
    running.await?;
    update.force_shutdown(&app, None).await?;
    assert!(*update.shutdown_ready.borrow());
    assert!(app.loaded(None)?.admit_work().is_err());
    assert!(mutation.await?.unwrap_err().to_string().contains("needs review"));
    Ok(())
}

#[tokio::test]
async fn shutdown_restart_waits_for_trial_and_usage_then_replays_input_and_goal_once() -> Result<()>
{
    // The supervised worker can inherit a ready marker, which makes the fixture
    // start in a pending trial. Isolate the test without changing other tests' environment.
    const SERVICE_ENV: [&str; 6] = [
        "FUZUKUE_SERVICE",
        "FUZUKUE_SELF_UPDATE_READY",
        "FUZUKUE_SELF_UPDATE_SOURCE",
        "LABO_SERVICE",
        "LABO_SELF_UPDATE_READY",
        "LABO_SELF_UPDATE_SOURCE",
    ];
    if SERVICE_ENV.iter().any(|key| std::env::var_os(key).is_some()) {
        const TEST: &str =
            "self_update::drain_tests::shutdown_restart_waits_for_trial_and_usage_then_replays_input_and_goal_once";
        let mut child = tokio::process::Command::new(std::env::current_exe()?);
        child.args(["--exact", TEST, "--nocapture"]).kill_on_drop(true);
        for key in SERVICE_ENV {
            child.env_remove(key);
        }
        let output = child.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        ensure!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let goal = workspace.hub.edit_threads(|threads| threads.create(None))?;
    crate::planner::write_test_plan(&workspace, &goal)?;
    let reset = crate::usage::now() + 3600;
    workspace
        .usage
        .update_claude(&serde_json::json!({"unifiedWindows":{"five_hour":{
            "utilization":0.99, "resetsAt":reset, "status":"rejected"
        }}}));
    let job = workspace.admit_work()?;
    update.request_shutdown(app.clone(), Duration::from_millis(100), true)?;
    assert!(workspace.admit_work().is_err());
    crate::handle(
        &workspace,
        ClientMsg::Prompt {
            session: session.clone(),
            text: "held input marker".into(),
        },
    )
    .await?;
    delegate::resume_goal(&workspace, &goal);
    delegate::resume_goal(&workspace, &goal);
    until(|| *update.shutdown_ready.borrow()).await?;
    assert_eq!(
        update.drain_status.borrow().as_ref().unwrap().progress,
        DrainProgress::TimedOut
    );
    assert!(workspace.admit_work().is_err());
    assert_eq!(prompt_count(&workspace, "held input marker"), 0);
    drop(job);

    // A new registry represents the selected process after startup or rollback.
    let new_app = crate::App::new(
        dir.path().into(),
        workspace.project_dir.clone(),
        workspace.state_dir.clone(),
        "127.0.0.1:0".into(),
        None,
    )?;
    install_desk_fixture(&new_app, dir.path())?;
    let next = Arc::new(Update::with_paths(
        Config::default(),
        None,
        workspace.project_dir.clone(),
        dir.path().join("next-builds"),
        PathBuf::new(),
    ));
    next.trial_pending
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(new_app.self_update.set(next.clone()).is_ok());
    let new_workspace = new_app.select(None)?;
    assert_eq!(
        new_workspace
            .usage
            .blocked("claude")
            .map(|(until, _)| until),
        Some(reset)
    );
    let held = new_workspace.drain.records(&new_workspace.state_dir)?;
    assert_eq!(
        held.iter()
            .filter(|record| matches!(record.work, drain::Work::Desk(_)))
            .count(),
        1
    );
    drain::start(&new_workspace);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(prompt_count(&new_workspace, "held input marker"), 0);
    next.trial_pending
        .store(false, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(prompt_count(&new_workspace, "held input marker"), 0);
    assert_eq!(prompt_count(&new_workspace, "Resume test goal"), 0);

    // Expire the restored window explicitly so restart can take as long as needed.
    new_workspace
        .usage
        .update_claude(&serde_json::json!({"unifiedWindows":{"five_hour":{
            "utilization":0.99, "resetsAt":crate::usage::now() - 1, "status":"rejected"
        }}}));
    assert!(new_workspace.usage.blocked("claude").is_none());
    until(|| prompt_count(&new_workspace, "held input marker") == 1).await?;
    until(|| prompt_count(&new_workspace, "Resume test goal") == 1).await?;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(prompt_count(&new_workspace, "held input marker"), 1);
    assert_eq!(prompt_count(&new_workspace, "Resume test goal"), 1);
    Ok(())
}

#[tokio::test]
async fn legacy_rollback_protects_a_goal_until_a_capable_server_reopens_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    crate::planner::write_test_plan(&workspace, &session)?;
    let old = dir.path().join("old-build");
    fs::write(&old, "#!/bin/sh\nexit 0")?;
    assert!(!drain::supports(&old)?);
    let marker = dir.path().join("compatible-build");
    fs::write(
        &marker,
        [vec![b' '; 65530], drain::FORMAT_MARKER.as_bytes().to_vec()].concat(),
    )?;
    assert!(drain::supports(&marker)?);
    let wait = begin(&update)?;
    app.protect_legacy_rollback()?;
    let plan = workspace
        .project_dir
        .join("threads")
        .join(&session)
        .join("plan.md");
    assert!(fs::read_to_string(&plan)?.contains("waiting_for_user"));
    assert!(fs::read_to_string(workspace.hub.thread_path(&session)?)?.contains("held-work.json"));
    let old_process_journal = drain::Journal::default();
    assert!(old_process_journal
        .records(&workspace.state_dir)?
        .iter()
        .any(|record| matches!(record.work, drain::Work::LegacyPlan(_))));
    drop(wait);
    drain::start(&workspace);
    until(|| fs::read_to_string(&plan).is_ok_and(|plan| !plan.contains("waiting_for_user")))
        .await?;
    Ok(())
}

#[tokio::test]
async fn actual_signals_close_admission_then_exit_on_the_second_signal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "self_update::drain_tests::signal_child",
            "--ignored",
            "--nocapture",
        ])
        .env("FUZUKUE_DRAIN_TEST_ROOT", dir.path())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().unwrap() as libc::pid_t;
    until(|| dir.path().join("signal-ready").is_file())
        .await
        .context("child registering signal handlers")?;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    until(|| dir.path().join("signal-draining").is_file())
        .await
        .context("child saving held input after SIGTERM")?;
    assert!(child.try_wait()?.is_none());
    let journal = drain::Journal::default().records(&dir.path().join("state"))?;
    assert!(journal
        .iter()
        .any(|record| matches!(record.work, drain::Work::Prompt { .. })));
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    assert!(tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await??
        .success());
    Ok(())
}

#[tokio::test]
#[ignore = "spawned by the signal integration test"]
async fn signal_child() -> Result<()> {
    let dir = PathBuf::from(
        std::env::var_os("FUZUKUE_DRAIN_TEST_ROOT").context("missing signal test root")?,
    );
    let (app, update, workspace) = load(&dir)?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let work = workspace.admit_work()?;
    workspace.hub.publish(ServerEvent::TurnActivity {
        session: session.clone(),
        running: true,
    });
    let mut progress = update.subscribe_drain();
    let handle = tokio::spawn(crate::handle_shutdown_signals(
        crate::ShutdownSignals::new()?,
        update,
        app,
        Duration::from_secs(60),
    ));
    fs::write(dir.join("signal-ready"), "")?;
    progress
        .wait_for(|state| {
            state.as_ref().is_some_and(|state| {
                state.reason == DrainReason::Shutdown && state.jobs_running >= 1
            })
        })
        .await?;
    assert!(workspace.admit_work().is_err());
    crate::handle(
        &workspace,
        ClientMsg::Prompt {
            session,
            text: "saved before second signal".into(),
        },
    )
    .await?;
    fs::write(dir.join("signal-draining"), "")?;
    handle.await?;
    drop(work);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn force_timeout_leaves_admission_closed_and_allows_cancel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    workspace.hub.publish(ServerEvent::TurnActivity {
        session,
        running: true,
    });
    let wait = begin(&update)?;
    let mut statuses = update.subscribe_drain();
    let action = async {
        statuses
            .wait_for(|status| {
                status
                    .as_ref()
                    .is_some_and(|status| status.jobs_running == 1)
            })
            .await?;
        update.control_wait(wait.id, WaitAction::Force)?;
        statuses
            .wait_for(|status| {
                status
                    .as_ref()
                    .is_some_and(|status| status.progress == DrainProgress::Forcing)
            })
            .await?;
        tokio::time::advance(Duration::from_secs(16)).await;
        statuses
            .wait_for(|status| {
                status
                    .as_ref()
                    .and_then(|status| status.message.as_deref())
                    .is_some_and(|message| message.contains("Timed out"))
            })
            .await?;
        assert!(workspace.admit_work().is_err());
        update.control_wait(wait.id, WaitAction::Cancel)?;
        Ok::<_, anyhow::Error>(())
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(update.wait_for_idle(&app, &wait, None), action)
    })
    .await??;
    assert!(matches!(outcome, WaitOutcome::Cancelled));
    Ok(())
}

#[tokio::test]
async fn explicit_update_force_reaps_unresponsive_claude_and_preserves_held_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, mut update) = tests::waiting_fixture(dir.path())?;
    update.config = toml::from_str("wait_stall_seconds = 30")?;
    let update = Arc::new(update);
    assert!(app.self_update.set(update.clone()).is_ok());
    install_desk_fixture(&app, dir.path())?;
    fs::write(dir.path().join("mock-claude"), r#"#!/bin/sh
case "$1" in --help) echo 'Claude'; exit 0;; esac
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fixture"}'
while IFS= read -r frame; do
    printf '%s\n' "$frame" >> prompts
done
"#)?;
    let workspace = app.select(None)?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let prompt = |text: &str| ClientMsg::Prompt { session: session.clone(), text: text.into() };
    crate::handle(&workspace, prompt("stuck active input")).await?;
    until(|| prompt_count(&workspace, "stuck active input") == 1).await?;
    crate::handle(&workspace, prompt("queued before drain")).await?;
    let wait = begin(&update)?;
    crate::handle(&workspace, prompt("held during drain")).await?;
    let mut status = update.subscribe_drain();

    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(12), async {
        tokio::try_join!(update.wait_for_idle(&app, &wait, None), async {
            status.wait_for(|status| status.as_ref().is_some_and(|status| status.jobs_running > 0)).await?;
            update.control_wait(wait.id, WaitAction::Force)?;
            status.wait_for(|status| status.as_ref().is_some_and(|status| status.progress == DrainProgress::Forcing)).await?;
            Ok::<_, anyhow::Error>(())
        })
    }).await??;
    assert!(matches!(outcome, WaitOutcome::Ready(_)));
    assert!(workspace.admit_work().is_err());
    assert!(!workspace.hub.thread_running(&session));
    assert_eq!(prompt_count(&workspace, r#""subtype":"interrupt""#), 1);
    assert_eq!(prompt_count(&workspace, "queued before drain"), 0);
    assert_eq!(prompt_count(&workspace, "held during drain"), 0);

    // Read the records from disk as a replacement server would.
    let records = drain::Journal::default().records(&workspace.state_dir)?;
    assert_eq!(records.iter().filter(|record| record.claimed && record.review.is_some()).count(), 1);
    let saved_inputs = records.iter().filter_map(|record| {
        (!record.claimed && matches!(record.work, drain::Work::Claude(_) | drain::Work::Prompt { .. }))
            .then(|| serde_json::to_string(&record.work))
    }).collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(saved_inputs.len(), 2);
    assert!(saved_inputs.iter().any(|input| input.contains("queued before drain")));
    assert!(saved_inputs.iter().any(|input| input.contains("held during drain")));
    assert!(records.iter().any(|record| matches!(&record.work, drain::Work::Checkpoint { result, .. }
        if result.contains("Review required before resuming"))));
    Ok(())
}

#[tokio::test]
async fn claude_result_during_drain_does_not_start_a_retirement_turn_or_queued_input() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    fs::write(dir.path().join("mock-claude"), r#"#!/usr/bin/python3
import json, sys, time
from pathlib import Path

print(json.dumps({"type":"system", "subtype":"init", "session_id":"fixture"}), flush=True)
for line in sys.stdin:
    with open("prompts", "a") as out:
        out.write(line)
    if "original active input" in line:
        while not Path("finish-turn").exists():
            time.sleep(0.01)
        print(json.dumps({"type":"result","result":"checkpoint result","is_error":False}), flush=True)
"#)?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let prompt = |text: &str| ClientMsg::Prompt { session: session.clone(), text: text.into() };
    crate::handle(&workspace, prompt("original active input")).await?;
    until(|| prompt_count(&workspace, "original active input") == 1).await?;
    crate::handle(&workspace, prompt("original queued input")).await?;
    let wait = begin(&update)?;
    let mut status = update.subscribe_drain();
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(update.wait_for_idle(&app, &wait, None), async {
            status.wait_for(|status| status.as_ref().is_some_and(|status| {
                status.blocking.blockers.iter().any(|blocker| blocker.kind == SelfUpdateBlockerKind::Turn)
            })).await?;
            fs::write(workspace.project_dir.join("finish-turn"), "")?;
            Ok::<_, anyhow::Error>(())
        })
    }).await??;
    assert!(matches!(outcome, WaitOutcome::Ready(_)));
    assert_eq!(prompt_count(&workspace, r#""type":"user""#), 1);
    assert!(!workspace.hub.thread_running(&session));
    let records = workspace.drain.records(&workspace.state_dir)?;
    assert!(records.iter().any(|r| matches!(&r.work, drain::Work::Checkpoint { result, .. }
        if result.contains("Finished.") && result.contains("checkpoint result"))));
    assert_eq!(records.iter().filter(|r| !r.claimed && matches!(r.work, drain::Work::Claude(_))).count(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn update_progress_resets_the_stall_timeout_then_aborts_without_force() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, mut update) = tests::waiting_fixture(dir.path())?;
    update.config.wait_stall_seconds = 5;
    let update = Arc::new(update);
    assert!(app.self_update.set(update.clone()).is_ok());
    let workspace = app.select(None)?;
    let finished = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let stuck = workspace.hub.edit_threads(|threads| threads.create(None))?;
    for session in [&finished, &stuck] {
        workspace.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });
    }
    let wait = begin(&update)?;
    let mut status = update.subscribe_drain();
    let action = async {
        status.wait_for(|status| status.as_ref().is_some_and(|status| status.jobs_running == 2)).await?;
        tokio::time::advance(Duration::from_secs(4)).await;
        workspace.hub.publish(ServerEvent::TurnEnded { session: finished, actor: Actor::Claude });
        status.wait_for(|status| status.as_ref().is_some_and(|status| status.jobs_running == 1)).await?;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(status.borrow().as_ref().unwrap().progress, DrainProgress::Draining);
        workspace.hub.publish(ServerEvent::TextDelta {
            session: stuck.clone(), item: "in-progress".into(), actor: Actor::Claude, text: "still working".into(),
        });
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(status.borrow().as_ref().unwrap().progress, DrainProgress::Draining);
        workspace.hub.publish(ServerEvent::TextDelta {
            session: stuck, item: "queued".into(), actor: Actor::System, text: "input held".into(),
        });
        tokio::time::advance(Duration::from_secs(6)).await;
        Ok::<_, anyhow::Error>(())
    };
    let (outcome, action) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(update.wait_for_idle(&app, &wait, None), action)
    }).await?;
    action?;
    let Err(error) = outcome else { panic!("a stalled drain must abort") };
    assert!(error.to_string().contains("drain stalled"));
    assert_ne!(status.borrow().as_ref().unwrap().progress, DrainProgress::Forcing);
    drop(wait);
    assert!(workspace.admit_work().is_ok());
    Ok(())
}

#[tokio::test]
async fn local_wait_control_requires_direct_loopback_current_id_and_force_confirmation() -> Result<()> {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    let dir = tempfile::tempdir()?;
    let (app, update, _) = load(dir.path())?;
    let access = Arc::new(crate::remote::Access::new(dir.path(), Default::default())?);
    let router = crate::router(app.clone(), access);
    let wait = begin(&update)?;
    let send = |peer: &str, mode, host, origin: Option<&str>, payload: Option<serde_json::Value>| {
        let mut request = Request::builder()
            .uri(wait_route::PATH)
            .header("host", host)
            .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()))
            .extension(mode);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let request = if let Some(payload) = payload {
            request.method("POST").header("content-type", "application/json").body(Body::from(payload.to_string()))
        } else {
            request.body(Body::empty())
        };
        router.clone().oneshot(request.unwrap())
    };
    use crate::remote::ListenMode::{Direct, Proxy};
    let peer = "127.0.0.1:1";
    let host = "127.0.0.1:7878";
    let response = send(peer, Direct, host, None, None).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let status: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1024).await?)?;
    assert_eq!(status, json!({"started_at": wait.id, "accepts_controls": true}));
    let force = json!({"started_at": wait.id, "action": "force", "confirm": true});
    for (peer, mode) in [("192.0.2.1:1", Direct), (peer, Proxy)] {
        assert_eq!(send(peer, mode, host, None, Some(force.clone())).await?.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(send(peer, Direct, "example.invalid", None, Some(force.clone())).await?.status(), StatusCode::FORBIDDEN);
    assert_eq!(send(peer, Direct, host, Some("https://example.invalid"), Some(force.clone())).await?.status(), StatusCode::FORBIDDEN);
    let unconfirmed = json!({"started_at": wait.id, "action": "force"});
    assert_eq!(send(peer, Direct, host, None, Some(unconfirmed)).await?.status(), StatusCode::BAD_REQUEST);
    let stale = json!({"started_at": wait.id - 1, "action": "cancel"});
    assert_eq!(send(peer, Direct, host, None, Some(stale)).await?.status(), StatusCode::CONFLICT);
    assert_eq!(send(peer, Direct, host, None, Some(force)).await?.status(), StatusCode::ACCEPTED);
    assert!(matches!(*update.wait_control.lock().unwrap(), WaitControl::Requested(id, DrainReason::Update, WaitAction::Force) if id == wait.id));

    drop(wait);
    let wait = begin(&update)?;
    let cancel = json!({"started_at": wait.id, "action": "cancel"});
    assert_eq!(send(peer, Direct, host, None, Some(cancel)).await?.status(), StatusCode::ACCEPTED);
    assert!(matches!(update.wait_for_idle(&app, &wait, None).await?, WaitOutcome::Cancelled));
    Ok(())
}

#[tokio::test]
async fn status_write_failure_reopens_admission_without_losing_held_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|threads| threads.create(None))?;
    let permit = workspace.admit_work()?;
    let wait = begin(&update)?;
    crate::handle(
        &workspace,
        ClientMsg::Prompt {
            session,
            text: "already saved".into(),
        },
    )
    .await?;
    fs::create_dir(update.root.join(STATUS_FILE))?;
    assert!(update.wait_for_idle(&app, &wait, None).await.is_err());
    drop(wait);
    drop(permit);
    assert!(workspace.admit_work().is_ok());
    assert_eq!(workspace.drain.records(&workspace.state_dir)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn signal_takes_over_an_update_that_has_not_handed_off() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (app, update, workspace) = load(dir.path())?;
    let wait = begin(&update)?;
    let WaitOutcome::Ready(gate) = update.wait_for_idle(&app, &wait, None).await? else {
        anyhow::bail!("update did not become ready");
    };
    update.request_shutdown(app.clone(), Duration::from_secs(5), true)?;
    let status = update.subscribe_drain().borrow().clone().unwrap();
    assert_eq!(status.reason, DrainReason::Shutdown);
    assert_ne!(status.started_at, wait.id);
    assert!(workspace.admit_work().is_err());
    drop(gate);
    drop(wait);
    until(|| *update.shutdown_ready.borrow()).await?;
    assert!(workspace.admit_work().is_err());
    Ok(())
}

#[tokio::test]
async fn compaction_admitted_after_draining_reopens() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_app, update, workspace) = load(dir.path())?;
    let session = workspace.hub.edit_threads(|store| store.create(None))?;
    let wait = begin(&update)?;

    crate::commands::dispatch(&workspace, &session, "/compact Keep decisions").await;
    crate::replay_prompts(&workspace).await;
    assert!(workspace.hub.compaction(&session).is_none());
    let records = workspace.drain.records(&workspace.state_dir)?;
    assert!(records.iter().any(|record| !record.claimed
        && matches!(&record.work, drain::Work::Compact { instructions } if instructions == "Keep decisions")));

    drop(wait);
    let (_, mut events) = workspace.hub.subscribe(u64::MAX);
    crate::replay_prompts(&workspace).await;
    // This thread has no backend session yet, so admission reaches the handler and it reports that.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::LocalNotice { localized: Some(message), .. } = events.recv().await?.event {
                assert_eq!(message.key, "command-failed");
                assert!(message.args["detail"].contains("Claude Code"));
                return Ok::<_, anyhow::Error>(());
            }
        }
    }).await??;
    Ok(())
}
