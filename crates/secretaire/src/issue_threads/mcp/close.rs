use anyhow::{ensure, Context, Result};
use chrono::Utc;
use fuzukue_proto::{Actor, GitHubLink, GitHubPolicy, LocalizedMessage};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::Workspace;

use super::super::{
    api, echo::Expected, endpoint, gh, intake, record_write, url_link, GitHubConfig, Kind, MARKER,
};
use super::approve;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    #[default]
    Completed,
    NotPlanned,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NotPlanned => "not_planned",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    number: u64,
    #[serde(default)]
    reason: Reason,
    comment: Option<String>,
    #[serde(default)]
    confirm: bool,
}

fn target(app: &Workspace, session: &str, number: u64) -> Result<(GitHubLink, GitHubPolicy)> {
    ensure!(number > 0, "Use a positive issue number");
    let state = app
        .hub
        .read_threads(|store| store.github_responder(session))
        .context("Thread is archived or missing")?;
    let known = if state.intake_repo.is_some() {
        intake::known_link(app, session, number)?
    } else {
        None
    };
    let mut candidates = state.links.into_iter().filter(|link| link.number == number);
    let link = known
        .or_else(|| candidates.next())
        .context("Link this issue to the thread or use its intake thread")?;
    ensure!(
        candidates.all(|candidate| candidate == link),
        "This number refers to multiple repositories. Use a thread linked only to the intended issue"
    );
    intake::allowed(
        app,
        &GitHubConfig::from_workspace(&app.project_dir)?,
        &link.repo,
    )?;
    let policy = app.issue_threads.write_policy(session, &link, state.policy);
    Ok((link, policy))
}

pub(super) async fn call(
    app: &Workspace,
    session: &str,
    actor: Actor,
    args: &Value,
) -> Result<Value> {
    let args = Args::deserialize(args)?;
    let reason = args.reason.as_str();
    let comment = args.comment.as_deref().map(str::trim);
    ensure!(
        comment.is_none_or(|body| !body.is_empty()),
        "Enter a GitHub reply"
    );
    let (link, policy) = target(app, session, args.number)?;
    let item = format!("{}#{}", link.repo, link.number);
    let needs_approval = policy != GitHubPolicy::Allow;
    if needs_approval {
        approve(
            app,
            session,
            actor,
            args.confirm,
            LocalizedMessage::new(
                "github-approve-close",
                [
                    ("item", item.clone()),
                    ("reason", reason.into()),
                    (
                        "has_comment",
                        if comment.is_some() { "yes" } else { "no" }.into(),
                    ),
                    ("comment", comment.unwrap_or_default().into()),
                ],
            ),
        )
        .await?;
    }

    let (already_closed, comment_result) = {
        let _serial = app.issue_threads.serial.lock().await;
        let (current_link, current_policy) = target(app, session, args.number)?;
        ensure!(
            link == current_link,
            "The GitHub target changed. Confirm the intended issue again"
        );
        ensure!(
            needs_approval || current_policy == GitHubPolicy::Allow,
            "The GitHub policy changed. Ask the user first, then set confirm=true"
        );

        let path = endpoint(&link);
        let current = api(app, &path).await?;
        ensure!(
            !current["pull_request"].is_object(),
            "Only GitHub issues can be closed with this tool"
        );
        ensure!(
            url_link(
                current["html_url"]
                    .as_str()
                    .context("GitHub response is missing the issue URL")?
            )? == link,
            "GitHub returned an issue outside the requested target"
        );
        let state = current["state"].as_str().unwrap_or_default();
        let already_closed = state.eq_ignore_ascii_case("closed");
        let mut comment_result = Ok(());
        if !already_closed {
            ensure!(
                state.eq_ignore_ascii_case("open"),
                "GitHub returned an unexpected issue state"
            );
            let at = Utc::now().timestamp();
            let response = gh(
                app,
                &[
                    "api",
                    "--hostname",
                    "github.com",
                    "--method",
                    "PATCH",
                    &path,
                    "-f",
                    "state=closed",
                    "-f",
                    &format!("state_reason={reason}"),
                ],
            )
            .await?;
            let updated: Value =
                serde_json::from_str(&response).context("Read the GitHub close response")?;
            ensure!(
                updated["state"] == "closed",
                "GitHub did not confirm the issue was closed"
            );
            record_write(
                app,
                session,
                &link,
                vec![Expected::Activity(Kind::Closed)],
                at,
            );
            if let Some(comment) = comment {
                let body = format!("{comment}\n\n{MARKER}");
                let at = Utc::now().timestamp();
                comment_result = gh(
                    app,
                    &[
                        "api",
                        "--hostname",
                        "github.com",
                        "--method",
                        "POST",
                        &format!("{path}/comments"),
                        "-f",
                        &format!("body={body}"),
                    ],
                )
                .await
                .map(|response| {
                    let id = serde_json::from_str::<Value>(&response)
                        .ok()
                        .and_then(|value| value["id"].as_u64());
                    record_write(
                        app,
                        session,
                        &link,
                        vec![Expected::Comment {
                            body: Some(body),
                            id,
                        }],
                        at,
                    );
                });
            }
        }
        (already_closed, comment_result)
    };
    let intake_result = intake::record_closed(app, &link).await;
    app.issues_refresh.notify_one();
    comment_result.with_context(|| {
        format!("GitHub issue {item} was closed. The closing comment could not be posted")
    })?;
    intake_result.with_context(|| {
        format!("GitHub issue {item} is closed. Could not update its local intake state")
    })?;
    Ok(
        json!({"closed": true, "already_closed": already_closed, "comment_posted": comment.is_some() && !already_closed}),
    )
}
