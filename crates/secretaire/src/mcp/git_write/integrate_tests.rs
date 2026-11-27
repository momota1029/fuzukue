use super::*;
use std::sync::Arc;

use crate::mcp::workspace_read::git_read::tests::{git, Fixture};
use crate::Hub;
use fuzukue_proto::ServerEvent;

/// A failure injected into a running integration on this test thread.
#[derive(Clone)]
enum Fault {
    /// The index cannot be published after the branch moved.
    LoseIndexLock(std::path::PathBuf),
    /// The index cannot be published and the branch cannot be rolled back.
    LoseIndexLockAndBlockRollback(std::path::PathBuf),
    /// The execution deadline passes while worktree files are being restored.
    RestoreTimeout,
    /// HEAD is pointed elsewhere just before the ref locks are taken.
    SwitchHead(std::path::PathBuf),
}

thread_local! {
    static FAULT: std::cell::RefCell<Option<Fault>> = const { std::cell::RefCell::new(None) };
    static INTERRUPT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Simulate a drain interrupting the operation at a named point.
pub(super) fn interrupt_at(point: &str, mutation: &Mutation<'_>) {
    if INTERRUPT.get() == Some(point) {
        mutation.interrupt_for_test();
    }
}

pub(super) async fn fault(point: &str) {
    let Some(fault) = FAULT.with(|fault| fault.borrow().clone()) else { return };
    match (point, fault) {
        ("after_ref_update", Fault::LoseIndexLock(git)) => fs::remove_file(git.join("index.lock")).unwrap(),
        ("after_ref_update", Fault::LoseIndexLockAndBlockRollback(git)) => {
            fs::remove_file(git.join("index.lock")).unwrap();
            fs::write(git.join("refs/heads/main.lock"), "").unwrap();
        }
        ("during_restore", Fault::RestoreTimeout) => {
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(600)).await;
            tokio::time::resume();
            std::future::pending::<()>().await;
        }
        ("before_ref_lock", Fault::SwitchHead(repo)) => {
            git(&repo, &["symbolic-ref", "HEAD", "refs/heads/other"]).unwrap();
        }
        _ => {}
    }
}

fn setup(fixture: &Fixture, patch: &str) -> Result<(Arc<Workspace>, String)> {
    let state = fixture.temp.path().join("state");
    fs::create_dir(&state)?;
    fs::create_dir_all(fixture.repo.join("target/job"))?;
    fs::write(fixture.repo.join("target/job/change.patch"), patch)?;
    let app = Arc::new(Workspace::new(
        fixture.repo.clone(),
        "".into(),
        state,
        Arc::new(Hub::new()),
    ));
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    Ok((app, session))
}

fn add_upstream(fixture: &Fixture) -> Result<std::path::PathBuf> {
    let bare = fixture.temp.path().join("remote.git");
    git(
        fixture.temp.path(),
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    )?;
    git(
        &fixture.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    )?;
    git(&fixture.repo, &["push", "-u", "origin", "main"])?;
    Ok(bare)
}

fn args() -> Value {
    json!({"patch": "target/job/change.patch", "message": "Apply worker changes"})
}

fn modification(path: &str, first_line: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,2 @@\n-first\n+{first_line}\n second\n"
    )
}

async fn integrate(app: &Arc<Workspace>, session: &str, args: Value) -> Result<Value> {
    call(
        app,
        session,
        crate::mcp::parse_args(&args)?,
        None,
        None,
        false,
        async { Ok(()) },
    )
    .await
}

#[tokio::test]
async fn commits_cached_patch_restores_clean_worktree_and_defaults_to_no_push() -> Result<()> {
    let fixture = Fixture::new()?;
    let (app, session) = setup(&fixture, &modification("tracked.txt", "changed"))?;
    let mut args = args();
    args["allow_paths"] = json!(["*.txt"]);
    fs::write(
        app.state_dir.join("fuzukue.toml"),
        "[delegate]\ngit_integrate_co_author = \"Example Contributor <contributor@example.invalid>\"\n",
    )?;

    let bare = add_upstream(&fixture)?;

    let response = integrate(&app, &session, args).await?;
    assert_eq!(response["isError"], false, "{response}");
    let data = &response["structuredContent"];
    assert_eq!(
        data["commit_sha"],
        git(&fixture.repo, &["rev-parse", "HEAD"])?
    );
    assert_eq!(data["paths"], json!(["tracked.txt"]));
    assert_eq!(data["restore"]["restored"], json!(["tracked.txt"]));
    assert_eq!(data["restore"]["left_untouched"], json!([]));
    assert_eq!(data["push"]["status"], "disabled");
    assert_eq!(git(&bare, &["rev-parse", "HEAD"])?, fixture.head);
    assert_eq!(
        git(&fixture.repo, &["diff", "--cached", "--name-only"])?,
        ""
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("tracked.txt"))?,
        "changed\nsecond\n"
    );
    assert_eq!(
        git(&fixture.repo, &["log", "-1", "--format=%B"])?,
        "Apply worker changes\n\nCo-Authored-By: Example Contributor <contributor@example.invalid>"
    );
    Ok(())
}

