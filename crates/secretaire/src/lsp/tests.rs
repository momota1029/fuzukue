use super::*;
use std::fs;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

const FIXTURE: &str = include_str!("../../../../tests/shots/fixtures/lsp/fake-lsp.py");
const MAIN: &str = include_str!("../../../../tests/shots/fixtures/lsp/main.rs");

fn fixture(idle: Duration) -> (TempDir, Manager) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fuzukue.toml"), "[lsp]\nrust = 'python3 ./fake-lsp.py trace.jsonl'\nlean = 'python3 ./fake-lsp.py trace.jsonl'").unwrap();
    fs::write(dir.path().join("fake-lsp.py"), FIXTURE).unwrap();
    fs::write(dir.path().join("main.rs"), MAIN).unwrap();
    fs::write(dir.path().join("helper.rs"), "fn helper() {}\n").unwrap();
    fs::write(
        dir.path().join("Main.lean"),
        "theorem demo : True := by sorry\n",
    )
    .unwrap();
    let mut manager = Manager::new(dir.path());
    manager.idle = idle;
    (dir, manager)
}

type Incoming = mpsc::UnboundedReceiver<ServerEvent>;

async fn until<T>(events: &mut Incoming, mut map: impl FnMut(ServerEvent) -> Option<T>) -> T {
    timeout(Duration::from_secs(6), async {
        loop {
            if let Some(value) = map(events.recv().await.expect("connection closed")) {
                return value;
            }
        }
    })
    .await
    .expect("event did not arrive")
}

async fn status(events: &mut Incoming, wanted: LspStatus) {
    until(events, |event| match event {
        ServerEvent::LspDocument {
            update: LspUpdate::Status { status },
            ..
        } if status == wanted => Some(()),
        _ => None,
    })
    .await
}

async fn diagnostic(events: &mut Incoming, version: u64) -> Value {
    until(events, |event| match event {
        ServerEvent::LspDocument {
            update:
                LspUpdate::Diagnostics {
                    version: received,
                    diagnostics,
                },
            ..
        } if received == version && !diagnostics.is_empty() => diagnostics.into_iter().next(),
        _ => None,
    })
    .await
}

async fn response(events: &mut Incoming, id: u64) -> LspOutcome {
    until(events, |event| match event {
        ServerEvent::LspResult { request, outcome } if request == id => Some(outcome),
        _ => None,
    })
    .await
}

fn open(connection: &mut Connection<'_>) {
    connection.update(
        1,
        LspBuffer::Open {
            path: "main.rs".into(),
            text: MAIN.into(),
            version: 1,
        },
    );
}

fn hover(character: u32) -> LspQuery {
    LspQuery::Hover {
        position: LspPosition { line: 1, character },
    }
}

