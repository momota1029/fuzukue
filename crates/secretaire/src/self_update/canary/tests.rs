use super::*;
use crate::self_update::{tests::source, CommandLine, Config, Status};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[path = "../../../../../tests/support/loopback.rs"]
mod loopback;

const TEST_PORT: u16 = 7879;

#[test]
fn busy_ports_are_skipped_and_range_is_bounded() -> Result<()> {
    let mut attempted = Vec::new();
    let selected = find_port(PORTS, |port| {
        attempted.push(port);
        if port < 7881 {
            Err(std::io::ErrorKind::AddrInUse.into())
        } else {
            Ok(())
        }
    })?;
    assert_eq!(selected, 7881);
    assert_eq!(attempted, [7879, 7880, 7881]);
    assert!(find_port(PORTS, |_| Err(std::io::ErrorKind::AddrInUse.into())).is_err());
    Ok(())
}

#[test]
fn runtime_environment_and_config_are_isolated() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, TEST_PORT));
    let cmd = runtime_command(
        Path::new("/bin/true"),
        dir.path(),
        dir.path(),
        // Synthetic listener in the Tailscale range.
        "[remote]\nlisten = ['100.64.0.31:7878']\nallowed_logins = ['user']\n[self_update]\nsource = '/live'",
        addr,
    )?;
    let env = cmd
        .as_std()
        .get_envs()
        .collect::<std::collections::BTreeMap<_, _>>();
    let value = |key: &str| {
        env.get(std::ffi::OsStr::new(key))
            .copied()
            .flatten()
            .unwrap()
    };
    assert_eq!(value("FUZUKUE_CANARY"), "1");
    assert_eq!(Path::new(value("XDG_STATE_HOME")), dir.path().join("state"));
    assert_eq!(Path::new(value("HOME")), dir.path().join("home"));
    assert!(!env.contains_key(std::ffi::OsStr::new("FUZUKUE_SELF_UPDATE_SOURCE")));
    assert!(!env.contains_key(std::ffi::OsStr::new("FUZUKUE_SERVICE")));
    let config: toml::Value = fs::read_to_string(value("FUZUKUE_SERVER_CONFIG"))?.parse()?;
    assert!(config.get("self_update").is_none());
    assert_eq!(
        config["remote"]["listen"][0].as_str(),
        Some("127.0.0.1:7879")
    );
    assert_eq!(config["remote"]["allowed_logins"][0].as_str(), Some("user"));
    Ok(())
}

