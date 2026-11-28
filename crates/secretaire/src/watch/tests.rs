use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::json;

use super::model::{
    Heartbeat, Input, Kind, Runtime, Wake, Watch, attention_line, report_matcher, report_number,
};
use super::poll::{self, Observation, Report, Sample};
use super::{Store, deliver, finish_poll};
use crate::{Workspace, git, hub::Hub, test_support::TestDir};

fn dots(repo: &Path, thread: &str) -> Watch {
    serde_json::from_value::<Input>(json!({
        "name": "dots-main", "repo": repo, "kind": "dots", "wake": "silent",
    }))
    .unwrap()
    .into_watch("test-dots".into(), repo, thread)
    .unwrap()
}

fn sample(head: &str, reports: &[u64], heartbeat: Option<Heartbeat>) -> Observation {
    Observation::Sources(vec![
        Ok(Sample::BranchHeads(if head.is_empty() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("work/dots-main".into(), head.into())])
        })),
        Ok(Sample::Reports(
            reports
                .iter()
                .map(|&number| Report {
                    number,
                    path: format!("notes/handoff/dots-cycle-{number}.md"),
                    text: format!("# C{number}\nSynthetic report"),
                })
                .collect(),
        )),
        Ok(Sample::Heartbeat(heartbeat)),
    ])
}

fn kinds(runtime: &Runtime) -> Vec<Kind> {
    runtime.outbox.iter().map(|event| event.kind).collect()
}

fn heartbeat(time: i64) -> Heartbeat {
    let time = DateTime::from_timestamp(time, 0).unwrap().to_rfc3339();
    Heartbeat::parse(&format!(
        "# dot heartbeat\n- cycle: 331\n- status: running\n- updated: {time}\n- activity: C331: synthetic fixture\n"
    )).unwrap()
}

#[test]
fn parses_heartbeat_markdown_milliseconds_alias_and_completed() -> Result<()> {
    let parsed = Heartbeat::parse(
        "# dot heartbeat\n- CyClE: 331\n- Status: running\n- Started: 2026-11-21T22:33:42Z\n- UpDaTeD: 2026-11-21T23:13:16.775Z\n- activity: C331: still running",
    )?;
    assert_eq!(parsed.cycle, Some(331));
    assert_eq!(parsed.status, "running");
    assert_eq!(parsed.activity, "C331: still running");
    assert_eq!(
        parsed.updated,
        Some(DateTime::parse_from_rfc3339("2026-11-21T23:13:16.775Z")?.timestamp())
    );
    let alias = Heartbeat::parse("heartbeat_utc: 2026-11-22T08:13:16+09:00")?;
    assert_eq!(alias.updated, parsed.updated);
    assert_eq!(Heartbeat::parse("- status: COMPLETED")?.status, "completed");
    assert!(Heartbeat::parse("- updated: invalid").is_err());
    Ok(())
}

#[test]
fn extracts_report_numbers_with_glob_regex_and_audit_preset() -> Result<()> {
    let matcher = report_matcher(Some("notes/handoff/dots-cycle-*.md"), None)?;
    let mut numbers: Vec<_> = ["10", "3", "002", "other"]
        .iter()
        .filter_map(|name| report_number(&matcher, &format!("notes/handoff/dots-cycle-{name}.md")))
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, [2, 3, 10]);
    assert_eq!(
        report_number(&matcher, "other/notes/handoff/dots-cycle-30.md"),
        None
    );
    let matcher = report_matcher(None, Some(r"^(?:notes/)?cycle-([0-9]+)\.md$"))?;
    assert_eq!(report_number(&matcher, "notes/cycle-314.md"), Some(314));
    assert!(report_matcher(None, Some(r"^(notes/)?cycle-([0-9]+)$")).is_err());

    let input: Input =
        serde_json::from_value(json!({"name":"dots-audit","repo":".","kind":"dots"}))?;
    let watch = input.into_watch("audit".into(), Path::new("/fixture"), "thread")?;
    let source = serde_json::to_value(&watch.sources[1])?;
    assert_eq!(source["branch"], "work/dots-audit");
    assert_eq!(source["glob"], "notes/audit/cycle-*.md");
    assert_eq!(source["order"], "number");
    Ok(())
}

#[test]
fn baseline_and_branch_report_heartbeat_transitions_are_once_only() -> Result<()> {
    let watch = dots(Path::new("/fixture"), "thread");
    let mut state = Runtime::default();
    let now = 10_000;
    state.apply(
        &watch,
        sample("old-sha", &[1, 2], Some(heartbeat(now))),
        now,
    );
    assert!(kinds(&state).is_empty());
    state.apply(
        &watch,
        sample("new-sha", &[2, 3, 10], Some(heartbeat(now))),
        now + 25 * 60,
    );
    assert_eq!(
        kinds(&state),
        [Kind::BranchAdvanced, Kind::Report, Kind::Report]
    );
    assert!(state.outbox[1].path.as_deref().unwrap().ends_with("-3.md"));
    assert!(state.outbox[2].path.as_deref().unwrap().ends_with("-10.md"));
    state.outbox.clear();

    let stale = now + 25 * 60 + 1;
    state.apply(
        &watch,
        sample("new-sha", &[10], Some(heartbeat(now))),
        stale,
    );
    state.apply(&watch, sample("new-sha", &[10], None), stale + 100);
    assert_eq!(kinds(&state), [Kind::Stale]);
    state.outbox.clear();
    let mut state: Runtime = serde_json::from_slice(&serde_json::to_vec(&state)?)?;
    let completed = Heartbeat::parse("- status: completed\n- updated: 1970-01-01T00:00:00Z")?;
    state.apply(
        &watch,
        sample("new-sha", &[10], Some(completed.clone())),
        stale + 101,
    );
    state.apply(
        &watch,
        sample("new-sha", &[10], Some(completed)),
        stale + 200,
    );
    assert_eq!(kinds(&state), [Kind::AliveAgain]);

    let mut state = Runtime::default();
    state.apply(&watch, sample("", &[], None), now);
    assert!(state.outbox.is_empty());
    state.apply(&watch, sample("first", &[1], Some(heartbeat(now))), now);
    assert_eq!(kinds(&state), [Kind::BranchCreated, Kind::Report]);
    Ok(())
}