async fn trace(dir: &TempDir, method: &str) -> Vec<Value> {
    let path = dir.path().join("trace.jsonl");
    timeout(Duration::from_secs(4), async {
        loop {
            let log = fs::read_to_string(&path).unwrap_or_default();
            let items: Vec<Value> = log
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            if items.iter().any(|item| item["method"] == method) {
                return items;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {method}"))
}

#[tokio::test]
async fn frames_allow_header_fields_and_unicode_and_reject_bad_lengths() {
    let value = json!({"text":"証明 λ"});
    let mut bytes = Vec::new();
    rpc::write(&mut bytes, &value).await.unwrap();
    rpc::write(&mut bytes, &Value::Null).await.unwrap();
    let mut input = bytes.as_slice();
    assert_eq!(rpc::read(&mut input).await.unwrap(), value);
    assert_eq!(rpc::read(&mut input).await.unwrap(), Value::Null);
    for invalid in [
        &b"Content-Length: 5\r\n\r\n{}"[..],
        &b"Content-Type: utf-8\r\n\r\n{}"[..],
        &b"Content-Length: 999999999\r\n\r\n"[..],
        &b"Content-Length: 2\r\ncontent-length: 2\r\n\r\n{}"[..],
    ] {
        assert!(rpc::read(&mut &*invalid).await.is_err());
    }
}

#[tokio::test]
async fn editor_buffers_fan_out_versioned_diagnostics_and_share_one_process() {
    let (dir, manager) = fixture(Duration::from_secs(30));
    assert!(!dir.path().join("trace.jsonl").exists());
    let (a, mut a_rx) = mpsc::unbounded_channel();
    let (b, mut b_rx) = mpsc::unbounded_channel();
    let mut a = manager.connection(a);
    let mut b = manager.connection(b);
    open(&mut a);
    open(&mut b);
    let (left, right) = tokio::join!(diagnostic(&mut a_rx, 1), diagnostic(&mut b_rx, 1));
    assert_eq!(left, right);
    let changed = format!("{MAIN}// 🦀\n");
    a.update(
        1,
        LspBuffer::Change {
            version: 2,
            text: changed.clone(),
        },
    );
    a.update(
        1,
        LspBuffer::Save {
            version: 2,
            text: changed.clone(),
        },
    );
    assert!(!diagnostic(&mut a_rx, 2).await.to_string().contains("STALE"));
    let tools = Tool::Diagnostics
        .call(&manager, &json!({"path":"main.rs"}))
        .await
        .unwrap();
    assert!(tools[0]["message"]
        .as_str()
        .unwrap()
        .ends_with(&format!("{} chars", changed.chars().count())));
    a.request(7, 1, hover(19));
    assert!(
        matches!(response(&mut a_rx, 7).await, LspOutcome::Value { value } if value["contents"]["value"].as_str().unwrap().contains("Returns"))
    );
    a.update(1, LspBuffer::Close);
    // The remaining tab takes over with its own version and contents.
    assert_eq!(diagnostic(&mut b_rx, 1).await, right);
    drop(b);
    let log = trace(&dir, "textDocument/didClose").await;
    for method in [
        "initialize",
        "initialized",
        "textDocument/didOpen",
        "textDocument/didSave",
        "textDocument/didClose",
    ] {
        assert_eq!(
            log.iter().filter(|item| item["method"] == method).count(),
            1,
            "{method}"
        );
    }
}

#[tokio::test]
async fn routes_editor_and_mcp_requests_and_scopes_locations_and_input() {
    let (dir, manager) = fixture(Duration::from_secs(30));
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("escape.rs")).unwrap();
    for path in [
        "../outside.rs",
        "/etc/passwd",
        "escape.rs",
        "fake/../../main.rs",
    ] {
        assert!(Tool::Hover
            .call(&manager, &json!({"path":path, "line":1,"col":1}))
            .await
            .is_err());
    }
    let pos = json!({"path":"main.rs", "line":2,"col":20});
    assert!(Tool::Hover
        .call(&manager, &json!({"path":"main.rs", "line":0,"col":1}))
        .await
        .is_err());
    assert!(Tool::Hover.call(&manager, &pos).await.unwrap()["contents"].is_object());
    for tool in [Tool::Definition, Tool::References] {
        let result = tool.call(&manager, &pos).await.unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["uri"], "helper.rs");
    }
    let found = Tool::Definition
        .call(&manager, &json!({"symbol":"helper","language":"rust"}))
        .await
        .unwrap();
    assert_eq!(found[0]["uri"], "helper.rs");
    let symbols = Tool::Symbols
        .call(&manager, &json!({"path":"main.rs"}))
        .await
        .unwrap();
    assert_eq!(symbols[0]["name"], "helper");
    let called = Tool::CallHierarchy.call(&manager, &pos).await.unwrap();
    assert_eq!(called[0]["item"]["uri"], "helper.rs");
    assert_eq!(called[0]["incoming"].as_array().unwrap().len(), 1);
    assert_eq!(called[0]["outgoing"][0]["to"]["uri"], "helper.rs");
    let goal = Tool::LeanGoal
        .call(&manager, &json!({"path":"Main.lean","line":1,"col":20}))
        .await
        .unwrap();
    assert_eq!(goal["goal"]["goals"][0], "⊢ True");
    assert_eq!(goal["term_goal"]["goal"], "⊢ Nat");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut editor = manager.connection(tx);
    open(&mut editor);
    editor.request(
        1,
        1,
        LspQuery::Completion {
            position: LspPosition {
                line: 1,
                character: 20,
            },
            context: json!({"triggerKind":1}),
        },
    );
    assert!(
        matches!(response(&mut rx, 1).await, LspOutcome::Value { value } if value["items"][0]["label"] == "helper")
    );
}

#[tokio::test]
async fn yulang_routes_editor_and_mcp_requests() {
    let dir = tempfile::tempdir().unwrap();
    let text = "my helper = 42\nmy value = helper\n";
    for (path, contents) in [
        (
            "fuzukue.toml",
            "[lsp]\nyulang = 'python3 ./fake-lsp.py trace.jsonl'",
        ),
        ("main.yu", text),
        ("helper.yu", "my helper = 42\n"),
    ] {
        fs::write(dir.path().join(path), contents).unwrap();
    }
    fs::write(
        dir.path().join("fake-lsp.py"),
        FIXTURE.replace("helper.rs", "helper.yu"),
    )
    .unwrap();

    let manager = Manager::new(dir.path());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut editor = manager.connection(tx);
    editor.update(
        1,
        LspBuffer::Open {
            path: "main.yu".into(),
            version: 1,
            text: text.into(),
        },
    );
    status(&mut rx, LspStatus::Running).await;
    let published = diagnostic(&mut rx, 1).await;
    let path = json!({"path":"main.yu"});
    assert_eq!(
        Tool::Diagnostics.call(&manager, &path).await.unwrap()[0],
        published
    );
    let pos = json!({"path":"main.yu","line":2,"col":12});
    assert!(Tool::Hover.call(&manager, &pos).await.unwrap()["contents"].is_object());
    assert_eq!(
        Tool::Definition.call(&manager, &pos).await.unwrap()[0]["uri"],
        "helper.yu"
    );
    assert_eq!(
        Tool::Symbols.call(&manager, &path).await.unwrap()[0]["name"],
        "helper"
    );
    let workspace = json!({"path":"main.yu","language":"yulang","query":"helper"});
    assert_eq!(
        Tool::Symbols.call(&manager, &workspace).await.unwrap()[0]["location"]["uri"],
        "helper.yu"
    );

    let messages = trace(&dir, "textDocument/didOpen").await;
    let open = messages
        .iter()
        .find(|message| message["method"] == "textDocument/didOpen")
        .unwrap();
    assert_eq!(open["params"]["textDocument"]["languageId"], "yulang");
    assert_eq!(open["params"]["textDocument"]["text"], text);
}

#[tokio::test]
async fn yulang_missing_binary_reports_unavailable_without_starting_an_actor() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("fuzukue.toml"),
        "[lsp]\nyulang = './missing-yulang server'",
    )
    .unwrap();
    let text = "my value = 42\n";
    fs::write(dir.path().join("main.yu"), text).unwrap();
    let manager = Manager::new(dir.path());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut editor = manager.connection(tx);
    editor.update(
        1,
        LspBuffer::Open {
            path: "main.yu".into(),
            version: 1,
            text: text.into(),
        },
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        ServerEvent::LspDocument {
            document: 1,
            update: LspUpdate::Status {
                status: LspStatus::Unavailable
            }
        }
    ));
    assert_eq!(
        Tool::Diagnostics
            .call(&manager, &json!({"path":"main.yu"}))
            .await
            .unwrap_err()
            .to_string(),
        "language server unavailable"
    );
    assert!(manager.servers.lock().unwrap().is_empty());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn crashes_back_off_reopen_buffers_and_idle_performs_shutdown_exit() {
    let (dir, manager) = fixture(Duration::from_millis(800));
    let (tx, mut events) = mpsc::unbounded_channel();
    let mut editor = manager.connection(tx);
    open(&mut editor);
    status(&mut events, LspStatus::Running).await;
    editor.request(1, 1, hover(98));
    let started = tokio::time::Instant::now();
    let (mut failed, mut restarted, mut rejected) = (false, false, false);
    until(&mut events, |event| {
        match event {
            ServerEvent::LspDocument {
                update:
                    LspUpdate::Status {
                        status: LspStatus::Failed,
                    },
                ..
            } => failed = true,
            ServerEvent::LspDocument {
                update:
                    LspUpdate::Status {
                        status: LspStatus::Running,
                    },
                ..
            } => restarted = true,
            ServerEvent::LspResult {
                request: 1,
                outcome: LspOutcome::Error { .. },
            } => rejected = true,
            _ => {}
        }
        (failed && restarted && rejected).then_some(())
    })
    .await;
    assert!(started.elapsed() >= Duration::from_millis(200));
    diagnostic(&mut events, 1).await;
    editor.request(2, 1, hover(19));
    assert!(matches!(
        response(&mut events, 2).await,
        LspOutcome::Value { .. }
    ));
    status(&mut events, LspStatus::Idle).await;
    let log = trace(&dir, "exit").await;
    assert_eq!(
        log.iter()
            .filter(|item| item["method"] == "initialize")
            .count(),
        2
    );
    assert_eq!(log[log.len() - 2]["method"], "shutdown");
    editor.request(3, 1, hover(19));
    assert!(matches!(
        response(&mut events, 3).await,
        LspOutcome::Value { .. }
    ));
}

