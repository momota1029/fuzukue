use super::*;
use image::{DynamicImage, Rgb, RgbImage};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

fn write_scenario(root: &Path, name: &str, scenario: Value) -> Result<()> {
    let dir = root.join("tests/shots");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{name}.json")), scenario.to_string())?;
    Ok(())
}

#[test]
fn source_root_prefers_live_workspace_then_override_then_build_checkout() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let project = root.join("live");
    let override_root = root.join("override");
    let build_root = root.join("build");
    for checkout in [&project, &override_root, &build_root] {
        fs::create_dir_all(checkout.join("tests/shots"))?;
        fs::create_dir_all(checkout.join("crates/fuzukue"))?;
        fs::write(checkout.join("crates/fuzukue/Cargo.toml"), "")?;
    }
    let resolve = || resolve_source_root(Some(&project), Some(&override_root), &build_root);

    assert_eq!(resolve()?, project);
    fs::remove_file(project.join("crates/fuzukue/Cargo.toml"))?;
    assert_eq!(resolve()?, override_root);
    fs::remove_dir(override_root.join("tests/shots"))?;
    assert_eq!(resolve()?, build_root);
    fs::remove_dir_all(&build_root)?;
    let error = resolve().unwrap_err().to_string();
    for searched in [&project, &override_root, &build_root] {
        assert!(error.contains(searched.to_str().unwrap()), "{error}");
    }
    Ok(())
}

fn test_scenario() -> Value {
    json!({
        "comment": "View the sidebar.\nSecond line.",
        "steps": [
            {"action": "size", "width": 1440, "height": 900},
            {"action": "theme", "theme": "light"},
            {"action": "connect"},
            {"action": "screenshot", "name": "light"},
            {"action": "theme", "theme": "dark"},
            {"action": "screenshot", "name": "dark"}
        ]
    })
}

fn args(scenario: &str) -> Args {
    Args {
        scenario: scenario.into(),
        names: None,
        size: None,
        theme: None,
    }
}

fn png(width: u32, height: u32) -> Result<Vec<u8>> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    let image = RgbImage::from_pixel(width, height, Rgb([127, 35, 247]));
    DynamicImage::ImageRgb8(image).write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

#[test]
fn allow_list_rejects_paths_symlinks_and_unknown_names() -> Result<()> {
    let root = tempfile::tempdir()?;
    write_scenario(root.path(), "sidebar", test_scenario())?;
    write_scenario(root.path(), "bad.name", test_scenario())?;
    let dir = scenario_directory(root.path())?;
    fs::create_dir(dir.join("directory.json"))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("sidebar.json"), dir.join("symlink.json"))?;
    assert!(load_scenario(&dir, "sidebar").is_ok());
    for name in [
        "",
        ".",
        "..",
        "../sidebar",
        "sidebar/1",
        "a\\b",
        "sidebar.json",
        "a..b",
        "directory",
        "symlink",
        "absent",
    ] {
        assert!(load_scenario(&dir, name).is_err(), "accepted {name}");
    }
    let listed = inventory(root.path())?;
    let scenarios = listed["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 1);
    assert_eq!(scenarios[0]["name"], "sidebar");
    assert_eq!(scenarios[0]["description"], "View the sidebar.");
    assert_eq!(scenarios[0]["names"], json!(["light", "dark"]));
    Ok(())
}

