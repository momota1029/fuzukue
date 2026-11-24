use anyhow::{ensure, Context, Result};
use fuzukue_proto::{Actor, HandoffIssueSelection, LocalizedMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path};

use crate::{issue_threads, Workspace};

struct Draft {
    id: String,
    title: String,
    body: String,
    ready: bool,
}

fn source_link(path: &str) -> String {
    let path = std::env::var_os("HOME")
        .and_then(|home| {
            path.strip_prefix("~/")
                .map(|tail| Path::new(&home).join(tail))
        })
        .unwrap_or_else(|| path.into());
    if let Ok(url) = url::Url::from_file_path(&path) {
        return format!("[`{}`](<{url}>)", path.display());
    }
    if super::workers::valid_thread_id(path.to_str().unwrap_or_default()) {
        return format!(
            "[`{}`](<codex://threads/{}>)",
            path.display(),
            path.display()
        );
    }
    format!("`{}`", path.display())
}

fn drafts(
    app: &Workspace,
    session: &str,
    selection: &[HandoffIssueSelection],
) -> Result<Vec<Draft>> {
    ensure!(!selection.is_empty(), "Select at least one handoff task");
    let (_, note) = super::received_note(app, session)?;
    let source = source_link(&app.project_dir.join(&note.summary.path).to_string_lossy());
    let transcript = (!note.transcript.is_empty()).then(|| source_link(&note.transcript));
    let mut seen = HashSet::new();
    selection
        .iter()
        .map(|choice| {
            ensure!(seen.insert(&choice.id), "Duplicate handoff task");
            let task = note
                .tasks
                .iter()
                .find(|task| task.id == choice.id)
                .context("The handoff task has changed. Reload the checklist.")?;
            let mut body = format!("{}\n\n---\nHandoff: {source}", task.markdown);
            if let Some(transcript) = &transcript {
                body.push_str(&format!("\n\nSource transcript: {transcript}"));
            }
            Ok(Draft {
                id: choice.id.clone(),
                title: task.title.clone(),
                body,
                ready: choice.ready,
            })
        })
        .collect()
}

pub(super) async fn create(
    app: &Workspace,
    session: &str,
    selection: &[HandoffIssueSelection],
) -> Result<Value> {
    publish(app, session, drafts(app, session, selection)?).await
}

async fn publish(app: &Workspace, session: &str, drafts: Vec<Draft>) -> Result<Value> {
    let _guard = app.handoff.issues.lock().await;
    let repo = app.research_issues.repo()?;
    let mut received = app
        .hub
        .read_threads(|store| store.handoff(session))
        .context("Thread has no handoff note")?;
    let mut created = Vec::new();
    for draft in drafts {
        let number = if let Some(number) = received.issues.get(&draft.id) {
            *number
        } else {
            let mut args = vec![
                "issue",
                "create",
                "--repo",
                repo,
                "--title",
                &draft.title,
                "--body",
                &draft.body,
            ];
            if draft.ready {
                args.extend(["--label", "dot:ready"]);
            }
            let output = issue_threads::gh(app, &args)
                .await
                .with_context(|| format!("Create issue: {}", draft.title))?;
            let url = output.lines().last().context("Missing issue URL")?.trim();
            let prefix = format!("https://github.com/{repo}/issues/");
            let number = url
                .strip_prefix(&prefix)
                .and_then(|n| n.parse().ok())
                .context("Unexpected issue URL")?;
            app.hub
                .edit_threads(|store| store.record_handoff_issue(session, draft.id.clone(), number))
                .with_context(|| {
                    format!("Issue created at {url}. Could not save the handoff receipt")
                })?;
            received.issues.insert(draft.id, number);
            app.issues_refresh.notify_one();
            app.research_issues.notify();
            number
        };
        let link = issue_threads::link(app, number)?;
        issue_threads::attach(app, session, link)
            .with_context(|| format!("Could not link issue #{number}"))?;
        created.push(
            json!({"number":number, "url":format!("https://github.com/{repo}/issues/{number}")}),
        );
    }
    Ok(json!({"issues":created}))
}

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Tasks,
    Create,
}

impl Tool {
    pub const ALL: [Self; 2] = [Self::Tasks, Self::Create];

    pub fn name(self) -> &'static str {
        match self {
            Self::Tasks => "handoff_tasks",
            Self::Create => "create_handoff_issues",
        }
    }

    pub fn writes(self) -> bool {
        matches!(self, Self::Create)
    }

    pub fn definition(self) -> Value {
        let (description, properties, required) = match self {
            Self::Tasks => (harness_text!(
                "Read the remaining tasks and known issue numbers attached to the current handoff thread.",
                "このスレッドに付いた引き継ぎの残作業と作成済みの Issue 番号を読む。",
            ), json!({}), json!([])),
            Self::Create => (harness_text!(
                "Create selected handoff issues in the workspace repository. Set confirm=true only with the user's agreement to the selected task IDs and ready flags. The ready flag defaults to false and applies dot:ready.",
                "選んだ引き継ぎタスクをこのワークスペースの Issue にする。タスク ID とすぐ拾ってよい項目について利用者が合意した後だけ confirm=true を指定する。ready の既定値は false で、true なら dot:ready を付ける。",
            ), json!({"confirm":{"type":"boolean"},"tasks":{"type":"array","minItems":1,"items":{
                "type":"object","properties":{"id":{"type":"string"},"ready":{"type":"boolean"}},
                "required":["id"],"additionalProperties":false
            }}}), json!(["confirm","tasks"])),
        };
        json!({"name":self.name(),"description":description,"inputSchema":{
            "type":"object","properties":properties,"required":required,"additionalProperties":false
        }})
    }

    pub async fn call(
        self,
        app: &Workspace,
        session: &str,
        actor: Actor,
        args: &Value,
    ) -> Result<Value> {
        match self {
            Self::Tasks => {
                ensure!(
                    args.as_object().is_some_and(|obj| obj.is_empty()),
                    "Unexpected handoff_tasks arguments"
                );
                let (received, mut note) = super::received_note(app, session)?;
                for task in &mut note.tasks {
                    task.issue = received.issues.get(&task.id).copied();
                }
                Ok(json!({"path":received.path,"transcript":note.transcript,"tasks":note.tasks}))
            }
            Self::Create => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    confirm: bool,
                    tasks: Vec<HandoffIssueSelection>,
                }
                let args = Args::deserialize(args)?;
                ensure!(args.confirm, "Ask the user before setting confirm=true");
                let drafts = drafts(app, session, &args.tasks)?;
                let repo = app.research_issues.repo()?;
                let tasks = drafts
                    .iter()
                    .map(|draft| {
                        format!(
                            "{}{}\n\n{}",
                            draft.title,
                            if draft.ready { " [dot:ready]" } else { "" },
                            draft.body
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                let summary = LocalizedMessage::new(
                    "handoff-approve-issues",
                    [("repo", repo.to_owned()), ("tasks", tasks)],
                );
                let decision = crate::delegate::request_approval(
                    app,
                    session,
                    issue_threads::APPROVAL,
                    actor,
                    summary,
                )
                .await?;
                ensure!(decision == "accept", "The user declined issue creation");
                let result = publish(app, session, drafts).await;
                if let Ok(detail) = super::detail(app, session) {
                    app.hub
                        .publish_snapshot(fuzukue_proto::ServerEvent::HandoffDetail { detail });
                }
                result
            }
        }
    }
}
