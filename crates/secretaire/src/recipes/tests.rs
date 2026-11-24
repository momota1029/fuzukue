use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

use tempfile::TempDir;

use super::*;

const RECIPES: &str = r#"
[recipes.echo]
command = ["/usr/bin/printf", "%s", "{filter}"]
cwd = "."
queue = "test"
args = { filter = { kind = "test-name", required = true } }
timeout_secs = 5
max_memory_mib = 64

[recipes.sleep]
command = ["/usr/bin/sleep", "30"]
cwd = "."
queue = "test"
timeout_secs = 1
max_memory_mib = 64

[recipes.verify-limits]
command = ["./verify-limits"]
cwd = "."
queue = "test"
env = { RECIPE_TEST_VALUE = "configured" }
timeout_secs = 5
max_memory_mib = 64
"#;

struct Fixture {
    _dir: TempDir,
    workspace: PathBuf,
    journal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let journal = dir.path().join("state/recipe-audit.jsonl");
        Self {
            _dir: dir,
            workspace,
            journal,
        }
    }

    fn open(&self, allow_unisolated: bool) -> Arc<Broker> {
        self.open_with(allow_unisolated, None)
    }

    fn open_with(&self, allow_unisolated: bool, bwrap: Option<PathBuf>) -> Arc<Broker> {
        let config = Config::parse(&format!(
            "[recipe_runner]\nallow_unisolated = {allow_unisolated}\n{RECIPES}"
        ))
        .unwrap();
        Broker::open(
            config,
            &self.journal,
            vec![self.journal.parent().unwrap().into()],
            Paths {
                bwrap,
                host_home: None,
            },
        )
        .unwrap()
    }

    fn submit(&self, broker: &Arc<Broker>, name: &str, args: &[(&str, &str)]) -> Result<Request> {
        broker.submit(
            caller(),
            RunArgs {
                name: name.into(),
                args: args
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            },
            self.workspace.clone(),
        )
    }
}

fn caller() -> Caller {
    Caller::new("workspace", "thread", Some("worker"))
}

async fn finish(mut request: Request) -> Record {
    request.wait(Duration::from_secs(5)).await;
    let record = request.record.borrow().clone();
    assert!(!record.state.active(), "recipe remained {:?}", record.state);
    record
}

