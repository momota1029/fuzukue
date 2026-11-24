use fuzukue_i18n::{FluentArgs, Locale};
use fuzukue_proto::{
    Actor, GitOutcome, LocalizedMessage, ServerEvent, DELEGATE_JOB, LOCALE_PROTOCOL, PLAN_NOTE,
};

pub(crate) fn negotiate(protocol: u32, tag: Option<&str>) -> Locale {
    tag.filter(|_| protocol >= LOCALE_PROTOCOL)
        .and_then(Locale::supported)
        .unwrap_or(Locale::JAPANESE)
}

pub(crate) fn render(locale: Locale, message: &LocalizedMessage) -> String {
    if message.key == "command-help" {
        let backend = if message.args.get("backend").is_some_and(|backend| backend == "codex") {
            fuzukue_proto::AgentBackend::Codex
        } else {
            fuzukue_proto::AgentBackend::Claude
        };
        return fuzukue_proto::slash::COMMANDS.iter().filter(|spec| spec.available(backend))
            .map(|spec| format!("{} — {}", spec.usage(), locale.text(spec.description)))
            .collect::<Vec<_>>().join("\n");
    }
    if message.args.is_empty() {
        return locale.text(&message.key).into();
    }
    let mut args = FluentArgs::new();
    for (key, value) in &message.args {
        let value = if message.key == "command-value" && key == "value" && value.is_empty() {
            locale.text("command-default")
        } else {
            value.as_str()
        };
        args.set(key.as_str(), value);
    }
    locale.format(&message.key, &args)
}

fn diagnostic(locale: Locale, key: &str, detail: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("detail", detail);
    locale.format(key, &args)
}

