use super::*;
use crate::{test_support::TestDir, Hub};

const DECK: &str = "papers/levy-families";
const CARD: &str = "ex-hamming-diameter";
const CARD_FILE: &str = "papers/levy-families/cards/ex/ex-hamming-diameter/card.md";
const LEAN_FILE: &str = "lean/LabSample/LabSample/Hamming.lean";

fn git(dir: &Path, args: &[&str]) {
    let result = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn init(dir: &Path, repo: &str) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(
        dir,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repo}.git"),
        ],
    );
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "Fixture",
        ],
    );
}

#[cfg(unix)]
fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn workshop_shot_auto_configures_the_server_without_flags() -> Result<()> {
    let root = TestDir::new();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let home = root.path().join("home");
    let project = home.join("project");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&project)?;
    std::fs::create_dir(&bin)?;
    // Exercise the runner's actual preparation without invoking Git writes or opening a socket.
    let fake_bin = root.path().join("fake-bin");
    root.write(
        "fake-bin/git",
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$2/fixture-git.log\"\n",
    );
    executable(&fake_bin.join("git"));
    let path = std::env::join_paths(std::iter::once(fake_bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    let setup = tokio::process::Command::new("bash")
        .arg("-ec")
        .arg(
            r#"
source "$1"
prepare_shot_git_fixture "$2" "$3" "$4" false false
"#,
        )
        .arg("shot-fixture")
        .arg(repo.join("scripts/shot-fixture.sh"))
        .arg(root.path())
        .arg(&repo)
        .arg(repo.join("tests/shots/workshop.json"))
        .env("PATH", path)
        .output()
        .await?;
    ensure!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let git_commands = std::fs::read_to_string(project.join("fixture-git.log"))?;
    assert!(git_commands.contains("init -q -b main"));
    assert!(git_commands.contains("remote add origin https://github.com/example-owner/lab.git"));

    init(&project, "example-owner/lab");
    let mut registry = App::new(
        home,
        project.clone(),
        root.path().join("state"),
        "".into(),
        None,
    )?;
    Arc::get_mut(&mut registry).unwrap().workshop.gh_bin_dir = Some(bin);
    let workspace = registry.select(None)?;
    assert_eq!(workspace.project_dir, project);
    let mut snapshots = workspace.research_issues.subscribe(&registry, &workspace);
    tokio::time::timeout(Duration::from_secs(5), snapshots.changed()).await??;
    let snapshot = snapshots.borrow_and_update();
    let Some(ServerEvent::ResearchIssues {
        repo,
        issues,
        error,
    }) = snapshot.as_ref()
    else {
        anyhow::bail!("research issue snapshot is missing");
    };
    assert_eq!(repo, "example-owner/lab");
    assert_eq!(error, &None);
    assert!(issues
        .iter()
        .any(|issue| issue.number == 12 && issue.state == State::Ready));
    Ok(())
}

async fn next_approval(
    events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>,
) -> (String, String) {
    let (request_id, summary) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::ApprovalRequested {
                request_id,
                summary,
                ..
            } = events.recv().await.unwrap().event
            {
                break (request_id, summary);
            }
        }
    })
    .await
    .expect("approval was requested");
    assert!(request_id.starts_with(fuzukue_proto::RESEARCH_ISSUE_APPROVAL));
    (request_id, summary)
}