#[tokio::test]
async fn canary_flag_blocks_github_and_research_pollers() -> Result<()> {
    const TEST: &str = "self_update::canary::tests::canary_flag_blocks_github_and_research_pollers";
    if !crate::canary() {
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", TEST, "--nocapture"])
            .env("FUZUKUE_CANARY", "1")
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (app, workspace) = fixture_app(dir.path())?;
    assert!(crate::canary());
    assert!(crate::issue_threads::watch(&workspace).is_none());
    let updates = workspace.research_issues.subscribe(&app, &workspace);
    assert!(!workspace.research_issues.watcher_started());
    assert!(updates.borrow().is_none());
    assert!(!crate::auto_review::enabled());
    assert!(!workspace.front_desk_nudge);
    assert!(crate::canary_snapshot().canary);
    Ok(())
}

fn fixture_app(dir: &Path) -> Result<(Arc<crate::App>, Arc<crate::Workspace>)> {
    let project = dir.join("live");
    fs::create_dir(&project)?;
    let app = crate::App::new(
        dir.to_path_buf(),
        project,
        dir.join("live-state"),
        "127.0.0.1:7878".into(),
        None,
    )?;
    let workspace = app.select(None)?;
    Ok((app, workspace))
}

fn script(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

struct Fixture {
    dir: tempfile::TempDir,
    app: Arc<crate::App>,
    update: Arc<Update>,
    commit: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let (app, workspace) = fixture_app(dir.path())?;
        let source = source(dir.path())?;
        let commit = git(&source, &["rev-parse", "HEAD"]).await?;
        let root = dir.path().join("server-builds");
        let config = Config {
            tests: vec![CommandLine::Shell("sleep 0.05; echo testing".into())],
            prebuild: Some(vec![CommandLine::Shell("echo prebuild >> \"$FUZUKUE_PROJECT_DIR/publish-log\"".into())]),
            publish: vec![CommandLine::Shell("test \"$FUZUKUE_PREBUILT\" = 1 && echo publish >> \"$FUZUKUE_PROJECT_DIR/publish-log\"".into())],
            canary_minutes: 1,
            ..Default::default()
        };
        // Wait keeps the swap from ever reaching a real supervisor in these tests.
        let update = Arc::new(Update::with_paths(
            config,
            Some(source),
            workspace.project_dir.clone(),
            root,
            PathBuf::new(),
        ));
        Ok(Self {
            dir,
            app,
            update,
            commit,
        })
    }

    async fn start(&self, good_health: bool) -> Result<()> {
        let fixture = self.dir.path().join("candidate");
        let outside = self.dir.path().join("outside");
        fs::write(&outside, "untouched")?;
        let actual_commit = if good_health {
            self.commit.as_str()
        } else {
            "wrong-build"
        };
        script(
            &fixture,
            &format!(
                r#"#!/bin/sh
set -eu
test "$FUZUKUE_CANARY" = 1
if (printf 'changed' > '{}') 2>/dev/null; then
  echo outside-write-succeeded
  exit 12
fi
printf 'HTTP/1.0 200 OK\r\nx-fuzukue-build-id: {}\r\n\r\n%s' "$$" > "$XDG_STATE_HOME/health"
echo candidate-running
python3 -c '
import os, time
os.setpgid(0, 0)
with open(os.environ["XDG_STATE_HOME"] + "/child", "w") as marker:
    marker.write(str(os.getpid()))
time.sleep(120)
' &
wait
"#,
                outside.display(),
                actual_commit
            ),
        )?;
        let mut build = Command::new("sh");
        build
            .args([
                "-ec",
                r#"
            mkdir -p "$CARGO_TARGET_DIR/release"
            cp "$1" "$CARGO_TARGET_DIR/release/secretaire"
            echo build >> "$FUZUKUE_PROJECT_DIR/build-log"
        "#,
                "build",
            ])
            .arg(fixture);
        let root = self.update.root.clone();
        let network = Network {
            port: || Ok(TEST_PORT),
            probe: Arc::new(move |addr, pid, build| {
                assert_eq!(addr, SocketAddr::from((Ipv4Addr::LOCALHOST, TEST_PORT)));
                for entry in root.read_dir()? {
                    let entry = entry?;
                    if entry.file_name().to_string_lossy().starts_with("canary-") {
                        if let Ok(response) = fs::read_to_string(entry.path().join("state/health"))
                        {
                            return Ok(supervisor::health_response_matches(&response, pid, build));
                        }
                    }
                }
                Ok(false)
            }),
        };
        self.update
            .start_canary_with(self.app.clone(), build, network)
            .await
    }

    fn directories(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in self.update.root.read_dir()? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("canary-") {
                paths.push(entry.path());
            }
        }
        Ok(paths)
    }

    async fn wait(&self, phase: Phase) -> Result<Status> {
        let mut update = self.update.subscribe();
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            update.wait_for(|status| {
                status
                    .canary_status
                    .as_ref()
                    .is_some_and(|canary| canary.state == phase || canary.state == Phase::Failed)
            }),
        )
        .await
        .context("expected canary phase")??
        .clone();
        assert_eq!(
            status.canary_status.as_ref().unwrap().state,
            phase,
            "{status:?}"
        );
        Ok(status)
    }
}