async fn running(request: &mut Request) {
    tokio::time::timeout(
        Duration::from_secs(5),
        request
            .record
            .wait_for(|record| record.state == State::Running && record.log_path.is_some()),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn allowed_run_returns_argv_output_exit_and_audit() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let record = finish(
        fixture
            .submit(&broker, "echo", &[("filter", "test::show-1/path.rs")])
            .unwrap(),
    )
    .await;
    assert_eq!(record.state, State::Exited, "{:?}", record.reason);
    assert_eq!(record.exit_code, Some(0));
    assert!(record.duration_ms.is_some());
    assert_eq!(record.log_tail, "test::show-1/path.rs");
    let log = record.log_path.unwrap();
    assert!(log.starts_with(fixture.workspace.join("target/recipe-logs")));
    assert_eq!(fs::read_to_string(log).unwrap(), record.log_tail);
    let audit = fs::read_to_string(&fixture.journal).unwrap();
    assert!(audit.contains(r#""thread":"thread","job":"worker""#));
    assert!(audit.contains(r#""exit_code":0"#));
}

#[tokio::test]
async fn undeclared_invalid_and_missing_args_denied() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    for (args, reason) in [
        (vec![("other", "valid")], "undeclared argument"),
        (vec![("filter", "a; echo bad")], "invalid value"),
        (vec![("filter", "--release")], "invalid value"),
        (vec![], "missing required argument"),
    ] {
        assert!(fixture
            .submit(&broker, "echo", &args)
            .err()
            .unwrap()
            .to_string()
            .contains(reason));
    }
    assert_eq!(broker.list(&caller())["requests"], json!([]));
}

#[tokio::test]
async fn unknown_recipes_and_worker_supplied_scope_denied() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    assert!(fixture
        .submit(&broker, "unknown", &[])
        .err()
        .unwrap()
        .to_string()
        .contains("unknown recipe"));
    assert!(fixture.submit(&broker, "../echo", &[]).is_err());
    assert!(
        serde_json::from_value::<RunArgs>(json!({"name": "echo", "workspace": "other"})).is_err()
    );
}

#[tokio::test]
async fn duplicate_request_is_deduplicated_only_for_same_caller() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let first = fixture.submit(&broker, "sleep", &[]).unwrap();
    let duplicate = fixture.submit(&broker, "sleep", &[]).unwrap();
    let other = broker
        .submit(
            Caller::new("workspace", "thread", Some("other")),
            RunArgs {
                name: "sleep".into(),
                args: BTreeMap::new(),
            },
            fixture.workspace.clone(),
        )
        .unwrap();

    assert!(duplicate.deduplicated);
    assert_eq!(
        first.snapshot()["request_id"],
        duplicate.snapshot()["request_id"]
    );
    assert_ne!(
        first.snapshot()["request_id"],
        other.snapshot()["request_id"]
    );
    assert!(!other.deduplicated);
    for request in [&first, &other] {
        let record = request.record.borrow().clone();
        broker.cancel(&record.caller, &record.request_id).unwrap();
    }
    assert_eq!(finish(first).await.state, State::Cancelled);
    assert_eq!(finish(other).await.state, State::Cancelled);
}

#[tokio::test]
async fn cross_workspace_thread_and_job_access_denied() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let owned = fixture.submit(&broker, "sleep", &[]).unwrap();
    let id = owned.record.borrow().request_id.clone();
    for caller in [
        Caller::new("other", "thread", Some("worker")),
        Caller::new("workspace", "other", Some("worker")),
        Caller::new("workspace", "thread", Some("other")),
        Caller::new("other", "thread", None),
    ] {
        assert!(broker.find(&caller, &id).is_err());
        assert!(broker.cancel(&caller, &id).is_err());
        assert_eq!(broker.list(&caller)["requests"], json!([]));
    }
    let desk = Caller::new("workspace", "thread", None);
    assert!(broker.find(&desk, &id).is_ok());
    broker.cancel(&desk, &id).unwrap();
    assert_eq!(finish(owned).await.state, State::Cancelled);
}

#[tokio::test]
async fn same_queue_runs_fifo_and_serializes_different_recipes() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let mut slow = fixture.submit(&broker, "sleep", &[]).unwrap();
    running(&mut slow).await;
    let first = fixture
        .submit(&broker, "echo", &[("filter", "first")])
        .unwrap();
    let dropped = fixture
        .submit(&broker, "echo", &[("filter", "cancelled")])
        .unwrap();
    let second = fixture
        .submit(&broker, "echo", &[("filter", "second")])
        .unwrap();
    let listed = broker.list(&caller());
    assert_eq!(listed["recipes"][0]["running"], true);
    assert_eq!(listed["recipes"][0]["queued"], 3);
    assert_eq!(first.record.borrow().state, State::Queued);
    assert_eq!(second.record.borrow().state, State::Queued);

    let cancelled = dropped.record.borrow().request_id.clone();
    broker.cancel(&caller(), &cancelled).unwrap();
    assert_eq!(finish(dropped).await.state, State::Cancelled);
    for row in broker.list(&caller())["recipes"].as_array().unwrap() {
        assert_eq!(row["queued"], 2);
    }
    let slow = finish(slow).await;
    let first = finish(first).await;
    let second = finish(second).await;
    assert_eq!(first.log_tail, "first");
    assert_eq!(second.log_tail, "second");
    assert!(slow.ended_at <= first.started_at);
    assert!(first.ended_at <= second.started_at);
}

#[tokio::test]
async fn timeout_kills_the_child() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let record = finish(fixture.submit(&broker, "sleep", &[]).unwrap()).await;
    assert_eq!(record.state, State::TimedOut, "{:?}", record.reason);
    assert_eq!(record.signal, Some(libc::SIGKILL));
    assert!(record.duration_ms.unwrap() >= 1000);
}

#[tokio::test]
async fn running_request_can_be_cancelled() {
    let fixture = Fixture::new();
    let broker = fixture.open(true);
    let mut request = fixture.submit(&broker, "sleep", &[]).unwrap();
    running(&mut request).await;
    let id = request.record.borrow().request_id.clone();
    broker.cancel(&caller(), &id).unwrap();
    assert_eq!(finish(request).await.state, State::Cancelled);
}

#[tokio::test]
async fn missing_isolation_refuses_and_does_not_execute() {
    let fixture = Fixture::new();
    let broker = fixture.open(false);
    let record = finish(
        fixture
            .submit(&broker, "echo", &[("filter", "never-ran")])
            .unwrap(),
    )
    .await;
    assert_eq!(record.state, State::Refused);
    assert!(record.reason.unwrap().contains("no bubblewrap"));
    assert_eq!(record.exit_code, None);
    assert!(fs::read(record.log_path.unwrap()).unwrap().is_empty());
}

