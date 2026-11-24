use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Arc;

use super::*;

fn deck(project: &Path) -> Result<PathBuf> {
    let deck = project.join(DECK);
    let cards = deck.join("cards");
    fs::create_dir_all(&cards)?;
    fs::write(deck.join("deck.toml"), "kind = \"notes\"\n")?;
    Ok(cards)
}

fn proposal(goal: &str, shape: &str) -> Proposal {
    Proposal {
        goal: goal.into(),
        reason: "Need a single summary per file".into(),
        command_or_script: shape.into(),
        readonly: true,
        suggested_signature: Some("(paths) -> summaries".into()),
    }
}

fn job(id: &str) -> Option<String> {
    Some(id.into())
}

#[test]
fn sanitizes_submission_text() -> Result<()> {
    for (input, expected) in [
        ("rg x /home/example/private/file", "rg x <home-path>"),
        (
            "cat '/Users/Example Person/Private Data/a.txt'",
            "cat <home-path>",
        ),
        (r#"type "C:\Users\Example Person\file""#, "type <home-path>"),
        (r"cat /var/home/example/a\ b", "cat <home-path>"),
        ("echo /root", "echo <home-path>"),
        ("echo contact@example.invalid", "echo <email>"),
        ("API_TOKEN=synthetic-value", "API_TOKEN=<redacted>"),
        (
            r#"{"client_secret":"two words"}"#,
            r#"{"client_secret":<redacted>}"#,
        ),
        ("read --api-key dummy-value", "read --api-key <redacted>"),
        (
            "fetch -H 'Authorization: Bearer fake-value'",
            "fetch -H <redacted>",
        ),
        (
            "git fetch https://test:dummy@example.invalid/repo",
            "git fetch https://<redacted>@example.invalid/repo",
        ),
        ("echo ghp_synthetic_example", "echo <redacted>"),
        (
            "ping example-device.example-tailnet.ts.net",
            "ping <private-host>",
        ),
        ("ping service.internal", "ping <private-host>"),
        (
            "connect 10.0.0.2 172.16.0.1 192.168.0.3 127.0.0.1 169.254.1.2 100.64.0.5",
            "connect <private-ip> <private-ip> <private-ip> <private-ip> <private-ip> <private-ip>",
        ),
        (
            "connect [fd00::1]:8080 fe80::1%test0 ::1 ::ffff:192.168.0.2",
            "connect [<private-ip>]:8080 <private-ip> <private-ip> <private-ip>",
        ),
        ("connect fd00::1.", "connect <private-ip>."),
        (
            "connect 192.0.2.1 [2001:db8::1]",
            "connect 192.0.2.1 [2001:db8::1]",
        ),
        ("printf '%s\\n' public-name", "printf '%s\\n' public-name"),
    ] {
        assert_eq!(
            sanitize::field("test", input, 1024, true)?,
            expected,
            "{input}"
        );
    }
    for unsafe_text in [
        "-----BEGIN PRIVATE KEY-----",
        "opaque AbC0123456789AbC0123456789AbC0123456789",
        "authorization: custom-credential",
        "TOKEN=\x60generate\x60",
    ] {
        let error = sanitize::field("test", unsafe_text, 1024, true).unwrap_err();
        assert!(error.to_string().contains("Replace"));
        assert!(!error.to_string().contains(unsafe_text));
    }
    Ok(())
}

#[test]
fn deduplicates_retries_and_preserves_triage() -> Result<()> {
    let project = tempfile::tempdir()?;
    let cards = deck(project.path())?;
    let record = |goal, shape, worker| propose(project.path(), proposal(goal, shape), job(worker));
    assert_eq!(
        record("Summarize Files", "rg x /home/example/one", "job-one")?,
        json!({"id": "HC-001", "count": 1})
    );
    let path = cards.join("HC-001.md");
    let initial = fs::read_to_string(&path)?;
    assert_eq!(
        record(
            "  summarize FILES  ",
            "  rg  x  /Users/example/two  ",
            "job-one"
        )?["count"],
        1
    );
    assert_eq!(fs::read_to_string(&path)?, initial);

    let older = "2025-01-01";
    let edited = initial
        .replace("status: candidate", "status: accepted\npromoted_to: R-90")
        .replace(&Utc::now().date_naive().to_string(), older)
        + "\n- Triaged by the editor.\n";
    fs::write(&path, edited)?;
    let mut changed = proposal("summarize files", "rg x /Users/example/two");
    changed.readonly = false;
    assert_eq!(
        propose(project.path(), changed, job("job-two"))?,
        json!({"id": "HC-001", "count": 2})
    );

    let saved = fs::read_to_string(&path)?;
    let (meta, body) = parse_card(&path, &saved)?;
    assert!(meta.status == Status::Accepted);
    assert_eq!(meta.promoted_to.as_deref(), Some("R-90"));
    assert_eq!(meta.first_seen.to_string(), older);
    assert_eq!(meta.last_seen, Utc::now().date_naive());
    assert!(!meta.readonly);
    assert!(!saved.contains("/home/example"));
    assert!(!saved.contains("job-one"));
    assert!(body.contains("Triaged by the editor."));
    assert_eq!(body.matches("再び出現").count(), 1);
    assert_eq!(
        fuzukue_cards::parse_card(project.path(), &path, &saved)?.id,
        "HC-001"
    );

    assert_eq!(
        record("summarize files", "rg y docs", "job-two")?["id"],
        "HC-002"
    );
    assert_eq!(
        record("summarize files", "rg z docs", "job-two")?["id"],
        "HC-003"
    );
    let result = list(
        project.path(),
        ListArgs {
            status: None,
            limit: Some(2),
        },
    )?;
    assert_eq!(result["total"], 3);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["candidates"][0]["id"], "HC-001");
    assert_eq!(result["candidates"][1]["id"], "HC-002");
    let accepted = list(
        project.path(),
        ListArgs {
            status: Some(Status::Accepted),
            limit: None,
        },
    )?;
    assert_eq!(accepted["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(accepted["candidates"][0]["promoted_to"], "R-90");
    Ok(())
}

#[test]
fn parallel_submissions_share_one_candidate() -> Result<()> {
    let project = tempfile::tempdir()?;
    let path = project.path();
    deck(path)?;
    std::thread::scope(|scope| {
        let calls: Vec<_> = ["job-one", "job-one", "job-two", "job-three"]
            .into_iter()
            .map(|id| {
                scope.spawn(move || {
                    propose(path, proposal("Count files", "rg --files | wc -l"), job(id))
                })
            })
            .collect();
        for call in calls {
            assert_eq!(call.join().unwrap()?["id"], "HC-001");
        }
        Ok::<_, anyhow::Error>(())
    })?;
    let found = list(
        path,
        ListArgs {
            status: None,
            limit: None,
        },
    )?;
    assert_eq!(found["total"], 1);
    assert_eq!(found["candidates"][0]["count"], 3);
    Ok(())
}

#[test]
fn invalid_inputs_leave_the_deck_unchanged() -> Result<()> {
    let project = tempfile::tempdir()?;
    let cards = deck(project.path())?;
    for (field, limit) in [
        ("goal", GOAL_BYTES),
        ("reason", DETAIL_BYTES),
        ("command_or_script", COMMAND_BYTES),
        ("suggested_signature", DETAIL_BYTES),
    ] {
        let too_long = "é".repeat(limit / 2 + 1);
        let mut input = proposal("Count files", "rg --files");
        match field {
            "goal" => input.goal = too_long,
            "reason" => input.reason = too_long,
            "command_or_script" => input.command_or_script = too_long,
            _ => input.suggested_signature = Some(too_long),
        }
        assert!(
            propose(project.path(), input, None)
                .unwrap_err()
                .to_string()
                .contains(field)
        );
    }
    let mut input = proposal("Count files", "rg --files");
    input.reason = "-----BEGIN PRIVATE KEY-----".into();
    assert!(
        propose(project.path(), input, None)
            .unwrap_err()
            .to_string()
            .contains("reason")
    );
    assert_eq!(fs::read_dir(cards)?.count(), 0);
    Ok(())
}

#[test]
fn only_the_fixed_deck_accepts_writes() -> Result<()> {
    let project = tempfile::tempdir()?;
    let target = tempfile::tempdir()?;
    deck(target.path())?;
    let deck_path = project.path().join(DECK);
    let run = || propose(project.path(), proposal("Count files", "rg --files"), None);
    assert!(
        run()
            .unwrap_err()
            .to_string()
            .contains("no harness-candidates deck")
    );
    assert!(!deck_path.exists());
    symlink(target.path().join(DECK), &deck_path)?;
    assert!(run().is_err());
    assert_eq!(
        fs::read_dir(target.path().join(DECK).join("cards"))?.count(),
        0
    );
    fs::remove_file(&deck_path)?;

    let cards = deck(project.path())?;
    fs::remove_dir(&cards)?;
    symlink(target.path().join(DECK).join("cards"), &cards)?;
    assert!(run().is_err());
    assert_eq!(
        fs::read_dir(target.path().join(DECK).join("cards"))?.count(),
        0
    );
    fs::remove_file(&cards)?;
    fs::create_dir(&cards)?;

    let outside = target.path().join("notes.md");
    fs::write(&outside, "unchanged")?;
    let card = cards.join("HC-001.md");
    symlink(&outside, &card)?;
    assert!(run().is_err());
    fs::remove_file(&card)?;
    fs::hard_link(&outside, &card)?;
    assert!(run().is_err());
    assert_eq!(fs::read_to_string(outside)?, "unchanged");
    Ok(())
}

#[tokio::test]
async fn list_clamps_its_limit() -> Result<()> {
    use crate::mcp::workspace_read::range::tests::{boundary_cases, wrong_types};

    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    deck(project.path())?;
    for i in 0..51 {
        let input = proposal("Count files", &format!("rg --files folder-{i}"));
        propose(project.path(), input, None)?;
    }
    let app = crate::Workspace::new(
        project.path().into(),
        "127.0.0.1:7878".into(),
        state.path().into(),
        Arc::new(crate::Hub::new()),
    );
    for (limit, expected) in boundary_cases(1, 50, 20) {
        let args = limit.map_or_else(|| json!({}), |limit| json!({"limit": limit}));
        let result = Tool::List.call(&app, None, &args).await?;
        assert_eq!(result["candidates"].as_array().unwrap().len(), expected);
        assert_eq!(result["truncated"], true);
    }
    for limit in wrong_types() {
        let error = Tool::List
            .call(&app, None, &json!({"limit": limit}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("expected an integer"));
    }
    Ok(())
}

#[tokio::test]
async fn mcp_visibility_schema_and_calls() -> Result<()> {
    use super::super as mcp;

    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    deck(project.path())?;
    let app = Arc::new(crate::Workspace::new(
        project.path().into(),
        "127.0.0.1:7878".into(),
        state.path().into(),
        Arc::new(crate::Hub::new()),
    ));
    let worker = mcp::worker_tool_names(&[]);
    let worker_policy = mcp::codex_tool_policy(Some(&worker));
    let desk_policy = mcp::codex_tool_policy(None);
    let desk = mcp::partner_tool_list(false);
    for tool in Tool::ALL {
        let name = tool.name();
        assert!(worker.iter().any(|entry| entry == name));
        assert_eq!(worker_policy[name]["approval_mode"], "approve");
        assert!(!mcp::worker_tool_names(&["fs_read".into()]).contains(&name.into()));
        assert!(!mcp::planning_tool_names(false).contains(&name.into()));
        assert!(!mcp::planning_tool_names(true).contains(&name.into()));
        assert!(
            desk["tools"]
                .as_array()
                .unwrap()
                .contains(&tool.definition())
        );
    }
    assert!(desk_policy.get("propose_harness").is_none());
    assert_eq!(
        desk_policy["list_harness_candidates"]["approval_mode"],
        "approve"
    );
    let schema = Tool::Propose.definition();
    let inputs = &schema["inputSchema"];
    assert_eq!(
        inputs["required"],
        json!(["goal", "reason", "command_or_script", "readonly"])
    );
    assert_eq!(
        inputs["properties"]["command_or_script"]["maxLength"],
        COMMAND_BYTES
    );
    assert_eq!(inputs["properties"]["readonly"]["type"], "boolean");
    assert_eq!(inputs["additionalProperties"], false);

    let request = |name, args| {
        mcp::handler(
            axum::extract::State(app.clone()),
            axum::extract::Query(mcp::McpQuery {
                session: "test".into(),
                backend: None,
                workspace: None,
                job: None,
            }),
            axum::http::HeaderMap::new(),
            axum::Json(
                json!({"id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
            ),
        )
    };
    let bad = request(
        "propose_harness",
        json!({"goal": "Count files", "reason": "Need a count",
        "command_or_script": "rg --files", "readonly": true, "path": "../outside"}),
    )
    .await;
    let bad: Value = serde_json::from_slice(&axum::body::to_bytes(bad.into_body(), 16_384).await?)?;
    assert_eq!(bad["result"]["isError"], true);

    let result = request(
        "propose_harness",
        json!({"goal": "Count files", "reason": "Need a count",
        "command_or_script": "rg --files", "readonly": true}),
    )
    .await;
    let result: Value =
        serde_json::from_slice(&axum::body::to_bytes(result.into_body(), 16_384).await?)?;
    assert_eq!(
        result["result"]["structuredContent"],
        json!({"id": "HC-001", "count": 1})
    );
    let listed = request("list_harness_candidates", json!({"status": "candidate"})).await;
    let listed: Value =
        serde_json::from_slice(&axum::body::to_bytes(listed.into_body(), 16_384).await?)?;
    assert_eq!(listed["result"]["structuredContent"]["total"], 1);
    Ok(())
}