/// Localize at delivery so live events and replay use the requesting connection's language.
pub(crate) fn event(event: &ServerEvent, locale: Locale, protocol: u32) -> Option<ServerEvent> {
    if let ServerEvent::ApprovalRequested {
        session,
        request_id,
        actor,
        localized: Some(message),
        shell_directory,
        shell_prefix,
        ..
    } = event
    {
        return Some(ServerEvent::ApprovalRequested {
            session: session.clone(),
            request_id: request_id.clone(),
            actor: actor.clone(),
            summary: render(locale, message),
            localized: (protocol >= LOCALE_PROTOCOL).then(|| message.clone()),
            shell_directory: shell_directory.clone(),
            shell_prefix: shell_prefix.clone(),
        });
    }
    if let ServerEvent::LocalNotice { session, item, localized: Some(message), .. } = event {
        return Some(ServerEvent::LocalNotice {
            session: session.clone(), item: item.clone(), text: render(locale, message),
            localized: Some(message.clone()),
        });
    }
    if protocol < LOCALE_PROTOCOL {
        return None;
    }
    Some(match event {
        ServerEvent::Error { message } => ServerEvent::Error {
            message: match message.as_str() {
                "スレッドが見つからない" => {
                    locale.text("server-thread-not-found").into()
                }
                "スレッドは実行中" | "Codex は実行中" => {
                    locale.text("server-thread-running").into()
                }
                "メッセージを入力する" => locale.text("server-message-required").into(),
                "添付が多すぎます" => locale.text("server-too-many-attachments").into(),
                "送るコメントがない" => locale.text("server-no-comments").into(),
                "codex app-server exited" => locale.text("server-codex-exited").into(),
                _ => operation_error(locale, message),
            },
        },
        ServerEvent::WorkspaceError { message } => ServerEvent::WorkspaceError {
            message: diagnostic(locale, "server-workspace-error", message),
        },
        ServerEvent::TestSuites {
            suites,
            error: Some(error),
        } => ServerEvent::TestSuites {
            suites: suites.clone(),
            error: Some(diagnostic(locale, "server-test-discovery-error", error)),
        },
        ServerEvent::GitResult {
            id,
            outcome: GitOutcome::Error { message },
        } => ServerEvent::GitResult {
            id: *id,
            outcome: GitOutcome::Error {
                message: diagnostic(locale, "server-git-error", message),
            },
        },
        ServerEvent::InboxFiled {
            path,
            destination,
            error: Some(error),
        } => ServerEvent::InboxFiled {
            path: path.clone(),
            destination: destination.clone(),
            error: Some(diagnostic(locale, "server-inbox-error", error)),
        },
        ServerEvent::BuildStatus {
            deck,
            state,
            message,
            pdf,
        } => {
            let key = match (state.as_str(), message.as_str()) {
                ("running", "PDF を作っている…") => "server-building-pdf",
                ("done", "PDF を作った") => "server-built-pdf",
                ("cancelled", "止めた") => "server-job-cancelled",
                _ => return None,
            };
            ServerEvent::BuildStatus {
                deck: deck.clone(),
                state: state.clone(),
                message: locale.text(key).into(),
                pdf: pdf.clone(),
            }
        }
        ServerEvent::ReviewStatus {
            deck,
            kind,
            state,
            message,
            model,
            details,
            started_at,
            count,
            card,
        } => {
            let text = match state.as_str() {
                "running" => locale.text("server-review-running").into(),
                "cancelled" => locale.text("server-job-cancelled").into(),
                "superseded" => locale.text("server-review-superseded").into(),
                "done" if count.is_some() => {
                    let mut args = FluentArgs::new();
                    args.set("count", count.unwrap());
                    locale.format("paper-findings", &args)
                }
                "failed" => diagnostic(locale, "server-review-error", message),
                "skipped" => diagnostic(locale, "server-review-skipped", message),
                _ => return None,
            };
            ServerEvent::ReviewStatus {
                deck: deck.clone(),
                kind: kind.clone(),
                state: state.clone(),
                message: text,
                model: model.clone(),
                details: details.clone(),
                started_at: *started_at,
                count: *count,
                card: card.clone(),
            }
        }
        ServerEvent::CompactStatus {
            session,
            state,
            message,
        } => {
            let text = match state.as_str() {
                "running" => locale.text("server-compacting").into(),
                "done" => locale.text("server-compacted").into(),
                "failed" => diagnostic(locale, "server-compaction-error", message),
                _ => return None,
            };
            ServerEvent::CompactStatus {
                session: session.clone(),
                state: state.clone(),
                message: text,
            }
        }
        ServerEvent::GitResult {
            id,
            outcome: GitOutcome::Done { message },
        } => {
            let text = match message.as_str() {
                "プッシュした" => locale.text("server-git-pushed").into(),
                "取り込みが完了した" => locale.text("server-git-pulled").into(),
                _ => diagnostic(
                    locale,
                    "server-git-committed",
                    message.strip_prefix("コミットした · ")?,
                ),
            };
            ServerEvent::GitResult {
                id: *id,
                outcome: GitOutcome::Done { message: text },
            }
        }
        // These system transcript entries predate v22. Match only the app-owned item
        // kinds and headings, leaving the text after the heading as authored.
        ServerEvent::TextDelta {
            session,
            item,
            actor: Actor::System,
            text,
        } if item.starts_with(PLAN_NOTE) => {
            let note = text.strip_prefix("方針を更新: ")?;
            let note = match note {
                "完了条件を満たした" => locale.text("server-plan-done").into(),
                "確認したい点がある" => locale.text("server-plan-waiting").into(),
                _ => diagnostic(locale, "server-plan-next", note.strip_prefix("次は ")?),
            };
            ServerEvent::TextDelta {
                session: session.clone(),
                item: item.clone(),
                actor: Actor::System,
                text: diagnostic(locale, "server-plan-updated", &note),
            }
        }
        ServerEvent::TextDelta {
            session,
            item,
            actor: Actor::System,
            text,
        } if item.starts_with(DELEGATE_JOB) && item.contains(":desk:") => {
            let (heading, body) = text.split_once("\n\n")?;
            let key = match heading {
                "**作業役から受付への質問**" => "server-worker-question",
                "**受付からの回答**" => "server-front-desk-reply",
                _ => return None,
            };
            ServerEvent::TextDelta {
                session: session.clone(),
                item: item.clone(),
                actor: Actor::System,
                text: format!("**{}**\n\n{body}", locale.text(key)),
            }
        }
        ServerEvent::ToolCall {
            session,
            item,
            actor: Actor::System,
            name,
            input,
        } if item.starts_with(DELEGATE_JOB) && name == "delegate" => {
            let backend = input.get("model").and_then(|model| model.as_str()).and_then(|model| model.strip_suffix(" 既定"));
            let waiting = input.get("waiting_localized")
                .and_then(|message| serde_json::from_value::<LocalizedMessage>(message.clone()).ok());
            if backend.is_none() && waiting.is_none() {
                return None;
            }
            let mut input = input.clone();
            if let Some(backend) = backend {
                input["model"] = diagnostic(locale, "server-default-model", backend).into();
            }
            if let Some(waiting) = waiting {
                input["waiting"] = render(locale, &waiting).into();
                input.as_object_mut()?.remove("waiting_localized");
            }
            ServerEvent::ToolCall {
                session: session.clone(),
                item: item.clone(),
                actor: Actor::System,
                name: name.clone(),
                input,
            }
        }
        ServerEvent::Paused {
            session,
            until,
            reason,
            queued,
        } => {
            let text = match reason.as_str() {
                "休憩中" => locale.text("server-paused").into(),
                "Claude が使用量の上限で止まった" => {
                    locale.text("server-claude-rate-limit").into()
                }
                reason => {
                    let (backend, usage) = reason.split_once(" の ")?;
                    let (window, percent) = usage.split_once(" 枠が ")?;
                    let mut args = FluentArgs::new();
                    args.set("backend", backend);
                    args.set("window", window);
                    args.set("percent", percent);
                    locale.format("server-rate-window", &args)
                }
            };
            ServerEvent::Paused {
                session: session.clone(),
                until: *until,
                reason: text,
                queued: *queued,
            }
        }
        _ => return None,
    })
}

