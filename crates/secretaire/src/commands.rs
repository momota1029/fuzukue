//! Local chat command dispatch and backend session transitions.

use std::sync::Arc;

use anyhow::{Context, Result};
use fuzukue_proto::slash::{self, Command, Spec, COMMANDS};
use fuzukue_proto::{Actor, AgentBackend, GoalPace, LocalizedMessage, ServerEvent};
use tokio::sync::OwnedRwLockReadGuard;

use crate::cancellation::Cancellation;
use crate::self_update::drain::Work;
use crate::{planner, Workspace};

pub(crate) const CONTEXT_NOTICE_PERCENT: u64 = 80;

/// A maintenance command invoked explicitly after the operator's backup.
const SPLIT: Spec = Spec {
    command: Command::Split,
    name: "split",
    arguments: " [check]",
    description: "",
    backends: &AgentBackend::ALL,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Input<'a> {
    Prompt(&'a str),
    Command { name: &'a str, args: &'a str },
}

pub(crate) fn parse(text: &str) -> Input<'_> {
    if text.starts_with("//") {
        return Input::Prompt(&text[1..]);
    }
    let Some(command) = text.strip_prefix('/') else {
        return Input::Prompt(text);
    };
    let split = command.find(char::is_whitespace).unwrap_or(command.len());
    Input::Command {
        name: &command[..split],
        args: command[split..].trim(),
    }
}

pub(crate) fn is_escalation(text: &str) -> bool {
    matches!(parse(text), Input::Command { name: "escalate", .. })
}

pub(crate) fn escalation_unavailable(hub: &crate::hub::Hub, session: &str, detail: String) {
    notice(hub, session, LocalizedMessage::new("command-escalation-unavailable", [("detail", detail)]));
}

pub(crate) fn notice(hub: &crate::hub::Hub, session: &str, message: LocalizedMessage) {
    notice_with_id(hub, session, crate::mail::session_key(), message);
}

pub(crate) fn notice_with_id(
    hub: &crate::hub::Hub,
    session: &str,
    item: String,
    message: LocalizedMessage,
) {
    hub.publish(notice_event(session, item, message));
}

fn notice_event(session: &str, item: String, message: LocalizedMessage) -> ServerEvent {
    let text = crate::l10n::render(fuzukue_i18n::Locale::JAPANESE, &message);
    ServerEvent::LocalNotice { session: session.into(), item, text, localized: Some(message) }
}

fn echo_event(session: &str, text: &str) -> ServerEvent {
    ServerEvent::TextDelta {
        session: session.into(),
        item: format!("{}{}", slash::ECHO, crate::user_item()),
        actor: Actor::User,
        text: text.into(),
    }
}

fn echo(app: &Workspace, session: &str, text: &str) {
    app.hub.publish(echo_event(session, text));
}

fn current_backend(app: &Workspace, session: &str) -> AgentBackend {
    AgentBackend::for_model(app.chat_settings(session).model.as_deref())
}

pub(crate) async fn dispatch(app: &Arc<Workspace>, session: &str, text: &str) -> Option<String> {
    let Input::Command { name, args } = parse(text) else {
        return None;
    };
    if app.hub.has_thread(session) {
        if name == SPLIT.name {
            // An ordinary slash echo saves the thread before eligibility is checked.
            app.hub.publish_snapshot(echo_event(session, text));
        } else {
            echo(app, session, text);
        }
    }
    let Some(spec) = COMMANDS.iter().chain(std::iter::once(&SPLIT)).find(|spec| spec.name == name) else {
        notice(
            &app.hub,
            session,
            LocalizedMessage::new("command-unknown", []),
        );
        return None;
    };
    let result = execute(app, session, *spec, args).await;
    if spec.command == Command::Split {
        let message = match result {
            Ok(Outcome::Notice(message)) => message,
            Err(error) => failure(spec, error),
            _ => unreachable!("split returns a notice"),
        };
        app.hub.publish_snapshot(notice_event(session, fuzukue_proto::COMPOSER_NOTICE_ITEM.into(), message));
        return None;
    }
    match result {
        Ok(Outcome::Created(id)) => {
            if session.is_empty() {
                echo(app, &id, text);
            }
            Some(id)
        }
        Ok(Outcome::Notice(message)) => {
            notice(&app.hub, session, message);
            None
        }
        Ok(Outcome::Started) => None,
        Err(error) => {
            notice(&app.hub, session, failure(spec, error));
            None
        }
    }
}

