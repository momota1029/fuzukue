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
fn worker_can_search_its_session_spill_outside_its_checkout() -> Result<()> {
    let f = Fixture::new()?;
    let owner = crate::mcp::result_spill::Owner::thread("owner");
    let path = crate::mcp::result_spill::save(&f.project, &owner, "txt", b"saved result\n")?;
    let scope = f.worker()?.with_results(owner);
    let file = scope.resolve(&path)?;
    scope.audit_read(&file)?;
    assert!(f.worker()?.resolve(&path).is_err());
    assert!(f.front_desk()?.resolve(&path.with_file_name(".gitignore")).is_err());
    let git = |args: &[&str]| Command::new("git").current_dir(&f.project).args(args).output();
    assert!(git(&["init", "-q"])?.status.success());
    let status = git(&["status", "--short", "--untracked-files=all"])?;
    assert!(status.status.success());
    assert!(!String::from_utf8(status.stdout)?.contains("mcp-results"));
    Ok(())
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
        let error = Tool::Jq
            .call(scope.clone(), &json!({"path": path, "filter": "."}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{path:?}: {error:#}");
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
async fn jq_queries_json_and_json_lines_in_the_read_scope() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.project.join("names.json"), "{\n\"names\": [\"雪\", \"Ada\"]\n}\n")?;
    fs::write(
        f.scratch.join("events.jsonl"),
        "{\"level\":\"info\",\"message\":\"start\"}\n{\"level\":\"error\",\"message\":\"雪\"}",
    )?;
    let result = Tool::Jq.call(f.front_desk()?, &json!({
        "path": "names.json", "filter": "def names: .names[]; names # comment",
    })).await?;
    assert_eq!(result, json!({"output": "\"雪\"\n\"Ada\"\n", "truncated": false}));
    let result = Tool::Jq.call(f.worker()?, &json!({
        "path": f.scratch.join("events.jsonl"), "filter": "select(.level == \"error\") | .message",
        "raw": true,
    })).await?;
    assert_eq!(result, json!({"output": "雪\n", "truncated": false}));
    let result = Tool::Jq.call(f.worker()?, &json!({
        "path": f.scratch.join("events.jsonl"), "filter": "[., inputs] | length", "raw": true,
    })).await?;
    assert_eq!(result["output"], "2\n");
    Ok(())
}

#[tokio::test]
async fn jq_reports_invalid_input_and_keeps_filters_inside_the_read_scope() -> Result<()> {
    let f = Fixture::new()?;
    let oversized = fs::File::create(f.project.join("large.json"))?;
    oversized.set_len((READ_SCAN_BYTES + 1) as u64)?;
    let error = Tool::Jq.call(f.front_desk()?, &json!({
        "path": "large.json", "filter": ".",
    })).await.unwrap_err();
    assert!(error.to_string().contains("input exceeds the 16 MiB limit"));
    fs::write(f.project.join("valid.json"), "{}")?;
    fs::write(f.project.join("invalid.jsonl"), "{}\nplain text\n")?;
    fs::write(f.outside.join("data.json"), "\"outside\"")?;
    fs::write(f.outside.join("fixture.jq"), "module {marker: \"outside\"};")?;
    let scope = f.front_desk()?;
    for (path, filter, expected) in [
        ("invalid.jsonl", ".".to_owned(), "Invalid JSON or JSON Lines"),
        ("valid.json", ".[,".into(), "syntax error"),
        ("valid.json", "--help".into(), "error"),
        ("valid.json", format!("import \"data\" as $data {{search: {}}}; $data",
            json!(f.outside)), "module imports are disabled"),
        ("valid.json", "\"fixture\" | modulemeta".into(), "compile error"),
    ] {
        let error = Tool::Jq.call(scope.clone(), &json!({"path": path, "filter": filter}))
            .await.unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    let result = Tool::Jq
        .call(scope, &json!({"path": "valid.json", "filter": "$ENV"}))
        .await?;
    assert_eq!(result["output"], "{}\n");
    Ok(())
}

#[tokio::test]
async fn jq_truncates_output_at_the_byte_limit_and_deadline() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.project.join("input.json"), "\"a雪\"")?;
    let scope = f.front_desk()?;
    let capped = Tool::Jq.call(scope.clone(), &json!({
        "path": "input.json", "filter": ".", "raw": true, "max_bytes": 2,
    })).await?;
    assert_eq!(capped["output"], "a");
    assert_eq!(capped["truncated"], true);
    assert!(capped["note"].as_str().unwrap().contains("2-byte limit"));
    let timed_out = Tool::Jq.call(scope, &json!({
        "path": "input.json", "filter": "def spin: spin; ., spin", "raw": true,
    })).await?;
    assert_eq!(timed_out["output"], "a雪\n");
    assert_eq!(timed_out["truncated"], true);
    assert!(timed_out["note"].as_str().unwrap().contains("time limit"));
    Ok(())
}

async fn check_integer_argument(
    tool: Tool,
    scope: &ReadScope,
    base: Value,
    field: &str,
    bounds: [usize; 3],
    check: impl Fn(&Value, usize),
) -> Result<()> {
    let [min, max, default] = bounds;
    let definition = tool.definition();
    let property = &definition["inputSchema"]["properties"][field];
    assert_eq!(property["minimum"], min);
    assert_eq!(property["maximum"], max);
    let description = property["description"].as_str().unwrap();
    assert!(description.contains("clamped") || description.contains("丸める"));
    if field == "end_line" {
        assert!(description.contains("399"));
    } else {
        assert_eq!(property["default"], default);
    }
    for (value, expected) in range::tests::boundary_cases(min, max, default) {
        let mut args = base.clone();
        if let Some(value) = value {
            args[field] = value;
        }
        let result = tool
            .call(scope.clone(), &args)
            .await
            .with_context(|| format!("calling {} with {args}", tool.name()))?;
        check(&result, expected);
    }
    for value in range::tests::wrong_types() {
        let mut args = base.clone();
        args[field] = value;
        let error = tool.call(scope.clone(), &args).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("expected an integer"),
            "{args}: {error:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn read_clamps_line_numbers_and_byte_budget() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.worktree.join("scan"), vec![b'\n'; READ_SCAN_BYTES])?;
    let definition = Tool::Read.definition();
    let start = &definition["inputSchema"]["properties"]["start_line"];
    assert_eq!(start["minimum"], -(READ_SCAN_BYTES as i64));
    assert_eq!(start["maximum"], READ_SCAN_BYTES);
    assert_eq!(start["default"], 1);
    let cases = [
        (Value::Null, 1),
        (json!(0), 1),
        (json!(1), 1),
        (json!(i64::MIN), 1),
        (json!(-(READ_SCAN_BYTES as i64)), 1),
        (json!(-1), READ_SCAN_BYTES),
        (json!(READ_SCAN_BYTES), READ_SCAN_BYTES),
        (json!(u64::MAX), READ_SCAN_BYTES),
    ];
    for (value, expected) in cases {
        let mut args = json!({"path": "scan", "max_bytes": 1});
        if !value.is_null() {
            args["start_line"] = value;
        }
        let result = Tool::Read.call(scope.clone(), &args).await?;
        assert!(result["text"].as_str().unwrap().starts_with(&format!("{expected}: \n")));
    }
    for value in range::tests::wrong_types() {
        let error = Tool::Read
            .call(scope.clone(), &json!({"path": "scan", "start_line": value}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("expected an integer"));
    }

    fs::write(f.worktree.join("lines"), "text\n".repeat(450))?;
    check_integer_argument(
        Tool::Read,
        &scope,
        json!({"path": "lines"}),
        "end_line",
        [1, 16 * 1024 * 1024, 400],
        |result, expected| assert_eq!(result["next_start_line"], expected.min(400) + 1),
    )
    .await?;

    fs::write(f.worktree.join("long"), "x".repeat(MAX_BYTES + 1))?;
    check_integer_argument(
        Tool::Read,
        &scope,
        json!({"path": "long"}),
        "max_bytes",
        [1, 65_536, 65_536],
        |result, expected| {
            let line = result["text"].as_str().unwrap().lines().next().unwrap();
            assert_eq!(line.len(), "1: ".len() + expected);
        },
    )
    .await?;

    let mut scan_capped = vec![b'\n'; READ_SCAN_BYTES - 1];
    scan_capped.extend_from_slice(b"xx\n");
    fs::write(f.worktree.join("scan"), scan_capped)?;
    let last = Tool::Read
        .call(
            scope.clone(),
            &json!({"path": "scan", "start_line": READ_SCAN_BYTES, "max_bytes": 1}),
        )
        .await?;
    assert_eq!(last["truncated"], true);
    assert_eq!(last["next_start_line"], Value::Null);
    assert_eq!(last["total_lines"], Value::Null);
    for selector in [json!({"path": "scan", "tail_lines": 1}), json!({"path": "scan", "start_line":-1})] {
        let error = Tool::Read.call(scope.clone(), &selector).await.unwrap_err();
        assert!(error.to_string().contains("16 MiB scan limit"));
    }

    let error = Tool::Read
        .call(
            scope,
            &json!({"path": "lines", "start_line": 2, "end_line": 0}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("end_line must be at least start_line"));
    Ok(())
}

#[tokio::test]
async fn read_tail_and_end_relative_ranges() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.worktree.join("lines"), "one\r\ntwo\r\nthree\r\nfour")?;
    fs::write(f.worktree.join("empty"), "")?;
    for selector in [json!({"path": "lines", "tail_lines": 2}), json!({"path": "lines", "start_line": -2})] {
        let result = Tool::Read.call(scope.clone(), &selector).await?;
        assert_eq!(result["text"], "3: three\n4: four\n");
        assert_eq!(result["total_lines"], 4);
        assert_eq!(result["truncated"], false);
    }
    let range = Tool::Read.call(scope.clone(), &json!({
        "path": "lines", "start_line": -3, "end_line": 3,
    })).await?;
    assert!(range["text"].as_str().unwrap().starts_with("2: two\n3: three\n"));
    assert_eq!(range["next_start_line"], 4);
    let error = Tool::Read.call(scope.clone(), &json!({
        "path": "lines", "tail_lines": 2, "start_line": 1,
    })).await.unwrap_err();
    assert!(error.to_string().contains("tail_lines on its own"));

    let empty = Tool::Read.call(scope.clone(), &json!({"path": "empty", "tail_lines": 1})).await?;
    assert_eq!(empty["total_lines"], 0);
    assert_eq!(empty["text"], "");

    fs::write(f.worktree.join("long"), "行\n".repeat(450))?;
    let schema = Tool::Read.definition();
    let property = &schema["inputSchema"]["properties"]["tail_lines"];
    assert_eq!(property["minimum"], 1);
    assert_eq!(property["maximum"], 400);
    for (value, count) in [(0, 1), (1, 1), (400, 400), (401, 400), (u64::MAX, 400)] {
        let result = Tool::Read.call(scope.clone(), &json!({"path": "long", "tail_lines": value})).await?;
        assert_eq!(result["total_lines"], 450);
        assert_eq!(result["text"].as_str().unwrap().lines().count(), count);
        assert!(result["text"].as_str().unwrap().starts_with(&format!("{}: 行\n", 451 - count)));
    }
    for value in range::tests::wrong_types() {
        let error = Tool::Read
            .call(scope.clone(), &json!({"path": "long", "tail_lines": value}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("expected an integer"));
    }
    let clipped = Tool::Read.call(scope, &json!({"path": "long", "tail_lines": 1, "max_bytes": 3})).await?;
    assert_eq!(clipped["total_lines"], 450);
    assert_eq!(clipped["truncated"], true);
    assert_eq!(clipped["next_start_line"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn list_and_search_clamp_depth_context_and_result_count() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    let nested = f.worktree.join("nested").join("folder/".repeat(63));
    fs::create_dir_all(&nested)?;
    fs::write(nested.join("file"), "")?;
    check_integer_argument(
        Tool::List,
        &scope,
        json!({"path": "nested"}),
        "depth",
        [1, 64, 1],
        |result, expected| assert_eq!(result["entries"].as_array().unwrap().len(), expected),
    )
    .await?;

    let padding = "line\n".repeat(5);
    fs::write(f.worktree.join("single"), format!("{padding}hit\n{padding}"))?;
    check_integer_argument(
        Tool::Search,
        &scope,
        json!({"path": "single", "pattern": "hit"}),
        "context",
        [0, 5, 0],
        |result, expected| {
            let context = result["matches"][0]["context"].as_array().unwrap();
            assert_eq!(context.len(), 2 * expected + 1);
        },
    )
    .await?;

    fs::write(f.worktree.join("many"), "hit\n".repeat(210))?;
    check_integer_argument(
        Tool::Search,
        &scope,
        json!({"path": "many", "pattern": "hit"}),
        "max_results",
        [1, 200, 100],
        |result, expected| {
            assert_eq!(result["matches"].as_array().unwrap().len(), expected);
            assert_eq!(result["truncated"], true);
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn plan_status_clamps_its_list_limit() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir_all(f.worktree.join("roadmap/cards"))?;
    fs::write(f.worktree.join("roadmap/deck.toml"), "")?;
    check_integer_argument(
        Tool::PlanStatus,
        &f.worker()?,
        json!({}),
        "limit",
        [1, 100, 50],
        |result, expected| assert_eq!(result["limit"], expected),
    )
    .await
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
    let sample = f.worktree.join("src/sub/deep/sample.rs");
    let file = fs::File::open(&sample)?;
    let timestamp = 1_600_000_000;
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(timestamp))?;
    let files = Tool::List.call(scope.clone(), &json!({"path": "src/sub/deep"})).await?;
    assert_eq!(files["entries"][0], json!({
        "path": "sample.rs", "type": "file", "size": 3, "mtime_unix_secs": timestamp,
    }));
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
    let deep = Tool::List
        .call(scope.clone(), &json!({"recursive": true, "glob": "*.rs"}))
        .await?;
    assert_eq!(deep["entries"][0]["path"], "src/sub/deep/sample.rs");
    assert_eq!(
        Tool::List.call(scope, &json!({"depth": u64::MAX, "glob": "*.rs"})).await?,
        deep
    );
    Ok(())
}

#[tokio::test]
async fn listing_accepts_and_clamps_both_result_limit_names() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    for i in 0..501 {
        fs::write(f.worktree.join(format!("{i:03}.txt")), "")?;
    }
    for key in ["max_results", "limit"] {
        for (value, expected) in [(json!(-1), 1), (json!(2), 2), (json!(u64::MAX), 500)] {
            let result = Tool::List.call(scope.clone(), &json!({(key): value})).await?;
            assert_eq!(result["entries"].as_array().unwrap().len(), expected);
            assert_eq!(result["truncated"], true);
            assert!(result["note"].as_str().unwrap().contains(TRUNCATED));
        }
        let result = Tool::List.call(scope.clone(), &json!({(key): 1, "glob": "000.txt"})).await?;
        assert_eq!(result["entries"].as_array().unwrap().len(), 1);
        assert_eq!(result["truncated"], false);
        let error = Tool::List.call(scope.clone(), &json!({(key): "1"})).await.unwrap_err();
        assert!(format!("{error:#}").contains(&format!("Invalid {key}")));
    }
    let error = Tool::List.call(scope, &json!({"limit": 1, "max_results": 2})).await.unwrap_err();
    assert!(format!("{error:#}").contains("Use either max_results or limit"));
    Ok(())
}

#[tokio::test]
async fn search_merges_roots_with_independent_row_limits() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    for dir in ["first", "second"] {
        fs::create_dir(f.worktree.join(dir))?;
        fs::write(f.worktree.join(dir).join("a.txt"), "hit\nhit\n")?;
    }
    fs::write(f.worktree.join("third.txt"), "hit\n")?;

    let merged = Tool::Search.call(scope.clone(), &json!({
        "path": "first", "paths": ["second", "third.txt"],
        "pattern": "hit", "max_results": 1, "stats": true,
    })).await?;
    assert_eq!(merged["paths"], json!(["first", "second", "third.txt"]));
    let labels: Vec<_> = merged["matches"].as_array().unwrap().iter()
        .map(|row| row["path"].as_str().unwrap()).collect();
    assert_eq!(labels, ["first/a.txt", "second/a.txt", "third.txt"]);
    assert_eq!(merged["truncated"], true);
    assert!(merged["note"].as_str().unwrap().contains(TRUNCATED));
    assert_eq!(merged["stats"]["files_searched"], 3);

    let files = Tool::Search.call(scope.clone(), &json!({
        "paths": ["second"], "output_mode": "files", "max_results": 1,
    })).await?;
    assert_eq!(files["files"], json!(["second/a.txt"]));
    assert_eq!(files["truncated"], false);

    let error = Tool::Search.call(scope.clone(), &json!({
        "paths": vec!["first"; 17], "output_mode": "files",
    })).await.unwrap_err();
    assert!(format!("{error:#}").contains("paths accepts at most 16"));

    let error = Tool::Search.call(scope, &json!({
        "path": "first", "paths": ["../"], "output_mode": "files", "max_results": 1,
    })).await.unwrap_err();
    assert!(format!("{error:#}").contains("Invalid path ../"));
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

#[tokio::test]
async fn search_output_modes_limit_files_and_count_matching_lines() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.worktree.join("a.txt"), "hit HIT\r\nHIT\r\nmiss")?;
    fs::write(f.worktree.join("b.txt"), "miss\nhit")?;
    fs::write(f.worktree.join("c.txt"), "miss\n")?;
    fs::write(f.worktree.join("outside.md"), "hit\n")?;

    for (mode, key, expected) in [
        ("files_with_matches", "files", json!(["a.txt", "b.txt"])),
        ("count", "counts", json!([{"path": "a.txt", "count": 2}, {"path": "b.txt", "count": 1}])),
    ] {
        let args = json!({
            "pattern": "hit", "glob": "*.txt", "fixed_string": true, "ignore_case": true,
            "output_mode": mode, "context": 2, "max_results": 2,
        });
        let result = Tool::Search.call(scope.clone(), &args).await?;
        assert_eq!(result[key], expected);
        assert!(result.get("matches").is_none());
        assert_eq!(result["truncated"], false);

        let mut limited = args;
        limited["max_results"] = json!(0);
        let result = Tool::Search.call(scope.clone(), &limited).await?;
        assert_eq!(result[key], json!([expected[0]]));
        assert_eq!(result["truncated"], true);
    }

    let empty = Tool::Search.call(scope.clone(), &json!({"pattern": "^$", "output_mode": "count"})).await?;
    assert_eq!(empty["counts"], json!([]));
    assert_eq!(empty["truncated"], false);
    assert!(Tool::Search.call(scope, &json!({"pattern": "hit", "output_mode": "unknown"})).await.is_err());
    Ok(())
}

#[tokio::test]
async fn search_types_globs_and_files_without_content() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::create_dir_all(f.worktree.join("fixtures/deep"))?;
    for name in [
        "Cargo.lock", "a.rs", "notes.mdx", "app.tsx", "a.pyi", "build.gradle.kts", "z.toml",
        "fixtures/deep/no.rs", "fixtures/keep.rs", "binary.dat",
    ] {
        fs::write(f.worktree.join(name), b"hit\0")?;
    }
    let files = Tool::Search.call(scope.clone(), &json!({
        "output_mode": "files",
        "types": ["rust", "ts", "md", "py", "toml", "kt"],
        "exclude_types": ["py", "toml"],
        "glob": "*",
        "globs": ["!fixtures/", "fixtures/keep.rs", "!notes.*"],
        "stats": true,
    })).await?;
    assert_eq!(files["files"], json!(["a.rs", "app.tsx", "build.gradle.kts", "fixtures/keep.rs"]));
    assert_eq!(files["stats"]["files_searched"], 0);
    assert_eq!(files["stats"]["bytes_scanned"], 0);
    assert_eq!(files["truncated"], false);
    let files = Tool::Search.call(scope.clone(), &json!({
        "output_mode": "files", "types": ["toml"], "globs": ["!z.*"]
    })).await?;
    assert_eq!(files["files"], json!(["Cargo.lock"]));
    let direct = Tool::Search.call(scope, &json!({
        "output_mode": "files", "path": "binary.dat"
    })).await?;
    assert_eq!(direct["files"], json!(["binary.dat"]));
    Ok(())
}

#[tokio::test]
async fn search_hidden_and_ignore_toggles_keep_the_read_scope() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    for dir in [".hidden", ".git", "target", "node_modules", "build", "src", "ignored-dir"] {
        fs::create_dir_all(f.worktree.join(dir))?;
        fs::write(f.worktree.join(dir).join("file.txt"), "inside\n")?;
    }
    for name in ["a.txt", "ignored.txt", "ignored-too.txt", "rescued.txt", "#name.txt", "name with space.txt"] {
        fs::write(f.worktree.join(name), "inside\n")?;
    }
    fs::write(f.worktree.join(".env"), "protected\n")?;
    fs::write(f.worktree.join(".gitignore"), "ignored*.txt\nignored-dir/\n/src/file.txt\nrescued.txt\n\\#name.txt\nname with space.txt  \n")?;
    fs::write(f.worktree.join(".ignore"), "!rescued.txt\n")?;
    fs::write(f.worktree.join("src/.gitignore"), "!file.txt\n")?;
    symlink(&f.worktree, f.worktree.join("src/loop"))?;
    symlink(&f.outside, f.worktree.join("escape"))?;
    symlink(&f.outside, f.worktree.join("target/escape"))?;
    fs::write(f.outside.join("outside.txt"), "outside\n")?;

    let base = json!({"output_mode": "files", "glob": "*.txt"});
    let normal = Tool::Search.call(scope.clone(), &base).await?;
    assert_eq!(normal["files"], json!(["a.txt", "rescued.txt", "src/file.txt"]));
    let mut hidden = base.clone();
    hidden["include_hidden"] = json!(true);
    assert_eq!(
        Tool::Search.call(scope.clone(), &hidden).await?["files"],
        json!([".hidden/file.txt", "a.txt", "rescued.txt", "src/file.txt"])
    );
    let mut ignored = base;
    ignored["include_ignored"] = json!(true);
    let listed = Tool::Search.call(scope.clone(), &ignored).await?;
    assert_eq!(listed["files"], json!([
        "#name.txt", "a.txt", "ignored-dir/file.txt", "ignored-too.txt", "ignored.txt",
        "name with space.txt", "rescued.txt", "src/file.txt"
    ]));
    assert_eq!(listed["truncated"], false);

    let inherited = Tool::Search.call(scope.clone(), &json!({"path": "src", "output_mode": "files", "glob": "*.txt"})).await?;
    assert_eq!(inherited["files"], json!(["file.txt"]));
    fs::remove_file(f.worktree.join("src/.gitignore"))?;
    let inherited = Tool::Search.call(scope.clone(), &json!({"path": "src", "output_mode": "files", "glob": "*.txt"})).await?;
    assert_eq!(inherited["files"], json!([]));

    let explicit = Tool::Search.call(scope.clone(), &json!({
        "path": "target", "pattern": "inside", "output_mode": "files_with_matches",
        "include_hidden": true, "include_ignored": true,
    })).await?;
    assert_eq!(explicit["files"], json!(["file.txt"]));
    for path in [f.outside.join("outside.txt"), "escape".into(), "../outside".into(), ".git".into(), ".env".into()] {
        let error = Tool::Search.call(scope.clone(), &json!({
            "path": path, "output_mode": "files", "include_hidden": true, "include_ignored": true
        })).await.unwrap_err();
        assert!(format!("{error:#}").contains("path"));
    }
    Ok(())
}

#[tokio::test]
async fn search_grep_modes_and_columns_use_the_same_matcher() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.worktree.join("a.txt"), "below\né hit HIT hitting\n-HIT\nmiss\n")?;
    fs::write(f.worktree.join("b.txt"), "miss\n")?;
    fs::write(f.worktree.join("empty.txt"), "")?;

    let rows = Tool::Search.call(scope.clone(), &json!({
        "patterns": ["hit", "-hit"], "fixed_string": true, "smart_case": true,
        "word": true, "only_matching": true, "column": true,
        "line_number": false, "with_path": false,
    })).await?;
    assert_eq!(rows["matches"], json!([
        {"text": "hit", "column": 4, "truncated": false},
        {"text": "HIT", "column": 8, "truncated": false},
        {"text": "-HIT", "column": 1, "truncated": false}
    ]));

    let exact = Tool::Search.call(scope.clone(), &json!({
        "path": "a.txt", "pattern": "-HIT", "whole_line": true, "smart_case": true,
        "context": 1, "before_context": 0, "after_context": 2,
    })).await?;
    assert_eq!(exact["matches"][0]["line"], 3);
    assert_eq!(exact["matches"][0]["context"], json!([
        {"line": 3, "text": "-HIT", "truncated": false},
        {"line": 4, "text": "miss", "truncated": false}
    ]));
    assert_eq!(exact["matches"].as_array().unwrap().len(), 1);
    let sensitive = Tool::Search.call(scope.clone(), &json!({
        "pattern": "-Hit", "whole_line": true, "smart_case": true
    })).await?;
    assert_eq!(sensitive["matches"], json!([]));

    let absent = Tool::Search.call(scope.clone(), &json!({
        "pattern": "hit", "ignore_case": true, "output_mode": "files_without_matches",
    })).await?;
    assert_eq!(absent["files"], json!(["b.txt", "empty.txt"]));
    let inverted = Tool::Search.call(scope.clone(), &json!({
        "pattern": "HIT", "path": "a.txt", "invert": true, "column": true, "max_count": 1
    })).await?;
    assert_eq!(inverted["matches"][0]["line"], 1);
    assert_eq!(inverted["matches"][0]["column"], Value::Null);
    assert_eq!(inverted["matches"].as_array().unwrap().len(), 1);
    assert_eq!(inverted["truncated"], true);
    let count = Tool::Search.call(scope, &json!({
        "pattern": "hit", "ignore_case": true, "output_mode": "count", "max_count": 0, "with_path": false
    })).await?;
    assert_eq!(count["counts"], json!([{"count": 1}]));
    assert_eq!(count["truncated"], true);
    Ok(())
}