#[tokio::test]
async fn integrates_rust_paths_and_loopback_examples_without_privacy_override() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = [
        "use ::std::collections::BTreeMap;",
        "use crate::module::Type;",
        "pub type Map = BTreeMap<String, Type>;",
        r#"pub const V6_URL: &str = "http://[::1]:8080";"#,
        r#"pub const V4_URL: &str = "http://127.0.0.2:8080";"#,
        r#"pub const LOCAL_URL: &str = "http://localhost:8080";"#,
        r#"pub const MAPPED_URL: &str = "http://[::ffff:127.0.0.1]:8080";"#,
    ];
    let patch = format!(
        "diff --git a/example.rs b/example.rs\nnew file mode 100644\n--- /dev/null\n+++ b/example.rs\n@@ -0,0 +1,{} @@\n+{}\n",
        source.len(),
        source.join("\n+")
    );
    let (app, session) = setup(&fixture, &patch)?;
    let response = integrate(&app, &session, args()).await?;
    assert_eq!(response["isError"], false, "{response}");
    assert_eq!(
        git(&fixture.repo, &["show", "HEAD:example.rs"])?,
        source.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn reports_actual_added_line_locations_without_exposing_values() -> Result<()> {
    let fixture = Fixture::new()?;
    let private = [
        "/home/synthetic-owner/source.rs",
        "worker@private.example",
        "device.example-tailnet.ts.net",
        "100.86.22.19",
        "ghp_synthetic_example",
        "fd00::1234",
        "[fe80::1%eth0]:8080",
        "::ffff:192.168.0.1",
    ];
    let patch = format!(
        "diff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1,2 +1,10 @@\n first\n second\n+{}\n",
        private.join("\n+")
    );
    let (app, session) = setup(&fixture, &patch)?;
    let response = integrate(&app, &session, args()).await?;
    let secret_file = fixture.repo.join("target/secret.txt");
    fs::write(
        &secret_file,
        format!("first\nsecond\n{}\n", private.join("\n")),
    )?;
    let expected_blob = git(
        &fixture.repo,
        &["hash-object", secret_file.to_str().unwrap()],
    )?;
    assert!(git(&fixture.repo, &["cat-file", "-e", &expected_blob]).is_err());
    let error = &response["structuredContent"]["error"];
    assert_eq!(response["isError"], true, "{response}");
    assert_eq!(error["code"], "privacy_refused");
    assert_eq!(
        error["locations"],
        json!([
            "tracked.txt:3",
            "tracked.txt:4",
            "tracked.txt:5",
            "tracked.txt:6",
            "tracked.txt:7",
            "tracked.txt:8",
            "tracked.txt:9",
            "tracked.txt:10"
        ])
    );
    for value in private {
        assert!(!response.to_string().contains(value));
    }
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
    assert_eq!(
        git(&fixture.repo, &["diff", "--cached", "--name-only"])?,
        ""
    );
    Ok(())
}

#[tokio::test]
async fn preserves_dirty_and_unrelated_paths_and_reports_already_applied_files() -> Result<()> {
    let fixture = Fixture::new()?;
    for name in ["already.txt", "clean.txt", "unrelated.txt", "remove.txt"] {
        fs::write(fixture.repo.join(name), "first\nsecond\n")?;
    }
    git(&fixture.repo, &["add", "--all"])?;
    git(&fixture.repo, &["commit", "-m", "More paths"])?;
    let remove = "diff --git a/remove.txt b/remove.txt\ndeleted file mode 100644\n--- a/remove.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-first\n-second\n";
    let add = "diff --git a/new/path.txt b/new/path.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new/path.txt\n@@ -0,0 +1 @@\n+new file\n";
    let patch = format!(
        "{}{}{}{}{}",
        modification("already.txt", "after"),
        modification("tracked.txt", "after"),
        modification("clean.txt", "after"),
        remove,
        add
    );
    let (app, session) = setup(&fixture, &patch)?;
    fs::write(
        fixture.repo.join("tracked.txt"),
        "first\nworker's extra edit\n",
    )?;
    fs::write(fixture.repo.join("already.txt"), "after\nsecond\n")?;
    fs::write(fixture.repo.join("unrelated.txt"), "unrelated edits\n")?;

    let response = integrate(&app, &session, args()).await?;
    assert_eq!(response["isError"], false, "{response}");
    let restore = &response["structuredContent"]["restore"];
    assert_eq!(
        restore["restored"],
        json!(["clean.txt", "new/path.txt", "remove.txt"])
    );
    assert_eq!(restore["already_current"], json!(["already.txt"]));
    assert_eq!(
        restore["left_untouched"],
        json!([{"path": "tracked.txt", "reason": "other_uncommitted_edits"}])
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("tracked.txt"))?,
        "first\nworker's extra edit\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("unrelated.txt"))?,
        "unrelated edits\n"
    );
    assert!(!fixture.repo.join("remove.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.repo.join("new/path.txt"))?,
        "new file\n"
    );
    assert_eq!(
        git(&fixture.repo, &["show", "HEAD:tracked.txt"])?,
        "after\nsecond"
    );
    Ok(())
}

#[tokio::test]
async fn refuses_an_unclean_index_without_changing_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let (app, session) = setup(&fixture, &modification("tracked.txt", "changed"))?;
    fs::write(fixture.repo.join("unrelated.txt"), "already staged\n")?;
    for flag in [Some("-N"), None] {
        if let Some(flag) = flag {
            git(&fixture.repo, &["add", flag, "unrelated.txt"])?;
        } else {
            git(&fixture.repo, &["add", "unrelated.txt"])?;
        }
        let index = fs::read(fixture.repo.join(".git/index"))?;
        let response = integrate(&app, &session, args()).await?;
        assert_eq!(response["isError"], true, "{response}");
        assert_eq!(
            response["structuredContent"]["error"]["message"],
            "The index has staged changes"
        );
        assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
        assert_eq!(fs::read(fixture.repo.join(".git/index"))?, index);
    }
    Ok(())
}

#[tokio::test]
async fn protects_patch_path_rejects_binary_diff_and_enforces_path_globs() -> Result<()> {
    let fixture = Fixture::new()?;
    let (app, session) = setup(&fixture, &modification("tracked.txt", "changed"))?;
    let mut denied = args();
    denied["allow_paths"] = json!(["other/**"]);
    let response = integrate(&app, &session, denied).await?;
    assert_eq!(response["isError"], true, "{response}");
    assert!(response["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("outside allow_paths"));

    for path in [
        "target/job/../job/change.patch",
        "/etc/passwd",
        "target/job/link.patch",
    ] {
        let mut denied = args();
        denied["patch"] = json!(path);
        assert_eq!(integrate(&app, &session, denied).await?["isError"], true);
    }
    symlink("change.patch", fixture.repo.join("target/job/link.patch"))?;
    let mut denied = args();
    denied["patch"] = json!("target/job/link.patch");
    assert_eq!(integrate(&app, &session, denied).await?["isError"], true);

    fs::write(fixture.repo.join("tracked.txt"), b"\0binary\n")?;
    let binary = git(&fixture.repo, &["diff", "--binary", "--", "tracked.txt"])?;
    fs::write(
        fixture.repo.join("target/job/change.patch"),
        format!("{binary}\n\n"),
    )?;
    let response = integrate(&app, &session, args()).await?;
    assert_eq!(response["isError"], true, "{response}");
    assert_eq!(
        response["structuredContent"]["error"]["message"],
        "Binary patches are refused"
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
    fs::write(
        fixture.repo.join("target/job/change.patch"),
        vec![b'x'; MAX_PATCH + 1],
    )?;
    let response = integrate(&app, &session, args()).await?;
    assert_eq!(response["isError"], true, "{response}");
    assert!(response["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("byte limit"));
    Ok(())
}

#[tokio::test]
async fn scans_both_sections_of_a_type_change_and_leaves_links_untouched() -> Result<()> {
    let fixture = Fixture::new()?;
    let tracked = fixture.repo.join("tracked.txt");
    fs::remove_file(&tracked)?;
    symlink("device.example-tailnet.ts.net", &tracked)?;
    let private_patch = git(&fixture.repo, &["diff", "--", "tracked.txt"])?;
    fs::remove_file(&tracked)?;
    symlink("safe-target", &tracked)?;
    let safe_patch = git(&fixture.repo, &["diff", "--", "tracked.txt"])?;
    fs::remove_file(&tracked)?;
    fs::write(&tracked, "first\nsecond\n")?;

    let (app, session) = setup(&fixture, &format!("{private_patch}\n"))?;
    let response = integrate(&app, &session, args()).await?;
    assert_eq!(
        response["structuredContent"]["error"]["code"], "privacy_refused",
        "{response}"
    );
    assert_eq!(
        response["structuredContent"]["error"]["locations"],
        json!(["tracked.txt:1"])
    );

    fs::write(
        fixture.repo.join("target/job/change.patch"),
        format!("{safe_patch}\n"),
    )?;
    let response = integrate(&app, &session, args()).await?;
    assert_eq!(response["isError"], false, "{response}");
    assert_eq!(
        response["structuredContent"]["paths"],
        json!(["tracked.txt"])
    );
    assert_eq!(
        response["structuredContent"]["restore"]["left_untouched"],
        json!([{"path": "tracked.txt", "reason": "unsafe_or_unavailable"}])
    );
    assert_eq!(fs::read_to_string(&tracked)?, "first\nsecond\n");
    assert_eq!(
        git(&fixture.repo, &["show", "HEAD:tracked.txt"])?,
        "safe-target"
    );
    Ok(())
}

#[tokio::test]
async fn guarded_push_refuses_a_branch_or_tip_changed_before_approval() -> Result<()> {
    let fixture = Fixture::new()?;
    let bare = add_upstream(&fixture)?;
    let (app, session) = setup(&fixture, &modification("tracked.txt", "changed"))?;
    let result = integrate(&app, &session, args()).await?;
    assert_eq!(result["isError"], false, "{result}");
    let integrated = result["structuredContent"]["commit_sha"].as_str().unwrap();

    // Move to another branch at the same commit, then move its tip too.
    git(&fixture.repo, &["switch", "-c", "other"])?;
    git(&fixture.repo, &["branch", "--set-upstream-to=origin/main"])?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let guarded = |branch| {
        app.git_remote_expected(
            &session,
            Actor::System,
            crate::git::RemoteOperation::Push,
            Some((branch, integrated)),
        )
    };
    let assert_rejected = |result: Result<String>| {
        assert_eq!(
            result.unwrap_err().to_string(),
            "The branch or commit changed before push approval"
        );
    };
    let deadline = std::time::Duration::from_secs(5);
    assert_rejected(tokio::time::timeout(deadline, guarded("refs/heads/main")).await?);
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "Other tip"],
    )?;
    assert_rejected(tokio::time::timeout(deadline, guarded("refs/heads/other")).await?);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event.event,
            ServerEvent::ApprovalRequested { .. }
        ));
    }
    assert_eq!(git(&bare, &["rev-parse", "HEAD"])?, fixture.head);
    Ok(())
}