#[test]
fn fixture_map_matches_repository_scenarios() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let list = inventory(&root)?;
    let scenarios = list["scenarios"].as_array().unwrap();
    for (name, mode, fixture) in [
        ("sidebar", "fixture", "examples/lab"),
        (
            "card-markdown",
            "fixture",
            "tests/shots/fixtures/card-markdown",
        ),
        (
            "paper-comment",
            "fixture",
            "tests/shots/fixtures/paper-comment",
        ),
        ("github-report", "fixture", "examples/lab"),
        ("right-pane-resize", "fixture", "examples/lab"),
        ("thread-switch", "fixture", "examples/lab"),
        ("safe-close", "fixture", "examples/lab"),
        ("slash-commands", "fixture", "examples/lab"),
        ("editor", "fixture", "tests/shots/fixtures/editor"),
        ("open-file", "fixture", "tests/shots/fixtures/editor"),
        ("extensions", "fixture", "examples/extensions"),
        ("lsp", "fixture", "tests/shots/fixtures/lsp"),
        ("tests", "fixture", "tests/shots/fixtures/tests"),
        ("git", "git", "examples/lab"),
        ("title-bar", "git", "examples/lab"),
        ("branch-review", "git", "examples/lab"),
        ("lean-statement-review", "git", "examples/lab"),
        ("workshop", "workshop", "examples/lab"),
        ("issue-threads", "workshop", "examples/lab"),
    ] {
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario["name"] == name)
            .unwrap();
        assert_eq!(scenario["mode"], mode, "{name}");
        assert_eq!(scenario["project"], fixture, "{name}");
        assert!(
            !scenario["description"].as_str().unwrap().is_empty(),
            "{name}"
        );
    }
    assert!(scenarios
        .iter()
        .any(|scenario| scenario["name"] == "workspaces"
            && scenario["second_project"] == "examples/code-pilot"));
    let url: Scenario = serde_json::from_value(json!({
        "mode": "url",
        "steps": [{"action":"connect"}, {"action":"screenshot","name":"readonly"}]
    }))?;
    assert_eq!(url.fixture("read-only").mode, Mode::Url);
    assert_eq!(url.describe(), "screenshot readonly");
    Ok(())
}

#[test]
fn prepares_selected_names_and_supported_overrides() -> Result<()> {
    let mut scenario: Scenario = serde_json::from_value(test_scenario())?;
    let args: Args = serde_json::from_value(json!({
        "scenario": "sidebar", "names": ["dark"], "theme": "dark",
        "size": {"width": 1000, "height": 750}
    }))?;
    assert_eq!(scenario.prepare(&args)?, ["dark"]);
    assert_eq!(
        scenario.steps[0],
        json!({"action":"size","width":1000,"height":750})
    );
    assert_eq!(scenario.steps[1]["theme"], "dark");
    assert_eq!(scenario.steps[3], json!({"action":"wait","ms":0}));
    assert_eq!(scenario.steps[4]["theme"], "dark");
    assert_eq!(scenario.steps[5]["name"], "dark");
    let mut scenario: Scenario = serde_json::from_value(test_scenario())?;
    let invalid: Args = serde_json::from_value(json!({"scenario":"sidebar", "names":["missing"]}))?;
    assert!(scenario.prepare(&invalid).is_err());
    let mut no_overrides: Scenario = serde_json::from_value(json!({
        "steps": [{"action":"screenshot", "name":"dark"}]
    }))?;
    assert!(no_overrides.prepare(&args).is_err());
    Ok(())
}

#[test]
fn keeps_ten_captures_across_owners_and_uses_worker_readable_paths() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let project = fs::canonicalize(temp.path())?;
    let scratch = project.join("scratch");
    let isolated = project.join("isolated");
    fs::create_dir(&scratch)?;
    fs::create_dir(&isolated)?;
    let shared_scope = crate::worker_scope::Scope::open(&project, &project, &scratch)?;
    let isolated_scope = crate::worker_scope::Scope::open(&project, &isolated, &scratch)?;
    let primary = choose_output_root(
        &project,
        Some(&WorkerStorage {
            cwd: project.clone(),
            scratch: scratch.clone(),
        }),
    )?;
    let fallback = choose_output_root(
        &project,
        Some(&WorkerStorage {
            cwd: isolated,
            scratch: scratch.clone(),
        }),
    )?;
    assert_eq!(primary, project.join("target/shots"));
    assert_eq!(fallback, scratch.join("shots"));
    for (scope, path) in [(shared_scope, &primary), (isolated_scope, &fallback)] {
        let output = output_directory(path, "worker-id")?;
        assert_eq!(
            scope.claude_decision(&json!({
                "tool_name": "Read", "tool_input": {"file_path": output.join("sidebar.png")}
            })),
            json!({})
        );
        fs::remove_dir(output)?;
    }
    for index in 1..=12 {
        fs::create_dir_all(
            primary
                .join(format!("job-{}", index % 2))
                .join(format!("{index:020}-test")),
        )?;
    }
    let unrelated = primary.join("job-0/important");
    fs::create_dir(&unrelated)?;
    prune(&primary)?;
    for index in 1..=12 {
        let path = primary
            .join(format!("job-{}", index % 2))
            .join(format!("{index:020}-test"));
        assert_eq!(path.exists(), index > 2, "{index}");
    }
    assert!(unrelated.is_dir());
    Ok(())
}