#[tokio::test]
async fn search_multiline_stays_within_files_and_reports_ranges() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    fs::write(f.worktree.join("a"), "foo\n")?;
    fs::write(f.worktree.join("b"), "bar\n")?;
    fs::write(f.worktree.join("c"), "before\r\né foo\r\nbar\r\nafter\nfoo")?;
    let search = json!({
        "pattern": "foo\\r?\\nbar", "multiline": true, "column": true, "before_context": 1, "after_context": 1
    });
    let result = Tool::Search.call(scope.clone(), &search).await?;
    let hits = result["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["path"], "c");
    assert_eq!(hits[0]["line"], 2);
    assert_eq!(hits[0]["end_line"], 3);
    assert_eq!(hits[0]["column"], 4);
    assert_eq!(hits[0]["context"][3]["text"], "after");
    let mut only = search.clone();
    only["only_matching"] = json!(true);
    assert_eq!(Tool::Search.call(scope.clone(), &only).await?["matches"][0]["text"], "foo\r\nbar");
    let mut inverted = search;
    inverted["path"] = json!("c");
    inverted["invert"] = json!(true);
    let inverse = Tool::Search.call(scope, &inverted).await?;
    let lines: Vec<_> = inverse["matches"].as_array().unwrap().iter().map(|row| &row["line"]).collect();
    assert_eq!(lines, [&json!(1), &json!(4), &json!(5)]);
    Ok(())
}