#[test]
fn third_error_once_per_streak_backoff_and_missing_repo_pause() {
    let watch = dots(Path::new("/fixture"), "thread");
    let mut state = Runtime::default();
    for attempt in 1..=15 {
        state.apply(
            &watch,
            Observation::RepoError("synthetic timeout".into()),
            attempt,
        );
        assert_eq!(state.outbox.len(), usize::from(attempt >= 3));
    }
    assert_eq!(kinds(&state), [Kind::Error]);
    assert_eq!(state.delay(60), 600);
    state.apply(&watch, sample("", &[], None), 16);
    assert_eq!(state.failures, 0);
    assert_eq!(state.delay(60), 60);
    assert!(state.last_error.is_none());
    state.apply(&watch, Observation::MissingRepo, 17);
    assert_eq!(state.paused, Some(true));
    assert!(state.last_error.is_some());
}

#[test]
fn attention_requires_explicit_marked_lines_and_supports_custom_substrings() -> Result<()> {
    let mut watch = dots(Path::new("/fixture"), "thread");
    watch.wake = Wake::Attention;
    let mut state = Runtime::default();
    state.apply(&watch, sample("sha", &[1], None), 100);
    let cases = [
        (
            "# C330: owner integration read\n- owner-specific authorization",
            false,
        ),
        ("本文で owner decision needed に触れた。", false),
        ("## owner decision required", true),
        ("- NEEDS OWNER", true),
        ("1. questions for owner", true),
        ("- **オーナー判断** の対象", true),
        ("## 決定待ち", true),
        ("\x60\x60\x60md\n## 決定待ち\n\x60\x60\x60", false),
    ];
    for (offset, (report, expected)) in cases.into_iter().enumerate() {
        let sample = Observation::Sources(vec![
            Ok(Sample::BranchHeads(BTreeMap::from([(
                "work/dots-main".into(),
                "sha".into(),
            )]))),
            Ok(Sample::Reports(vec![Report {
                number: offset as u64 + 2,
                path: "sample.md".into(),
                text: report.into(),
            }])),
            Ok(Sample::Heartbeat(None)),
        ]);
        state.apply(&watch, sample, 101);
        let event = state.outbox.pop_front().unwrap();
        let mut delivery = Runtime::default();
        delivery.record(&watch, event);
        assert_eq!(!delivery.wake_digest.is_empty(), expected, "{report}");
    }
    assert_eq!(
        attention_line("before\nroute *B*\n", &["ROUTE *b*".into()], false),
        Some(1)
    );
    assert_eq!(
        attention_line("before\nroute *B*\n", &["ROUTE *b*".into()], true),
        None
    );
    Ok(())
}

#[test]
fn wake_policy_and_persisted_rate_limit() -> Result<()> {
    let mut watch = dots(Path::new("/fixture"), "thread");
    let mut state = Runtime::default();
    state.apply(&watch, sample("", &[], Some(heartbeat(1))), 1);
    let mut next = sample("first", &[2, 3], Some(heartbeat(1)));
    if let Observation::Sources(samples) = &mut next {
        if let Ok(Sample::Reports(reports)) = &mut samples[1] {
            reports[1].text = format!(
                "{}## 決定待ち\nA or B\nB を推奨。",
                "synthetic line\n".repeat(45)
            );
        }
    }
    state.apply(&watch, next, 2000);
    state.apply(
        &watch,
        sample("advanced", &[3], Some(heartbeat(2001))),
        2001,
    );
    for attempt in 2002..=2004 {
        state.apply(&watch, Observation::RepoError("timeout".into()), attempt);
    }

    for (event, attention) in state
        .outbox
        .iter()
        .cloned()
        .zip([false, false, true, true, false, true, true])
    {
        for (policy, expected) in [
            (Wake::All, true),
            (Wake::Attention, attention),
            (Wake::Silent, false),
        ] {
            watch.wake = policy;
            let mut state = Runtime::default();
            state.record(&watch, event.clone());
            assert_eq!(
                state.take_wake(1).is_some(),
                expected,
                "{:?} under {policy:?}",
                event.kind
            );
            if expected
                && event
                    .path
                    .as_deref()
                    .is_some_and(|path| path.ends_with("-3.md"))
            {
                state.record(&watch, event.clone());
                assert!(state.wake_digest.contains("B を推奨"));
            }
        }
    }

    watch.wake = Wake::All;
    let mut state2 = Runtime::default();
    let mut events = state.outbox;
    state2.record(&watch, events.pop_front().unwrap());
    assert!(state2.take_wake(100).is_some());
    state2.record(&watch, events.pop_front().unwrap());
    state2.record(&watch, events.pop_front().unwrap());
    let mut reloaded: Runtime = serde_json::from_slice(&serde_json::to_vec(&state2)?)?;
    assert!(reloaded.take_wake(699).is_none());
    let digest = reloaded.take_wake(700).unwrap();
    assert!(digest.contains("A or B"));
    assert!(digest.contains("dots-cycle-2.md"));
    assert!(digest.chars().count() <= 6000);
    assert!(reloaded.take_wake(1300).is_none());
    Ok(())
}