#[test]
fn downscales_png_and_preserves_original_dimensions_in_summary() -> Result<()> {
    let input = png(2000, 1000)?;
    let (scaled, original) = downscale_png(&input)?;
    assert_eq!(original, (2000, 1000));
    let image = image::load_from_memory(&scaled)?;
    assert_eq!((image.width(), image.height()), (1568, 784));
    let small = png(120, 80)?;
    assert_eq!(downscale_png(&small)?, (small, (120, 80)));
    Ok(())
}

#[cfg(unix)]
fn status(code: i32) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    Exit::Status(std::process::ExitStatus::from_raw(code << 8))
}

#[cfg(unix)]
#[test]
fn formats_result_images_and_failures() -> Result<()> {
    let output = tempfile::tempdir()?;
    let path = output.path();
    let names: Vec<String> = ["a", "b", "c", "d"].map(str::to_owned).into();
    let png_bytes = png(20, 30)?;
    for name in &names {
        fs::write(path.join(format!("{name}.png")), &png_bytes)?;
    }
    let mut args = args("sidebar");
    let elapsed = Duration::from_secs(3);
    let reply = result(&args, &names, path, elapsed, Ok(status(0)), &[BUILDING]);
    let content = reply["content"].as_array().unwrap();
    let summary = content[0]["text"].as_str().unwrap();
    assert_eq!(reply["isError"], false);
    assert!(summary.contains("Scenario: sidebar\nDuration: 3.0 s\nExit status: 0"));
    assert!(summary.contains(&format!(
        "{} ({} bytes, 20×30 px)",
        path.join("d.png").display(),
        png_bytes.len()
    )));
    assert_eq!(content.len(), 4);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert_eq!(
        BASE64_STANDARD.decode(content[1]["data"].as_str().unwrap())?,
        png_bytes
    );
    args.names = Some(names.clone());
    assert_eq!(
        result(&args, &names, path, elapsed, Ok(status(0)), &[BUILDING])["content"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    let log = (1..=24)
        .map(|line| format!("client line {line:02}\n"))
        .collect::<String>();
    fs::write(
        path.join("client.log"),
        format!("{log}Error: scenario step 9: timed out waiting for attachment connected\n"),
    )?;
    let failure = result(
        &args,
        &names,
        path,
        elapsed,
        Ok(status(1)),
        &[QUEUED, BUILDING],
    );
    assert_eq!(failure["isError"], true);
    let text = failure["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("撮影待ち: 前の撮影が終わるまで\nビルド中"));
    assert!(text.contains("Exit status: 1"));
    assert!(text.contains("scenario step 9: timed out waiting for attachment connected"));
    assert!(!text.contains("client line 05"));
    assert!(text.contains("client line 06"));

    fs::remove_file(path.join("client.log"))?;
    fs::write(
        path.join("shot.log"),
        "headless capture needs loopback sockets; this environment forbids them\n",
    )?;
    let skip = result(&args, &names, path, elapsed, Ok(status(77)), &[BUILDING]);
    assert_eq!(skip["isError"], true);
    let text = skip["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Exit status: 77"));
    assert!(text.contains("撮影できませんでした（headless capture needs loopback sockets; this environment forbids them）"));

    let timeout = result(
        &args,
        &names,
        path,
        elapsed,
        Ok(Exit::Timeout(elapsed)),
        &[BUILDING],
    );
    assert_eq!(timeout["isError"], true);
    let text = timeout["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Exit status: timeout"));
    assert!(text.contains("撮影できませんでした（3 s timeout）"));
    Ok(())
}

#[cfg(unix)]
struct FakeRunner {
    started: mpsc::UnboundedSender<String>,
    release: Semaphore,
}

#[cfg(unix)]
impl Runner for FakeRunner {
    async fn run(&self, run: Run<'_>) -> Result<Exit> {
        self.started.send(run.name.to_owned())?;
        self.release.acquire().await?.forget();
        for name in run.scenario.names() {
            fs::write(run.output.join(format!("{name}.png")), png(16, 16)?)?;
        }
        Ok(status(0))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn runs_one_capture_at_a_time_in_fifo_order() -> Result<()> {
    let root = tempfile::tempdir()?;
    let queue = Arc::new(Mutex::new(()));
    let (started, mut starts) = mpsc::unbounded_channel();
    let runner = Arc::new(FakeRunner {
        started,
        release: Semaphore::new(0),
    });
    let mut calls = Vec::new();
    for (index, name) in ["first", "second", "third"].into_iter().enumerate() {
        write_scenario(root.path(), name, test_scenario())?;
        let root = root.path().to_path_buf();
        let runner = runner.clone();
        let queue = queue.clone();
        let (tx, mut progress) = mpsc::unbounded_channel();
        let call = tokio::spawn(async move {
            capture_with(
                runner.as_ref(),
                &queue,
                Request {
                    root: &root,
                    project: &root,
                    storage: None,
                    owner: name,
                    args: args(name),
                    timeout: Duration::from_secs(30),
                },
                |message| {
                    tx.send(message).ok();
                },
            )
            .await
        });
        assert_eq!(
            progress.recv().await,
            Some(if index == 0 { BUILDING } else { QUEUED })
        );
        calls.push((call, progress));
    }
    for (index, (call, mut progress)) in calls.into_iter().enumerate() {
        if index != 0 {
            assert_eq!(progress.recv().await, Some(BUILDING));
        }
        assert_eq!(
            starts.recv().await.as_deref(),
            Some(["first", "second", "third"][index])
        );
        assert!(starts.try_recv().is_err());
        runner.release.add_permits(1);
        assert_eq!(call.await??["isError"], false);
    }
    Ok(())
}

#[test]
fn worker_tools_include_captures_and_respect_explicit_allow_list() {
    let default = crate::mcp::worker_tool_names(&[]);
    assert!(default.iter().any(|tool| tool == LIST));
    assert!(default.iter().any(|tool| tool == CAPTURE));
    let filtered = crate::mcp::worker_tool_names(&[LIST.into()]);
    assert_eq!(filtered, [LIST, "escalate"]);
    assert!(worker_tool_timeout(&default) >= capture_timeout().as_secs());
    assert_eq!(worker_tool_timeout(&filtered), 200);
}

#[test]
fn permits_only_local_read_only_endpoints() -> Result<()> {
    validate_url("ws://127.0.0.1:7879/ws")?;
    validate_url("ws://[::1]:7879/ws")?;
    for url in [
        "ws://192.0.2.2:7879/ws",
        "http://127.0.0.1/ws",
        "ws://127.0.0.1/wrong",
        "ws://127.0.0.1/ws?token=123",
        "ws://user:pass@127.0.0.1/ws",
    ] {
        assert!(validate_url(url).is_err(), "accepted {url}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn script_runner_invokes_fixture_and_kills_timed_out_processes() -> Result<()> {
    use rustix::process::Pid;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir()?;
    let root = root.path();
    fs::create_dir_all(root.join("examples/lab"))?;
    fs::create_dir(root.join("scripts"))?;
    let script = root.join("scripts/shot.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$1" "$2" "$3" "$HOME" "$XDG_STATE_HOME"
trap 'wait "$child"; exit 143' TERM
sleep 60 &
child=$!
printf '%s\n' "$child" > "$FUZUKUE_SHOT_CLIENT_LOG"
wait "$child"
"#,
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let output = root.join("output");
    fs::create_dir(&output)?;
    let scenario: Scenario = serde_json::from_value(test_scenario())?;
    let timeout = Duration::from_secs(1);
    let exit = Script
        .run(Run {
            root,
            output: &output,
            name: "sidebar",
            fixture: scenario.fixture("sidebar"),
            scenario,
            timeout,
        })
        .await?;
    assert!(matches!(exit, Exit::Timeout(elapsed) if elapsed == timeout));
    let log = fs::read_to_string(output.join("shot.log"))?;
    let args: Vec<_> = log.lines().collect();
    assert_eq!(Path::new(args[0]), root.join("examples/lab"));
    assert!(Path::new(args[1]).starts_with(&output) && args[1].ends_with("/sidebar.json"));
    assert_eq!(Path::new(args[2]), output);
    assert!(Path::new(args[3]).starts_with(&output) && args[3].ends_with("/home"));
    assert!(Path::new(args[4]).starts_with(&output) && args[4].ends_with("/xdg/state"));
    let pid = fs::read_to_string(output.join("client.log"))?
        .trim()
        .parse()?;
    let pid = Pid::from_raw(pid).unwrap();
    // The script waits for its child in the TERM handler, so both are reaped.
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    Ok(())
}