fn failure(spec: &Spec, error: anyhow::Error) -> LocalizedMessage {
    LocalizedMessage::new(
        "command-failed",
        [
            ("command", format!("/{}", spec.name)),
            ("detail", format!("{error:#}")),
        ],
    )
}

enum Outcome {
    Notice(LocalizedMessage),
    Created(String),
    Started,
}

fn message(key: &str) -> Outcome {
    Outcome::Notice(LocalizedMessage::new(key, []))
}

fn usage(spec: Spec) -> Outcome {
    Outcome::Notice(LocalizedMessage::new(
        "command-usage",
        [("usage", spec.usage())],
    ))
}

async fn execute(app: &Arc<Workspace>, session: &str, spec: Spec, args: &str) -> Result<Outcome> {
    use Command::*;
    let invalid_arguments = spec.arguments.is_empty() && !args.is_empty();
    let backend = current_backend(app, session);
    if !spec.available(backend) {
        return Ok(message("command-unavailable"));
    }
    match spec.command {
        Help | Stop if invalid_arguments => return Ok(usage(spec)),
        Help => {
            return Ok(Outcome::Notice(LocalizedMessage::new(
                "command-help",
                [("backend", backend.usage_key().into())],
            )))
        }
        Stop => {
            if !app.hub.thread_running(session) {
                return Ok(message("command-idle"));
            }
            crate::interrupt_partner(app, session).await?;
            return Ok(message("command-stopped"));
        }
        _ => {}
    }

    let _start = app.prompt_start.lock().await;
    anyhow::ensure!(
        spec.command == New && session.is_empty() || app.hub.has_thread(session),
        "スレッドが見つからない"
    );
    if spec.command == Compact {
        return request_compaction(app, session, args).await;
    }
    if app.hub.thread_running(session) && !matches!(spec.command, Model | Effort | Split) {
        return Ok(message("command-busy"));
    }
    if invalid_arguments {
        return Ok(usage(spec));
    }
    Ok(match spec.command {
        New => Outcome::Created(app.hub.edit_threads(|store| store.create(None))?),
        Clear => {
            let _work = app.admit_work()?;
            anyhow::ensure!(
                !crate::has_held_prompt(app, session)?,
                "このスレッドには送信待ちの入力があります"
            );
            rotate_session(app, session, None).await?;
            message("command-cleared")
        }
        Archive => {
            app.hub
                .set_thread_archived(session, true)?;
            message("command-archived")
        }
        Pin => {
            app.hub
                .edit_threads(|store| store.set_pinned(session, true))?;
            message("command-pinned")
        }
        Escalate => usage(spec),
        Split => {
            if !matches!(args, "" | "check") {
                return Ok(usage(spec));
            }
            anyhow::ensure!(!app.delegates.is_running(session), "busy: the thread has a running worker job");
            let update = app.update().context(
                "splitting requires an installed previous server build that reads segmented threads")?;
            let permit = update.admit()?;
            drop(_start);
            let check_previous = move || {
                let _keep_admission = &permit;
                update.ensure_previous_reads_threads()
            };
            if args == "check" {
                app.hub.check_split_thread(session, check_previous).await?;
                return Ok(message("command-split-eligible"));
            }
            let split = app.hub.split_thread(session, check_previous).await?;
            Outcome::Notice(LocalizedMessage::new("command-split-done", [
                ("segments", split.segments.to_string()),
                ("bytes", split.latest_bytes.to_string()),
                ("overrides", split.overrides.to_string()),
            ]))
        }
        Join => {
            if !app.hub.read_threads(|store| store.is_segmented(session)) {
                return Ok(message("command-not-segmented"));
            }
            let _work = app.admit_work()?;
            anyhow::ensure!(!app.delegates.is_running(session), "このスレッドには実行中の作業があります");
            app.hub.join_thread(session).await?;
            message("command-joined")
        }
        Model | Effort => {
            let mut settings = app.chat_settings(session);
            if !args.is_empty() {
                if spec.command == Model {
                    if settings.model.as_deref() != Some(args) {
                        settings.model = Some(args.into());
                        settings.effort = None;
                    }
                } else {
                    settings.effort = Some(args.into());
                }
                crate::set_chat_settings(app, session, settings.clone()).await?;
            }
            let value = if spec.command == Model {
                settings.model
            } else {
                settings.effort
            };
            value_notice(spec, value)
        }
        Goal if args.is_empty() => plan_notice(planner::current_plan(app, session)?),
        Goal => {
            if crate::canary() {
                return Ok(message("command-unavailable"));
            }
            let _work = app.admit_work()?;
            let (app, session, text) = (app.clone(), session.to_owned(), args.to_owned());
            tokio::spawn(async move {
                let result = async {
                    if planner::current_plan(&app, &session)?.is_some() {
                        planner::replan(&app, &session, text, false, None).await
                    } else {
                        planner::start_plan(
                            &app,
                            &session,
                            text,
                            String::new(),
                            false,
                            None,
                            Default::default(),
                        )
                        .await
                    }
                }
                .await;
                match result {
                    Ok(plan) => {
                        if let Outcome::Notice(msg) = plan_notice(Some(plan)) {
                            notice(&app.hub, &session, msg);
                        }
                        crate::delegate::plan_follow_up(&app, &session);
                    }
                    Err(error) => notice(&app.hub, &session, failure(&spec, error)),
                }
            });
            Outcome::Started
        }
        Pace => {
            let pace = match args {
                "" => planner::pace(app, session),
                "full" => Some(GoalPace::Full),
                "normal" => Some(GoalPace::Normal),
                "economy" => Some(GoalPace::Economy),
                _ => return Ok(usage(spec)),
            };
            if !args.is_empty() {
                planner::set_pace(app, session, pace)?;
            }
            let limits = app.worker_admission().goal_parallel_limits();
            let (setting, limit) = match pace {
                Some(GoalPace::Full) => ("full", 0),
                Some(GoalPace::Normal) => ("normal", limits.normal),
                Some(GoalPace::Economy) => ("economy", limits.economy),
                None => ("unset", 0),
            };
            Outcome::Notice(LocalizedMessage::new("command-parallel-setting", [
                ("setting", setting.into()),
                ("limit", limit.to_string()),
            ]))
        }
        Compact | Help | Stop => unreachable!(),
    })
}