#[tokio::test]
async fn broken_isolation_does_not_fall_back_even_when_opted_in() {
    let fixture = Fixture::new();
    let broker = fixture.open_with(true, Some("/usr/bin/false".into()));
    let record = finish(
        fixture
            .submit(&broker, "echo", &[("filter", "never-ran")])
            .unwrap(),
    )
    .await;
    assert_eq!(record.state, State::Refused);
    assert!(record.reason.unwrap().contains("bubblewrap"));
    assert!(record.log_tail.is_empty());
}

#[tokio::test]
async fn restart_marks_queued_and_running_interrupted() {
    let fixture = Fixture::new();
    let (mut journal, _) = Journal::open(&fixture.journal).unwrap();
    for (id, state) in [
        ("old-queued", State::Queued),
        ("old-running", State::Running),
    ] {
        let record = Record {
            request_id: id.into(),
            caller: caller(),
            recipe: "echo".into(),
            args: BTreeMap::from([("filter".into(), "must-not-run".into())]),
            queue: "test".into(),
            queued_at: Utc::now().timestamp_millis(),
            started_at: (state == State::Running).then(|| Utc::now().timestamp_millis()),
            ended_at: None,
            state,
            exit_code: None,
            signal: None,
            duration_ms: None,
            reason: None,
            log_path: None,
            log_tail: String::new(),
        };
        journal.append(&record).unwrap();
    }
    drop(journal);
    for _ in 0..2 {
        let broker = fixture.open(true);
        assert_eq!(
            broker.list(&caller())["requests"].as_array().unwrap().len(),
            2
        );
        for id in ["old-queued", "old-running"] {
            let record = finish(broker.find(&caller(), id).unwrap()).await;
            assert_eq!(record.state, State::Interrupted);
            assert!(record.reason.unwrap().contains("restarted"));
            assert!(record.ended_at.is_some());
        }
        assert!(!fixture.workspace.join("target").exists());
    }
}