mod deferred {
    use super::*;
    use crate::deferred_approvals::{self as deferred, tests::crash_at, Phase, Status};
    use crate::self_update::drain::Work;
    use fuzukue_proto::{ApprovalDecision, ApprovalGrantScope, ApprovalState};
    use std::time::Duration;

    const SECRET: &str = "ghp_synthetic_example";
    const GRANT: ApprovalDecision = ApprovalDecision::Grant { scope: ApprovalGrantScope::Once };

    fn override_args() -> Value {
        let mut request = args();
        request["override_privacy"] = json!(true);
        request["restore_worktree"] = json!(false);
        request
    }

    async fn request(app: &Arc<Workspace>, session: &str, args: Value, actor: Option<Actor>, trusted: bool) -> Result<Value> {
        // The call must return without waiting for anyone.
        tokio::time::timeout(Duration::from_secs(20), call(
            app, session, crate::mcp::parse_args(&args)?, actor, None, trusted, async { Ok(()) },
        )).await?
    }

    async fn pending(app: &Arc<Workspace>, session: &str, args: Value) -> Result<String> {
        let response = request(app, session, args, Some(Actor::Claude), false).await?;
        assert_eq!(response["isError"], false, "{response}");
        let data = &response["structuredContent"];
        assert_eq!(data["status"], "pending", "{response}");
        Ok(data["approval_id"].as_str().unwrap().to_owned())
    }

    fn results(app: &Workspace) -> Result<Vec<(String, String)>> {
        Ok(app.drain.records(&app.state_dir)?.into_iter().filter_map(|record| match record.work {
            Work::Desk(crate::delegate::DeskTurn::WorkshopReport { approval_id: Some(approval_id), text }) => {
                assert_eq!(record.id, format!("{}{approval_id}", deferred::RESULT_PREFIX));
                Some((approval_id, text))
            }
            _ => None,
        }).collect())
    }