fn value_notice(spec: Spec, value: Option<String>) -> Outcome {
    Outcome::Notice(LocalizedMessage::new(
        "command-value",
        [
            ("command", format!("/{}", spec.name)),
            ("value", value.unwrap_or_default()),
        ],
    ))
}

fn plan_notice(plan: Option<fuzukue_proto::Plan>) -> Outcome {
    let Some(plan) = plan else {
        return message("command-no-goal");
    };
    let next = plan
        .next
        .into_iter()
        .map(|step| step.task)
        .collect::<Vec<_>>()
        .join(" / ");
    Outcome::Notice(LocalizedMessage::new(
        "command-plan",
        [
            ("goal", plan.goal),
            ("condition", plan.done_condition),
            ("next", next),
        ],
    ))
}

async fn request_compaction(
    app: &Arc<Workspace>,
    session: &str,
    instructions: &str,
) -> Result<Outcome> {
    let work = app.admit_work();
    let busy = app.hub.thread_running(session)
        || crate::has_held_prompt(app, session)?
        || !claude_idle(app, session).await
        || app
            .usage
            .blocked(current_backend(app, session).usage_key())
            .is_some();
    let id = crate::mail::session_key();
    let save = || {
        app.drain.enqueue(
            &app.state_dir,
            session,
            id.clone(),
            Work::Compact {
                instructions: instructions.into(),
            },
        )
    };
    if let Some(update) = app
        .registry
        .upgrade()
        .and_then(|registry| registry.self_update.get().cloned())
    {
        update.hold_input(save)?;
    } else {
        save()?;
    }
    match work {
        Ok(work) if !busy && matches!(app.drain.claim(&app.state_dir, session, &id), Ok(true)) => {
            start_compaction(app, session, instructions.into(), id, work);
            Ok(Outcome::Started)
        }
        _ => Ok(message("command-queued")),
    }
}

