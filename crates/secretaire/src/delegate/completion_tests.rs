//! How a Claude worker's report ends the job, and what the report lists.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

const PROSE: &str = r#"printf '%s\n' '{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}'
printf '%s\n' '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Fixture report"}]}}'
"#;
const END_TURN: &str = r#"printf '%s\n' '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_stop"}}'
"#;
const RESULT: &str = r#"printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Fixture report"}'
"#;

/// A background command that is still running.
const BACKGROUND: &str = r#"printf '%s\n' '{"type":"assistant","message":{"id":"m0","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 60","run_in_background":true}}]}}'
printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Command running in background with ID: b1."}]}}'
"#;

fn changed_line(report: &str) -> Result<&str> {
    report.lines().find(|line| line.starts_with("Changed files (in progress): ")).context("missing changed files")
}

struct Fixture {
    directory: crate::test_support::TestDir,
    app: Arc<App>,
    session: String,
}

impl Fixture {
    /// A fake Claude CLI that reads the prompt, runs `body` and then lingers.
    fn new(body: &str) -> Result<Self> {
        let directory = crate::test_support::TestDir::new();
        directory.write("checkout/.keep", "");
        let script = format!("#!/bin/sh\nread -r prompt\n{body}exec sleep 600\n");
        directory.write("claude", &script);
        let program = directory.path().join("claude");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        let checkout = directory.path().join("checkout").canonicalize()?;
        let mut app = App::new(checkout, String::new(), directory.path().join("state"), Arc::new(crate::Hub::new()));
        app.claude_program = Some(program);
        let app = Arc::new(app);
        let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Completion fixture".into())))?;
        Ok(Self { directory, app, session })
    }

    async fn launch(&self, touches: &[&str]) -> Result<(String, tokio::task::JoinHandle<Result<String>>)> {
        self.launch_with("Synthetic assignment", touches).await
    }

    async fn launch_with(&self, prompt: &str, touches: &[&str]) -> Result<(String, tokio::task::JoinHandle<Result<String>>)> {
        let selection = AgentSelection { backend: AgentBackend::Claude, model: None, effort: None };
        launch(
            &self.app, &self.session, prompt.into(), None, Some(self.app.project_dir.clone()), None,
            Some(selection), None, None, Default::default(), Some(touches.iter().map(|path| (*path).into()).collect()),
        ).await
    }
}

async fn wait_for_prose(events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await?.event {
                ServerEvent::TextDelta { text, .. } if text == "Fixture report" => return Ok(()),
                _ => {}
            }
        }
    }).await?
}

async fn finish(run: tokio::task::JoinHandle<Result<String>>) -> Result<Result<String>> {
    Ok(tokio::time::timeout(Duration::from_secs(30), run).await??)
}

#[tokio::test]
async fn one_result_for_a_prompt_sent_mid_turn_completes_the_job() -> Result<()> {
    // The CLI answers the steering message inside the running turn.
    let fixture = Fixture::new(&format!("{PROSE}read -r steer\n{RESULT}"))?;
    let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
    let (job, run) = fixture.launch(&[]).await?;
    wait_for_prose(&mut events).await?;
    message_worker(&fixture.app, &fixture.session, &job, "Include the verification", true)?;
    assert!(finish(run).await??.starts_with("Fixture report"));
    assert_eq!(fixture.app.delegates.job_status(&fixture.app, &job, jobs::View::Full)?["state"], "finished");
    Ok(())
}

#[tokio::test]
async fn a_queued_prompt_that_starts_a_turn_replaces_the_held_report() -> Result<()> {
    for frame in [
        r#"{"type":"system","subtype":"init"}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        r#"{"type":"system","subtype":"api_retry"}"#,
    ] {
        let second = format!(r#"sleep 1
printf '%s\n' '{frame}'
sleep 3
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"Second report"}}'
"#);
        let fixture = Fixture::new(&format!("{PROSE}read -r steer\n{RESULT}{second}"))?;
        let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
        let (job, run) = fixture.launch(&[]).await?;
        wait_for_prose(&mut events).await?;
        message_worker(&fixture.app, &fixture.session, &job, "Check once more", true)?;
        assert!(finish(run).await??.starts_with("Second report"), "{frame}");
    }
    Ok(())
}

