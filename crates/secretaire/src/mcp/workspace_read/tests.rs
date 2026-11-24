use super::*;
use crate::mcp::{self, McpQuery};
use crate::worker_scope::Scope;
use anyhow::Context;
use axum::body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::sync::Arc;

struct Fixture {
    _temp: tempfile::TempDir,
    worktree: PathBuf,
    project: PathBuf,
    outside: PathBuf,
    reference: PathBuf,
    state: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let worktree = root.join("job-tree");
        let project = root.join("project");
        let outside = root.join("outside");
        let reference = root.join("references");
        let state = root.join("state");
        let scratch = state.join("worker-own");
        for dir in [&worktree, &project, &outside, &reference, &state, &scratch] {
            fs::create_dir(dir)?;
        }
        fs::write(
            fuzukue_proto::compat::workspace_config(&project),
            "[agents]\nread_roots = ['../references', '../state']",
        )?;
        Ok(Self {
            _temp: temp,
            worktree,
            project,
            outside,
            reference,
            state,
            scratch,
        })
    }

    fn worker(&self) -> Result<ReadScope> {
        ReadScope::new(
            &self.project,
            &self.state,
            Some(&Scope::open(&self.project, &self.worktree, &self.scratch)?),
        )
    }

    fn front_desk(&self) -> Result<ReadScope> {
        ReadScope::new(&self.project, &self.state, None)
    }
}

#[test]
fn read_root_config_cannot_follow_a_protected_file() -> Result<()> {
    let f = Fixture::new()?;
    let config = fuzukue_proto::compat::workspace_config(&f.project);
    let private = f.state.join("private");
    fs::write(&private, "this is protected and invalid TOML")?;
    fs::remove_file(&config)?;
    symlink(&private, config)?;
    assert!(f.front_desk().is_err());
    Ok(())
}