#[test]
fn wakes_keep_the_batch_and_all_item_ids_until_a_durable_outcome() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let mut watch = dots(root.path(), &thread);
    watch.wake = Wake::All;
    let mut runtime = Runtime::default();
    runtime.apply(&watch, sample("", &[], None), 1);
    runtime.apply(&watch, sample("new-sha", &[2], None), 2);
    for event in std::mem::take(&mut runtime.outbox) {
        runtime.record(&watch, event);
    }
    app.watches.lock()?.edit(|state| {
        state.watches.insert(watch.id.clone(), watch.clone());
        state.runtime.insert(watch.id.clone(), runtime);
    })?;

    let (id, prompt) = super::begin_wake(&app, &watch.id, &thread)?.unwrap();
    assert!(prompt.contains("new-sha"));
    assert!(prompt.contains("dots-cycle-2.md"));
    let pending = app.watches.lock()?.runtime(&watch).pending_wakes;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].items.len(), 2);
    assert!(pending[0].items.iter().all(|item| item.thread == thread));
    drop(app);

    let app = workspace(root.path());
    assert_eq!(super::begin_wake(&app, &watch.id, &thread)?.unwrap().0, id);

    // Simulate the crash after the reply was saved but before the ack.
    app.hub.save_note(&thread, &id.reply_item(), "A or B?".into(), fuzukue_proto::Actor::Claude, true)?;
    drop(app);
    let app = workspace(root.path());
    assert!(super::begin_wake(&app, &watch.id, &thread)?.is_none());
    assert_eq!(transcript(&app, &thread).matches("A or B?").count(), 1);
    assert!(app.watches.lock()?.runtime(&watch).pending_wakes.is_empty());
    Ok(())
}

fn workspace(root: &Path) -> Arc<Workspace> {
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let state = root.join("state");
    Arc::new(Workspace::new(
        project,
        String::new(),
        state.clone(),
        Arc::new(Hub::with_timeline(&state)),
    ))
}

fn enqueue(app: &Workspace, watch: &Watch) -> Result<String> {
    let mut store = app.watches.lock()?;
    let mut state = store.runtime(watch);
    state.apply(watch, sample("", &[], None), 1);
    state.apply(watch, sample("sha", &[], None), 2);
    let item = state.outbox.front().unwrap().item(&watch.id);
    store.edit(|data| {
        data.watches.insert(watch.id.clone(), watch.clone());
        data.runtime.insert(watch.id.clone(), state);
    })?;
    Ok(item)
}

fn transcript(app: &Workspace, thread: &str) -> String {
    fs::read_to_string(app.hub.thread_path(thread).unwrap()).unwrap()
}

#[test]
fn live_thread_append_is_idempotent_across_restarts() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let watch = dots(root.path(), &thread);
    let item = enqueue(&app, &watch)?;
    // The process exits after writing the thread note but before acknowledging the outbox.
    app.hub
        .deliver_system_note(&thread, &item, "saved once".into())?;
    drop(app);

    let app = workspace(root.path());
    let mut store = app.watches.lock()?;
    deliver(&app, &mut store, &watch)?;
    assert_eq!(transcript(&app, &thread).matches("saved once").count(), 1);
    assert_eq!(store.runtime(&watch).thread(&watch), thread);
    assert!(store.runtime(&watch).outbox.is_empty());
    assert!(store.runtime(&watch).last_event.is_some());
    let (replay, _) = app.hub.subscribe_client(0, true);
    assert_eq!(
        replay
            .iter()
            .filter(|event| matches!(&event.event,
        fuzukue_proto::ServerEvent::TextDelta { item: id, .. } if id == &item))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn watch_event_resumes_an_explicitly_paused_goal() -> Result<()> {
    use crate::planner;

    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    planner::write_test_plan(&app, &thread)?;
    planner::pause_goal(&app, &thread)?;
    let mut watch = dots(root.path(), &thread);
    watch.wake = Wake::Silent;
    enqueue(&app, &watch)?;
    let mut store = app.watches.lock()?;
    deliver(&app, &mut store, &watch)?;

    assert!(!planner::is_paused(&app, &thread));
    Ok(())
}

#[test]
fn archived_or_missing_thread_rebinds_and_leaves_backlink() -> Result<()> {
    for archived in [true, false] {
        let root = TestDir::new();
        let app = workspace(root.path());
        let thread = if archived {
            app.hub.edit_threads(|store| {
                let thread = store.create(None)?;
                store.set_archived(&thread, true)?;
                Ok(thread)
            })?
        } else {
            "thread-missing".into()
        };
        let watch = dots(root.path(), &thread);
        let item = enqueue(&app, &watch)?;
        let mut store = app.watches.lock()?;
        deliver(&app, &mut store, &watch)?;
        let runtime = store.runtime(&watch);
        let rebound = runtime.thread(&watch);
        assert_ne!(rebound, thread);
        assert!(
            app.hub
                .read_threads(|store| store.is_live(rebound) && store.has_item(rebound, &item))
        );
        let text = transcript(&app, rebound);
        assert!(text.contains("監視: dots-main"));
        let backlink = text
            .find(harness_text!("Previous watch thread", "前の監視スレッド"))
            .unwrap();
        assert!(text[backlink..].contains(&thread));
        assert!(backlink < text.find(&item).unwrap());
        if archived {
            assert!(text.contains("[[file:threads/"));
            assert!(!transcript(&app, &thread).contains(&item));
        }
    }
    Ok(())
}

#[test]
fn static_watches_keep_runtime_in_json_without_changing_toml() -> Result<()> {
    let root = TestDir::new();
    let config = r#"
[[watch]]
name = "dots-audit"
repo = "./fixture"
kind = "dots"

[[watch]]
name = "release reports"
repo = "."
[[watch.sources]]
kind = "new-file"
branch = "work/release"
glob = "reports/round-*.md"
order = "number"
"#;
    root.write("fuzukue.toml", config);
    let state_path = root.path().join("state");
    let mut store = Store::load(root.path(), &state_path)?;
    assert_eq!(store.watches().count(), 2);
    let id = store.get("dots-audit").unwrap().id.clone();
    store.edit(|state| state.runtime.entry(id.clone()).or_default().paused = Some(true))?;
    let reopened = Store::load(root.path(), &state_path)?;
    assert_eq!(
        reopened.runtime(reopened.get(&id).unwrap()).paused,
        Some(true)
    );
    assert!(reopened.state.watches.is_empty());
    assert_eq!(
        fs::read_to_string(root.path().join("fuzukue.toml"))?,
        config
    );
    Ok(())
}

async fn command(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git::run(repo, args, 64 * 1024).await?;
    ensure!(
        output.status.success(),
        "{}",
        git::error(&args.join(" "), &output)
    );
    Ok(String::from_utf8(output.stdout)?)
}

async fn push_cycle(root: &TestDir, author: &Path, number: u64, update: i64) -> Result<()> {
    let report = format!("author/notes/handoff/dots-cycle-{number}.md");
    root.write(
        &report,
        &format!("# C{number}: synthetic\n## 検証と次の狙い\nGit test fixture"),
    );
    let update = DateTime::from_timestamp(update, 0).unwrap().to_rfc3339();
    root.write("author/notes/heartbeat/dots-main.md",
        &format!("# dot heartbeat\n- cycle: {number}\n- status: running\n- heartbeat_utc: {update}\n- activity: synthetic\n"));
    command(author, &["add", "notes"]).await?;
    command(
        author,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-m",
            "Synthetic cycle",
        ],
    )
    .await?;
    command(
        author,
        &["push", "origin", "HEAD:refs/heads/work/dots-main"],
    )
    .await?;
    Ok(())
}