#[tokio::test]
async fn one_result_with_a_background_command_running_completes_the_job() -> Result<()> {
    // A notification after the result does not start another turn by itself.
    let notice = r#"printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"b2","status":"completed"}'
printf '%s\n' '{"type":"system","subtype":"status","status":null}'
"#;
    let fixture = Fixture::new(&format!("{BACKGROUND}{PROSE}read -r steer\n{RESULT}{notice}"))?;
    let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
    let (job, run) = fixture.launch(&[]).await?;
    wait_for_prose(&mut events).await?;
    message_worker(&fixture.app, &fixture.session, &job, "Include the verification", true)?;
    assert!(finish(run).await??.starts_with("Fixture report"));
    assert_eq!(fixture.app.delegates.job_status(&fixture.app, &job, jobs::View::Full)?["state"], "finished");
    Ok(())
}

#[tokio::test]
async fn late_subagent_frames_keep_the_held_report() -> Result<()> {
    let late = r#"sleep 1
printf '%s\n' '{"type":"assistant","parent_tool_use_id":"t9","message":{"id":"s1","content":[{"type":"text","text":"Subagent note"}]}}'
printf '%s\n' '{"type":"user","parent_tool_use_id":"t9","message":{"content":[]}}'
"#;
    let fixture = Fixture::new(&format!("{PROSE}read -r steer\n{RESULT}{late}"))?;
    let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
    let (job, run) = fixture.launch(&[]).await?;
    wait_for_prose(&mut events).await?;
    message_worker(&fixture.app, &fixture.session, &job, "Include the verification", true)?;
    assert!(finish(run).await??.starts_with("Fixture report"));
    Ok(())
}