fn operation_error(locale: Locale, message: &str) -> String {
    if let Some((prefix, detail)) = message.split_once(": ") {
        let key = match prefix {
            "方針の更新" => return locale.text("server-plan-update-error").into(),
            "目標の継続" => return locale.text("server-goal-continuation-error").into(),
            "方針の見直し" => "server-plan-review-error",
            "受付への連絡" => "server-front-desk-error",
            "スレッドを保存できない" => "server-thread-save-error",
            "作業役の集計" => "server-agent-usage-error",
            "resume failed" => "server-resume-error",
            "auto approve" => "server-auto-approval-error",
            "bad message" => "server-invalid-message",
            "Codex" | "codex" => "server-codex-error",
            _ => return diagnostic(locale, "server-request-error", message),
        };
        return diagnostic(locale, key, detail);
    }
    diagnostic(locale, "server-request-error", message)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parallel_wait_is_localized_in_delivered_delegate_activity() {
        use crate::delegate::admission::WaitReason;
        use fuzukue_proto::GoalPace;
        let reason = WaitReason::PlannerPace { pace: GoalPace::Normal, running: 2, limit: 2, delay_ms: None };
        let activity = ServerEvent::ToolCall {
            session: "thread".into(), item: format!("{DELEGATE_JOB}test"), actor: Actor::System,
            name: "delegate".into(),
            input: serde_json::json!({
                "model": "claude-sonnet-4-6",
                "waiting_localized": reason.localized().unwrap(),
            }),
        };
        for (locale, expected) in [
            (Locale::JAPANESE, "並行数の上限に達している（2/2 件が実行中。このスレッドの設定: 標準）"),
            (Locale::supported("en").unwrap(), "Parallel limit reached (2 of 2 running; this thread's setting: normal)"),
        ] {
            let Some(ServerEvent::ToolCall { input, .. }) = event(&activity, locale, LOCALE_PROTOCOL) else {
                panic!("delegate activity was not localized");
            };
            assert_eq!(input["waiting"], expected);
            assert!(input.get("waiting_localized").is_none());
        }
        assert_eq!(reason.message(), "goal pacing limit reached (2/2 running)");
    }

    use super::*;

    #[test]
    fn legacy_planner_errors_omit_diagnostics() {
        let en = Locale::supported("en").unwrap();
        let raw = "Verification rejected the attempt: unknown field `goal`";
        assert_eq!(operation_error(en, &format!("方針の更新: {raw}")), "Couldn’t update the plan");
        assert_eq!(operation_error(en, &format!("目標の継続: {raw}")), "Couldn’t continue the goal");
        assert_eq!(operation_error(Locale::JAPANESE, &format!("目標の継続: {raw}")), "目標を継続できませんでした");
    }

    #[test]
    fn approval_localizes_per_connection_and_gates_metadata() {
        let approval = ServerEvent::ApprovalRequested {
            session: "s".into(),
            request_id: "1".into(),
            actor: Actor::System,
            summary: "original".into(),
            shell_directory: None,
            shell_prefix: None,
            localized: Some(LocalizedMessage::new(
                "approval-run-command",
                [("command", "git status".into()), ("reason", String::new())],
            )),
        };
        for (protocol, expected, structured) in [
            (21, "コマンドの実行: git status", false),
            (22, "Run command: git status", true),
        ] {
            let locale = negotiate(protocol, Some("en-US"));
            let Some(ServerEvent::ApprovalRequested {
                summary, localized, ..
            }) = event(&approval, locale, protocol)
            else {
                panic!("expected an approval");
            };
            assert_eq!(summary, expected);
            assert_eq!(localized.is_some(), structured);
        }
    }
}