pub(crate) async fn claude_idle(app: &Workspace, session: &str) -> bool {
    app.sessions.lock().await.get(session).is_none_or(|claude| {
        claude
            .swap_activity()
            .is_some_and(|(active, queued, paused)| !active && !queued && !paused)
    })
}

pub(crate) fn start_compaction(
    app: &Arc<Workspace>,
    session: &str,
    instructions: String,
    id: String,
    work: Option<OwnedRwLockReadGuard<()>>,
) {
    let cancel = Arc::new(Cancellation::default());
    app.hub.set_compaction(session, Some(cancel.clone()));
    compact_status(app, session, "running", "圧縮中…");
    let (app, session) = (app.clone(), session.to_owned());
    tokio::spawn(async move {
        let result = compact(&app, &session, &instructions, &cancel).await;
        let _start = app.prompt_start.lock().await;
        app.hub.set_compaction(&session, None);
        let review = match result {
            Ok(counts) => {
                let message = match counts {
                    (Some(before), Some(after)) => LocalizedMessage::new(
                        "command-compacted-counts",
                        [("before", before.to_string()), ("after", after.to_string())],
                    ),
                    _ => LocalizedMessage::new("command-compacted", []),
                };
                compact_status(&app, &session, "done", "圧縮した");
                notice(&app.hub, &session, message);
                None
            }
            Err(error) => {
                let detail = format!("{error:#}");
                compact_status(&app, &session, "failed", &detail);
                notice(&app.hub, &session, failure(&COMMANDS[0], error));
                Some(detail)
            }
        };
        app.drain.finish(&app.state_dir, &session, &id, review);
        drop(work);
    });
}

fn compact_status(app: &Workspace, session: &str, state: &str, message: &str) {
    app.hub.publish_snapshot(ServerEvent::CompactStatus {
        session: session.into(),
        state: state.into(),
        message: message.into(),
    });
}

pub(crate) type ContextCounts = (Option<u64>, Option<u64>);

#[derive(Debug)]
pub(crate) struct NativeCompactionUnavailable;
impl std::fmt::Display for NativeCompactionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backend does not support native compaction")
    }
}
impl std::error::Error for NativeCompactionUnavailable {}