async fn resolve_approval(
    caller: &Workspace,
    events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>,
    decision: &str,
    inspect: impl FnOnce(&str),
) {
    let (request_id, summary) = next_approval(events).await;
    assert!(caller
        .delegates
        .resolve_approval(&request_id, "accept", true)
        .unwrap());
    inspect(&summary);
    crate::resolve_approval(caller, request_id, decision.into(), false)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sample_workshop_creates_starts_and_accepts_across_repositories() -> Result<()> {
    sample_workshop(false).await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn forced_workshop_acceptance_leaves_a_reviewable_partial_result() -> Result<()> {
    sample_workshop(true).await
}

#[cfg(unix)]
async fn sample_workshop(force: bool) -> Result<()> {
    use fuzukue_proto::{DrainProgress, SelfUpdateWaitAction};

    let root = TestDir::new();
    let lab = root.path().join("lab");
    let paper = root.path().join("paper-checkout");
    let state = root.path().join("state");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/lab");
    for entry in walkdir::WalkDir::new(&fixture)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".lake" | "target")
            ) && !entry.file_type().is_symlink()
        })
    {
        let entry = entry?;
        let target = lab.join(entry.path().strip_prefix(&fixture)?);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    let lab_card = std::fs::read_to_string(lab.join(CARD_FILE))?;
    root.write(
        "paper-checkout/deck.toml",
        "[deck]\ntitle='別リポジトリの原稿'\n",
    );
    let paper_card = "---\nid: statement\nkind: lemma\ntitle: 別リポジトリの補題\nissues: [old#3]\n---\n## Statement\n元の主張。\n";
    root.write("paper-checkout/cards/statement.md", paper_card);
    init(&lab, "example-owner/lab");
    init(&paper, "test/sample-paper");
    root.write("lab/.git/workshop-fixture.json", "[]");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/gh"),
        bin.join("gh-fixture"),
    )?;
    executable(&bin.join("gh-fixture"));
    root.write("bin/gh", r#"#!/bin/sh
if [ "$1" = issue ] && [ "$2" = comment ]; then
    /usr/bin/touch .git/accept-entered
    while [ ! -f .git/accept-release ]; do /bin/sleep 0.02; done
fi
exec "${0%/*}/gh-fixture" "$@"
"#);
    executable(&bin.join("gh"));
    let stub = bin.join("claude");
    root.write(
        "bin/claude",
        r#"#!/usr/bin/python3
from pathlib import Path
import json
import sys

parts = json.loads(input())["message"]["content"]
prompt = "\n".join(part["text"] for part in parts if part["type"] == "text")
if "English work report below" in prompt or "以下の英語の作業報告" in prompt:
    assert "lake build: ok" in prompt and "Quot.sound" in prompt
    print(json.dumps({"type": "assistant", "message": {"id": "workshop-summary", "content": [
        {"type": "text", "text": "下書きを作って、ビルドと公理を確かめたよ〜。"}
    ]}}))
    print(json.dumps({"type": "result", "is_error": False}))
    sys.exit(0)
assert "別リポジトリの補題" in prompt
assert "Hamming 立方体の直径" in prompt
assert Path.cwd().name == "lab-issue-12"
notes = Path("notes/workshop/issue-12")
notes.mkdir(parents=True, exist_ok=True)
(notes / "1.md").write_text("検証済みの下書き。\n")
(notes / "2.md").write_text("研究カード用の下書き。\n")
lean = Path("lean/LabSample/LabSample/Hamming.lean")
lean.write_text(lean.read_text() + "\n-- Verified in the workshop fixture.\n")
print(json.dumps({"type": "result", "result": """Draft: notes/workshop/issue-12/1.md
lake build: ok
'LabSample.Hamming.hamming_le_length' depends on axioms: [propext, Quot.sound]
'LabSample.Hamming.hamming_map_not' does not depend on any axioms
'LabSample.Hamming.hamming_triangle' depends on axioms: [Classical.choice]"""}))
"#,
    );
    executable(&stub);
    let mut registry = App::new(
        root.path().into(),
        lab.clone(),
        state.clone(),
        "".into(),
        None,
    )?;
    Arc::get_mut(&mut registry).unwrap().workshop.gh_bin_dir = Some(bin);
    let update = Arc::new(crate::self_update::Update::shutdown_only(&lab, &state));
    assert!(registry.self_update.set(update.clone()).is_ok());
    assert!(registry.test_claude_program.set(stub).is_ok());
    let paper_id = registry.register(paper.to_str().unwrap())?;
    let caller = registry.select(None)?;
    let (_, mut events) = caller.hub.subscribe(0);
    let args = json!({
        "title": "Hamming 距離の上界",
        "cards": ["sample-paper:.#statement", format!("example-owner/lab:{DECK}#{CARD}")],
        "kind": "lean", "assignee": CLAUDE_OPUS,
        "acceptance": "lake build と公理を確認する"
    });
    let send = || mcp::Tool::Create.call(&caller, "", Actor::System, &args);
    let (denied, ()) = tokio::join!(
        send(),
        resolve_approval(&caller, &mut events, "decline", |summary| {
            assert!(summary.contains("example-owner/lab"));
        })
    );
    assert!(denied.is_err());
    assert!(list(&registry, &caller, None, None, false).await?.is_empty());
    assert_eq!(
        std::fs::read_to_string(paper.join("cards/statement.md"))?,
        paper_card
    );
    let (created, ()) = tokio::join!(
        send(),
        resolve_approval(&caller, &mut events, "accept", |_| {})
    );
    assert_eq!(created?["number"], 12);
    let result = mcp::Tool::List
        .call(
            &caller,
            "",
            Actor::System,
            &json!({"state":"ready","kind":"lean"}),
        )
        .await?;
    assert_eq!(result["issues"][0]["assignee"], CLAUDE_OPUS);
    assert_eq!(
        result["issues"][0]["card_workspaces"],
        json!([paper_id, caller.id])
    );
    let linked = std::fs::read_to_string(paper.join("cards/statement.md"))?;
    let card = fuzukue_cards::parse_card(&paper, &paper.join("cards/statement.md"), &linked)?;
    assert_eq!(card.issues, ["old#3", "lab#12"]);
    assert_eq!(
        fuzukue_cards::split_frontmatter(&linked).unwrap().1,
        fuzukue_cards::split_frontmatter(paper_card).unwrap().1
    );

    let args = json!({"number":12});
    let started = mcp::Tool::Start
        .call(&caller, "", Actor::System, &args)
        .await?;
    let session = started["session"].as_str().unwrap();
    assert_eq!(
        caller.hub.thread_attachments(session),
        [&format!("{paper_id}:."), DECK]
    );
    assert!(mcp::Tool::Start
        .call(&caller, "", Actor::System, &args)
        .await
        .is_err());
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let env = events.recv().await.unwrap();
            if let ServerEvent::TextDelta {
                text,
                actor: Actor::System,
                ..
            } = env.event
            {
                assert!(!text.contains("未完"), "{text}");
                break;
            }
        }
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if matches!(events.recv().await.unwrap().event,
                ServerEvent::TextDelta { actor: Actor::Claude, text, .. }
                    if text == "下書きを作って、ビルドと公理を確かめたよ〜。"
            ) {
                break;
            }
        }
    }).await?;
    let reloaded = Workspace::new(
        lab.clone(),
        "".into(),
        state.join("reloaded"),
        Arc::new(Hub::new()),
    );
    let (history, _) = reloaded.hub.subscribe_client(0, true);
    assert!(history.iter().any(
        |env| matches!(&env.event, ServerEvent::TextDelta { item, text, .. }
        if item.starts_with(REPORT_ITEM) && text.contains("Quot.sound"))
    ));
    let issues = list(&registry, &caller, Some(State::NeedsReview), None, false).await?;
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].workspace.as_deref(), Some(caller.id.as_str()));
    assert_eq!(
        std::fs::read_to_string(paper.join("cards/statement.md"))?,
        linked
    );
    assert!(!std::fs::read_to_string(lab.join(LEAN_FILE))?.contains("workshop fixture"));

    let send = || mcp::Tool::Accept.call(&caller, session, Actor::System, &args);
    let (denied, ()) = tokio::join!(
        send(),
        resolve_approval(&caller, &mut events, "decline", |summary| {
            assert!(summary.contains("取り込み"));
            assert!(summary.contains("Quot.sound"));
        })
    );
    assert!(denied.is_err());
    assert_eq!(
        std::fs::read_to_string(paper.join("cards/statement.md"))?,
        linked
    );
    assert_eq!(
        list(&registry, &caller, Some(State::NeedsReview), None, false).await?.len(),
        1
    );

    let desktop_accept = || {
        let (registry, caller) = (registry.clone(), caller.clone());
        tokio::spawn(async move { accept(&registry, &caller, "", Actor::System, 12).await })
    };
    let accepted = desktop_accept();
    let (request_id, _) = next_approval(&mut events).await;
    let late = desktop_accept();
    let (late_id, _) = next_approval(&mut events).await;
    crate::resolve_approval(&caller, request_id, "accept".into(), false).await?;
    crate::test_support::wait_for_path(&lab.join(".git/accept-entered")).await?;
    assert!(caller.research_issues.acceptance_marker(12).exists());
    assert!(std::fs::read_to_string(lab.join(LEAN_FILE))?.contains("workshop fixture"));

    let mut status = update.subscribe_drain();
    let (wait, ready) = update.test_update_drain(registry.clone())?;
    tokio::time::timeout(Duration::from_secs(5), status.wait_for(|status| {
        status.as_ref().is_some_and(|s| s.progress == DrainProgress::Draining && s.blocking.gate_held
            && s.blocking.blockers.iter().any(|blocker| blocker.id == late_id))
    })).await??;
    crate::resolve_approval(&caller, late_id.clone(), "accept".into(), false).await?;
    assert!(tokio::time::timeout(Duration::from_secs(5), late).await??.unwrap_err().to_string().contains("progress"));
    tokio::time::timeout(Duration::from_secs(5), status.wait_for(|status| {
        status.as_ref().is_some_and(|s| s.progress == DrainProgress::Draining && s.blocking.gate_held
            && !s.blocking.blockers.iter().any(|blocker| blocker.id == late_id))
    })).await??;
    assert!(!ready.is_finished());
    if force {
        update.control_wait(wait, SelfUpdateWaitAction::Force)?;
    } else {
        root.write("lab/.git/accept-release", "");
    }
    tokio::time::timeout(Duration::from_secs(5), ready).await???;
    let accepted = accepted.await?;
    if force {
        assert!(accepted.unwrap_err().to_string().contains("needs review"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await?.event, ServerEvent::Error { message } if message.contains("needs review")) {
                    return anyhow::Ok(());
                }
            }
        }).await??;
        let mut reloaded = Workspace::new(lab.clone(), "".into(), state, Arc::new(Hub::new()));
        reloaded.registry = Arc::downgrade(&registry);
        let reloaded = Arc::new(reloaded);
        let issues = list(&registry, &reloaded, Some(State::NeedsReview), None, false).await?;
        assert!(issues[0].summary.as_deref().unwrap().contains("コメントとクローズは未確認"));
        let error = start(&registry, &reloaded, "", Actor::System, 12).await.unwrap_err();
        assert!(error.to_string().contains("取り込みは確認が必要"));
        let error = accept(&registry, &reloaded, "", Actor::System, 12).await.unwrap_err();
        assert!(error.to_string().contains("取り込みは確認が必要"));
        let data: Vec<Value> = serde_json::from_slice(&std::fs::read(lab.join(".git/workshop-fixture.json"))?)?;
        assert_eq!(data[0]["state"], "OPEN");
        assert!(data[0]["comments"].as_array().unwrap().is_empty());
        return Ok(());
    }
    accepted?;
    assert!(!caller.research_issues.acceptance_marker(12).exists());
    assert_eq!(list(&registry, &caller, Some(State::Done), None, false).await?.len(), 1);
    let card = std::fs::read_to_string(lab.join(CARD_FILE))?;
    assert!(card.starts_with(lab_card.split("lean:").next().unwrap()));
    assert!(card.contains("研究カード用の下書き。"));
    assert_eq!(card.matches("## Notes").count(), 1);
    let paper = std::fs::read_to_string(paper.join("cards/statement.md"))?;
    assert!(paper.contains("## Statement\n元の主張。\n\n## Notes"));
    assert_eq!(paper.matches("検証済みの下書き。").count(), 1);
    assert!(std::fs::read_to_string(lab.join(LEAN_FILE))?.contains("workshop fixture"));
    let data: Vec<Value> =
        serde_json::from_slice(&std::fs::read(lab.join(".git/workshop-fixture.json"))?)?;
    assert_eq!(data[0]["comments"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn lean_report_checks_the_requested_target_build_and_axioms() {
    let target = vec!["Lab.test".into()];
    let build = "lake build: ok\n";
    for tail in [
        "does not depend on any axioms",
        "depends on axioms: [propext, Classical.choice, Quot.sound]",
    ] {
        assert!(validate_report(Kind::Lean, &format!("{build}'Lab.test' {tail}"), &target).is_ok());
    }
    for tail in [
        "depends on axioms: [sorryAx]",
        "depends on axioms: [untrusted]",
    ] {
        assert!(
            validate_report(Kind::Lean, &format!("{build}'Lab.test' {tail}"), &target).is_err()
        );
    }
    assert!(validate_report(
        Kind::Lean,
        &format!("{build}'Lab.other' does not depend on any axioms"),
        &target
    )
    .is_err());
    assert!(validate_report(
        Kind::Lean,
        "'Lab.test' does not depend on any axioms",
        &target
    )
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn research_queues_isolate_repositories_and_issue_numbers() -> Result<()> {
    let root = TestDir::new();
    let lab = root.path().join("lab");
    let labo = root.path().join("labo");
    let state = root.path().join("state");
    root.write("lab/workspace", "");
    root.write("lab/subfolder/marker", "");
    root.write("labo/fuzukue.toml", "[research]\nrepo = 'owner/labo'");
    // Legacy server-wide settings and state may come from the previous default project.
    root.write("state/workspaces.toml", "[research]\nworkspace = '~/lab'\nrepo = 'owner/lab'");
    root.write("state/research-issues/jobs.json", "{}");
    init(&lab, "owner/lab");
    init(&labo, "owner/other");
    assert!(resolve_repo(&lab.join("subfolder")).is_err());
    let bin = root.path().join("bin");
    root.write("bin/gh", r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
name = Path.cwd().name
repo = f'owner/{name}'
args = sys.argv[1:]
comment = dict(body=name, user=dict(login='fixture'))
issue = dict(number=1, title=name, state='open', labels=[],
    html_url=f'https://github.com/{repo}/issues/1',
    body=f'---\nkind: other\ncards: [{name}:.#card]\n---\n{repo}',
    comments=1)
if args[:2] == ['api', '-i']:
    request = args[args.index('--hostname') + 2]
    assert request.startswith(f'repos/{repo}/issues'), args
    print('HTTP/2 200 OK\n')
    print(json.dumps([comment] if 'comments?' in request else [issue]))
else:
    assert args[:2] == ['issue', 'list'] and args[args.index('--repo') + 1] == repo, args
    print(json.dumps([dict(issue, comments=[comment])]))
"#);
    executable(&bin.join("gh"));
    let mut registry = App::new(root.path().into(), labo.clone(), state, "".into(), None)?;
    Arc::get_mut(&mut registry).unwrap().workshop.gh_bin_dir = Some(bin);
    let lab_id = registry.register(lab.to_str().unwrap())?;
    let lab = registry.select(Some(&lab_id))?;
    let labo = registry.select(None)?;

    {
        let mut jobs = lab.research_issues.jobs.lock().await;
        jobs.insert(1, Job {
            state: State::NeedsReview, model: "fixture".into(), workspace: lab_id,
            session: "lab-thread".into(), base: String::new(),
            metadata: Metadata { cards: Vec::new(), kind: Kind::Other, assignee: None, producer: None },
            acceptance: String::new(), summary: Some("lab report".into()),
        });
        lab.research_issues.save(&jobs)?;
    }
    let mut lab_updates = lab.research_issues.subscribe(&registry, &lab);
    let mut labo_updates = labo.research_issues.subscribe(&registry, &labo);
    tokio::time::timeout(Duration::from_secs(5), async {
        lab_updates.changed().await?;
        labo_updates.changed().await
    }).await??;
    for (workspace, update, repo, session) in [
        (&lab, lab_updates, "owner/lab", Some("lab-thread")),
        (&labo, labo_updates, "owner/labo", None),
    ] {
        let result = mcp::Tool::List.call(workspace, "", Actor::System, &json!({})).await?;
        assert_eq!(result["repo"], repo);
        assert_eq!(result["issues"][0]["session"].as_str(), session);
        let update = update.borrow();
        let Some(ServerEvent::ResearchIssues { repo: snapshot_repo, issues, error: None }) = update.as_ref() else {
            anyhow::bail!("missing snapshot: {update:?}");
        };
        assert_eq!(snapshot_repo, repo);
        assert_eq!(issues[0].session.as_deref(), session);
        let details = issues[0].details.as_ref().unwrap();
        assert!(details.body.ends_with(repo));
        assert_eq!(details.comments[0].author.as_deref(), Some("fixture"));
        let event = update.as_ref().unwrap().clone();
        for protocol in [fuzukue_proto::CLOSE_TRASH_PROTOCOL, fuzukue_proto::RESEARCH_ISSUE_DETAILS_PROTOCOL, fuzukue_proto::PROTOCOL_VERSION] {
            let wire = crate::wire_event(&crate::snapshot(event.clone()), protocol, None, fuzukue_i18n::Locale::JAPANESE);
            let json: Value = serde_json::from_str(wire.to_text()?)?;
            assert_eq!(json["event"]["issues"][0].get("details").is_some(), protocol >= fuzukue_proto::RESEARCH_ISSUE_DETAILS_PROTOCOL);
        }
    }
    Ok(())
}