async fn observe_fixture(watch: &Watch, runtime: &Runtime, root: &Path) -> Result<Observation> {
    let samples = poll::observe_in(watch, runtime, Ok(root.to_owned()))
        .await?
        .unwrap();
    Ok(Observation::Sources(samples))
}

#[tokio::test]
async fn real_git_catches_up_after_restart_and_only_changes_private_refs() -> Result<()> {
    let root = TestDir::new();
    fs::create_dir(root.path().join("author"))?;
    command(root.path(), &["init", "--bare", "remote.git"]).await?;
    let author = root.path().join("author");
    command(&author, &["init", "-b", "work/dots-main"]).await?;
    command(&author, &["remote", "add", "origin", "../remote.git"]).await?;
    let now = Utc::now().timestamp();
    push_cycle(&root, &author, 1, now).await?;
    command(
        root.path(),
        &["clone", "-b", "work/dots-main", "remote.git", "clone"],
    )
    .await?;
    let clone = root.path().join("clone");
    command(&clone, &["fetch", "--no-tags", "origin"]).await?;
    command(&clone, &["config", "fetch.prune", "true"]).await?;
    command(&clone, &["config", "remote.origin.pruneTags", "true"]).await?;
    root.write(
        "clone/notes/handoff/dots-cycle-1.md",
        "local working tree change",
    );
    root.write("clone/untracked", "keep me");
    let git_status = command(&clone, &["status", "--porcelain"]).await?;
    let origin = command(&clone, &["for-each-ref", "refs/remotes/origin"]).await?;
    let fetch_head = fs::read(clone.join(".git/FETCH_HEAD"))?;
    let head = command(&clone, &["rev-parse", "HEAD"]).await?;

    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let watch = dots(&clone, &thread);
    app.watches
        .lock()?
        .edit(|state| state.watches.insert(watch.id.clone(), watch.clone()))?;
    let observed = observe_fixture(&watch, &Runtime::default(), root.path()).await?;
    finish_poll(&app, &watch, observed)?;
    assert!(app.watches.lock()?.runtime(&watch).outbox.is_empty());
    assert_eq!(
        head,
        command(&clone, &["rev-parse", "refs/fuzukue/watch/work/dots-main"]).await?
    );
    drop(app);

    push_cycle(&root, &author, 2, now - 26 * 60).await?;
    push_cycle(&root, &author, 10, now - 26 * 60).await?;
    let app = workspace(root.path());
    let before = app.watches.lock()?.runtime(&watch);
    finish_poll(
        &app,
        &watch,
        observe_fixture(&watch, &before, root.path()).await?,
    )?;
    {
        let mut store = app.watches.lock()?;
        assert_eq!(
            kinds(&store.runtime(&watch)),
            [
                Kind::BranchAdvanced,
                Kind::Report,
                Kind::Report,
                Kind::Stale
            ]
        );
        deliver(&app, &mut store, &watch)?;
    }
    assert_eq!(
        transcript(&app, &thread).matches("# C2: synthetic").count(),
        1
    );
    assert_eq!(
        transcript(&app, &thread)
            .matches("# C10: synthetic")
            .count(),
        1
    );
    drop(app);

    let app = workspace(root.path());
    let before = app.watches.lock()?.runtime(&watch);
    finish_poll(
        &app,
        &watch,
        observe_fixture(&watch, &before, root.path()).await?,
    )?;
    assert!(app.watches.lock()?.runtime(&watch).outbox.is_empty());
    assert_eq!(
        transcript(&app, &thread)
            .matches("# C10: synthetic")
            .count(),
        1
    );
    assert_eq!(
        command(&clone, &["rev-parse", "refs/fuzukue/watch/work/dots-main"]).await?,
        command(&author, &["rev-parse", "HEAD"]).await?
    );
    assert_eq!(command(&clone, &["rev-parse", "HEAD"]).await?, head);
    assert_eq!(
        command(&clone, &["for-each-ref", "refs/remotes/origin"]).await?,
        origin
    );
    assert_eq!(fs::read(clone.join(".git/FETCH_HEAD"))?, fetch_head);
    assert_eq!(
        command(&clone, &["status", "--porcelain"]).await?,
        git_status
    );
    assert_eq!(
        fs::read_to_string(clone.join("notes/handoff/dots-cycle-1.md"))?,
        "local working tree change"
    );

    let wildcard = serde_json::from_value::<Input>(json!({
        "name": "work branches", "repo": clone,
        "sources": [{"kind":"branch-heads","pattern":"refs/heads/work/*"}]
    }))?
    .into_watch("work-branches".into(), root.path(), &thread)?;
    let mut state = Runtime::default();
    let baseline = observe_fixture(&wildcard, &state, root.path()).await?;
    state.apply(&wildcard, baseline, now);
    command(&author, &["push", "origin", "HEAD:refs/heads/work/dots-T3"]).await?;
    let observed = observe_fixture(&wildcard, &state, root.path()).await?;
    state.apply(&wildcard, observed, now);
    assert_eq!(kinds(&state), [Kind::BranchCreated]);
    let created = state.outbox.pop_front().unwrap();
    assert!(created.summary.contains("work/dots-T3"));
    state.record(&wildcard, created);
    assert!(state.wake_digest.is_empty());

    fs::rename(&clone, root.path().join("relocated-clone"))?;
    assert!(
        poll::observe_in(&watch, &before, Ok(root.path().into()))
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn tool_actions_keep_previews_and_static_definitions_read_only() -> Result<()> {
    let root = TestDir::new();
    let config = "[[watch]]\nname = \"dots-main\"\nrepo = \"missing\"\nkind = \"dots\"\n";
    root.write("project/fuzukue.toml", config);
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let run = |args| {
        let app = &app;
        let thread = &thread;
        async move { super::call(app, thread, &args).await }
    };

    let listing = run(json!({"action":"list"})).await?;
    assert_eq!(listing["watches"][0]["name"], "dots-main");
    assert!(listing["watches"][0]["last_poll"].is_null());
    let invalid = run(json!({
        "action": "create", "name": "new-watch", "repo": ".", "kind": "dots", "interval": 14,
    }))
    .await
    .unwrap_err();
    assert!(invalid.to_string().contains("15"));
    assert_eq!(
        run(json!({"action":"test","name":"dots-main"})).await?["would_pause"],
        true
    );
    assert!(!app.state_dir.join("watch.json").exists());
    let state = run(json!({"action":"status","id":"dots-main"})).await?;
    assert_eq!(state["paused"], false);
    assert_eq!(state["sources"].as_array().unwrap().len(), 3);
    assert_eq!(
        run(json!({"action":"pause","id":"dots-main"})).await?["paused"],
        true
    );
    assert_eq!(
        run(json!({"action":"resume","id":"dots-main"})).await?["paused"],
        false
    );
    assert!(
        run(json!({"action":"delete","id":"dots-main"}))
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(app.project_dir.join("fuzukue.toml"))?,
        config
    );
    Ok(())
}

#[test]
fn startup_restores_an_unopened_registered_workspace_with_watches() -> Result<()> {
    let root = TestDir::new();
    for path in ["default", "watched", "ordinary"] {
        fs::create_dir(root.path().join(path))?;
    }
    root.write(
        "watched/fuzukue.toml",
        "[[watch]]\nname = \"dots-main\"\nkind = \"dots\"\nrepo = \".\"\n",
    );
    let create = || {
        crate::App::new(
            root.path().into(),
            root.path().join("default"),
            root.path().join("state"),
            String::new(),
            None,
        )
    };
    let registry = create()?;
    let watched = registry.register(root.path().join("watched").to_str().unwrap())?;
    let ordinary = registry.register(root.path().join("ordinary").to_str().unwrap())?;
    drop(registry);

    let registry = create()?;
    assert!(registry.loaded(Some(&watched)).is_err());
    registry.restore_watches();
    let app = registry.loaded(Some(&watched))?;
    assert!(app.watches.lock()?.get("dots-main").is_some());
    assert!(registry.loaded(Some(&ordinary)).is_err());
    Ok(())
}

#[tokio::test]
async fn watch_wake_held_by_drain_resumes_once_after_cancel() -> Result<()> {
    use std::time::Duration;
    use fuzukue_proto::{DrainProgress, DrainReason};

    let root = TestDir::new();
    let project = root.path().join("project");
    fs::create_dir(&project)?;
    let state = root.path().join("state");
    let registry = crate::App::new(root.path().into(), project.clone(), state, "127.0.0.1:0".into(), None)?;
    let update = crate::self_update::test_gate(root.path());
    assert!(registry.self_update.set(update.clone()).is_ok());
    let app = registry.select(None)?;
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let mut watch = dots(&project, &thread);
    watch.wake = Wake::All;
    let item = enqueue(&app, &watch)?;
    // Hold the drain open independently of the pending watch event.
    let permit = app.admit_work()?;
    let (id, drain) = update.test_update_drain(registry)?;
    super::schedule(&app)?;
    assert!(!transcript(&app, &thread).contains(&item));
    assert_eq!(app.watches.lock()?.runtime(&watch).outbox.len(), 1);

    let mut progress = update.subscribe_drain();
    assert_eq!(progress.borrow().as_ref().unwrap().reason, DrainReason::Update);
    update.control_wait(id, fuzukue_proto::SelfUpdateWaitAction::Cancel)?;
    tokio::time::timeout(Duration::from_secs(3), progress.wait_for(|status| status.as_ref()
        .is_some_and(|status| status.progress == DrainProgress::Cancelled))).await??;
    assert!(drain.await?.is_err());
    drop(permit);

    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", r#"
while IFS= read -r line; do
    printf '%s\n' "$line" >> watch-prompts
    printf '%s\n' '{"type":"result","is_error":false,"result":"watch seen"}'
done
"#]);
    let claude = crate::ClaudeSession::spawn_command(command, thread.clone(), project.clone(), &app, Box::new(|_| {}))?;
    app.sessions.lock().await.insert(thread.clone(), claude);
    let mut window = fuzukue_proto::UsageWindow {
        agent: "claude".into(),
        name: "test quota".into(),
        used: 1.0,
        resets_at: Some(crate::usage::now() + 600),
        rejected: true,
        recent_pace: None,
    };
    app.usage.restore(vec![window.clone()]);
    super::schedule(&app)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!project.join("watch-prompts").exists());
    assert!(!app.watches.lock()?.runtime(&watch).wake_digest.is_empty());
    window.used = 0.0;
    window.rejected = false;
    window.resets_at = Some(crate::usage::now() + 1200);
    app.usage.restore(vec![window]);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            super::schedule(&app)?;
            if project.join("watch-prompts").is_file() { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    super::schedule(&app)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fs::read_to_string(project.join("watch-prompts"))?.lines().count(), 1);
    assert_eq!(transcript(&app, &thread).matches(&item).count(), 1);
    Ok(())
}

pub(super) fn delivery_checkpoint(app: &Workspace, checkpoint: &str) {
    let Some(root) = std::env::var_os("FUZUKUE_TEST_WATCH_CRASH_DIR") else { return };
    if app.project_dir.parent() != Some(Path::new(&root))
        || std::env::var("FUZUKUE_TEST_WATCH_CRASH_BOUNDARY").as_deref() != Ok(checkpoint)
    {
        return;
    }
    fs::write(Path::new(&root).join("ready"), checkpoint).unwrap();
    loop {
        std::thread::park();
    }
}

async fn crash_test_backend(app: &Arc<Workspace>, thread: &str, reply: &str) -> Result<()> {
    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", r#"
while IFS= read -r line; do
    printf '%s\n' "$line" >> watch-prompts
    if [ "$2" = "after-start" ]; then
        : > ../ready
    else
        printf '%s\n' "$1"
    fi
done
"#, "watch-backend"])
        .arg(json!({"type": "result", "is_error": false, "result": reply}).to_string())
        .arg(std::env::var("FUZUKUE_TEST_WATCH_CRASH_BOUNDARY").unwrap_or_default());
    let claude = crate::ClaudeSession::spawn_command(
        command, thread.into(), app.project_dir.clone(), app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(thread.into(), claude);
    Ok(())
}

#[tokio::test]
async fn persisted_delivery_survives_process_kill_at_each_boundary() -> Result<()> {
    const REPLY: &str = "Choose A or B?";
    for (boundary, silent) in [
        ("after-note", false),
        ("after-start", false),
        ("after-acceptance", false),
        ("before-ack", false),
        ("before-ack", true),
    ] {
        let root = TestDir::new();
        let app = workspace(root.path());
        let thread = app.hub.edit_threads(|store| store.create(None))?;
        let mut watch = dots(&app.project_dir, &thread);
        watch.wake = Wake::All;
        watch.paused = true;
        let item = enqueue(&app, &watch)?;
        drop(app);
        let reply = if silent { "<silent/>" } else { REPLY };
        kill_at_checkpoint(root.path(), boundary, &thread, Some(reply)).await?;

        let app = workspace(root.path());
        let runtime = app.watches.lock()?.runtime(&watch);
        assert_eq!(transcript(&app, &thread).matches(&item).count(), 1, "{boundary}");
        if boundary == "after-note" {
            assert_eq!(runtime.outbox.len(), 1);
            assert!(runtime.pending_wakes.is_empty());
        } else {
            assert!(runtime.outbox.is_empty());
            let batch = &runtime.pending_wakes[0];
            assert_eq!(batch.items.len(), 1);
            assert_eq!(batch.items[0].item, item);
            assert_eq!(batch.items[0].thread, thread);
            assert_eq!(matches!(batch.acceptance, Some(super::WakeAcceptance::Silent)), silent);
            assert_eq!(batch.acceptance.is_some(), boundary != "after-start");
        }
        let saved_replies = usize::from(boundary == "before-ack" && !silent);
        assert_eq!(transcript(&app, &thread).matches(REPLY).count(), saved_replies, "{boundary}");

        crash_test_backend(&app, &thread, reply).await?;
        super::start(&app);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let runtime = app.watches.lock()?.runtime(&watch);
                if runtime.outbox.is_empty() && runtime.wake_digest.is_empty() && runtime.pending_wakes.is_empty() {
                    break Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        super::schedule(&app)?;
        let body = transcript(&app, &thread);
        assert_eq!(body.matches(&item).count(), 1, "{boundary}");
        assert_eq!(body.matches(REPLY).count(), usize::from(!silent), "{boundary}");
        assert!(!body.contains("<silent/>"));
        let prompts = fs::read_to_string(app.project_dir.join("watch-prompts"))?.lines().count();
        assert_eq!(prompts, if boundary == "after-start" { 2 } else { 1 }, "{boundary}");
    }
    Ok(())
}

#[tokio::test]
async fn watch_delivery_crash_child() -> Result<()> {
    let Some(root) = std::env::var_os("FUZUKUE_TEST_WATCH_CRASH_DIR") else { return Ok(()) };
    let app = workspace(Path::new(&root));
    if let Ok(reply) = std::env::var("FUZUKUE_TEST_WATCH_CRASH_REPLY") {
        let thread = std::env::var("FUZUKUE_TEST_WATCH_CRASH_THREAD")?;
        crash_test_backend(&app, &thread, &reply).await?;
    }
    super::start(&app);
    std::future::pending::<Result<()>>().await
}

async fn kill_at_checkpoint(root: &Path, boundary: &str, thread: &str, reply: Option<&str>) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(["--exact", "watch::tests::watch_delivery_crash_child"])
        .env("FUZUKUE_TEST_WATCH_CRASH_DIR", root)
        .env("FUZUKUE_TEST_WATCH_CRASH_BOUNDARY", boundary)
        .env("FUZUKUE_TEST_WATCH_CRASH_THREAD", thread)
        .env_remove("FUZUKUE_TEST_WATCH_CRASH_REPLY");
    if let Some(reply) = reply {
        command.env("FUZUKUE_TEST_WATCH_CRASH_REPLY", reply);
    }
    let mut child = command.spawn()?;
    let ready = root.join("ready");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !ready.is_file() && child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let reached = ready.is_file();
    let _ = child.kill();
    assert!(!child.wait()?.success());
    ensure!(reached, "child did not reach {boundary}");
    Ok(())
}

#[tokio::test]
async fn external_acceptance_is_durable_and_idempotent_before_and_after_delivery() -> Result<()> {
    use super::{ExternalAcceptance, accept_external_notification};

    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let other = app.hub.edit_threads(|store| store.create(None))?;
    let id = "scope/run/desk/1";
    let mut submissions = Vec::new();
    for _ in 0..2 {
        submissions.push(tokio::spawn(accept_external_notification(
            app.clone(), id.into(), thread.clone(), "External result".into(),
        )));
    }
    let mut accepted = 0;
    for submission in submissions {
        accepted += usize::from(submission.await?? == ExternalAcceptance::Accepted);
    }
    assert_eq!(accepted, 1);
    assert!(super::configured(&app.project_dir, &app.state_dir));
    assert!(!transcript(&app, &thread).contains("External result"));
    drop(app);

    kill_at_checkpoint(root.path(), "before-external-ack", &thread, None).await?;
    let app = workspace(root.path());
    assert_eq!(transcript(&app, &thread).matches("External result").count(), 1);
    assert_eq!(accept_external_notification(
        app.clone(), id.into(), other.clone(), "Conflicting retry".into(),
    ).await?, ExternalAcceptance::AlreadyAccepted);
    assert!(super::configured(&app.project_dir, &app.state_dir));
    super::schedule(&app)?;
    assert!(!super::configured(&app.project_dir, &app.state_dir));
    drop(app);

    let app = workspace(root.path());
    assert_eq!(accept_external_notification(
        app.clone(), id.into(), other.clone(), "Conflicting retry".into(),
    ).await?, ExternalAcceptance::AlreadyAccepted);
    super::schedule(&app)?;
    assert_eq!(transcript(&app, &thread).matches("External result").count(), 1);
    assert!(!transcript(&app, &other).contains("Conflicting retry"));
    Ok(())
}

#[test]
fn migrates_legacy_wake_digest_and_fences_previous_binary() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let mut watch = dots(root.path(), &thread);
    watch.wake = Wake::All;
    let legacy_path = app.state_dir.join("watch.json");
    let config = format!(r#"
[[watch]]
name = "static-dots"
repo = "."
kind = "dots"
wake = "all"
thread = "{thread}"
"#);
    fs::write(app.project_dir.join("fuzukue.toml"), &config)?;
    let static_id = "static:static-dots";
    fs::write(&legacy_path, serde_json::to_vec(&json!({
        "watches": { (watch.id.clone()): watch },
        "runtime": {
            (watch.id.clone()): { "wake_digest": "old pending text", "serial": 42 },
            (static_id): { "wake_digest": "old static pending text", "last_poll": 123, "paused": false }
        }
    }))?)?;
    drop(app);
    let app = workspace(root.path());
    let (id, prompt) = super::begin_wake(&app, &watch.id, &thread)?.unwrap();
    assert!(prompt.contains("old pending text"));
    let (static_batch, static_prompt) = super::begin_wake(&app, static_id, &thread)?.unwrap();
    assert!(static_prompt.contains("old static pending text"));
    super::protect_legacy_rollback(&app)?;

    // Simulate the previous build serializing its own Runtime and dropping new fields.
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&legacy_path)?)?;
    assert_eq!(legacy["runtime"][&watch.id]["paused"], true);
    assert_eq!(legacy["runtime"][&watch.id]["wake_digest"], "");
    assert_eq!(legacy["runtime"][static_id]["paused"], true);
    assert_eq!(legacy["runtime"][static_id]["wake_digest"], "");
    for entry in legacy["runtime"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("wake_items");
        entry.as_object_mut().unwrap().remove("pending_wakes");
    }
    legacy["runtime"].as_object_mut().unwrap().remove(super::ROLLBACK_FENCE);
    fs::write(legacy_path, serde_json::to_vec(&legacy)?)?;
    drop(app);
    let app = workspace(root.path());
    assert_eq!(fs::read_to_string(app.project_dir.join("fuzukue.toml"))?, config);
    {
        let store = app.watches.lock()?;
        assert_eq!(store.runtime(&watch).paused, None);
        let runtime = store.runtime(store.by_id(static_id).unwrap());
        assert_eq!(runtime.last_poll, Some(123));
        assert_eq!(runtime.paused, Some(false));
    }
    assert_eq!(super::begin_wake(&app, &watch.id, &thread)?.unwrap().0, id);
    assert_eq!(super::begin_wake(&app, static_id, &thread)?.unwrap().0, static_batch);
    super::persist_wake_reply(&app, &thread, id, "<silent/>", fuzukue_proto::Actor::Claude)?;
    super::persist_wake_reply(&app, &thread, static_batch, "Static reply", fuzukue_proto::Actor::Claude)?;
    assert!(!transcript(&app, &thread).contains(&id.reply_item()));
    assert_eq!(transcript(&app, &thread).matches("Static reply").count(), 1);
    assert!(app.watches.lock()?.runtime(&watch).pending_wakes.is_empty());
    Ok(())
}