#[tokio::test]
async fn search_sort_and_stats_cover_scanned_files_beyond_the_response_cap() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    let same_second = std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    for (path, text, millis) in [("a", "hit\n", 200), ("b", "hit\nhit\n", 100), ("c", "miss\n", 500)] {
        let path = f.worktree.join(path);
        fs::write(&path, text)?;
        fs::File::open(path)?.set_modified(same_second + Duration::from_millis(millis))?;
    }
    let result = Tool::Search.call(scope, &json!({
        "pattern": "hit", "output_mode": "count", "sort": "modified", "max_results": 1, "stats": true
    })).await?;
    assert_eq!(result["counts"], json!([{"path": "b", "count": 2}]));
    assert_eq!(result["truncated"], true);
    assert_eq!(result["stats"]["files_searched"], 3);
    assert_eq!(result["stats"]["files_matched"], 2);
    assert_eq!(result["stats"]["bytes_scanned"], 17);
    assert!(result["stats"]["elapsed_ms"].is_u64());
    Ok(())
}

#[tokio::test]
async fn search_marks_scan_byte_depth_and_ignore_limits() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    let megabyte = vec![b'x'; 1024 * 1024];
    for index in 0..33 {
        fs::write(f.worktree.join(format!("file{index:02}")), &megabyte)?;
    }
    let result = Tool::Search.call(scope.clone(), &json!({"pattern": "missing", "stats": true})).await?;
    assert_eq!(result["matches"], json!([]));
    assert_eq!(result["stats"]["files_searched"], 32);
    assert_eq!(result["stats"]["bytes_scanned"], 32 * 1024 * 1024);
    assert_eq!(result["truncated"], true);

    let deep = "nest/".repeat(65);
    fs::create_dir_all(f.worktree.join(&deep))?;
    fs::write(f.worktree.join(deep).join("file.rs"), "hit")?;
    fs::write(f.worktree.join("z.rs"), "hit")?;
    let result = Tool::Search.call(scope.clone(), &json!({"output_mode": "files", "glob": "*.rs"})).await?;
    assert_eq!(result["files"], json!(["z.rs"]));
    assert_eq!(result["truncated"], true);

    fs::write(f.worktree.join(".gitignore"), &megabyte)?;
    let result = Tool::Search.call(scope, &json!({"output_mode": "files"})).await?;
    assert_eq!(result["files"], json!([]));
    assert_eq!(result["truncated"], true);
    Ok(())
}

