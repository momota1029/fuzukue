use super::*;
use crate::{test_support::TestDir, App, Hub};
use fuzukue_proto::{Actor, HandoffIssueSelection, HandoffWorkerMode, HandoffWorkerStatus};
use serde_json::{json, Value};
use tokio::time::timeout;

const PATH: &str = ".fuzukue/handoff/2026-01-01-proof-notes.md";
const NOTE: &str = include_str!("../../tests/fixtures/handoff/proof-notes.md");
const FIRST_ID: &str = "00000000-0000-4000-8000-000000000001";
const MISSING_ID: &str = "00000000-0000-4000-8000-000000000000";

fn workspace(dir: &TestDir) -> Workspace {
    Workspace::new(
        dir.path().join("project"),
        "127.0.0.1:1".into(),
        dir.path().join("state"),
        Arc::new(Hub::new()),
    )
}

fn fixture(dir: &TestDir) {
    dir.write(&format!("project/{PATH}"), NOTE);
}

async fn git(root: &Path, args: &[&str]) -> Result<()> {
    let output = crate::git::run(root, args, 65536).await?;
    ensure!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn parses_template_and_missing_sections() -> Result<()> {
    let note = parse(PATH, &NOTE.replace('\n', "\r\n"))?;
    assert_eq!(note.summary.title, "サンプル原稿の参照を整える");
    assert_eq!(note.summary.date, "2026-01-01");
    assert!(note.summary.source.starts_with("Claude Code session "));
    assert!(note.transcript.ends_with(".jsonl"));
    assert_eq!(note.summary.tasks, 2);
    assert_eq!(note.summary.workers, 1);
    assert!(note.tasks[0]
        .markdown
        .contains("- 受け入れ条件: `fuzukue-cards check` が通る。"));
    assert_eq!(note.tasks[1].title, "原稿の PDF を確認する");
    assert_eq!(note.workers[0].thread_id, FIRST_ID);
    assert_eq!(note.workers[0].worktree, "/home/example/lab-definitions");
    assert_eq!(note.workers[0].branch, "work/definitions");
    assert_eq!(note.workers[0].state, "作業中");

    let minimal = parse(PATH, "---\nkind: handoff\n---\n# Empty\n")?;
    assert!(minimal.tasks.is_empty() && minimal.workers.is_empty());
    let fenced = parse(PATH, "---\nkind: handoff\n---\n# Example\n## 残っている仕事\n### 1. Check\n~~~md\n### Ignore\n## Also ignore\n~~~\n- 内容: Example\n## 次\n### Not a task")?;
    assert_eq!(fenced.tasks.len(), 1);
    assert!(fenced.tasks[0].markdown.contains("### Ignore"));
    assert!(!fenced.tasks[0].markdown.contains("Not a task"));
    Ok(())
}

#[tokio::test]
async fn discovers_only_handoffs_and_watches_new_directory() -> Result<()> {
    let dir = TestDir::new();
    fs::create_dir(dir.path().join("project"))?;
    let app = Arc::new(workspace(&dir));
    assert!(discover(&app.project_dir).is_empty());
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    watch(&app);
    dir.write(
        "project/.fuzukue/handoff/ignore.md",
        "---\nkind: design\n---\n# Ignore",
    );
    dir.write("project/.fuzukue/handoff/ignore.txt", NOTE);
    dir.write("project/.fuzukue/handoff/sub/ignore.md", NOTE);
    std::os::unix::fs::symlink("sub/ignore.md", app.project_dir.join(DIR).join("linked.md"))?;
    fixture(&dir);
    timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::Handoffs { notes } = live.recv().await?.event {
                assert_eq!(notes.len(), 1);
                assert_eq!(notes[0].path, PATH);
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn receives_titled_thread_with_first_attachment_and_front_desk_request() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::new();
    fixture(&dir);
    dir.write(
        "partner",
        r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"handoff-partner"}'
while IFS= read -r line; do
    printf '%s\n' "$line" >> handoff-prompt.jsonl
    echo '{"type":"result","is_error":false,"session_id":"handoff-partner"}'
done
"#,
    );
    let partner = dir.path().join("partner");
    fs::set_permissions(&partner, fs::Permissions::from_mode(0o755))?;
    let mut app = workspace(&dir);
    app.claude_program = Some(partner);
    let app = Arc::new(app);
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let (reply, mut responses) = mpsc::unbounded_channel();
    handle(
        &app,
        HandoffRequest::Receive { path: PATH.into() },
        fuzukue_i18n::Locale::JAPANESE,
        &reply,
    )
    .await?;
    let Some(ServerEvent::HandoffOpened { session }) = responses.recv().await else {
        panic!("Missing opened event")
    };
    let Some(ServerEvent::HandoffDetail { detail }) = responses.recv().await else {
        panic!("Missing checklist")
    };
    assert_eq!(detail.session, session);
    assert_eq!(detail.tasks.len(), 2);
    assert_eq!(app.hub.thread_artifacts(&session), [PATH]);
    let ServerEvent::Threads { threads } = app.hub.read_threads(|store| store.snapshot()) else {
        unreachable!()
    };
    assert!(threads
        .iter()
        .any(|t| t.id == session && t.title == detail.note.title));
    assert_eq!(receive(&app, PATH)?, (session.clone(), false));
    timeout(Duration::from_secs(6), async {
        loop {
            if matches!(live.recv().await?.event, ServerEvent::TurnEnded { session: ref id, .. } if id == &session) {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    let prompt = fs::read_to_string(app.project_dir.join("handoff-prompt.jsonl"))?;
    assert!(
        prompt.contains(PATH)
            && (prompt.contains("Summarise the goal") || prompt.contains("目標、進捗"))
    );
    Ok(())
}

#[tokio::test]
async fn approved_tasks_use_verbatim_body_ready_choice_and_persisted_links() -> Result<()> {
    check_approved_tasks("project").await
}

#[tokio::test]
async fn approved_tasks_round_trip_links_for_percent_space_and_unicode_paths() -> Result<()> {
    check_approved_tasks("project%20 日本語").await
}

fn issue_file_link_path(footer: &str, label: &str) -> Result<PathBuf> {
    let destination = footer
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|link| link.rsplit_once("](<"))
        .and_then(|(_, url)| url.strip_suffix(">)"))
        .with_context(|| format!("Missing {label}link in issue body"))?;
    ensure!(
        destination.is_ascii() && !destination.bytes().any(|byte| byte.is_ascii_whitespace()),
        "Unescaped file URL: {destination}"
    );
    let url = url::Url::parse(destination)?;
    ensure!(url.scheme() == "file", "Expected a file URL: {url}");
    url.to_file_path()
        .map_err(|()| anyhow::anyhow!("Invalid file path URL: {url}"))
}

async fn check_approved_tasks(project_name: &str) -> Result<()> {
    let dir = TestDir::new();
    dir.write(&format!("{project_name}/{PATH}"), NOTE);
    dir.write(
        &format!("{project_name}/fuzukue.toml"),
        "[research]\nrepo = \"example-owner/lab\"\n",
    );
    let project = dir.path().join(project_name);
    git(&project, &["init", "-b", "main"]).await?;
    let bin = dir.path().join("bin");
    fs::create_dir(&bin)?;
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/gh"),
        bin.join("gh"),
    )?;
    let mut registry = App::new(
        dir.path().into(),
        project.clone(),
        dir.path().join("state"),
        "127.0.0.1:1".into(),
        None,
    )?;
    Arc::get_mut(&mut registry).unwrap().workshop.gh_bin_dir = Some(bin);
    let app = registry.select(None)?;
    let (session, _) = receive(&app, PATH)?;
    let Note {
        tasks, transcript, ..
    } = parse(PATH, NOTE)?;
    let selection = vec![
        HandoffIssueSelection {
            id: tasks[0].id.clone(),
            ready: false,
        },
        HandoffIssueSelection {
            id: tasks[1].id.clone(),
            ready: true,
        },
    ];
    let listed = Tool::Tasks
        .call(&app, &session, Actor::System, &json!({}))
        .await?;
    assert_eq!(listed["tasks"][0]["markdown"], tasks[0].markdown);
    assert!(Tool::Create
        .call(
            &app,
            &session,
            Actor::System,
            &json!({"confirm":false,"tasks":selection})
        )
        .await
        .is_err());

    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let pending = {
        let (app, session, selection) = (app.clone(), session.clone(), selection.clone());
        tokio::spawn(async move {
            Tool::Create
                .call(
                    &app,
                    &session,
                    Actor::System,
                    &json!({"confirm":true,"tasks":selection}),
                )
                .await
        })
    };
    let request_id = timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::ApprovalRequested { request_id, .. } = live.recv().await?.event {
                return Ok::<_, anyhow::Error>(request_id);
            }
        }
    })
    .await??;
    assert!(!project.join(".git/workshop-fixture.json").exists());
    crate::resolve_approval(&app, request_id, "accept".into(), false).await?;
    let result = pending.await??;
    assert_eq!(result["issues"][0]["number"], 13);
    assert_eq!(result["issues"][1]["number"], 14);
    let records: Value =
        serde_json::from_slice(&fs::read(project.join(".git/workshop-fixture.json"))?)?;
    let body = records[1]["body"].as_str().unwrap();
    let footer = body
        .strip_prefix(&tasks[0].markdown)
        .context("Issue body did not preserve task Markdown")?;
    assert_eq!(
        issue_file_link_path(footer, "Handoff: ")?,
        project.join(PATH)
    );
    assert_eq!(
        issue_file_link_path(footer, "Source transcript: ")?,
        Path::new(&transcript)
    );
    assert_eq!(records[1]["labels"], json!([]));
    assert_eq!(records[2]["labels"], json!([{"name":"dot:ready"}]));

    drop(app);
    drop(registry);
    let mut registry = App::new(
        dir.path().into(),
        project.clone(),
        dir.path().join("state"),
        "127.0.0.1:1".into(),
        None,
    )?;
    Arc::get_mut(&mut registry).unwrap().workshop.gh_bin_dir = Some(dir.path().join("bin"));
    let restored = registry.select(None)?;
    assert_eq!(
        issues::create(&restored, &session, &selection).await?,
        result
    );
    let state = restored
        .hub
        .read_threads(|store| store.github_state(&session))
        .unwrap();
    assert_eq!(
        state
            .links
            .iter()
            .map(|link| link.number)
            .collect::<Vec<_>>(),
        [13, 14]
    );
    assert_eq!(detail(&restored, &session)?.tasks[1].issue, Some(14));
    Ok(())
}

