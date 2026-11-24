use std::collections::HashSet;

use fuzukue_proto::{
    AgentOutcome, DelegateReport, Envelope, NotificationKind as Kind, PlanStatus, ServerEvent,
};
use tokio::sync::broadcast;

use super::{Hub, Inner};

#[derive(Default)]
pub(super) struct State {
    blocked_goals: HashSet<String>,
    steered: HashSet<String>,
    feed: Option<(String, broadcast::Sender<Envelope>)>,
}

impl State {
    pub(super) fn goal_change(&mut self, event: &ServerEvent) -> Option<String> {
        let ServerEvent::PlanChanged { session, plan, .. } = event else {
            return None;
        };
        let blocked = !plan.paused_by_user
            && (plan.status == PlanStatus::WaitingForUser
                || plan.status == PlanStatus::Active
                    && !plan.next.is_empty()
                    && plan.next.iter().all(|step| {
                        step.blocker
                            .as_ref()
                            .is_some_and(|blocker| blocker.needs.is_some())
                    }));
        if blocked {
            self.blocked_goals
                .insert(session.clone())
                .then(|| session.clone())
        } else {
            self.blocked_goals.remove(session);
            None
        }
    }
}

pub(super) fn before_publish(inner: &Inner, event: &ServerEvent) -> Option<(String, Kind)> {
    match event {
        ServerEvent::ApprovalRequested {
            session,
            request_id,
            ..
        } if !inner.approvals.contains_key(request_id) => Some((session.clone(), Kind::Approval)),
        ServerEvent::ToolResult {
            session,
            item,
            ok,
            output,
        } if item.starts_with(fuzukue_proto::DELEGATE_JOB)
            && inner.workers.contains_key(&(session.clone(), item.clone())) =>
        {
            let kind = if *ok {
                Kind::WorkerFinished
            } else if serde_json::from_str::<DelegateReport>(output).is_ok_and(|report| {
                report
                    .attempts
                    .last()
                    .is_some_and(|attempt| attempt.outcome == AgentOutcome::Stalled)
            }) {
                Kind::WorkerStalled
            } else {
                Kind::WorkerFailed
            };
            Some((session.clone(), kind))
        }
        _ => None,
    }
}

pub(super) fn send_notice(inner: &Inner, session: &str, kind: Kind) {
    let Some((workspace, feed)) = &inner.notifications.feed else {
        return;
    };
    let title = inner
        .threads
        .as_ref()
        .and_then(|threads| threads.title(session))
        .filter(|title| !title.is_empty());
    let event = ServerEvent::Notify {
        session: session.into(),
        kind,
        title,
    };
    let _ = feed.send(Envelope {
        workspace: Some(workspace.clone()),
        seq: 0,
        event,
    });
}

impl Hub {
    pub(crate) fn attach_notification_feed(
        &self,
        workspace: &str,
        feed: broadcast::Sender<Envelope>,
    ) {
        self.inner.lock().unwrap().notifications.feed = Some((workspace.into(), feed));
    }

    pub(crate) fn mark_user_steer(&self, session: &str) {
        self.inner
            .lock()
            .unwrap()
            .notifications
            .steered
            .insert(session.into());
    }

    pub(crate) fn take_user_steer(&self, session: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .notifications
            .steered
            .remove(session)
    }

    pub(crate) fn notify_turn_finished(&self, session: &str, user_turn: bool, reply: &str) {
        if user_turn && !crate::front_desk::blank_reply(reply) {
            send_notice(&self.inner.lock().unwrap(), session, Kind::TurnFinished);
        }
    }

    pub(crate) fn notify_turn_failed(&self, session: &str) {
        send_notice(&self.inner.lock().unwrap(), session, Kind::TurnFailed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::Actor;

    #[test]
    fn notify_only_after_a_visible_user_turn_and_deliver_only_to_live_subscribers() {
        let hub = Hub::new();
        let (feed, mut live) = broadcast::channel(8);
        hub.attach_notification_feed("lab", feed.clone());
        hub.notify_turn_finished("thread", false, "A routine digest.");
        hub.notify_turn_finished("thread", true, "**`<silent/>`**");
        assert!(live.try_recv().is_err());
        hub.notify_turn_finished("thread", true, "Work finished.");
        let envelope = live.try_recv().unwrap();
        assert_eq!(envelope.workspace.as_deref(), Some("lab"));
        assert!(matches!(
            envelope.event,
            ServerEvent::Notify {
                kind: Kind::TurnFinished,
                ..
            }
        ));
        assert!(feed.subscribe().try_recv().is_err());
    }

    #[test]
    fn terminal_worker_events_and_live_approvals_only_notify_once() {
        let hub = Hub::new();
        let (feed, mut live) = broadcast::channel(8);
        hub.attach_notification_feed("lab", feed);
        let terminal = ServerEvent::ToolResult {
            session: "thread".into(),
            item: "delegate:1".into(),
            ok: true,
            output: String::new(),
        };
        hub.publish(terminal.clone());
        assert!(live.try_recv().is_err());
        hub.publish(ServerEvent::ToolCall {
            session: "thread".into(),
            item: "delegate:1".into(),
            actor: Actor::Claude,
            name: "delegate".into(),
            input: serde_json::json!({}),
        });
        hub.publish(terminal.clone());
        hub.publish(terminal);
        assert!(matches!(
            live.try_recv().unwrap().event,
            ServerEvent::Notify {
                kind: Kind::WorkerFinished,
                ..
            }
        ));
        let approval = ServerEvent::ApprovalRequested {
            session: "thread".into(),
            request_id: "approval".into(),
            actor: Actor::Claude,
            summary: "Private details".into(),
            localized: None, shell_directory: None, shell_prefix: None,
        };
        hub.publish(approval.clone());
        hub.publish(approval);
        assert!(matches!(
            live.try_recv().unwrap().event,
            ServerEvent::Notify {
                kind: Kind::Approval,
                ..
            }
        ));
        assert!(live.try_recv().is_err());
    }
}