#[test]
fn prepared_migration_defers_to_legacy_until_the_fence_is_saved() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let watch = dots(root.path(), &thread);
    let legacy = |digest| json!({
        "watches": { (watch.id.clone()): watch },
        "runtime": { (watch.id.clone()): { "wake_digest": digest } }
    });
    let mut prepared = legacy("earlier digest");
    prepared["migration_pending"] = json!(true);
    super::persist_state(
        &app.state_dir.join("watch-v1/watch.json"), &serde_json::from_value(prepared)?,
    )?;
    fs::write(app.state_dir.join("watch.json"), serde_json::to_vec(&legacy("updated digest"))?)?;
    drop(app);

    let app = workspace(root.path());
    assert_eq!(app.watches.lock()?.runtime(&watch).wake_digest, "updated digest");
    super::protect_legacy_rollback(&app)?;
    let app = workspace(root.path());
    assert_eq!(app.watches.lock()?.runtime(&watch).wake_digest, "updated digest");
    Ok(())
}

#[test]
fn a_retried_front_desk_acceptance_keeps_the_first_outcome() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let mut watch = dots(root.path(), &thread);
    watch.wake = Wake::All;
    let mut runtime = Runtime::default();
    runtime.wake_digest = "pending digest".into();
    app.watches.lock()?.edit(|state| {
        state.watches.insert(watch.id.clone(), watch.clone());
        state.runtime.insert(watch.id.clone(), runtime);
    })?;
    let (id, _) = super::begin_wake(&app, &watch.id, &thread)?.unwrap();
    let path = app.hub.thread_path(&thread)?;
    let unavailable = root.path().join("unavailable.md");
    fs::rename(&path, &unavailable)?;
    assert!(super::persist_wake_reply(&app, &thread, id, "First outcome", fuzukue_proto::Actor::Claude).is_err());
    assert!(app.watches.lock()?.runtime(&watch).pending_wakes[0].acceptance.is_some());
    fs::rename(&unavailable, &path)?;

    super::persist_wake_reply(&app, &thread, id, "Conflicting outcome", fuzukue_proto::Actor::Claude)?;
    super::persist_wake_reply(&app, &thread, id, "Conflicting outcome", fuzukue_proto::Actor::Claude)?;
    let body = transcript(&app, &thread);
    assert_eq!(body.matches("First outcome").count(), 1);
    assert!(!body.contains("Conflicting outcome"));
    assert!(app.watches.lock()?.runtime(&watch).pending_wakes.is_empty());
    Ok(())
}

