//! Current waiting reasons for the chat footer.

use std::collections::{BTreeMap, HashSet};

use fuzukue_proto::{
    AgentBackend, ChatWaitReason, ChatWaiting, GoalProgress, SelfUpdateCanaryState,
    SelfUpdateStatus, SelfUpdateStepKind as Step, SelfUpdateStepState,
};

use crate::Workspace;

#[derive(Clone, Copy, Default)]
struct Waiting {
    turn_running: bool,
    compacting: bool,
    usage: Option<Option<i64>>,
    update: Option<ChatWaitReason>,
    worker: bool,
    test: bool,
    build: bool,
    queued: bool,
    goal: bool,
    static_goal: bool,
    watch: bool,
}

impl Waiting {
    fn reason(self) -> Option<ChatWaitReason> {
        use ChatWaitReason::*;

        if self.compacting {
            Some(Compacting)
        } else if self.turn_running || self.static_goal {
            None
        } else {
            self.usage
                .map(|resume_at| UsagePaused { resume_at })
                .or(self.update)
                .or(self.worker.then_some(Worker))
                .or(self.test.then_some(BackgroundTest))
                .or(self.build.then_some(BackgroundBuild))
                .or(self.queued.then_some(QueuedTurn))
                .or(self.goal.then_some(Goal))
                .or(self.watch.then_some(Watch))
        }
    }
}

pub(crate) fn update_reason(status: &SelfUpdateStatus) -> Option<ChatWaitReason> {
    if !status.active {
        return None;
    }
    if status
        .canary_status
        .as_ref()
        .is_some_and(|trial| trial.state == SelfUpdateCanaryState::Running)
    {
        return Some(ChatWaitReason::SelfUpdateCanaryApproval);
    }
    let stage = status
        .steps
        .iter()
        .find(|step| step.state == SelfUpdateStepState::Running)
        .or_else(|| {
            status
                .steps
                .iter()
                .find(|step| step.state == SelfUpdateStepState::Pending)
        })
        .map_or(Step::Swap, |step| step.kind);
    Some(ChatWaitReason::SelfUpdate { stage })
}

#[derive(Default)]
pub(crate) struct Feed {
    last: Option<ChatWaiting>,
    claude_queued: HashSet<String>,
}