#[tokio::test]
async fn silence_without_a_result_frame_keeps_the_job_running() -> Result<()> {
    // Progress prose alone, and a turn that ends without a result.
    for body in [PROSE.to_owned(), format!("{PROSE}{END_TURN}")] {
        let fixture = Fixture::new(&body)?;
        let (job, mut run) = fixture.launch(&[]).await?;
        assert!(tokio::time::timeout(Duration::from_secs(5), &mut run).await.is_err());
        fixture.app.delegates.cancel_job(&job, "fixture stop")?;
        assert!(finish(run).await?.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn a_stalled_job_reports_the_last_message_as_possibly_unfinished() -> Result<()> {
    let fixture = Fixture::new("")?;
    let gate = fixture.directory.path().join("go");
    let script = format!(
        "#!/bin/sh\nread -r prompt\n{PROSE}while [ ! -e '{}' ]; do sleep 0.05; done\n{RESULT}exec sleep 600\n",
        gate.display(),
    );
    fixture.directory.write("claude", &script);
    fixture.app.agent_settings.lock().unwrap().tiers.max_escalations = 0;
    let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
    let (job, run) = fixture.launch(&[]).await?;
    wait_for_prose(&mut events).await?;
    // Stand in for the five-minute watchdog.
    *fixture.app.delegates.running.lock().unwrap()[&job].retry.lock().unwrap() =
        Some(crate::tiers::Retry::Stalled("fixture silence".into()));
    fs::write(gate, "")?;
    assert!(finish(run).await?.is_err());
    let status = fixture.app.delegates.job_status(&fixture.app, &job, jobs::View::Report)?;
    assert_eq!(status["state"], "failed");
    assert_eq!(status["error"], true);
    let report = status["final_report"].as_str().unwrap();
    assert!(report.contains("fixture silence"), "{report}");
    assert!(report.contains(harness_text!("possibly unfinished", "未完了の可能性")), "{report}");
    assert!(report.ends_with("Fixture report"), "{report}");
    Ok(())
}

#[tokio::test]
async fn shell_writes_and_deletions_outside_the_declared_set_stay_visible() -> Result<()> {
    // Outside a Git repository, nothing is known to be ignored, so even build
    // output is listed. A declaration alone does not confirm who wrote a path.
    let writes = "echo other > outside.txt\nrm victim.txt\nmkdir target\necho built > target/artifact\necho mine > declared.txt\n";
    for touches in [&["declared.txt"][..], &[][..]] {
        let fixture = Fixture::new(&format!("{writes}{PROSE}{RESULT}"))?;
        fixture.directory.write("checkout/victim.txt", "keep me\n");
        let (_, run) = fixture.launch(touches).await?;
        let report = finish(run).await??;
        let entry = |path: &str, action: &str| report.lines().any(|line| {
            line.starts_with(&format!("- \"{path}\" — {action}")) && line.ends_with("(writer unconfirmed)")
        });
        assert!(report.contains("## Out-of-scope writes"), "{report}");
        assert!(entry("outside.txt", "copied to quarantine"), "{report}");
        assert!(entry("victim.txt", "missing after the job"), "{report}");
        assert!(entry("target/artifact", "copied to quarantine"), "{report}");
        assert_eq!(entry("declared.txt", "copied to quarantine"), touches.is_empty(), "{report}");
        assert!(report.contains("manifest.json"), "{report}");
        let changed = changed_line(&report)?;
        for path in ["outside.txt", "victim.txt", "target/artifact", "declared.txt"] {
            assert!(changed.contains(&format!("{path} (writer unconfirmed)")), "{report}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_deletion_under_another_jobs_declaration_stays_listed() -> Result<()> {
    for hands_off in [false, true] {
        // The first job declares everything and waits. The second one deletes a
        // file in a directory named like build output.
        let fixture = Fixture::new("")?;
        if hands_off {
            fixture.app.hub.set_full_approval(
                fuzukue_proto::FullApprovalChange::Full { duration: fuzukue_proto::FullApprovalDuration::UntilDisabled },
                &crate::remote::SettingsPrincipal::fixture(),
            )?;
        }
        let gate = fixture.directory.path().join("go");
        let script = format!(
            "#!/bin/sh\nread -r prompt\ncase \"$prompt\" in\n*hold-open*) {PROSE}while [ ! -e '{}' ]; do sleep 0.05; done ;;\n\
             *) rm src/build/x\n{PROSE};;\nesac\n{RESULT}exec sleep 600\n",
            gate.display(),
        );
        fixture.directory.write("claude", &script);
        fixture.directory.write("checkout/src/build/x", "keep me\n");
        let (_, mut events) = fixture.app.hub.subscribe(u64::MAX);
        let (_, holder) = fixture.launch_with("hold-open", &["**"]).await?;
        wait_for_prose(&mut events).await?;
        let (_, run) = fixture.launch(&[]).await?;
        let report = finish(run).await??;
        let label = "declared or written by another job";
        assert!(changed_line(&report)?.contains(&format!("src/build/x ({label})")), "{report}");
        // Full approval keeps no quarantine record.
        assert_eq!(report.contains("## Out-of-scope writes"), !hands_off, "{report}");
        assert_eq!(report.lines().any(|line| {
            line.starts_with("- \"src/build/x\" — missing after the job") && line.ends_with(&format!("({label})"))
        }), !hands_off, "{report}");
        fs::write(gate, "")?;
        finish(holder).await??;
    }
    Ok(())
}

#[tokio::test]
async fn only_untracked_ignored_files_under_target_are_counted() -> Result<()> {
    let writes = "rm target/old\necho new > target/tracked\necho a > target/other/a\necho b > target/other/b\necho c > notes.txt\n";
    let fixture = Fixture::new(&format!("{writes}{PROSE}{RESULT}"))?;
    for (path, text) in [(".gitignore", "target/\n"), ("target/tracked", "old\n"), ("target/old", "old\n"),
        ("target/other/.keep", "")] {
        fixture.directory.write(&format!("checkout/{path}"), text);
    }
    for args in [&["init", "-q"][..], &["add", "-f", ".gitignore", "target/tracked"]] {
        let status = std::process::Command::new("git").args(args)
            .current_dir(fixture.directory.path().join("checkout")).status()?;
        assert!(status.success());
    }
    let (_, run) = fixture.launch(&[]).await?;
    let report = finish(run).await??;
    let changed = changed_line(&report)?;
    for path in ["notes.txt", "target/old", "target/tracked"] {
        assert!(changed.contains(path), "{report}");
        assert!(report.contains(&format!("- \"{path}\" — ")), "{report}");
    }
    let summary = "target/other/: 2 untracked ignored files (writer unconfirmed)";
    assert!(changed.contains(summary), "{report}");
    assert!(report.contains(&format!("- {summary}")), "{report}");
    assert!(!report.contains("target/other/a") && !report.contains("target/other/b"), "{report}");
    Ok(())
}