#[test]
fn wake_batches_keep_unclipped_references_and_complete_in_order() -> Result<()> {
    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let mut watch = dots(root.path(), &thread);
    watch.wake = Wake::All;
    let mut runtime = Runtime::default();
    runtime.apply(&watch, sample("base", &[], None), 1);
    for number in 1..=80 {
        runtime.apply(&watch, sample(&format!("head-{number}"), &[number], None), 2);
    }
    let expected: Vec<_> = runtime.outbox.iter().map(|event| event.item(&watch.id)).collect();
    for event in std::mem::take(&mut runtime.outbox) {
        runtime.record(&watch, event);
    }
    let now = Utc::now().timestamp();
    assert!(runtime.seal_wake(now - 600));
    runtime.apply(&watch, sample("later", &[], None), 3);
    let event = runtime.outbox.pop_front().unwrap();
    runtime.record(&watch, event);
    assert!(runtime.seal_wake(now));
    let first = &runtime.pending_wakes[0];
    let second = &runtime.pending_wakes[1];
    assert_eq!(first.items.iter().map(|item| &item.item).collect::<Vec<_>>(), expected.iter().collect::<Vec<_>>());
    assert!(first.digest.chars().count() <= 6000);
    let (first, second) = (first.id, second.id);

    app.watches.lock()?.edit(|state| {
        state.watches.insert(watch.id.clone(), watch.clone());
        state.runtime.insert(watch.id.clone(), runtime);
    })?;
    drop(app);
    let app = workspace(root.path());
    assert_eq!(super::begin_wake(&app, &watch.id, &thread)?.unwrap().0, first);
    super::persist_wake_reply(&app, &thread, first, "<silent/>", fuzukue_proto::Actor::Claude)?;
    assert_eq!(super::begin_wake(&app, &watch.id, &thread)?.unwrap().0, second);
    super::persist_wake_reply(&app, &thread, second, "A decision is pending.", fuzukue_proto::Actor::Claude)?;
    assert!(app.watches.lock()?.runtime(&watch).pending_wakes.is_empty());
    Ok(())
}