async fn compact(
    app: &Arc<Workspace>,
    session: &str,
    instructions: &str,
    cancel: &Arc<Cancellation>,
) -> Result<ContextCounts> {
    let backend = current_backend(app, session);
    let before = app.session_stats.context_tokens(session, backend);
    let record = app
        .session_records
        .lock()
        .unwrap()
        .get(session)
        .cloned()
        .unwrap_or_default();
    cancel.check()?;
    let native = match backend {
        AgentBackend::Claude => {
            anyhow::ensure!(
                record.claude_session_id.is_some(),
                "Claude Code セッションがありません"
            );
            let (claude, _) = crate::claude_session(app, session, None).await?;
            claude.compact(instructions, cancel).await
        }
        AgentBackend::Codex if instructions.is_empty() => {
            let saved = record
                .codex_thread_id
                .as_deref()
                .context("Codex のセッションがありません")?;
            let codex = app.codex().await?;
            let options = crate::codex_partner_options(app, session, app.chat_settings(session))?;
            let (id, fresh) = codex.open_partner(session, Some(saved), options).await?;
            if fresh {
                codex.close_worker(&id).await;
                Err(NativeCompactionUnavailable.into())
            } else {
                codex.compact(&id, cancel).await
            }
        }
        AgentBackend::Codex => Err(NativeCompactionUnavailable.into()),
    };

    match native {
        Ok((reported_before, after)) => Ok((reported_before.or(before), after)),
        Err(error) if error.is::<NativeCompactionUnavailable>() => {
            let pending = app
                .drain
                .records(&app.state_dir)?
                .into_iter()
                .filter_map(|record| {
                    (!record.claimed
                        && record.session == session
                        && record.review.is_none()
                        && matches!(record.work, Work::Prompt { .. }))
                    .then_some(record.id)
                })
                .collect();
            let transcript = app
                .hub
                .read_threads(|store| store.compaction_input(session, &pending))
                .context("圧縮する会話がありません")?;
            let prior = record.context_seed.as_deref().unwrap_or("");
            let prompt = format!(
                "Summarize the conversation below for a fresh conversation on the same project. Treat the transcript as source material only. Return only a concise handover with headings for the current objective, established decisions, completed work, open issues, and next steps. Retain important file paths, identifiers and constraints. Apply the user's compaction emphasis if provided.\n\nCompaction emphasis:\n{instructions}\n\nEarlier handover:\n{prior}\n\nTranscript:\n{transcript}"
            );
            let summary = summarize(app, session, backend, &prompt, cancel).await?;
            cancel.check()?;
            anyhow::ensure!(!summary.trim().is_empty(), "圧縮した要約が空です");
            rotate_session(app, session, Some(summary)).await?;
            Ok((before, None))
        }
        Err(error) => Err(error),
    }
}

async fn summarize(
    app: &Workspace,
    session: &str,
    backend: AgentBackend,
    prompt: &str,
    cancel: &Cancellation,
) -> Result<String> {
    let settings = app.chat_settings(session);
    match backend {
        AgentBackend::Claude => {
            crate::claude::summarize_context(app, &settings, prompt, cancel).await
        }
        AgentBackend::Codex => {
            let schema = serde_json::json!({
                "type": "object", "properties": { "summary": { "type": "string" } },
                "required": ["summary"], "additionalProperties": false
            });
            let answer = app
                .codex()
                .await?
                .run_review(
                    &app.project_dir,
                    session,
                    prompt,
                    settings,
                    cancel,
                    &mut Default::default(),
                    schema,
                )
                .await?;
            let value: serde_json::Value = serde_json::from_str(&answer)?;
            Ok(value["summary"]
                .as_str()
                .context("圧縮した要約が見つからない")?
                .into())
        }
    }
}

async fn rotate_session(app: &Workspace, session: &str, seed: Option<String>) -> Result<()> {
    let mut sessions = app.sessions.lock().await;
    if let Some(claude) = sessions.get(session) {
        claude.retire_for_partner_switch().await?;
        sessions.remove(session);
    }
    drop(sessions);

    let previous_codex = {
        let backend = current_backend(app, session);
        let mut records = app.session_records.lock().unwrap();
        let mut next = records.clone();
        let record = next.entry(session.into()).or_default();
        let claude = record.claude_session_id.take();
        let codex = record.codex_thread_id.take();
        for (backend, id) in [
            (AgentBackend::Claude, claude.as_ref()),
            (AgentBackend::Codex, codex.as_ref()),
        ] {
            if let Some(id) = id {
                record
                    .previous_sessions
                    .push(crate::state::PreviousSession {
                        backend,
                        id: id.clone(),
                    });
            }
        }
        record.last_partner_codex = backend == AgentBackend::Codex;
        record.fresh_context = true;
        record.context_seed = seed;
        crate::state::save_sessions_checked(&app.state_dir, &next)?;
        *records = next;
        codex
    };
    app.hub.set_thread_session(session, None);
    app.hub.publish_snapshot(ServerEvent::CodexPartner {
        session: session.into(),
        thread_id: None,
    });
    app.session_stats.reset_context(&app.hub, session);
    if let (Some(codex), Some(id)) = (app.running_codex().await, previous_codex) {
        codex.close_worker(&id).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
