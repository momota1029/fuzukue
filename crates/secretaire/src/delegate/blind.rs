//! Adapt generic blind delegation to the one-shot model runner.

use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{AgentBackend, AgentCaller, AgentSelection};
use serde::{Deserialize, Serialize};

use crate::{
    agent_usage::RunMetadata,
    cancellation::{Cancellation, Cancelled},
    isolated_model::{self, Connection, FailureStage, Input},
    Workspace,
};

pub(crate) use isolated_model::Material as Artifact;

pub(super) const ROLE: &str = "blind-review";

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Isolation {
    Blind,
}

pub(crate) async fn start(
    app: &Arc<Workspace>,
    session: &str,
    prompt: String,
    title: Option<String>,
    label: Option<String>,
    backend: AgentBackend,
    model: String,
    artifacts: Vec<Artifact>,
) -> Result<String> {
    ensure!(!prompt.trim().is_empty(), "Blind review requires a prompt");
    ensure!(
        !model.trim().is_empty(),
        "Blind review requires a provider model ID"
    );
    Connection::open(app, backend)?;
    let message = review_message(&prompt, &artifacts)?;
    let input = Input::legacy(vec![prompt], artifacts, message.clone());
    super::start_assignment(
        app,
        session,
        super::Assignment {
            prompt: message,
            title,
            label,
            worktree: None,
            agent: None,
            adopted: None,
            selection: Some(AgentSelection {
                backend,
                model: Some(model),
                effort: None,
            }),
            caller: Some(AgentCaller::FrontDesk),
            routing: Default::default(),
            touches: Some(Vec::new()),
            issue: None,
            isolation: Some(Isolation::Blind),
            blind_input: Some(input),
            folder_grants: Vec::new(),
            exec_trusted: false,
        },
    )
    .await
}

fn review_message(prompt: &str, artifacts: &[Artifact]) -> Result<String> {
    let mut message = prompt.to_owned();
    for artifact in artifacts {
        ensure!(
            !artifact.label.trim().is_empty() && !artifact.label.contains(['\r', '\n']),
            "Blind review artifact labels must be nonempty single lines"
        );
        message.push_str("\n\n[");
        message.push_str(&artifact.label);
        message.push_str("]\n");
        message.push_str(&artifact.text);
    }
    Ok(message)
}

pub(super) async fn run(
    app: &Workspace,
    job: &str,
    selection: &AgentSelection,
    message: String,
    input: Option<Input>,
    cancel: &Arc<Cancellation>,
    metadata: &mut RunMetadata,
) -> Result<String> {
    cancel.check()?;
    let model = selection
        .model
        .as_deref()
        .context("Blind review requires a model")?;
    let client = Connection::open(app, selection.backend)?;
    // Jobs queued by an older server have only their original assembled prompt.
    let input = input.unwrap_or_else(|| Input::legacy(vec![message.clone()], Vec::new(), message));
    let receipt =
        isolated_model::run(app, job.into(), client, model, input, cancel, || Ok(())).await?;
    let outcome = receipt.outcome.expect("finished blind run");
    metadata.model = outcome.resolved_model;
    metadata.tokens = outcome.total_tokens;
    if let Some(failure) = outcome.failure {
        if matches!(failure.stage, FailureStage::Interrupted) {
            return Err(Cancelled.into());
        }
        anyhow::bail!("{}", failure.reason);
    }
    cancel.check()?;
    outcome.text.context("Blind review returned no text")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn queued_blind_job_has_no_mcp_tools_or_steering_and_records_isolation() -> Result<()> {
        let project = tempfile::tempdir()?;
        let app = Arc::new(Workspace::new(
            project.path().canonicalize()?,
            "127.0.0.1:7878".into(),
            project.path().join("state"),
            Arc::new(crate::Hub::new()),
        ));
        let session = app.hub.edit_threads(|store| store.create(None))?;
        let _hold = super::super::hold_all_slots(&app).await;
        let (job, task) = super::super::launch_assignment(
            &app,
            &session,
            "delegate:blind-fixture".into(),
            super::super::Assignment {
                prompt: "Read AGENTS.md".into(),
                title: None,
                label: None,
                worktree: None,
                agent: None,
                adopted: None,
                selection: Some(AgentSelection {
                    backend: AgentBackend::Codex,
                    model: Some("fixture".into()),
                    effort: None,
                }),
                caller: Some(AgentCaller::FrontDesk),
                routing: Default::default(),
                touches: Some(Vec::new()),
                issue: None,
                isolation: Some(Isolation::Blind),
                blind_input: None,
                folder_grants: Vec::new(),
                exec_trusted: false,
            },
            false,
        )
        .await?;
        let running = app.delegates.running.lock().unwrap()[&job].clone();
        assert!(running.key.is_empty() && running.tools.lock().unwrap().is_empty());
        assert_eq!(
            app.delegates.worker_tools(&session, &job, &running.key),
            None
        );
        assert!(running.input.lock().unwrap().is_none());
        assert!(running.capture_scope.lock().unwrap().is_none());
        let status = app.delegates.job_status(&app, &job, Default::default())?;
        assert_eq!(status["isolation"], "blind");
        assert_eq!(status["backend"], "codex");
        assert_eq!(status["model"], "fixture");
        assert!(status.get("worktree").is_none());
        app.delegates.interrupt_job(&session, &job);
        assert!(task.await?.is_err());
        Ok(())
    }

    #[test]
    fn artifacts_are_inline_and_preserve_whitespace() -> Result<()> {
        let prompt = "序文を比較する\n";
        let artifacts: Vec<Artifact> = serde_json::from_value(json!([
            {"label": "A", "text": " Aの序文\r\n"}, {"label": "B", "text": " Bの序文\n"}
        ]))?;
        assert_eq!(
            review_message(prompt, &artifacts)?,
            "序文を比較する\n\n\n[A]\n Aの序文\r\n\n\n[B]\n Bの序文\n"
        );
        assert!(serde_json::from_value::<Artifact>(json!({
            "label": "A", "text": "", "path": "preface.tex",
        }))
        .is_err());
        Ok(())
    }
}