#[test]
fn roots_follow_cargo_workspaces_and_nearest_lakefile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let member = root.join("pkg");
    let isolated = root.join("other");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::create_dir_all(isolated.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['pkg']\nexclude=['other']",
    )
    .unwrap();
    for child in [&member, &isolated] {
        fs::write(
            child.join("Cargo.toml"),
            "[package]\nname='example'\nversion='0.1.0'",
        )
        .unwrap();
    }
    fs::write(
        root.join("fuzukue.toml"),
        "[lsp]\nrust='fake-server'\nlean='fake-server'",
    )
    .unwrap();
    fs::write(member.join("lakefile.toml"), "name='Example'").unwrap();
    let discover = |path: &Path, language| {
        project::discover(root, path, language)
            .unwrap()
            .unwrap()
            .root
    };
    assert_eq!(discover(&member.join("src"), "rust"), root);
    assert_eq!(discover(&isolated.join("src"), "rust"), isolated);
    assert_eq!(discover(&member.join("src"), "lean"), member);
}

#[tokio::test]
#[ignore = "requires a locally installed rust-analyzer and Rust toolchain"]
async fn real_rust_analyzer_on_cargo_project() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname='example'\nversion='0.1.0'\nedition='2021'",
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn sample() -> u32 { 42 }\npub fn demo() -> u32 { sample() }\n",
    )
    .unwrap();
    let manager = Manager::new(dir.path());
    let result = timeout(Duration::from_secs(35), async {
        loop {
            let result = Tool::Hover
                .call(&manager, &json!({"path":"src/lib.rs","line":2,"col":25}))
                .await;
            if let Ok(value) = result {
                if value.to_string().contains("sample") {
                    break value;
                }
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("rust-analyzer did not return type information");
    assert!(result.to_string().contains("u32"));
}

#[tokio::test]
#[ignore = "requires a locally installed Lean toolchain"]
async fn real_lake_goals_and_sorry_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lakefile.toml"), "name = 'Example'\n").unwrap();
    fs::write(
        dir.path().join("Example.lean"),
        "theorem demo : True := by\n  skip\n  sorry\ndef sample : Nat := 42\n",
    )
    .unwrap();
    let manager = Manager::new(dir.path());
    let goals = Tool::LeanGoal
        .call(&manager, &json!({"path":"Example.lean","line":2,"col":4}))
        .await
        .unwrap();
    assert!(goals["goal"].to_string().contains("True"), "{goals}");
    let term = Tool::LeanGoal
        .call(&manager, &json!({"path":"Example.lean","line":4,"col":22}))
        .await
        .unwrap();
    assert!(term["term_goal"].to_string().contains("Nat"), "{term}");
    let diagnostics = Tool::Diagnostics
        .call(&manager, &json!({"path":"Example.lean"}))
        .await
        .unwrap();
    assert!(diagnostics.to_string().contains("sorry"), "{diagnostics}");
}

#[tokio::test]
#[ignore = "requires a locally installed yulang on PATH"]
async fn real_yulang_diagnostics_hover_and_definition() {
    let dir = tempfile::tempdir().unwrap();
    let text = "my helper = 42\nmy value = helper\nmy malformed = )\n";
    fs::write(dir.path().join("main.yu"), text).unwrap();
    let manager = Manager::new(dir.path());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut editor = manager.connection(tx);
    editor.update(
        1,
        LspBuffer::Open {
            path: "main.yu".into(),
            version: 1,
            text: text.into(),
        },
    );
    status(&mut rx, LspStatus::Running).await;
    let published = diagnostic(&mut rx, 1).await;
    assert_eq!(published["range"]["start"]["line"], 2);

    let pos = json!({"path":"main.yu","line":2,"col":13});
    let hover = Tool::Hover.call(&manager, &pos).await.unwrap();
    assert!(hover["contents"].to_string().contains("helper"), "{hover}");
    let definition = Tool::Definition.call(&manager, &pos).await.unwrap();
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(location["uri"], "main.yu");
    assert_eq!(location["range"]["start"]["line"], 0);
}