#[tokio::test]
async fn legacy_wakes_stay_available_for_review() -> Result<()> {
    use crate::self_update::drain::{self, Work};

    let root = TestDir::new();
    let app = workspace(root.path());
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let id = "legacy-notification";
    app.drain.enqueue(&app.state_dir, &thread, id.into(), Work::WatchWake {
        watch: "test-dots".into(), text: "reviewable legacy excerpt".into(),
    })?;
    assert!(app.drain.claim(&app.state_dir, &thread, id)?);
    app.drain.finish(&app.state_dir, &thread, id, Some("Review the saved transcript".into()));
    let unclaimed = "unclaimed-legacy-notification";
    app.drain.enqueue(&app.state_dir, &thread, unclaimed.into(), Work::WatchWake {
        watch: "test-dots".into(), text: "unclaimed legacy excerpt".into(),
    })?;
    drop(app);

    let app = workspace(root.path());
    drain::start(&app);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if app.drain.records(&app.state_dir)?.iter().any(|record| record.id == unclaimed && record.review.is_some()) {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await??;
    let records = app.drain.records(&app.state_dir)?;
    assert!(records.iter().any(|record| record.id == id
        && record.review.as_deref() == Some("Review the saved transcript")
        && matches!(&record.work, Work::WatchWake { text, .. } if text == "reviewable legacy excerpt")));
    assert!(records.iter().any(|record| record.id == unclaimed
        && record.review.is_some()
        && matches!(&record.work, Work::WatchWake { text, .. } if text == "unclaimed legacy excerpt")));
    Ok(())
}