#[tokio::test]
async fn read_scope_applies_to_all_filesystem_tools() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.outside.join("private"), "outside")?;
    fs::write(f.state.join("other-job"), "other worker")?;
    fs::write(f.project.join("other-tree"), "front desk only")?;
    fs::write(f.reference.join("notes"), "external configured reference")?;
    fs::write(f.scratch.join("notes"), "own scratch")?;
    fs::write(f.worktree.join("file"), "worktree")?;
    for secret in [
        ".env",
        ".env.local",
        "x.pem",
        "id_rsa_backup",
        "keystore.properties",
        "remote-token",
    ] {
        fs::write(f.worktree.join(secret), "secret")?;
    }
    symlink(&f.outside, f.worktree.join("escape"))?;
    symlink(f.state.join("other-job"), f.worktree.join("state-alias"))?;
    symlink(".env", f.worktree.join("secret-alias"))?;
    symlink("file", f.worktree.join("inside-alias"))?;
    fs::hard_link(f.state.join("other-job"), f.worktree.join("hard-link"))?;
    for (path, expected) in [
        (f.outside.join("private"), "outside the workspace"),
        (f.project.join("other-tree"), "outside the workspace"),
        (PathBuf::from("../outside/private"), "Parent-directory"),
        (PathBuf::from("escape/private"), "outside the workspace"),
        (PathBuf::from("state-alias"), "configuration or state"),
        (PathBuf::from("hard-link"), "hard-linked"),
        (PathBuf::from("secret-alias"), "secret-looking"),
        (f.state.join("other-job"), "configuration or state"),
        (PathBuf::from("/etc/passwd"), "outside the workspace"),
    ] {
        let error = Tool::Read
            .call(scope.clone(), &json!({"path": path}))
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{path:?}: {error:#}"
        );
    }
    for path in [
        ".env",
        ".env.local",
        "x.pem",
        "id_rsa_backup",
        "keystore.properties",
        "remote-token",
    ] {
        let error = Tool::Read
            .call(scope.clone(), &json!({"path": path}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("secret-looking"));
    }
    for (path, text) in [
        ("inside-alias".into(), "worktree"),
        (f.reference.join("notes"), "external configured reference"),
        (f.scratch.join("notes"), "own scratch"),
    ] {
        let result = Tool::Read
            .call(scope.clone(), &json!({"path": path}))
            .await?;
        assert_eq!(result["text"], format!("1: {text}\n"));
    }
    let listed = Tool::List
        .call(scope.clone(), &json!({"path": &f.reference}))
        .await?;
    assert_eq!(listed["entries"][0]["path"], "notes");
    let found = Tool::Search
        .call(
            scope.clone(),
            &json!({"path": &f.reference, "pattern": "configured"}),
        )
        .await?;
    assert_eq!(found["matches"][0]["path"], "notes");
    assert!(
        Tool::List
            .call(scope.clone(), &json!({"path": "escape"}))
            .await
            .is_err()
    );
    assert!(
        Tool::Search
            .call(scope, &json!({"pattern": "outside", "path": "escape"}))
            .await
            .is_err()
    );
    assert!(
        Tool::Read
            .call(f.front_desk()?, &json!({"path": f.scratch.join("notes")}))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn numbered_read_paginates_and_refuses_binary_and_media() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(
        f.worktree.join("lines"),
        (1..=450).map(|i| format!("Line {i}\n")).collect::<String>(),
    )?;
    let first = Tool::Read
        .call(scope.clone(), &json!({"path": "lines"}))
        .await?;
    assert!(first["text"].as_str().unwrap().starts_with("1: Line 1\n"));
    assert!(first["text"].as_str().unwrap().contains("400: Line 400\n"));
    assert_eq!(first["truncated"], true);
    assert_eq!(first["next_start_line"], 401);
    assert!(first["text"].as_str().unwrap().contains(TRUNCATED));
    let second = Tool::Read
        .call(scope.clone(), &json!({"path": "lines", "start_line": 401}))
        .await?;
    assert_eq!(second["truncated"], false);
    assert!(
        second["text"]
            .as_str()
            .unwrap()
            .ends_with("450: Line 450\n")
    );
    let bytes = Tool::Read
        .call(scope.clone(), &json!({"path": "lines", "max_bytes": 7}))
        .await?;
    assert_eq!(bytes["next_start_line"], 2);
    let range = Tool::Read
        .call(
            scope.clone(),
            &json!({"path": "lines", "start_line": 2, "end_line": 3}),
        )
        .await?;
    assert!(
        range["text"]
            .as_str()
            .unwrap()
            .starts_with("2: Line 2\n3: Line 3\n")
    );
    assert_eq!(range["next_start_line"], 4);
    fs::write(f.worktree.join("long"), "日本語\nnext\n")?;
    let long = Tool::Read
        .call(scope.clone(), &json!({"path": "long", "max_bytes": 5}))
        .await?;
    assert!(long["text"].as_str().unwrap().starts_with("1: 日\n"));
    assert_eq!(long["next_start_line"], 2);
    for (name, content, marker) in [
        ("nul", b"text\0".as_slice(), "Binary"),
        ("bad-utf8", b"\xff".as_slice(), "UTF-8"),
        ("paper.pdf", b"text".as_slice(), "locate_pdf"),
        ("image.png", b"text".as_slice(), "image viewer"),
        ("renamed", b"%PDF-1.7".as_slice(), "locate_pdf"),
    ] {
        fs::write(f.worktree.join(name), content)?;
        let error = Tool::Read
            .call(scope.clone(), &json!({"path": name}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(marker));
    }
    Ok(())
}

#[tokio::test]
async fn listing_bounds_depth_hides_generated_entries_and_caps_results() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    for dir in [".git", "target", "node_modules", ".hidden", "src/sub/deep"] {
        fs::create_dir_all(f.worktree.join(dir))?;
        fs::write(f.worktree.join(dir).join("sample.rs"), "abc")?;
    }
    fs::write(f.worktree.join(".env"), "secret")?;
    let normal = Tool::List
        .call(scope.clone(), &json!({"depth": 3, "glob": "*.rs"}))
        .await?;
    assert_eq!(normal["entries"], json!([]));
    let roots = Tool::List.call(scope.clone(), &json!({})).await?;
    assert_eq!(roots["entries"].as_array().unwrap().len(), 1);
    assert_eq!(roots["entries"][0]["path"], "src");
    let all = Tool::List
        .call(
            scope.clone(),
            &json!({"depth": 2, "include_hidden": true, "include_ignored": true}),
        )
        .await?;
    let names: Vec<_> = all["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    for path in [
        ".git",
        ".hidden/sample.rs",
        "target/sample.rs",
        "node_modules/sample.rs",
    ] {
        assert!(names.contains(&path));
    }
    for path in [".git/sample.rs", ".env"] {
        assert!(!names.contains(&path));
    }
    for i in 0..501 {
        fs::write(f.worktree.join(format!("item-{i:03}")), "")?;
    }
    let capped = Tool::List
        .call(scope.clone(), &json!({"glob": "item-*"}))
        .await?;
    assert_eq!(capped["entries"].as_array().unwrap().len(), 500);
    assert!(capped["note"].as_str().unwrap().contains(TRUNCATED));
    let max_depth = Tool::List.call(scope.clone(), &json!({"depth": 3})).await?;
    assert_eq!(Tool::List.call(scope, &json!({"depth": 4})).await?, max_depth);
    Ok(())
}

#[tokio::test]
async fn search_is_bounded_and_passes_option_like_patterns_as_data() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.worktree.join("a.txt"), "above\r\n-HIT\r\nbelow\r\n-hit")?;
    fs::write(f.worktree.join("b.txt"), "-hit\nother\n")?;
    fs::write(f.worktree.join("wrong.md"), "-hit\n")?;
    fs::write(f.worktree.join("binary.txt"), "-hit\0")?;
    fs::write(f.worktree.join(".env"), "-hit\n")?;
    fs::create_dir(f.worktree.join("target"))?;
    fs::write(f.worktree.join("target/skipped.txt"), "-hit")?;
    fs::write(f.worktree.join("huge.txt"), "-hit\n".repeat(220_000))?;
    symlink(&f.outside, f.worktree.join("link"))?;
    let scope = f.worker()?;
    let result = Tool::Search.call(scope.clone(), &json!({
        "pattern": "-hit", "glob": "*.txt", "fixed_string": true, "ignore_case": true, "context": 1, "max_results": 2,
    })).await?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 2);
    assert_eq!(result["matches"][0]["path"], "a.txt");
    assert_eq!(result["matches"][0]["line"], 2);
    assert_eq!(result["matches"][0]["context"][0]["text"], "above");
    assert_eq!(result["matches"][1]["context"].as_array().unwrap().len(), 2);
    assert!(result["note"].as_str().unwrap().contains(TRUNCATED));
    let all = Tool::Search
        .call(
            scope,
            &json!({"pattern": "^-hit$", "glob": "*.txt", "ignore_case": true}),
        )
        .await?;
    assert_eq!(all["matches"].as_array().unwrap().len(), 3);
    assert_eq!(all["matches"][2]["path"], "b.txt");
    assert_eq!(all["matches"][2]["line"], 1);
    assert_eq!(all["truncated"], false);
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git").current_dir(dir).args(args).status()?;
    ensure!(status.success(), "Git fixture failed: {args:?}");
    Ok(())
}