impl Feed {
    pub(crate) fn latest(
        &mut self,
        app: &Workspace,
        update: Option<ChatWaitReason>,
    ) -> Option<ChatWaiting> {
        let (running, paused) = app.hub.chat_activity();
        let watches = if crate::canary() {
            HashSet::new()
        } else {
            app.watches.waiting_threads()
        };
        let (watched_threads, unbound_watch) = app.hub.read_threads(|store| {
            watches
                .into_iter()
                .partition::<Vec<_>, _>(|session| store.is_live(session))
        });
        let workspace = Waiting {
            update,
            test: app.has_running_tests(),
            build: app.active_build().is_some(),
            watch: !unbound_watch.is_empty(),
            ..Waiting::default()
        };
        let mut threads = BTreeMap::new();
        let mut goal_status = BTreeMap::new();
        // Sample Claude without waiting on its I/O. Keep the last queue state
        // while a turn transition holds the lock.
        if let Ok(sessions) = app.sessions.try_lock() {
            self.claude_queued
                .retain(|session| sessions.contains_key(session));
            for (session, claude) in sessions.iter() {
                match claude.swap_activity() {
                    Some((_, true, _)) => {
                        self.claude_queued.insert(session.clone());
                    }
                    Some((_, false, _)) => {
                        self.claude_queued.remove(session);
                    }
                    None => {}
                }
            }
        }
        for session in &self.claude_queued {
            threads.entry(session.clone()).or_insert(workspace).queued = true;
        }
        for session in app.delegates.queued_desk_sessions() {
            let backend = AgentBackend::for_model(app.chat_settings(&session).model.as_deref());
            let resume_at = app
                .usage
                .blocked(backend.usage_key())
                .map(|(until, _)| until);
            let waiting = threads.entry(session).or_insert(workspace);
            waiting.queued = true;
            waiting.usage = resume_at.map(Some);
        }
        for (session, progress, status) in app.plans.waiting_goals() {
            if let Some(status) = status {
                goal_status.insert(session.clone(), status);
                threads.entry(session).or_insert(workspace).static_goal = true;
                continue;
            }
            let waiting = threads.entry(session).or_insert(workspace);
            waiting.goal = true;
            if let Some(GoalProgress::Paused { resume_at }) = progress {
                waiting.usage.get_or_insert(resume_at);
            }
        }
        for (session, until) in paused {
            threads.entry(session).or_insert(workspace).usage = Some(Some(until));
        }
        for session in app.delegates.running_sessions() {
            threads.entry(session).or_insert(workspace).worker = true;
        }
        for session in watched_threads {
            threads.entry(session).or_insert(workspace).watch = true;
        }
        for session in running {
            goal_status.remove(&session);
            threads.entry(session).or_insert(workspace).turn_running = true;
        }

        for session in app.hub.compaction_sessions() {
            goal_status.remove(&session);
            threads.entry(session).or_insert(workspace).compacting = true;
        }

        let workspace = workspace.reason();
        let waiting = ChatWaiting {
            workspace,
            goal_status,
            threads: threads
                .into_iter()
                .filter_map(|(session, waiting)| {
                    let reason = waiting.reason();
                    (reason != workspace).then_some((session, reason))
                })
                .collect(),
        };
        if self.last.as_ref() == Some(&waiting) {
            return None;
        }
        self.last = Some(waiting.clone());
        Some(waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_reason_priority_and_idle_state() {
        use ChatWaitReason::*;

        let mut waiting = Waiting {
            usage: Some(Some(123)),
            update: Some(SelfUpdate { stage: Step::Build }),
            worker: true,
            test: true,
            build: true,
            queued: true,
            goal: true,
            watch: true,
            ..Waiting::default()
        };
        assert_eq!(
            waiting.reason(),
            Some(UsagePaused {
                resume_at: Some(123)
            })
        );
        waiting.usage = Some(None);
        assert_eq!(waiting.reason(), Some(UsagePaused { resume_at: None }));
        waiting.usage = None;
        assert_eq!(waiting.reason(), Some(SelfUpdate { stage: Step::Build }));
        waiting.update = None;
        assert_eq!(waiting.reason(), Some(Worker));
        waiting.worker = false;
        assert_eq!(waiting.reason(), Some(BackgroundTest));
        waiting.test = false;
        assert_eq!(waiting.reason(), Some(BackgroundBuild));
        waiting.build = false;
        assert_eq!(waiting.reason(), Some(QueuedTurn));
        waiting.queued = false;
        assert_eq!(waiting.reason(), Some(Goal));
        waiting.goal = false;
        assert_eq!(waiting.reason(), Some(Watch));
        waiting.watch = false;
        assert_eq!(waiting.reason(), None);
    }

    #[test]
    fn running_turn_suppresses_waiting_even_during_a_usage_pause() {
        let waiting = Waiting {
            turn_running: true,
            usage: Some(Some(123)),
            worker: true,
            ..Waiting::default()
        };
        assert_eq!(waiting.reason(), None);
    }

    #[test]
    fn feed_replaces_session_overrides_and_sends_only_changes() {
        use fuzukue_proto::ServerEvent;
        use std::sync::Arc;

        let fixture = crate::test_support::TestDir::new();
        let app = Workspace::new(
            fixture.path().to_owned(),
            String::new(),
            fixture.path().join("state"),
            Arc::new(crate::Hub::new()),
        );
        let mut feed = Feed::default();
        assert_eq!(feed.latest(&app, None), Some(ChatWaiting::default()));
        assert_eq!(feed.latest(&app, None), None);

        let session = "thread-fixture".to_owned();
        let update = Some(ChatWaitReason::SelfUpdate { stage: Step::Build });
        app.hub.publish(ServerEvent::TurnActivity {
            session: session.clone(),
            running: true,
        });
        assert_eq!(
            feed.latest(&app, update),
            Some(ChatWaiting {
                workspace: update,
                threads: BTreeMap::from([(session.clone(), None)]),
                ..Default::default()
            })
        );

        app.hub.publish(ServerEvent::TurnActivity {
            session: session.clone(),
            running: false,
        });
        app.hub.publish(ServerEvent::Paused {
            session: session.clone(),
            until: 123,
            reason: String::new(),
            queued: 1,
        });
        assert_eq!(
            feed.latest(&app, update).unwrap().threads,
            BTreeMap::from([(
                session.clone(),
                Some(ChatWaitReason::UsagePaused {
                    resume_at: Some(123)
                })
            ),])
        );
        app.hub.publish(ServerEvent::Resumed { session });
        assert!(feed.latest(&app, update).unwrap().threads.is_empty());
        assert_eq!(feed.latest(&app, None), Some(ChatWaiting::default()));
    }

    #[test]
    fn self_update_only_reports_the_current_stage_of_active_updates() {
        let step = |kind, state| fuzukue_proto::SelfUpdateStep {
            kind,
            state,
            log: String::new(),
        };
        let mut status = SelfUpdateStatus {
            active: true,
            steps: vec![
                step(Step::Build, SelfUpdateStepState::Passed),
                step(Step::Test, SelfUpdateStepState::Running),
                step(Step::Wait, SelfUpdateStepState::Pending),
            ],
            ..Default::default()
        };
        assert_eq!(
            update_reason(&status),
            Some(ChatWaitReason::SelfUpdate { stage: Step::Test })
        );
        status.steps[1].state = SelfUpdateStepState::Passed;
        assert_eq!(
            update_reason(&status),
            Some(ChatWaitReason::SelfUpdate { stage: Step::Wait })
        );
        status.active = false;
        assert_eq!(update_reason(&status), None);
    }
}