#[tokio::test]
async fn adopts_same_codex_thread_and_reports_missing_rollout_without_starting_another_worker(
) -> Result<()> {
    let dir = TestDir::new();
    let root = dir.path().join("project");
    fixture(&dir);
    git(&root, &["init", "-b", "main"]).await?;
    git(
        &root,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "Start",
        ],
    )
    .await?;
    let worktree = dir.path().join("worker tree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "work/definitions",
            worktree.to_str().unwrap(),
        ],
    )
    .await?;
    let real_note = NOTE
        .replace("/home/example/lab-definitions", worktree.to_str().unwrap())
        .replace(
            "## リポジトリの状態",
            &format!(
                "| {MISSING_ID} | missing | {} | running | none |\n\n## リポジトリの状態",
                worktree.display()
            ),
        );
    dir.write(&format!("project/{PATH}"), &real_note);
    let app = workspace(&dir);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/codex");
    let log = dir.path().join("codex.jsonl");
    let request_log = log.clone();
    assert!(app.worker_codex_command.set(Box::new(move || {
        let mut command = tokio::process::Command::new(&fixture);
        command
            .env("FUZUKUE_CODEX_FIXTURE_LOG", &request_log)
            .env("FUZUKUE_CODEX_FIXTURE_MISSING_THREAD", MISSING_ID)
            .env_remove("FUZUKUE_CODEX_SHOT");
        command
    })).is_ok());
    let app = Arc::new(app);
    let (session, _) = receive(&app, PATH)?;
    let mut workers = parse(PATH, &real_note)?.workers.into_iter();
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    workers::start(
        &app,
        session.clone(),
        workers.next().unwrap(),
        HandoffWorkerMode::Adopt,
    )
    .await?;
    workers::start(
        &app,
        session.clone(),
        workers.next().unwrap(),
        HandoffWorkerMode::Adopt,
    )
    .await?;

    timeout(Duration::from_secs(10), async {
        let (mut adopted, mut finished, mut failed, mut streamed) = (false, false, false, false);
        while !(adopted && finished && failed && streamed) {
            match live.recv().await?.event {
                ServerEvent::HandoffWorkerUpdated {
                    thread_id, status, ..
                } => match (thread_id.as_str(), status) {
                    (FIRST_ID, HandoffWorkerStatus::Adopted) => adopted = true,
                    (FIRST_ID, HandoffWorkerStatus::Finished(text)) => {
                        assert!(!text.is_empty());
                        finished = true;
                    }
                    (MISSING_ID, HandoffWorkerStatus::Failed(error)) => {
                        assert!(error.contains("no rollout found"));
                        failed = true;
                    }
                    _ => {}
                },
                ServerEvent::TextDelta {
                    actor:
                        Actor::Codex {
                            thread_id,
                            parent_tool_use: Some(job),
                        },
                    ..
                } if thread_id == FIRST_ID && job.starts_with(fuzukue_proto::DELEGATE_JOB) => {
                    streamed = true
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let requests = fs::read_to_string(log)?;
    let requests: Vec<Value> = requests
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    for id in [FIRST_ID, MISSING_ID] {
        let request = requests
            .iter()
            .find(|r| r["method"] == "thread/resume" && r["params"]["threadId"] == id)
            .unwrap();
        assert_eq!(request["params"]["cwd"], worktree.to_str().unwrap());
        assert_eq!(request["params"]["approvalPolicy"], "never");
    }
    assert!(requests
        .iter()
        .filter(|r| r["method"] == "turn/start")
        .all(|r| r["params"]["threadId"] == FIRST_ID));
    assert!(!requests.iter().any(|r| r["method"] == "thread/start"));
    Ok(())
}

#[test]
fn offers_installed_local_and_workspace_rule_pack_skills() -> Result<()> {
    let dir = TestDir::new();
    let skill = "---\nname: proof-handoff-example\ndescription: Check proof notes\n---\nCompare proof references.\n";
    dir.write("project/fuzukue.toml", "[[rules]]\nsource = \"pack\"\n");
    dir.write("project/pack/skills/workspace-method/SKILL.md", skill);
    dir.write("home/.claude/skills/proof-handoff-example/SKILL.md", skill);
    dir.write("home/.codex/skills/unrelated/SKILL.md", skill);
    let project = dir.path().join("project");
    let homes = [
        dir.path().join("home/.claude"),
        dir.path().join("home/.codex"),
    ];
    let procedures =
        "- スキル: `~/.claude/skills/proof-handoff-example/SKILL.md`、workspace-method。";
    let candidates = skill_offers(&project, &homes, procedures)?;
    assert_eq!(
        candidates
            .iter()
            .map(|s| (s.name.as_str(), s.enabled))
            .collect::<Vec<_>>(),
        [("proof-handoff-example", false), ("workspace-method", true)]
    );
    let offer = &candidates[0];
    crate::rules::install_handoff_skill(&project, &offer.name, Path::new(&offer.path))?;
    assert!(skill_offers(&project, &homes, procedures)?
        .iter()
        .all(|s| s.enabled));
    assert!(
        crate::agents::load(&project, &crate::rules::resolve(&project)?)?
            .contains_key("proof-handoff-example")
    );
    Ok(())
}

#[test]
fn discovers_legacy_notes_alongside_new_writes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().canonicalize()?;
    let legacy = ".labo/handoff/legacy.md";
    fs::create_dir_all(project.join(".labo/handoff"))?;
    fs::write(project.join(legacy), NOTE)?;
    assert_eq!(discover(&project)[0].path, legacy);
    assert!(read(&project, legacy).is_ok());
    fs::write(create_note_dir(&project)?.join("current.md"), NOTE)?;
    assert_eq!(discover(&project).len(), 2);
    fs::write(project.join(".fuzukue/handoff/legacy.md"), NOTE)?;
    assert_eq!(discover(&project).len(), 2);
    assert!(discover(&project).iter().all(|note| note.path.starts_with(DIR)));
    Ok(())
}