#[tokio::test]
async fn child_environment_is_scrubbed_and_memory_limit_is_inherited() {
    let fixture = Fixture::new();
    let script = fixture.workspace.join("verify-limits");
    fs::write(
        &script,
        r#"#!/bin/sh
test -n "$HOME" && test -d "$HOME" && test ! -e "$HOME/.config" || exit 2
test -z "$USER" && test -z "$FUZUKUE_SELF_UPDATE_READY" || exit 3
test "$RECIPE_TEST_VALUE" = configured || exit 4
exec /usr/bin/cat /proc/self/limits
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let broker = fixture.open(true);
    let record = finish(fixture.submit(&broker, "verify-limits", &[]).unwrap()).await;
    assert_eq!(record.exit_code, Some(0), "{:?}", record.reason);
    let row = record
        .log_tail
        .lines()
        .find(|line| line.starts_with("Max address space"))
        .unwrap();
    assert_eq!(
        row.split_whitespace().skip(3).take(2).collect::<Vec<_>>(),
        ["67108864", "67108864"]
    );
}

#[tokio::test]
async fn log_directory_symlink_is_refused() {
    let fixture = Fixture::new();
    let outside = fixture.workspace.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, fixture.workspace.join("target")).unwrap();
    let broker = fixture.open(true);
    let record = finish(
        fixture
            .submit(&broker, "echo", &[("filter", "never-ran")])
            .unwrap(),
    )
    .await;
    assert_eq!(record.state, State::Refused);
    assert!(record.reason.unwrap().contains("symlinks are forbidden"));
    assert!(fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn recipe_mcp_tools_follow_worker_allowlist_and_codex_policy() {
    let worker = crate::mcp::worker_tool_names(&[]);
    let desk = crate::mcp::available_tool_names(false);
    let claude = crate::mcp::claude_tool_names(false, true).collect::<Vec<_>>();
    for tool in Tool::ALL {
        assert!(worker.iter().any(|name| name == tool.name()));
        let claude_name = format!("mcp__fuzukue__{}", tool.name());
        assert!(desk.contains(&claude_name));
        assert!(claude.contains(&claude_name));
        assert_eq!(
            codex_tool_policy(Some(&worker))[tool.name()]["approval_mode"],
            "approve"
        );
    }
    let restricted = crate::mcp::worker_tool_names(&["list_recipes".into()]);
    assert_eq!(
        codex_tool_policy(Some(&restricted)),
        json!({"list_recipes": {"approval_mode": "approve"}})
    );
    assert!(codex_tool_policy(None).get("run_tests").is_none());
}

#[tokio::test]
async fn reading_companion_recipes_load_and_accept_the_documented_arguments() -> Result<()> {
    const EXAMPLE: &str = include_str!("../../../fuzukue-reading/examples/recipes.toml");
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "check-outline",
            &[("spec", "synthetic/companion/companion.json")],
        ),
        (
            "build-companion",
            &[
                ("spec", "synthetic/companion/companion.json"),
                ("html", "synthetic/build/companion.html"),
            ],
        ),
        (
            "check-companion",
            &[
                ("spec", "synthetic/companion/companion.json"),
                ("html", "synthetic/build/companion.html"),
            ],
        ),
        (
            "check-section-record",
            &[("record", "synthetic/section.json")],
        ),
        (
            "build-section-page",
            &[
                ("record", "synthetic/section.json"),
                ("out", "synthetic/build/sections"),
            ],
        ),
        (
            "build-section-page-with-original",
            &[
                ("record", "synthetic/section.json"),
                ("out", "synthetic/build/cropped"),
                ("pdf", "synthetic/note/main.pdf"),
            ],
        ),
        ("archive-pair", &[("pair", "synthetic/pair.json")]),
        (
            "proof-book-refs",
            &[("project", "proof-book-example"), ("block", "P")],
        ),
        ("proof-book-build", &[("project", "proof-book-example")]),
        ("proof-book-check", &[("project", "proof-book-example")]),
    ];
    let config = Config::parse(EXAMPLE)?;
    assert_eq!(config.recipes.len(), cases.len());
    for (name, args) in cases {
        let arguments = args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let argv = config.recipes[*name].argv(&arguments)?;
        assert_eq!(argv[0], "/usr/local/bin/reading-companion");
    }

    // Opt in to running the example against a locally built executable and TeX installation.
    let Some(binary) = std::env::var_os("FUZUKUE_TEST_READING_COMPANION") else {
        return Ok(());
    };
    let binary = PathBuf::from(binary).canonicalize()?;
    let mut source = EXAMPLE.replace(
        "\"/usr/local/bin/reading-companion\"",
        &toml::Value::String(binary.to_str().context("non-UTF-8 executable path")?.into())
            .to_string(),
    );
    for tool in ["proof-book-build", "proof-book-check"] {
        let marker = format!("[recipes.{tool}]");
        let env: toml::value::Table = [
            "TEXMFDBS",
            "TEXINPUTS",
            "LUAINPUTS",
            "TFMFONTS",
            "T1FONTS",
            "ENCFONTS",
            "OPENTYPEFONTS",
            "TEXFONTMAPS",
            "MKTEXPK",
            "TEXFORMATS",
            "KATEX_DIR",
        ]
        .into_iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.into(), value.into()))
        })
        .collect();
        if !env.is_empty() {
            let env = toml::Value::Table(env).to_string();
            source = source.replace(&marker, &format!("{marker}\nenv = {env}"));
        }
    }
    let fixture = Fixture::new();
    let inputs =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/reading-companion");
    for entry in walkdir::WalkDir::new(&inputs).min_depth(1) {
        let entry = entry?;
        let target = fixture.workspace.join(entry.path().strip_prefix(&inputs)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    fs::write(
        fixture.workspace.join(".gitignore"),
        "/synthetic/build/\n/writing/.corpus/\n",
    )?;
    for path in [".", "writing/.corpus"] {
        let status = std::process::Command::new("git")
            .args(["init", "-q", path])
            .current_dir(&fixture.workspace)
            .status()?;
        assert!(status.success());
    }

    // The fixture uses only local inputs and verifies the same broker, queue, argv and timeout
    // path in environments without bubblewrap or a loopback listener.
    let config = Config::parse(&format!(
        "[recipe_runner]\nallow_unisolated = true\n{source}"
    ))?;
    let broker = Broker::open(
        config,
        &fixture.journal,
        vec![fixture.journal.parent().unwrap().into()],
        Paths {
            bwrap: None,
            host_home: None,
        },
    )?;
    assert_eq!(
        broker.list(&caller())["recipes"].as_array().unwrap().len(),
        cases.len()
    );
    for (name, args) in cases {
        let mut request = fixture.submit(&broker, name, args)?;
        tokio::time::timeout(Duration::from_secs(60), async {
            while request.record.borrow().state.active() {
                request.record.changed().await.unwrap();
            }
        })
        .await?;
        let report = request.record.borrow();
        assert_eq!(report.state, State::Exited, "{name}: {:?}", report.reason);
        assert_eq!(report.exit_code, Some(0), "{name}: {}", report.log_tail);
    }
    Ok(())
}
