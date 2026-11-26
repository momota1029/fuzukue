//! Save the outcome in the requesting thread and offer the desk a wake.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{STATUS_FILE, State, Status, Step, Update, write_json};
use crate::{App, Workspace};

const LOG_TAIL_BYTES: usize = 1500;

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Recipient {
    workspace: String,
    thread: String,
}

impl Recipient {
    pub(crate) fn new(workspace: &Workspace, thread: &str) -> Self {
        Self { workspace: workspace.id.clone(), thread: thread.into() }
    }
}

#[derive(Deserialize, Serialize)]
struct Request {
    id: String,
    recipient: Option<Recipient>,
    rollback: bool,
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct SavedStatus {
    #[serde(flatten)]
    pub(super) status: Status,
    // Older servers deserialize the flattened Status and ignore this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<Request>,
}

impl SavedStatus {
    pub(super) fn read(root: &Path) -> Result<Self> {
        match fs::read(root.join(STATUS_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read self-update status"),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn write(&self, root: &Path) -> Result<()> {
        write_json(&root.join(STATUS_FILE), self)
    }

    pub(super) fn save(root: &Path, status: &Status) -> Result<()> {
        let mut saved = Self::read(root)?;
        saved.status = status.clone();
        saved.write(root)
    }
}

impl Request {
    fn deliver(self, app: &Arc<App>, status: &Status) -> Result<()> {
        if let Some(recipient) = self.recipient.filter(|recipient| app.has_workspace(&recipient.workspace)) {
            let workspace = app.known_workspace(&recipient.workspace)?;
            if workspace.hub.has_thread(&recipient.thread) {
                let text = result_text(status, self.rollback);
                let title = harness_text!("Server self-update result", "サーバーの自己更新結果");
                let note = format!("{title}\n\n```json\n{text}\n```");
                if workspace.hub.deliver_update_note(&recipient.thread, &self.id, note)?
                    && workspace.hub.read_threads(|threads| threads.is_live(&recipient.thread))
                {
                    crate::delegate::wake_for_update_result(&workspace, &recipient.thread, text);
                }
            }
        }
        Ok(())
    }
}

impl Update {
    pub(super) fn begin_result(
        &self, status: Status, recipient: Option<Recipient>, rollback: bool,
    ) -> Result<()> {
        let _notice = self.result_notice.lock().unwrap();

        // Commit the recipient and the fresh attempt status in the same file.
        ensure!(SavedStatus::read(&self.root)?.request.is_none(), "previous update result is still pending");
        let id = format!("server-update:{}", crate::mail::session_key());
        let request = Request { id, recipient, rollback };
        let saved = SavedStatus { status, request: Some(request) };
        saved.write(&self.root)?;
        self.status.send_replace(saved.status);
        Ok(())
    }

    pub(crate) fn watch_result(&self, recipient: Recipient) -> Result<()> {
        let _notice = self.result_notice.lock().unwrap();
        ensure!(
            !self.trial_pending() && !*self.swapping.borrow() && self.status.borrow().active,
            "there is no update accepting a result recipient"
        );
        let mut saved = SavedStatus::read(&self.root)?;
        let request = saved.request.as_mut().context("there is no active server update")?;
        if let Some(saved) = &request.recipient {
            ensure!(*saved == recipient, "the update already has a result recipient");
            return Ok(());
        }
        request.recipient = Some(recipient);
        saved.write(&self.root)
    }

    pub(super) fn save_result_progress(&self) {
        let _notice = self.result_notice.lock().unwrap();
        if let Ok(mut saved) = SavedStatus::read(&self.root) {
            if saved.request.is_some() {
                saved.status = self.status.borrow().clone();
                if let Err(error) = saved.write(&self.root) {
                    tracing::warn!(%error, "save server update progress for result notification");
                }
            }
        }
    }

    pub(super) fn save_status(&self) -> Result<()> {
        let _notice = self.result_notice.lock().unwrap();
        SavedStatus::save(&self.root, &self.status.borrow())
    }

    pub(super) async fn report_result_with_retry(&self, app: &Arc<App>) {
        while let Err(error) = self.report_result(app) {
            tracing::warn!(%error, "deliver server update result");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    pub(super) fn report_result(&self, app: &Arc<App>) -> Result<()> {
        let _notice = self.result_notice.lock().unwrap();
        let status = self.status.borrow().clone();
        if status.active {
            return Ok(());
        }
        let mut saved = SavedStatus::read(&self.root)?;
        if let Some(request) = saved.request.take() {
            request.deliver(app, &status)?;
            // Retrying after a crash finds the same item in the thread transcript.
            saved.status = status;
            saved.write(&self.root)?;
        }
        Ok(())
    }
}

fn result_text(status: &Status, rollback: bool) -> String {
    let failed = status.steps.iter().find(|step| step.state == State::Failed);
    let swap = status.steps.iter().find(|step| step.kind == Step::Swap);
    let outcome = if status.recovered || rollback && swap.is_some_and(|step| step.state == State::Passed) {
        "rollback"
    } else if status.cancelled || status.error.is_some() || failed.is_some()
        || !swap.is_some_and(|step| step.state == State::Passed)
    {
        "failure"
    } else {
        "success"
    };
    let tail = |text: &str| {
        let start = text.ceil_char_boundary(text.len().saturating_sub(LOG_TAIL_BYTES));
        text[start..].to_owned()
    };
    let error = status.error.as_deref().or(status.cancelled.then_some("server swap cancelled"));
    serde_json::json!({
        "outcome": outcome,
        "running_revision": status.build_id,
        "failed_step": failed.map(|step| step.kind),
        "error": error.map(tail),
        "log_tail": failed.or(swap).map(|step| step.log.as_str())
            .filter(|log| !log.is_empty())
            .or_else(|| status.canary_status.as_ref().map(|canary| canary.log.as_str()).filter(|log| !log.is_empty()))
            .or(error)
            .map(tail).unwrap_or_default(),
    }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::SelfUpdateStep;

    #[test]
    fn result_uses_the_running_build_and_bounds_the_failed_step_log() -> Result<()> {
        let mut status = Status {
            build_id: "running-build".into(),
            source_commit: Some("different-source-build".into()),
            steps: vec![SelfUpdateStep {
                kind: Step::Swap,
                state: State::Passed,
                log: String::new(),
            }],
            ..Status::default()
        };
        let summary = |status: &Status, rollback| serde_json::from_str::<serde_json::Value>(&result_text(status, rollback));
        assert_eq!(summary(&status, false)?["outcome"], "success");
        assert_eq!(summary(&status, true)?["outcome"], "rollback");

        status.steps[0].state = State::Failed;
        status.steps[0].log = format!("{}last line", "界".repeat(1000));
        status.error = Some("startup timed out".into());
        status.recovered = true;
        let result = summary(&status, false)?;
        assert_eq!(result["outcome"], "rollback");
        assert_eq!(result["running_revision"], "running-build");
        assert_eq!(result["failed_step"], "swap");
        let tail = result["log_tail"].as_str().unwrap();
        assert!(tail.len() <= LOG_TAIL_BYTES && tail.ends_with("last line"));
        Ok(())
    }
}
