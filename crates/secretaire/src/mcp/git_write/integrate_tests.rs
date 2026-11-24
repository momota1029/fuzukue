use super::*;
use std::sync::Arc;

use crate::mcp::workspace_read::git_read::tests::{git, Fixture};
use crate::Hub;
use fuzukue_proto::ServerEvent;

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

async fn integrate(app: &Workspace, session: &str, args: Value) -> Result<Value> {
    call(
        app,
        session,
        crate::mcp::parse_args(&args)?,
        None,
        None,
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

#[tokio::test]
async fn privacy_override_requires_human_approval_and_pins_the_checked_patch() -> Result<()> {
    let fixture = Fixture::new()?;
    let secret = "ghp_synthetic_example";
    let (app, session) = setup(&fixture, &modification("tracked.txt", secret))?;
    let mut request = args();
    request["override_privacy"] = json!(true);
    request["restore_worktree"] = json!(false);

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let run = {
        let app = app.clone();
        tokio::spawn(async move {
            call(
                &app,
                &session,
                crate::mcp::parse_args(&request)?,
                Some(Actor::System),
                None,
                async { Ok(()) },
            )
            .await
        })
    };
    let (id, summary) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let ServerEvent::ApprovalRequested {
                request_id,
                summary,
                ..
            } = events.recv().await?.event
            {
                return Ok::<_, anyhow::Error>((request_id, summary));
            }
        }
    })
    .await??;
    assert!(summary.contains("Privacy override"));
    assert!(summary.contains("tracked.txt:1"));
    assert!(!summary.contains(secret));
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);

    fs::write(
        fixture.repo.join("target/job/change.patch"),
        modification("tracked.txt", "replaced"),
    )?;
    crate::resolve_approval(&app, id, "accept".into(), false).await?;
    let response = run.await??;
    assert_eq!(response["isError"], false, "{response}");
    assert_eq!(
        git(&fixture.repo, &["show", "HEAD:tracked.txt"])?,
        format!("{secret}\nsecond")
    );
    assert_eq!(response["structuredContent"]["restore"]["enabled"], false);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("tracked.txt"))?,
        "first\nsecond\n"
    );
    Ok(())
}