#[tokio::test]
async fn search_argument_errors_name_the_parameter_and_suggest_supported_syntax() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.worker()?;
    for (args, parameter, hint) in [
        (json!({"pattern": "hit", "before_context": "2"}), "before_context", "expected an integer"),
        (json!({"pattern": "hit", "after_context": 2.5}), "after_context", "expected an integer"),
        (json!({"pattern": "hit", "max_count": null}), "max_count", "expected an integer"),
        (json!({"pattern": "hit", "stats": "yes"}), "stats", "boolean"),
        (json!({"pattern": null}), "pattern", "string"),
        (json!({"pattern": "hit", "sort": "size"}), "sort", "modified"),
        (json!({"pattern": "hit", "types": [2]}), "types", "string"),
        (json!({"pattern": "hit", "exclude_types": ["made-up"]}), "exclude_types[0]", "glob"),
        (json!({"pattern": "hit", "globs": ["*.rs", "["]}), "globs[1]", "Invalid"),
        (json!({"pattern": "hit", "-n": true}), "-n", "named arguments"),
        (json!({"pattern": "look(?=ahead)"}), "pattern", "lookaround"),
        (json!({"patterns": ["ok", "look(?<=behind)"]}), "patterns[1]", "context"),
        (json!({"patterns": ["a\nb"]}), "patterns[0]", "multiline=true"),
        (json!({"pattern": "hit", "output_mode": "files"}), "pattern", "Omit"),
        (json!({}), "pattern", "output_mode=files"),
    ] {
        let error = Tool::Search.call(scope.clone(), &args).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains(parameter) && error.contains(hint), "{error}");
    }
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
    assert_eq!(mcp::worker_tool_names(&["jq".into()]), ["jq", "escalate"]);
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
    assert!(is_review_launch(&lease.launch));
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
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    assert!(tools.iter().any(|tool| tool["name"] == "jq"));
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