    fn entry(app: &Workspace, id: &str) -> deferred::Entry {
        let bytes = fs::read(app.state_dir.join("deferred-approvals").join(format!("{id}.json"))).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn outcome(app: &Workspace, id: &str) -> (Status, Value) {
        match entry(app, id).phase {
            Phase::HandedOff { outcome, .. } => (outcome.status, outcome.result),
            phase => panic!("not handed off: {phase:?}"),
        }
    }

    fn reopen(fixture: &Fixture, app: Arc<Workspace>) -> Arc<Workspace> {
        let state = app.state_dir.clone();
        drop(app);
        Arc::new(Workspace::new(fixture.repo.clone(), "".into(), state, Arc::new(Hub::new())))
    }

    fn head(fixture: &Fixture) -> Result<String> {
        git(&fixture.repo, &["rev-parse", "HEAD"])
    }

    #[tokio::test]
    async fn returns_pending_without_deadline_and_runs_the_bound_patch_after_a_late_approval() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), true).await?;
        let data = &response["structuredContent"];
        assert_eq!(data["status"], "pending", "{response}");
        assert!(data["what_to_do_next"]["result"].as_str().unwrap().contains("new message"));
        let id = data["approval_id"].as_str().unwrap().to_owned();

        // Nothing waits: no live approval, no Git lock, no change.
        assert!(app.hub.live_approval_threads().is_empty());
        assert!(app.git_lock.try_lock().is_ok());
        assert_eq!(head(&fixture)?, fixture.head);
        let (queue, _) = app.hub.pending_approvals()?;
        let [summary] = queue.as_slice() else { panic!("one pending card: {}", queue.len()) };
        assert_eq!(summary.approval_id, id);
        assert_eq!(summary.deadline(), None);
        assert_eq!(summary.unattended, ApprovalUnattended::NeverUnattended);
        assert_eq!(summary.owner.thread, session);
        assert!(summary.reason.contains("privacy override"));
        assert_eq!(summary.action.args["locations"], json!(["tracked.txt:1"]));
        assert!(!serde_json::to_string(summary)?.contains(SECRET));

        // The worker file changes after the request. The approved bytes are used.
        fs::write(fixture.repo.join("target/job/change.patch"), modification("tracked.txt", "replaced"))?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        deferred::process(&app, &id).await?;
        assert_eq!(git(&fixture.repo, &["show", "HEAD:tracked.txt"])?, format!("{SECRET}\nsecond"));
        assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD~1"])?, fixture.head);
        let (status, result) = outcome(&app, &id);
        assert_eq!(status, Status::Completed);
        assert_eq!(result["commit_sha"], head(&fixture)?);
        assert_eq!(result["restore"]["enabled"], false);
        let results = results(&app)?;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("completed"));
        assert!(deferred::ready_for_delivery(&app, &id));
        assert!(entry(&app, &id).payload.is_null());
        Ok(())
    }

    #[tokio::test]
    async fn approval_after_restart_runs_once_and_a_second_decision_is_refused() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        let app = reopen(&fixture, app);
        let (queue, _) = app.hub.pending_approvals()?;
        assert_eq!(queue.iter().map(|item| &item.approval_id).collect::<Vec<_>>(), [&id]);

        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        assert!(app.hub.decide_approval(&id, GRANT, "local_user", None).is_err());
        assert!(app.hub.decide_approval(&id, ApprovalDecision::Deny, "local_user", None).is_err());
        deferred::recover(&app).await?;
        deferred::process(&app, &id).await?;
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        assert_eq!(git(&fixture.repo, &["rev-list", "--count", "HEAD"])?, "2");
        assert_eq!(results(&app)?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn denial_and_withdrawal_change_nothing_and_report_once() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        // The same snapshot reuses the pending card.
        let again = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        assert_eq!(again["structuredContent"]["approval_id"], id);
        assert_eq!(again["structuredContent"]["reused"], true);
        assert_eq!(app.hub.pending_approvals()?.0.len(), 1);

        app.hub.decide_approval(&id, ApprovalDecision::Deny, "local_user", None)?;
        deferred::process(&app, &id).await?;
        deferred::process(&app, &id).await?;
        assert_eq!(outcome(&app, &id).0, Status::Denied);
        assert_eq!(results(&app)?.len(), 1);
        assert_eq!(head(&fixture)?, fixture.head);

        // A new request after a denial is a new card. The desk withdraws it silently.
        let second = pending(&app, &session, override_args()).await?;
        assert_ne!(second, id);
        let other = app.hub.edit_threads(|threads| threads.create(None))?;
        assert!(deferred::withdraw(&app, &other, &second, None).await.is_err());
        deferred::withdraw(&app, &session, &second, Some("No longer needed".into())).await?;
        assert!(deferred::withdraw(&app, &session, &second, None).await.is_err());
        assert!(app.hub.decide_approval(&second, GRANT, "local_user", None).is_err());
        deferred::recover(&app).await?;
        assert_eq!(outcome(&app, &second).0, Status::Withdrawn);
        assert!(!deferred::ready_for_delivery(&app, &second));
        assert_eq!(results(&app)?.len(), 1);
        assert_eq!(head(&fixture)?, fixture.head);
        Ok(())
    }

    #[tokio::test]
    async fn non_human_paths_cannot_approve_the_privacy_override() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), true).await?;
        let id = response["structuredContent"]["approval_id"].as_str().unwrap().to_owned();
        // A card already shown to a person stays theirs when the mode changes.
        enable_full_mode(&app)?;
        app.hub.edit_threads(|threads| threads.set_partner_permission(&session, fuzukue_proto::PermissionMode::BypassPermissions))?;
        app.hub.sweep_thread_approvals(&session)?;
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        let (queue, _) = app.hub.pending_approvals()?;
        assert_eq!(queue.iter().map(|item| (&item.approval_id, item.state)).collect::<Vec<_>>(),
            [(&id, ApprovalState::Pending)]);
        // The legacy live-approval reply path does not know queue IDs.
        assert!(crate::resolve_approval(&app, id.clone(), "accept".into(), false).await.is_err());
        assert!(!app.delegates.resolve_approval(&id, "accept", false)?);
        // A worker of the same thread cannot withdraw or claim it.
        let worker = fuzukue_proto::ApprovalOwner { thread: session.clone(), job: "delegate:x".into(), requester: "worker".into() };
        assert!(app.hub.withdraw_own_approval(&id, &worker, None).is_err());
        let (_, action) = app.hub.approval_record(&id)?.unwrap();
        assert!(!app.hub.consume_once_approval(&id, &deferred::owner(&session), &action)?);
        deferred::process(&app, &id).await?;
        assert_eq!(head(&fixture)?, fixture.head);
        assert!(results(&app)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn hands_off_turns_commit_findings_record_them_and_list_them_before_a_push() -> Result<()> {
        use fuzukue_proto::PermissionMode;
        for full_mode in [false, true] {
            let fixture = Fixture::new()?;
            let bare = add_upstream(&fixture)?;
            let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
            if full_mode {
                enable_full_mode(&app)?;
            } else {
                app.hub.edit_threads(|threads| threads.set_partner_permission(&session, PermissionMode::BypassPermissions))?;
            }
            // Untrusted and external turns still get the refusal and the human card.
            let refused = request(&app, &session, args(), Some(Actor::Claude), false).await?;
            assert_eq!(refused["structuredContent"]["error"]["code"], "privacy_refused", "{refused}");
            assert_eq!(head(&fixture)?, fixture.head);

            let (_, mut events) = app.hub.subscribe(u64::MAX);
            let response = request(&app, &session, args(), Some(Actor::Claude), true).await?;
            assert_eq!(response["isError"], false, "{response}");
            let result = &response["structuredContent"];
            assert_eq!(result["commit_sha"], head(&fixture)?);
            assert_eq!(result["privacy_findings"]["locations"], json!(["tracked.txt:1"]));
            assert_eq!(result["privacy_findings"]["recorded"], true);
            assert!(app.hub.pending_approvals()?.0.is_empty());
            let notice = std::iter::from_fn(|| events.try_recv().ok()).find_map(|envelope| match envelope.event {
                ServerEvent::LocalNotice { text, item, .. } if item.starts_with("privacy-findings:") => Some(text),
                _ => None,
            }).expect("a visible note");
            assert!(notice.contains("tracked.txt:1") && !notice.contains(SECRET), "{notice}");

            // The push waits for a person, whatever the mode, and lists the places.
            let app_for_push = app.clone();
            let push_session = session.clone();
            let push = tokio::spawn(async move {
                app_for_push.git_remote(&push_session, Actor::System, crate::git::RemoteOperation::Push).await
            });
            let (id, summary) = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let ServerEvent::ApprovalRequested { request_id, summary, .. } = events.recv().await?.event {
                        return anyhow::Ok((request_id, summary));
                    }
                }
            }).await??;
            assert!(summary.contains("tracked.txt:1"), "{summary}");
            assert!(app.delegates.resolve_approval(&id, "accept", false)?);
            push.await??;
            assert_eq!(git(&bare, &["rev-parse", "HEAD"])?, head(&fixture)?);
            assert!(crate::privacy_findings::unpublished(&app.project_dir, &app.state_dir).await.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn hands_off_turns_ask_when_findings_cannot_be_recorded() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        app.hub.edit_threads(|threads| {
            threads.set_partner_permission(&session, fuzukue_proto::PermissionMode::BypassPermissions)
        })?;
        // The record file cannot be read or replaced.
        std::fs::create_dir_all(app.state_dir.join("privacy-findings.json"))?;
        let response = request(&app, &session, args(), Some(Actor::Claude), true).await?;
        assert_eq!(response["structuredContent"]["status"], "pending", "{response}");
        assert_eq!(head(&fixture)?, fixture.head);
        assert_eq!(app.hub.pending_approvals()?.0.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn records_of_unpublished_commits_are_never_dropped_and_unused_reservations_are() -> Result<()> {
        let fixture = Fixture::new()?;
        let state = tempfile::tempdir()?;
        drop(crate::privacy_findings::reserve(state.path(), 1, &["unused:1".into()])?);
        assert!(crate::privacy_findings::unpublished(&fixture.repo, state.path()).await.is_none());
        for index in 0..250 {
            crate::privacy_findings::reserve(state.path(), 1, &[format!("file:{index}")])?.publishing();
        }
        let listed = crate::privacy_findings::unpublished(&fixture.repo, state.path()).await.context("findings")?;
        assert!(listed.ends_with("+220"), "{listed}");
        let stored: Value = serde_json::from_slice(&std::fs::read(state.path().join("privacy-findings.json"))?)?;
        assert_eq!(stored.as_array().map(Vec::len), Some(250));
        Ok(())
    }

    #[tokio::test]
    async fn head_moves_apply_cleanly_or_fail_without_changes() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        // An unrelated commit moves HEAD. The bound patch still applies.
        fs::write(fixture.repo.join("unrelated.txt"), "unrelated\n")?;
        git(&fixture.repo, &["add", "unrelated.txt"])?;
        git(&fixture.repo, &["commit", "-m", "Unrelated"])?;
        let moved = head(&fixture)?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        deferred::process(&app, &id).await?;
        assert_eq!(outcome(&app, &id).0, Status::Completed);
        assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD~1"])?, moved);

        // A conflicting commit makes the next bound patch stale.
        fs::write(fixture.repo.join("target/job/change.patch"), modification("tracked.txt", "other"))?;
        git(&fixture.repo, &["reset", "--hard", &fixture.head])?;
        let id = pending(&app, &session, args()).await?;
        fs::write(fixture.repo.join("tracked.txt"), "conflict\nsecond\n")?;
        git(&fixture.repo, &["commit", "-am", "Conflict"])?;
        let conflicted = head(&fixture)?;
        let index = fs::read(fixture.repo.join(".git/index"))?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        deferred::process(&app, &id).await?;
        let (status, result) = outcome(&app, &id);
        assert_eq!(status, Status::Failed);
        assert!(result["message"].as_str().unwrap().contains("Nothing was changed"), "{result}");
        assert_eq!(head(&fixture)?, conflicted);
        assert_eq!(fs::read(fixture.repo.join(".git/index"))?, index);

        // A staged change since the request is refused the same way.
        git(&fixture.repo, &["reset", "--hard", &fixture.head])?;
        let id = pending(&app, &session, args()).await?;
        fs::write(fixture.repo.join("unrelated.txt"), "staged\n")?;
        git(&fixture.repo, &["add", "unrelated.txt"])?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        deferred::process(&app, &id).await?;
        let (status, result) = outcome(&app, &id);
        assert_eq!(status, Status::Failed);
        assert!(result["message"].as_str().unwrap().contains("staged"), "{result}");
        assert_eq!(head(&fixture)?, fixture.head);
        assert_eq!(results(&app)?.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn privacy_findings_beyond_the_approved_ones_are_refused() -> Result<()> {
        let fixture = Fixture::new()?;
        // The request has no findings, so no override was shown for the approval.
        let (app, session) = setup(&fixture, &modification("tracked.txt", "plain"))?;
        let id = pending(&app, &session, args()).await?;
        let mut payload = entry(&app, &id).payload;
        payload["patch"] = json!(modification("tracked.txt", SECRET));
        payload["patch_sha256"] = json!(format!("{:x}", Sha256::digest(modification("tracked.txt", SECRET).as_bytes())));
        let mutation = Mutation::new(&app, &session, "Approved operation".into())?;
        let Err(error) = execute_snapshot(&app, &mutation, &payload, || panic!("claimed")).await else { panic!("unreviewed findings ran") };
        assert!(error.to_string().contains("privacy findings"), "{error:#}");
        assert_eq!(head(&fixture)?, fixture.head);
        Ok(())
    }

    fn state(app: &Workspace, id: &str) -> ApprovalState {
        app.hub.approval_record(id).unwrap().unwrap().0.state
    }

    fn save_entry(app: &Workspace, id: &str, entry: &deferred::Entry) -> Result<()> {
        fs::write(app.state_dir.join("deferred-approvals").join(format!("{id}.json")), serde_json::to_vec(entry)?)?;
        Ok(())
    }

    fn enable_full_mode(app: &Workspace) -> Result<()> {
        app.hub.set_full_approval(
            fuzukue_proto::FullApprovalChange::Full { duration: fuzukue_proto::FullApprovalDuration::OneHour },
            &crate::remote::SettingsPrincipal::fixture(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn requests_that_need_a_person_never_run_under_full_mode() -> Result<()> {
        use fuzukue_proto::PermissionMode;
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", "changed"))?;
        let thread = || app.hub.edit_threads(|threads| threads.create(None));
        let message = |text: &str| {
            let mut request = args();
            request["message"] = json!(text);
            request
        };
        let mut waiting = Vec::new();

        // Requested before full mode was enabled: a trusted turn that still needed a person.
        let response = request(&app, &session, message("Earlier"), Some(Actor::Claude), true).await?;
        waiting.push(response["structuredContent"]["approval_id"].as_str().unwrap().to_owned());
        enable_full_mode(&app)?;

        // An untrusted or external session.
        waiting.push(pending(&app, &thread()?, message("Untrusted")).await?);
        // Thread modes that ask a person.
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            let session = thread()?;
            app.hub.edit_threads(|threads| threads.set_partner_permission(&session, mode))?;
            let response = request(&app, &session, message(&format!("{mode:?}")), Some(Actor::Claude), true).await?;
            assert_eq!(response["structuredContent"]["status"], "pending", "{response}");
            waiting.push(response["structuredContent"]["approval_id"].as_str().unwrap().to_owned());
        }
        // A turn that has read external input.
        let external = thread()?;
        app.hub.publish_partner_turn_started(&external, None, false, None);
        app.hub.mark_external_approval_input(&external);
        let response = request(&app, &external, message("External input"), Some(Actor::Claude), true).await?;
        assert_eq!(response["structuredContent"]["status"], "pending", "{response}");
        waiting.push(response["structuredContent"]["approval_id"].as_str().unwrap().to_owned());

        // Enabling the mode again, sweeping threads and reloading the queue grant nothing.
        enable_full_mode(&app)?;
        app.hub.sweep_thread_approvals(&session)?;
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        for id in &waiting {
            let (summary, _) = app.hub.approval_record(id)?.unwrap();
            assert_eq!(summary.state, ApprovalState::Pending);
            assert_eq!(summary.unattended, ApprovalUnattended::NeverUnattended);
        }
        assert_eq!(head(&fixture)?, fixture.head);
        assert!(results(&app)?.is_empty());

        // A trusted full-mode session still runs at once.
        let trusted = app.hub.edit_threads(|threads| threads.create(None))?;
        let response = request(&app, &trusted, message("Trusted"), Some(Actor::Claude), true).await?;
        assert_eq!(response["isError"], false, "{response}");
        assert_eq!(response["structuredContent"]["commit_sha"], head(&fixture)?);
        assert!(waiting.iter().all(|id| state(&app, id) == ApprovalState::Pending));
        Ok(())
    }

    #[tokio::test]
    async fn a_repeated_request_never_reports_a_decided_card_as_waiting() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        // Granted and not yet run: no second card, and the caller learns a result is coming.
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        let data = &response["structuredContent"];
        assert_eq!((&data["status"], &data["approval_id"]), (&json!("decided"), &json!(id)), "{response}");
        assert!(app.hub.pending_approvals()?.0.is_empty());

        // Consumed and handed off: the same request is a new decision.
        deferred::process(&app, &id).await?;
        assert_eq!(outcome(&app, &id).0, Status::Completed);
        git(&fixture.repo, &["reset", "--hard", &fixture.head])?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        let data = &response["structuredContent"];
        assert_eq!(data["status"], "pending", "{response}");
        assert_eq!(data["reused"], false);
        let second = data["approval_id"].as_str().unwrap().to_owned();
        assert_ne!(second, id);
        assert_eq!(state(&app, &second), ApprovalState::Pending);
        Ok(())
    }

    #[tokio::test]
    async fn a_moved_head_reuses_stored_values_or_replaces_outdated_findings() -> Result<()> {
        let fixture = Fixture::new()?;
        fs::write(fixture.repo.join("notes.txt"), "a\nb\nc\nd\ne\n")?;
        git(&fixture.repo, &["add", "notes.txt"])?;
        git(&fixture.repo, &["commit", "-m", "Notes"])?;
        let first_head = head(&fixture)?;
        let patch = format!("diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -2,3 +2,3 @@\n b\n-c\n+{SECRET}\n d\n");
        let (app, session) = setup(&fixture, &patch)?;
        let id = pending(&app, &session, override_args()).await?;

        // An unrelated commit leaves the card accurate. The reply repeats what it is bound to.
        fs::write(fixture.repo.join("unrelated.txt"), "unrelated\n")?;
        git(&fixture.repo, &["add", "unrelated.txt"])?;
        git(&fixture.repo, &["commit", "-m", "Unrelated"])?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        let data = &response["structuredContent"];
        assert_eq!(data["approval_id"], id, "{response}");
        assert_eq!(data["reused"], true);
        assert_eq!(data["head"], first_head);
        assert_eq!(data["locations"], json!(["notes.txt:3"]));

        // A moved line makes the card outdated. It is replaced by one accurate card.
        fs::write(fixture.repo.join("notes.txt"), "new\na\nb\nc\nd\ne\n")?;
        git(&fixture.repo, &["commit", "-am", "Insert a line"])?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        let data = &response["structuredContent"];
        let second = data["approval_id"].as_str().unwrap().to_owned();
        assert_ne!(second, id, "{response}");
        assert_eq!(data["replaced_approval_id"], id);
        assert_eq!(data["head"], head(&fixture)?);
        assert_eq!(data["locations"], json!(["notes.txt:4"]));
        assert_eq!(state(&app, &id), ApprovalState::Withdrawn);
        let (queue, _) = app.hub.pending_approvals()?;
        assert_eq!(queue.iter().map(|item| &item.approval_id).collect::<Vec<_>>(), [&second]);
        assert_eq!(queue[0].action.args["locations"], json!(["notes.txt:4"]));
        deferred::recover(&app).await?;
        assert!(results(&app)?.is_empty(), "a replaced card reports nothing");
        Ok(())
    }

    #[tokio::test]
    async fn errors_after_writing_began_need_review_and_are_never_retried() -> Result<()> {
        for case in 0..3 {
            let fixture = Fixture::new()?;
            let add = "diff --git a/added.txt b/added.txt\nnew file mode 100644\n--- /dev/null\n+++ b/added.txt\n@@ -0,0 +1 @@\n+added\n";
            let patch = format!("{add}{}", modification("tracked.txt", "changed"));
            let (app, session) = setup(&fixture, &patch)?;
            let id = pending(&app, &session, args()).await?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            let metadata = fixture.repo.join(".git");
            FAULT.with(|fault| *fault.borrow_mut() = Some(match case {
                0 => Fault::LoseIndexLockAndBlockRollback(metadata),
                1 => Fault::LoseIndexLock(metadata),
                _ => Fault::RestoreTimeout,
            }));
            let processed = deferred::process(&app, &id).await;
            FAULT.with(|fault| *fault.borrow_mut() = None);
            processed?;

            let (status, result) = outcome(&app, &id);
            assert_eq!(status, Status::Unknown, "case {case}: {result}");
            assert_eq!(result["needs_review"], true);
            assert_eq!(result["head_before"], fixture.head);
            assert_eq!(result["head_now"], head(&fixture)?);
            let landed = &result["landed"];
            assert_eq!(landed["ref_updated"], true, "case {case}: {result}");
            assert_eq!(landed["rolled_back"], case == 1);
            assert_eq!(landed["index_published"], case == 2);
            if case == 1 {
                assert_eq!(head(&fixture)?, fixture.head);
            } else {
                assert_eq!(landed["commit_sha"], head(&fixture)?);
            }
            assert_eq!(landed["restored"], if case == 2 { json!(["added.txt"]) } else { json!([]) });
            assert_eq!(fixture.repo.join("added.txt").exists(), case == 2);

            let delivered = results(&app)?;
            assert_eq!(delivered.len(), 1);
            assert!(delivered[0].1.contains("needs review"), "{}", delivered[0].1);
            assert!(!delivered[0].1.contains("without changes"));
            // Neither a later pass nor a restart runs it again.
            let before = head(&fixture)?;
            let app = reopen(&fixture, app);
            deferred::recover(&app).await?;
            deferred::process(&app, &id).await?;
            assert_eq!(head(&fixture)?, before);
            assert_eq!(results(&app)?.len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn execution_is_bound_to_the_approved_record() -> Result<()> {
        let fixture = Fixture::new()?;
        let (mut app, _) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        for case in 0..4 {
            let session = app.hub.edit_threads(|threads| threads.create(None))?;
            let id = pending(&app, &session, override_args()).await?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            let mut entry = entry(&app, &id);
            match case {
                0 => entry.payload["message"] = json!("Another message"),
                1 => entry.payload["findings_locations"] = json!(["tracked.txt:99"]),
                2 => entry.session = app.hub.edit_threads(|threads| threads.create(None))?,
                // Even with a matching digest, the snapshot must describe the approved action.
                _ => entry.payload["restore_worktree"] = json!(true),
            }
            save_entry(&app, &id, &entry)?;
            if case == 3 {
                let path = app.state_dir.join("async-approvals.json");
                let mut queue: Value = serde_json::from_slice(&fs::read(&path)?)?;
                let record = queue["records"].as_array_mut().unwrap().iter_mut()
                    .find(|record| record["summary"]["approval_id"] == id.as_str()).unwrap();
                record["binding"] = json!(deferred::binding(&entry.tool, &entry.session, &entry.payload));
                fs::write(&path, serde_json::to_vec(&queue)?)?;
                app = reopen(&fixture, app);
            }
            deferred::process(&app, &id).await?;
            let (status, result) = outcome(&app, &id);
            assert_eq!(status, Status::Failed, "case {case}: {result}");
            assert!(result["message"].as_str().unwrap().contains("does not match the approved record"), "{result}");
            assert_eq!(state(&app, &id), ApprovalState::Withdrawn);
            assert_eq!(head(&fixture)?, fixture.head, "case {case}");
            // The refusal goes to the thread that owns the approval.
            let record = app.drain.records(&app.state_dir)?.into_iter()
                .find(|record| record.id == format!("{}{id}", deferred::RESULT_PREFIX)).unwrap();
            assert_eq!(record.session, session, "case {case}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn crash_windows_never_run_twice_and_report_once() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;

        // Stopped after claiming the grant: the outcome is unknown and never replayed.
        let id = pending(&app, &session, override_args()).await?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        crash_at(Some("after_consume"));
        assert!(deferred::process(&app, &id).await.is_err());
        crash_at(None);
        assert!(matches!(entry(&app, &id).phase, Phase::Running { .. }));
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        let (status, _) = outcome(&app, &id);
        assert_eq!(status, Status::Unknown);
        assert_eq!(head(&fixture)?, fixture.head);
        assert_eq!(results(&app)?.len(), 1);

        // Stopped after marking running but before claiming: the grant is still unused.
        fs::write(fixture.repo.join("target/job/change.patch"), modification("tracked.txt", "fresh"))?;
        let id = pending(&app, &session, args()).await?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        let mut running = entry(&app, &id);
        running.phase = Phase::Running { started_at: 0 };
        fs::write(app.state_dir.join("deferred-approvals").join(format!("{id}.json")), serde_json::to_vec(&running)?)?;
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        assert_eq!(outcome(&app, &id).0, Status::Completed);
        assert_eq!(git(&fixture.repo, &["show", "HEAD:tracked.txt"])?, "fresh\nsecond");

        // Stopped after queuing the result but before saving the hand-off.
        fs::write(fixture.repo.join("target/job/change.patch"),
            modification("tracked.txt", "again").replace("-first", "-fresh"))?;
        let id = pending(&app, &session, args()).await?;
        app.hub.decide_approval(&id, ApprovalDecision::Deny, "local_user", None)?;
        crash_at(Some("after_enqueue"));
        assert!(deferred::process(&app, &id).await.is_err());
        crash_at(None);
        assert!(matches!(entry(&app, &id).phase, Phase::Finished { .. }));
        assert!(!deferred::ready_for_delivery(&app, &id));
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        deferred::recover(&app).await?;
        let delivered: Vec<_> = results(&app)?.into_iter().filter(|(approval, _)| approval == &id).collect();
        assert_eq!(delivered.len(), 1);
        assert!(deferred::ready_for_delivery(&app, &id));

        // After delivery removes the journal record, recovery does not queue it again.
        app.drain.finish(&app.state_dir, &session, &format!("{}{id}", deferred::RESULT_PREFIX), None);
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        assert!(results(&app)?.iter().all(|(approval, _)| approval != &id));
        assert_eq!(git(&fixture.repo, &["rev-list", "--count", "HEAD"])?, "2");
        Ok(())
    }

    /// Every ref and what HEAD names.
    fn refs(fixture: &Fixture) -> Result<(String, String)> {
        Ok((
            fs::read_to_string(fixture.repo.join(".git/HEAD"))?,
            git(&fixture.repo, &["for-each-ref", "--format=%(refname) %(objectname)"])?,
        ))
    }

    fn assert_unlocked(fixture: &Fixture) {
        for lock in ["index.lock", "HEAD.lock", "refs/heads/main.lock"] {
            assert!(!fixture.repo.join(".git").join(lock).exists(), "{lock}");
        }
    }

    fn assert_refused(app: &Workspace, id: &str, requested: &str) {
        let (status, result) = outcome(app, id);
        assert_eq!(status, Status::Failed, "{result}");
        assert!(result["message"].as_str().unwrap().contains(requested), "{result}");
        assert!(app.hub.approval_record(id).unwrap().unwrap().0.consumed_at.is_some());
    }

    #[tokio::test]
    async fn approval_on_another_checkout_is_refused_without_changes() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        // Another branch at the same commit, and a detached HEAD at the same commit.
        for switch in [&["switch", "-c", "other"][..], &["switch", "--detach"]] {
            let id = pending(&app, &session, override_args()).await?;
            let (summary, _) = app.hub.approval_record(&id)?.unwrap();
            assert_eq!(summary.action.args["branch"], "main");
            assert!(summary.action.args.get("detached_head").is_none());
            git(&fixture.repo, switch)?;
            let before = refs(&fixture)?;
            let index = fs::read(fixture.repo.join(".git/index"))?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            deferred::process(&app, &id).await?;
            assert_refused(&app, &id, "`main`");
            assert_eq!(refs(&fixture)?, before);
            assert_eq!(fs::read(fixture.repo.join(".git/index"))?, index);
            assert_unlocked(&fixture);
            git(&fixture.repo, &["switch", "main"])?;
        }

        // The recorded branch is part of the digest.
        let id = pending(&app, &session, override_args()).await?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        let mut tampered = entry(&app, &id);
        tampered.payload["checkout"] = json!({"branch": "refs/heads/other"});
        save_entry(&app, &id, &tampered)?;
        git(&fixture.repo, &["switch", "other"])?;
        deferred::process(&app, &id).await?;
        let (status, result) = outcome(&app, &id);
        assert_eq!(status, Status::Failed, "{result}");
        assert!(result["message"].as_str().unwrap().contains("does not match the approved record"), "{result}");
        assert_eq!(state(&app, &id), ApprovalState::Withdrawn);
        assert_eq!(head(&fixture)?, fixture.head);
        Ok(())
    }

    #[tokio::test]
    async fn detached_requests_run_only_on_the_same_detached_commit() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let short = &fixture.head[..12];
        git(&fixture.repo, &["switch", "--detach"])?;
        // A branch at the same commit, then another detached commit.
        for case in 0..2 {
            let id = pending(&app, &session, override_args()).await?;
            let (summary, _) = app.hub.approval_record(&id)?.unwrap();
            assert_eq!(summary.action.args["detached_head"], fixture.head);
            assert!(summary.action.args.get("branch").is_none());
            if case == 0 {
                git(&fixture.repo, &["switch", "main"])?;
            } else {
                git(&fixture.repo, &["commit", "--allow-empty", "-m", "Moved"])?;
            }
            let before = refs(&fixture)?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            deferred::process(&app, &id).await?;
            assert_refused(&app, &id, short);
            assert_eq!(refs(&fixture)?, before);
            git(&fixture.repo, &["switch", "--detach", &fixture.head])?;
        }
        let id = pending(&app, &session, override_args()).await?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        deferred::process(&app, &id).await?;
        assert_eq!(outcome(&app, &id).0, Status::Completed);
        assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD~1"])?, fixture.head);
        assert_eq!(git(&fixture.repo, &["rev-parse", "main"])?, fixture.head);
        assert_eq!(fs::read_to_string(fixture.repo.join(".git/HEAD"))?.trim(), head(&fixture)?);
        assert_unlocked(&fixture);
        Ok(())
    }

    #[tokio::test]
    async fn a_request_from_another_branch_replaces_the_pending_card() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let first = pending(&app, &session, override_args()).await?;
        git(&fixture.repo, &["switch", "-c", "other"])?;
        let response = request(&app, &session, override_args(), Some(Actor::Claude), false).await?;
        let data = &response["structuredContent"];
        assert_eq!(data["replaced_approval_id"], first, "{response}");
        assert_eq!(data["branch"], "other");
        assert_eq!(state(&app, &first), ApprovalState::Withdrawn);
        Ok(())
    }

    #[tokio::test]
    async fn a_switch_during_execution_lands_nowhere() -> Result<()> {
        for detached in [false, true] {
            let fixture = Fixture::new()?;
            let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
            git(&fixture.repo, &["branch", "other"])?;
            if detached {
                git(&fixture.repo, &["switch", "--detach"])?;
            }
            let id = pending(&app, &session, override_args()).await?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            let branches = refs(&fixture)?.1;
            let index = fs::read(fixture.repo.join(".git/index"))?;
            FAULT.with(|fault| *fault.borrow_mut() = Some(Fault::SwitchHead(fixture.repo.clone())));
            let processed = deferred::process(&app, &id).await;
            FAULT.with(|fault| *fault.borrow_mut() = None);
            processed?;
            assert_refused(&app, &id, if detached { &fixture.head[..12] } else { "`main`" });
            assert_eq!(refs(&fixture)?, ("ref: refs/heads/other\n".into(), branches));
            assert_eq!(fs::read(fixture.repo.join(".git/index"))?, index);
            assert_unlocked(&fixture);
        }
        Ok(())
    }

    #[tokio::test]
    async fn head_cannot_be_switched_once_publication_holds_its_lock() -> Result<()> {
        for detached in [false, true] {
            let fixture = Fixture::new()?;
            let (app, _) = setup(&fixture, &modification("tracked.txt", "changed"))?;
            git(&fixture.repo, &["branch", "other"])?;
            if detached {
                git(&fixture.repo, &["switch", "--detach"])?;
            }
            let root = app.project_dir.canonicalize()?;
            let prepared = prepare(&root, &app.state_dir, &crate::mcp::parse_args(&args())?).await?;
            let staged = prepared.stage(&app.state_dir).await?;
            assert!(git(&fixture.repo, &["symbolic-ref", "HEAD", "refs/heads/other"]).is_err());
            let commit = prepared.publish(staged, || Ok(()), &Progress::default()).await?;
            assert_eq!(head(&fixture)?, commit);
            assert_eq!(git(&fixture.repo, &["rev-parse", "other"])?, fixture.head);
            assert_unlocked(&fixture);
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_drain_before_the_first_write_keeps_the_grant_for_after_the_restart() -> Result<()> {
        for point in ["preparing", "staged"] {
            let fixture = Fixture::new()?;
            let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
            let id = pending(&app, &session, override_args()).await?;
            app.hub.decide_approval(&id, GRANT, "local_user", None)?;
            let (_, mut events) = app.hub.subscribe(u64::MAX);
            INTERRUPT.set(Some(point));
            let processed = deferred::process(&app, &id).await;
            INTERRUPT.set(None);
            processed?;
            assert_eq!(entry(&app, &id).phase, Phase::Waiting, "{point}");
            let (summary, _) = app.hub.approval_record(&id)?.unwrap();
            assert_eq!((summary.state, summary.consumed_at), (ApprovalState::Granted, None));
            assert_eq!(head(&fixture)?, fixture.head);
            // Neither a result nor a review notice is left behind.
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event.event, ServerEvent::LocalNotice { .. }), "{point}");
            }
            assert!(app.drain.records(&app.state_dir)?.is_empty(), "{point}");
            assert!(deferred::withdraw(&app, &session, &id, None).await.is_err());

            let app = reopen(&fixture, app);
            deferred::recover(&app).await?;
            deferred::recover(&app).await?;
            assert_eq!(outcome(&app, &id).0, Status::Completed, "{point}");
            assert_eq!(git(&fixture.repo, &["rev-list", "--count", "HEAD"])?, "2");
            assert_eq!(results(&app)?.len(), 1);
            assert_unlocked(&fixture);
        }
        Ok(())
    }

    #[tokio::test]
    async fn withdrawal_never_cancels_a_decision_of_the_user() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        app.hub.decide_approval(&id, GRANT, "local_user", None)?;
        let error = deferred::withdraw(&app, &session, &id, None).await.unwrap_err();
        assert!(error.to_string().contains("already been decided"), "{error:#}");
        assert_eq!(state(&app, &id), ApprovalState::Granted);
        deferred::process(&app, &id).await?;
        assert_eq!(outcome(&app, &id).0, Status::Completed);
        Ok(())
    }

    #[tokio::test]
    async fn requests_interrupted_before_the_snapshot_was_saved_are_withdrawn() -> Result<()> {
        let fixture = Fixture::new()?;
        let (app, session) = setup(&fixture, &modification("tracked.txt", SECRET))?;
        let id = pending(&app, &session, override_args()).await?;
        fs::remove_file(app.state_dir.join("deferred-approvals").join(format!("{id}.json")))?;
        let app = reopen(&fixture, app);
        deferred::recover(&app).await?;
        let (summary, _) = app.hub.approval_record(&id)?.unwrap();
        assert_eq!(summary.state, ApprovalState::Withdrawn);
        assert!(app.hub.pending_approvals()?.0.is_empty());
        assert_eq!(head(&fixture)?, fixture.head);
        Ok(())
    }
}