#[tokio::test]
async fn git_history_supports_worktree_head_and_checks_revision_paths() -> Result<()> {
    let f = Fixture::new()?;
    git(&f.project, &["init", "-q"])?;
    fs::write(f.project.join("draft.txt"), "committed text\n")?;
    fs::write(f.project.join(".env"), "committed secret\n")?;
    fs::write(f.project.join("long.txt"), "a".repeat(MAX_BYTES + 1))?;
    symlink("draft.txt", f.project.join("historical-link"))?;
    git(&f.project, &["add", "--all"])?;
    git(
        &f.project,
        &[
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "-qm",
            "first",
        ],
    )?;
    git(
        &f.project,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            f.worktree.to_str().unwrap(),
        ],
    )?;
    fs::remove_file(f.worktree.join("draft.txt"))?;
    let scope = f.worker()?;
    let metadata = Tool::GitShow
        .call(scope.clone(), &json!({"rev": "HEAD"}))
        .await?;
    assert!(metadata["text"].as_str().unwrap().contains("first"));
    assert!(
        !metadata["text"]
            .as_str()
            .unwrap()
            .contains("committed secret")
    );
    let old = Tool::GitShow
        .call(scope.clone(), &json!({"rev": "HEAD", "path": "draft.txt"}))
        .await?;
    assert_eq!(old["text"], "committed text\n");
    let long = Tool::GitShow
        .call(scope.clone(), &json!({"rev": "HEAD", "path": "long.txt"}))
        .await?;
    assert_eq!(long["truncated"], true);
    assert!(long["text"].as_str().unwrap().contains(TRUNCATED));
    for args in [
        json!({"rev": "--help"}),
        json!({"rev": "HEAD:.env"}),
        json!({"rev": "HEAD", "path": ".env"}),
        json!({"rev": "HEAD", "path": "historical-link"}),
        json!({"rev": "HEAD", "path": "../project/draft.txt"}),
        json!({"rev": "HEAD", "path": f.project.join("draft.txt")}),
    ] {
        assert!(
            Tool::GitShow.call(scope.clone(), &args).await.is_err(),
            "{args}"
        );
    }
    let alternates = f.project.join(".git/objects/info/alternates");
    fs::write(&alternates, f.outside.display().to_string())?;
    let error = Tool::GitShow
        .call(scope, &json!({"rev": "HEAD"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("alternate object stores"));

    Ok(())
}

#[tokio::test]
async fn read_tools_follow_allowlists_and_review_credentials() -> Result<()> {
    let f = Fixture::new()?;
    let worker = mcp::worker_tool_names(&[]);
    let codex = mcp::codex_tool_policy(Some(&worker));
    let front_desk = mcp::codex_tool_policy(None);
    let restrictive = mcp::worker_tool_names(&["fs_read".into()]);
    let claude = mcp::claude_tool_names(false, true).collect::<Vec<_>>();
    for tool in Tool::ALL {
        let name = tool.name();
        assert!(worker.iter().any(|entry| entry == name));
        assert_eq!(codex[name]["approval_mode"], "approve");
        assert_eq!(front_desk[name]["approval_mode"], "approve");
        assert!(claude.contains(&format!("mcp__fuzukue__{name}")));
        assert!(mcp::codex_tool_policy(Some(&[])).get(name).is_none());
        assert!(!mcp::planning_tool_names(false).contains(&name.to_owned()));
        assert!(!mcp::planning_tool_names(true).contains(&name.to_owned()));
        assert_eq!(restrictive.contains(&name.to_owned()), name == "fs_read");
    }

    let build = "build_paper";
    assert!(worker.iter().any(|entry| entry == build));
    assert_eq!(codex[build]["approval_mode"], "approve");
    assert!(front_desk.get(build).is_none());
    assert!(!restrictive.iter().any(|entry| entry == build));
    for consult in [false, true] {
        assert!(!mcp::planning_tool_names(consult).iter().any(|entry| entry == build));
    }

    let app = Arc::new(crate::Workspace::new(
        f.project.clone(),
        "127.0.0.1:7878".into(),
        f.state.clone(),
        Arc::new(crate::Hub::new()),
    ));
    fs::write(f.project.join("notes"), "review text")?;
    let lease = ReviewLease::open(&app, "review-math")?.context("Expected review MCP")?;
    let url = url::Url::parse(lease.launch.mcp_url.as_ref().unwrap())?;
    let job = url
        .query_pairs()
        .find(|(key, _)| key == "job")
        .unwrap()
        .1
        .into_owned();
    let config: Value = serde_json::from_str(&review_claude_config(Some(&lease.launch)))?;
    assert_eq!(config["mcpServers"]["fuzukue"]["url"], url.to_string());
    let key = lease.launch.key.clone();
    let request = |session: &str, token: Option<&str>, method: &str, params: Value| {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        mcp::handler(
            State(app.clone()),
            Query(McpQuery {
                session: session.into(),
                backend: None,
                workspace: None,
                job: Some(job.clone()),
            }),
            headers,
            axum::Json(json!({"id": 1, "method": method, "params": params})),
        )
    };
    assert_eq!(
        request("review-math", None, "tools/list", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request("another", Some(&key), "tools/list", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let listed = request("review-math", Some(&key), "tools/list", json!({})).await;
    let listed: Value = serde_json::from_slice(&body::to_bytes(listed.into_body(), 16_384).await?)?;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 4);
    let read = request(
        "review-math",
        Some(&key),
        "tools/call",
        json!({"name": "fs_read", "arguments": {"path": "notes"}}),
    )
    .await;
    let read: Value = serde_json::from_slice(&body::to_bytes(read.into_body(), 16_384).await?)?;
    assert!(read.to_string().contains("review text"));
    for name in ["git_commit", "plan_status", build] {
        let denied = request(
            "review-math",
            Some(&key),
            "tools/call",
            json!({"name": name}),
        )
        .await;
        let denied: Value = serde_json::from_slice(&body::to_bytes(denied.into_body(), 16_384).await?)?;
        assert_eq!(denied["result"]["isError"], true, "{name}");
    }
    drop(lease);
    assert_eq!(
        request("review-math", Some(&key), "tools/list", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn plan_status_reads_the_worker_deck_and_reports_dependency_frontiers() -> Result<()> {
    let f = Fixture::new()?;
    let deck = f.worktree.join("roadmap");
    fs::create_dir_all(deck.join("tasks"))?;
    fs::write(
        deck.join("deck.toml"),
        "cards_dir = 'tasks'\nstructure = 'priorities.yaml'",
    )?;
    fs::write(
        deck.join("priorities.yaml"),
        "sections:\n- title: Priority\n  items: [R-08, R-03, R-02, R-01, R-04]\n",
    )?;
    for (id, status, requires) in [
        ("R-01", "done", ""),
        ("R-02", "todo", "'roadmap#R-01'"),
        ("R-03", "blocked-on-user", ""),
        ("R-04", "todo", "R-03, R-02"),
        ("R-05", "doing", "R-02"),
        ("R-06", "todo", "R-07"),
        ("R-07", "todo", "R-06"),
        ("R-08", "todo", ""),
        ("R-09", "todo", "absent"),
        ("R-10", "todo", "R-09"),
        ("R-11", "todo", "R-11"),
    ] {
        fs::write(
            deck.join(format!("tasks/{id}.md")),
            format!(
                "---\nid: {id}\nstatus: {status}\ntitle: Task {id}\nrequires: [{requires}]\nissues: ['#42']\n---\n"
            ),
        )?;
    }
    fs::create_dir_all(f.project.join("roadmap/cards"))?;
    fs::write(f.project.join("roadmap/deck.toml"), "")?;
    fs::write(
        f.project.join("roadmap/cards/shared.md"),
        "---\nid: shared\nstatus: todo\nrequires: []\n---\n",
    )?;

    let scope = f.worker()?;
    let report = Tool::PlanStatus.call(scope.clone(), &json!({})).await?;
    assert_eq!(report["deck"], "roadmap");
    assert_eq!(
        report["counts"],
        json!({"todo": 8, "done": 1, "doing": 1, "blocked-on-user": 1})
    );
    assert_eq!(report["ready"], json!(["R-08", "R-02"]));
    assert_eq!(
        report["by_status"]["blocked-on-user"][0]["blocks"],
        json!(["R-04"])
    );
    assert_eq!(report["by_status"]["doing"][0]["requires"], json!(["R-02"]));
    assert_eq!(report["by_status"]["doing"][0]["issues"], json!(["#42"]));
    assert_eq!(
        report["topological_order"],
        json!(["R-08", "R-03", "R-02", "R-04", "R-05"])
    );
    assert_eq!(
        report["unscheduled"],
        json!(["R-06", "R-07", "R-09", "R-10", "R-11"])
    );
    assert_eq!(
        report["requires_cycles"][0]["cards"],
        json!(["R-06", "R-07"])
    );
    assert_eq!(report["requires_cycles"][1]["cards"], json!(["R-11"]));
    assert_eq!(
        report["unresolved_requires"],
        json!([
            {"from": "R-09", "target": "absent", "type": "dangling_requires"}
        ])
    );
    assert!(report["by_status"].get("done").is_none());
    assert_eq!(report["truncated"], false);

    let filtered = Tool::PlanStatus
        .call(
            scope.clone(),
            &json!({
                "deck": "./roadmap", "status": "todo", "id_prefix": "R-0", "limit": -10
            }),
        )
        .await?;
    assert_eq!(filtered["limit"], 1);
    assert_eq!(filtered["by_status"]["todo"][0]["id"], "R-08");
    assert_eq!(filtered["totals"]["ready"], 2);
    assert_eq!(filtered["counts"], report["counts"]);
    assert_eq!(filtered["truncated"], true);

    let done = Tool::PlanStatus
        .call(scope, &json!({"status": "done", "limit": 101}))
        .await?;
    assert_eq!(done["limit"], 100);
    assert_eq!(done["by_status"]["done"][0]["id"], "R-01");
    assert_eq!(done["by_status"]["done"][0]["blocks"], json!(["R-02"]));
    assert_eq!(done["ready"], json!([]));
    let desk = Tool::PlanStatus
        .call(f.front_desk()?, &json!({"include_done": true}))
        .await?;
    assert_eq!(desk["ready"], json!(["shared"]));
    Ok(())
}

#[tokio::test]
async fn plan_status_rejects_out_of_scope_deck_sources() -> Result<()> {
    let f = Fixture::new()?;
    let deck = f.worktree.join("custom");
    fs::create_dir(&deck)?;
    fs::write(deck.join("deck.toml"), "dirs = ['escape']")?;
    symlink(&f.project, deck.join("escape"))?;

    let scope = f.worker()?;
    assert!(
        Tool::PlanStatus
            .call(scope.clone(), &json!({"deck": "custom"}))
            .await
            .is_err()
    );
    assert!(
        Tool::PlanStatus
            .call(scope.clone(), &json!({"deck": "../project"}))
            .await
            .is_err()
    );

    let dir = deck.join("tasks");
    fs::create_dir(&dir)?;
    fs::write(deck.join("deck.toml"), "dirs = ['tasks']")?;
    fs::write(
        dir.join("A.md"),
        "---\nid: A\nstatus: todo\nrequires: [other:B]\n---\n",
    )?;
    fs::write(
        f.outside.join("hidden.md"),
        "---\nid: hidden\nstatus: todo\n---\n",
    )?;
    symlink(f.outside.join("hidden.md"), dir.join("linked.md"))?;
    let report = Tool::PlanStatus
        .call(scope, &json!({"deck": "custom"}))
        .await?;
    assert_eq!(report["counts"], json!({"todo": 1}));
    assert_eq!(report["ready"], json!([]));
    assert_eq!(
        report["by_status"]["todo"][0]["unresolved_requires"],
        json!([
            {"target": "other:B", "type": "external_requires"}
        ])
    );
    Ok(())
}