#[tokio::test]
async fn fake_candidate_runs_and_replacement_reaps_the_old_process() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut status = fixture.update.subscribe();
    let real_update = fixture.update.run.lock().await;
    assert!(fixture.start(true).await.is_err());
    drop(real_update);
    fixture.start(true).await?;
    assert_eq!(
        status.borrow().canary_status.as_ref().unwrap().state,
        Phase::Building
    );
    let tested = tokio::time::timeout(
        Duration::from_secs(10),
        status.wait_for(|status| status.canary_status.as_ref().unwrap().state == Phase::Testing),
    )
    .await??
    .clone();
    assert!(tested.active);
    let running = fixture.wait(Phase::Running).await?;
    let running = running.canary_status.unwrap();
    assert_eq!(running.commit.as_deref(), Some(fixture.commit.as_str()));
    assert_eq!(running.port, Some(TEST_PORT));
    assert_eq!(
        running.connect_url.as_deref(),
        Some("ws://127.0.0.1:7879/ws")
    );
    assert_eq!(
        running.auto_discard_at.unwrap() - running.started_at.unwrap(),
        60_000
    );
    assert!(running
        .log
        .contains("Windows クライアントの事前ビルドと配布を省く"));
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("outside"))?,
        "untouched"
    );
    assert!(!fixture.update.project.join("publish-log").exists());
    assert!(fixture.update.start(fixture.app.clone(), false).is_err());

    let old = fixture.directories()?.pop().unwrap();
    let pid = fs::read_to_string(old.join("pid"))?;
    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = fs::read_to_string(old.join("state/child")) {
                if !pid.is_empty() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    fixture.start(true).await?;
    fixture.wait(Phase::Running).await?;
    let dirs = fixture.directories()?;
    assert_eq!(dirs.len(), 1);
    assert_ne!(dirs[0], old);
    assert!(!old.exists());
    assert!(!Path::new("/proc").join(pid).exists());
    tokio::time::timeout(Duration::from_secs(5), async {
        while fs::read_to_string(Path::new("/proc").join(&child).join("stat"))
            .is_ok_and(|stat| !stat.rsplit_once(')').unwrap().1.starts_with(" Z "))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("separate canary job group survived replacement")?;

    fixture.update.discard_canary().await?;
    let discarded = fixture.wait(Phase::Discarded).await?;
    let log = discarded.canary_status.unwrap().log;
    assert!(log.contains("candidate-running"));
    assert!(log.contains("1 件削除"));
    assert!(fixture.directories()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn promote_refuses_moved_head_then_reuses_the_tested_binary() -> Result<()> {
    let fixture = Fixture::new().await?;
    let hold = fixture.update.gate.read().await;
    fixture.start(true).await?;
    fixture.wait(Phase::Running).await?;
    let old = fixture.directories()?.pop().unwrap();
    let staged = staged_binary_under(&old.join(&fixture.commit))?;
    let ino = staged.metadata()?.ino();
    let source = fixture.update.source.as_deref().unwrap();
    git(
        source,
        &[
            "-c",
            "user.email=test@example.org",
            "-c",
            "user.name=Test",
            "commit",
            "--allow-empty",
            "-qm",
            "move",
        ],
    )
    .await?;
    let error = fixture.update.promote_canary().await.unwrap_err();
    assert!(error.to_string().contains("コミットが変わった"));
    assert_eq!(fixture.update.canary_phase(), Some(Phase::Running));
    assert!(!fixture.update.project.join("publish-log").exists());
    git(source, &["reset", "--hard", &fixture.commit]).await?;

    fixture.update.promote_canary().await?;
    let mut status = fixture.update.subscribe();
    tokio::time::timeout(
        Duration::from_secs(10),
        status.wait_for(|status| {
            status
                .steps
                .iter()
                .any(|step| step.kind == Step::Wait && step.state == State::Running)
        }),
    )
    .await??;
    assert_eq!(fixture.update.canary_phase(), Some(Phase::Promoting));
    let promoted = staged_binary_under(&fixture.update.root.join(&fixture.commit))?;
    assert_eq!(promoted.metadata()?.ino(), ino);
    assert_eq!(
        fs::read_to_string(fixture.update.project.join("build-log"))?,
        "build\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.update.project.join("publish-log"))?,
        "prebuild\n"
    );
    assert!(fixture.directories()?.is_empty());
    assert!(fixture.start(true).await.is_err());
    assert!(fixture.update.discard_canary().await.is_err());
    fixture.update.shutdown_canary().await;
    assert_eq!(fixture.update.canary_phase(), Some(Phase::Discarded));
    assert_eq!(
        fs::read_to_string(fixture.update.project.join("publish-log"))?,
        "prebuild\n"
    );
    drop(hold);
    Ok(())
}

fn staged_binary_under(dir: &Path) -> Result<PathBuf> {
    for entry in dir.read_dir()? {
        let path = entry?.path().join("secretaire");
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("no staged binary under {}", dir.display())
}

#[tokio::test]
async fn fake_clock_discards_a_running_candidate_at_the_deadline() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.start(true).await?;
    fixture.wait(Phase::Running).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(fixture.update.canary_phase(), Some(Phase::Running));
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let status = fixture.wait(Phase::Discarded).await?;
    assert!(status.canary_status.unwrap().log.contains("自動で破棄"));
    assert!(fixture.directories()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn mismatched_health_fails_and_preserves_the_candidate_log() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.start(false).await?;
    let mut status = fixture.update.subscribe();
    tokio::time::timeout(
        Duration::from_secs(10),
        status.wait_for(|status| status.canary_status.as_ref().unwrap().port.is_some()),
    )
    .await??;
    let dir = fixture.directories()?.pop().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dir.join("state/child").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    tokio::time::pause();
    tokio::time::advance(HEALTH_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::resume();
    let status = fixture.wait(Phase::Failed).await?;
    let log = status.canary_status.unwrap().log;
    assert!(log.contains("ヘルスチェック"));
    assert!(log.contains("candidate-running"));
    assert!(fixture.directories()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn tcp_health_checks_the_fake_shell_candidate() -> Result<()> {
    let Some(socket) = loopback::listen()? else {
        return Ok(());
    };
    let addr = socket.local_addr()?;
    drop(socket);
    let root = tempfile::tempdir()?;
    let binary = root.path().join("candidate");
    let project = root.path().join("project");
    fs::create_dir(&project)?;
    script(
        &binary,
        &format!(
            r#"#!/bin/sh
exec python3 - <<'PYTHON'
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
class Health(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('x-fuzukue-build-id', 'candidate-commit')
        self.end_headers()
        self.wfile.write(str(os.getpid()).encode())
HTTPServer(('127.0.0.1', {}), Health).serve_forever()
PYTHON
"#,
            addr.port()
        ),
    )?;
    let mut runtime = Runtime::start(&binary, &project, root.path(), "", addr)?;
    runtime
        .wait_healthy(
            "candidate-commit",
            Duration::from_secs(5),
            Arc::new(supervisor::probe_build),
        )
        .await?;
    assert!(!supervisor::probe_build(
        addr,
        &runtime.pid.to_string(),
        Some("wrong")
    )?);
    runtime.stop().await?;
    Ok(())
}
