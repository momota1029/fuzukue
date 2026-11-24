use std::time::{Duration, Instant};

use fuzukue_proto::{
    ClientMsg, SelfUpdateBlocker, SelfUpdateBlockerKind as BlockerKind, SelfUpdateCanaryAction,
    SelfUpdateCanaryState as Canary, SelfUpdateStatus, SelfUpdateStepKind as Step,
    SelfUpdateStepState as State, SelfUpdateWait, SelfUpdateWaitAction,
};
use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, Sizable as _, WindowExt as _,
};
use gpui_kit::{div, prelude::*, px, relative, Context, Div, Task, Window};

use crate::{chat::chat_activity::status_row, Workbench};
use fuzukue_i18n::STATUS_SEPARATOR;

#[derive(Default)]
pub(super) struct Panel {
    pub status: Option<SelfUpdateStatus>,
    expanded: Vec<Step>,
    wait_clock: Option<Task<()>>,
    received_at: Option<Instant>,
}

impl Workbench {
    pub(super) fn update_self_update(&mut self, status: SelfUpdateStatus, cx: &mut Context<Self>) {
        self.self_update.received_at = status.wait.as_ref().map(|_| Instant::now());
        if status.wait.is_none() {
            self.self_update.wait_clock = None;
        } else if self.self_update.wait_clock.is_none() {
            self.self_update.wait_clock = Some(cx.spawn(async move |view, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }));
        }
        self.self_update.status = Some(status);
    }

    pub(super) fn render_self_update(&self, cx: &Context<Self>) -> Option<Div> {
        let status = self.self_update.status.as_ref()?;
        let connected = matches!(self.status, crate::ConnectionStatus::Connected);
        let idle = connected && !status.active && !status.canary;
        let unknown = tr!("self-update-unknown");
        let build = if status.build_id.is_empty() {
            unknown
        } else {
            &status.build_id
        };
        let commit = status.source_commit.as_deref().unwrap_or(unknown);
        let muted = cx.theme().muted_foreground;
        Some(
            v_flex()
                .gap_2()
                .pt_3()
                .child(div().font_semibold().child(tr!("self-update-heading")))
                .child(
                    div()
                        .text_sm()
                        .child(tr!("self-update-build-id", id = build)),
                )
                .child(
                    div()
                        .text_sm()
                        .child(tr!("self-update-source-commit", id = commit)),
                )
                .children(status.canary_status.as_ref().map(|canary| {
                    let deadline = canary
                        .auto_discard_at
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                        .map(|time| {
                            time.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        });
                    v_flex()
                        .id("self-update-canary")
                        .gap_1()
                        .child(status_row(&tr!(
                            "self-update-canary-state",
                            state = canary_state(canary.state)
                        ), muted).text_sm().font_semibold())
                        .children(
                            canary
                                .connect_url
                                .as_ref()
                                .map(|url| div().text_sm().child(url.clone())),
                        )
                        .children(deadline.filter(|_| canary.state == Canary::Running).map(
                            |time| {
                                div()
                                    .text_sm()
                                    .child(tr!("self-update-canary-expires", time = time.as_str()))
                            },
                        ))
                        .when(
                            self.protocol >= fuzukue_proto::SELF_UPDATE_CANARY_PROTOCOL,
                            |section| {
                                section.child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("self-update-canary-promote")
                                                .label(tr!("self-update-canary-promote"))
                                                .disabled(!connected || !can_promote(canary.state))
                                                .on_click(cx.listener(|this, _, _, _| {
                                                    let _ = this.out.send(
                                                        ClientMsg::SelfUpdateCanary {
                                                            action: SelfUpdateCanaryAction::Promote,
                                                        },
                                                    );
                                                })),
                                        )
                                        .child(
                                            Button::new("self-update-canary-discard")
                                                .label(tr!("self-update-canary-discard"))
                                                .disabled(!connected || !can_discard(canary.state))
                                                .on_click(cx.listener(|this, _, _, _| {
                                                    let _ = this.out.send(
                                                        ClientMsg::SelfUpdateCanary {
                                                            action: SelfUpdateCanaryAction::Discard,
                                                        },
                                                    );
                                                })),
                                        ),
                                )
                            },
                        )
                }))
                .children(
                    (!status.available && !status.canary)
                        .then(|| div().text_sm().child(tr!("self-update-no-source"))),
                )
                .when(!status.canary, |section| {
                    section.child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Button::new("self-update-start")
                                    .label(tr!("self-update-start"))
                                    .primary()
                                    .disabled(
                                        !idle
                                            || !status.available
                                            || status.source_commit.as_deref() == Some(build),
                                    )
                                    .on_click(cx.listener(|this, _, _, _| {
                                        let _ = this.out.send(ClientMsg::StartSelfUpdate);
                                    })),
                            )
                            .child(
                                Button::new("self-update-rollback")
                                    .label(tr!("self-update-rollback"))
                                    .disabled(!idle || !status.can_rollback)
                                    .on_click(cx.listener(|this, _, _, _| {
                                        let _ = this.out.send(ClientMsg::RollbackServer);
                                    })),
                            ),
                    )
                })
                .children(
                    status
                        .recovered
                        .then(|| div().text_sm().child(tr!("self-update-recovered"))),
                )
                .children(status.cancelled.then(|| div().text_sm().child(tr!("self-update-cancelled"))))
                .children(status.planner_work_held.map(|count| {
                    status_row(&tr!("self-update-planner-work-held", count = count), muted).text_sm()
                }))
                .children(status.error.as_ref().map(|error| {
                    div()
                        .text_sm()
                        .child(tr!("self-update-error", error = error.as_str()))
                }))
                .children(status.steps.iter().map(|step| {
                    let kind = step.kind;
                    let (id, label) = step_label(kind);
                    let expanded = self.self_update.expanded.contains(&kind);
                    v_flex()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(status_row(&tr!(
                                    "self-update-step",
                                    step = label,
                                    state = step_state(step.state)
                                ), muted).text_sm().flex_1())
                                .child(
                                    Button::new(format!("self-update-log-{id}"))
                                        .small()
                                        .ghost()
                                        .label(if expanded {
                                            tr!("self-update-hide-log")
                                        } else {
                                            tr!("self-update-show-log")
                                        })
                                        .disabled(step.log.is_empty())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let open = &mut this.self_update.expanded;
                                            if open.contains(&kind) {
                                                open.retain(|step| *step != kind);
                                            } else {
                                                open.push(kind);
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(status.wait.as_ref().filter(|_| kind == Step::Wait)
                            .map(|wait| self.render_self_update_wait(wait, connected, cx)))
                        .when((expanded || kind == Step::Wait && status.wait.is_some()) && !step.log.is_empty(), |row| {
                            row.child(
                                div()
                                    .id(format!("self-update-output-{id}"))
                                    .p_2()
                                    .text_xs()
                                    .bg(cx.theme().secondary)
                                    .child(step.log.clone()),
                            )
                        })
                })),
        )
    }

    fn render_self_update_wait(&self, wait: &SelfUpdateWait, connected: bool, cx: &Context<Self>) -> Div {
        let started_at = wait.started_at;
        let seconds = wait.elapsed_secs + self.self_update.received_at.map_or(0, |at| at.elapsed().as_secs());
        let reason = &wait.reason;
        let disabled = !connected || wait.forcing;
        let now = started_at.saturating_add((seconds as i64).saturating_mul(1000));
        let elapsed = |blocker: &SelfUpdateBlocker| now.saturating_sub(blocker.started_at).max(0) / 1000;
        v_flex()
            .min_w_0()
            .gap_1()
            .pb_2()
            .child(div().text_sm().child(tr!("self-update-wait-elapsed", seconds = seconds.to_string())))
            .children((seconds >= 300).then(|| div().text_sm().child(tr!("self-update-wait-stuck", minutes = seconds / 60))))
            .children(reason.threads.iter().enumerate().filter_map(|(index, thread)| {
                let mut blockers = reason.blockers.iter().filter(|blocker| {
                    blocker.workspace == thread.workspace && blocker.session.as_deref() == Some(thread.id.as_str())
                });
                // A timed turn and its thread state belong on the same row.
                let turn = blockers.clone().find(|blocker| blocker.kind == BlockerKind::Turn);
                let has_worker = blockers.any(|blocker| blocker.kind == BlockerKind::Worker);
                let states = [
                    (thread.running || turn.is_some(), tr!("self-update-wait-active-turn")),
                    (thread.worker && !has_worker, tr!("self-update-wait-worker")),
                    (thread.queued, tr!("self-update-wait-queued")),
                    (thread.paused, tr!("self-update-wait-paused")),
                    (thread.checking, tr!("self-update-wait-checking")),
                ];
                states.iter().any(|(show, _)| *show).then(|| {
                    let title = wait_title(thread.title.as_deref(), &thread.id)
                        .or_else(|| turn.and_then(|turn| wait_title(turn.title.as_deref(), &thread.id)))
                        .unwrap_or(tr!("self-update-wait-untitled-thread"));
                    wait_row(
                        &format!("self-update-wait-thread-{index}"), title,
                        self.wait_reference(&thread.workspace, Some(&thread.id)),
                        tr!("self-update-wait-kind-thread"),
                        states.into_iter().filter_map(|(show, state)| show.then_some(state)),
                        turn.map(elapsed), cx,
                    )
                })
            }))
            .children(reason.blockers.iter().enumerate().filter_map(|(index, blocker)| {
                let thread = reason.threads.iter().find(|thread| {
                    thread.workspace == blocker.workspace && blocker.session.as_deref() == Some(thread.id.as_str())
                });
                if blocker.kind == BlockerKind::Turn && thread.is_some() {
                    return None;
                }
                let (kind, state) = match blocker.kind {
                    BlockerKind::Turn => (tr!("self-update-wait-kind-thread"), tr!("self-update-wait-active-turn")),
                    BlockerKind::Worker => (tr!("self-update-wait-kind-worker"), tr!("self-update-wait-worker")),
                    BlockerKind::Approval => (tr!("self-update-wait-kind-approval"), tr!("self-update-wait-approval")),
                    BlockerKind::Question => (tr!("self-update-wait-kind-question"), tr!("self-update-wait-question")),
                    BlockerKind::Build => (tr!("self-update-wait-kind-build"), tr!("self-update-running")),
                    BlockerKind::Review => (tr!("self-update-wait-kind-review"), tr!("self-update-running")),
                    BlockerKind::Test => (tr!("self-update-wait-kind-test"), tr!("self-update-running")),
                };
                let title = if matches!(blocker.kind, BlockerKind::Build | BlockerKind::Review | BlockerKind::Test) {
                    // Older servers put the name of the build, review, or test suite in id alone.
                    blocker.title.as_deref().unwrap_or(&blocker.id)
                } else {
                    wait_title(blocker.title.as_deref(), &blocker.id)
                        .or_else(|| thread.and_then(|thread| wait_title(thread.title.as_deref(), &thread.id)))
                        .unwrap_or(match blocker.kind {
                            BlockerKind::Worker => tr!("self-update-wait-untitled-job"),
                            _ => tr!("self-update-wait-untitled-thread"),
                        })
                };
                Some(wait_row(
                    &format!("self-update-wait-blocker-{index}"), title,
                    self.wait_reference(&blocker.workspace, (title != blocker.id).then_some(&blocker.id)),
                    kind, [state], Some(elapsed(blocker)), cx,
                ))
            }))
            .children(reason.checking_sessions.then(|| div().text_sm().child(tr!("self-update-wait-checking"))))
            .children(reason.gate_held.then(|| div().text_sm().child(tr!("self-update-wait-gate"))))
            .children(wait.forcing.then(|| div().text_sm().child(tr!("self-update-wait-stopping"))))
            .when(self.protocol >= fuzukue_proto::SELF_UPDATE_WAIT_PROTOCOL, |section| {
                section.child(h_flex().gap_2().flex_wrap()
                    .child(Button::new("self-update-cancel")
                        .label(tr!("self-update-cancel"))
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, _, _| {
                            let _ = this.out.send(ClientMsg::SelfUpdateWait { started_at, action: SelfUpdateWaitAction::Cancel });
                        })))
                    .child(Button::new("self-update-force")
                        .label(tr!("self-update-force"))
                        .danger()
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_self_update_force(started_at, window, cx);
                        }))))
            })
    }

    fn wait_reference(&self, workspace: &str, id: Option<&str>) -> String {
        let workspace = self.workspaces.entries.iter().find(|entry| entry.id == workspace)
            .map(|entry| entry.title.as_str());
        id.map(short_wait_reference).into_iter().chain(workspace.map(str::to_owned))
            .collect::<Vec<_>>().join(STATUS_SEPARATOR)
    }

    fn confirm_self_update_force(&self, started_at: i64, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        let epoch = self.connection_epoch;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog.title(tr!("self-update-force-title"))
                .overlay_closable(false)
                .w(px(440.))
                .child(v_flex().gap_4()
                    .child(tr!("self-update-force-confirmation"))
                    .child(h_flex().justify_end().gap_2()
                        .child(Button::new("self-update-force-back").label(tr!("self-update-force-back"))
                            .on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("self-update-force-confirm").label(tr!("self-update-force-title")).danger()
                            .on_click(move |_, window, cx| {
                                if let Some(view) = view.upgrade() {
                                    let this = view.read(cx);
                                    if this.connection_epoch == epoch && matches!(this.status, crate::ConnectionStatus::Connected)
                                        && this.self_update.status.as_ref().and_then(|status| status.wait.as_ref())
                                            .is_some_and(|wait| wait.started_at == started_at && !wait.forcing)
                                    {
                                        let _ = this.out.send(ClientMsg::SelfUpdateWait { started_at, action: SelfUpdateWaitAction::Force });
                                    }
                                }
                                window.close_dialog(cx);
                            }))))
        });
    }
}

fn wait_title<'a>(title: Option<&'a str>, id: &str) -> Option<&'a str> {
    title.map(str::trim).filter(|title| !title.is_empty() && *title != id)
}

fn short_wait_reference(reference: &str) -> String {
    let mut chars = reference.chars();
    let mut short: String = chars.by_ref().take(16).collect();
    if chars.next().is_some() {
        short.push('…');
    }
    short
}

fn wait_row(
    id: &str,
    title: &str,
    reference: String,
    kind: &str,
    states: impl IntoIterator<Item = &'static str>,
    elapsed: Option<i64>,
    cx: &Context<Workbench>,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .id(id.to_owned())
        .min_w_0()
        .py_1()
        .gap_1()
        .child(
            h_flex().min_w_0().gap_2()
                .child(div().id(format!("{id}-title")).flex_1().min_w_0().text_sm().truncate().child(title.to_owned()))
                .children((!reference.is_empty()).then(|| {
                    div().max_w(relative(0.4)).min_w_0().truncate().text_xs()
                        .text_color(theme.muted_foreground).child(reference)
                })),
        )
        .child(
            h_flex().id(format!("{id}-status")).min_w_0().flex_wrap().gap_2().text_xs()
                .child(div().flex_none().whitespace_nowrap().px_2().rounded_md()
                    .bg(theme.secondary).text_color(theme.secondary_foreground).child(kind.to_owned()))
                .children(states.into_iter().map(|state| {
                    div().flex_none().whitespace_nowrap().child(state)
                }))
                .children(elapsed.map(|seconds| {
                    div().flex_none().whitespace_nowrap().text_color(theme.muted_foreground)
                        .child(tr!("self-update-wait-item-elapsed", seconds = seconds))
                })),
        )
}

fn step_label(step: Step) -> (&'static str, &'static str) {
    match step {
        Step::Check => ("check", tr!("self-update-check")),
        Step::Build => ("build", tr!("self-update-build")),
        Step::Test => ("test", tr!("self-update-test")),
        Step::Shot => ("shot", tr!("self-update-shot")),
        Step::Publish => ("publish", tr!("self-update-publish")),
        Step::Wait => ("wait", tr!("self-update-wait")),
        Step::Swap => ("swap", tr!("self-update-swap")),
    }
}

fn step_state(state: State) -> &'static str {
    match state {
        State::Pending => tr!("self-update-pending"),
        State::Running => tr!("self-update-running"),
        State::Passed => tr!("self-update-passed"),
        State::Failed => tr!("self-update-failed"),
        State::Skipped => tr!("self-update-skipped"),
    }
}

fn canary_state(state: Canary) -> &'static str {
    match state {
        Canary::Building => tr!("self-update-canary-building"),
        Canary::Testing => tr!("self-update-canary-testing"),
        Canary::Running => tr!("self-update-canary-running"),
        Canary::Promoting => tr!("self-update-canary-promoting"),
        Canary::Discarded => tr!("self-update-canary-discarded"),
        Canary::Failed => tr!("self-update-canary-failed"),
    }
}

fn can_promote(state: Canary) -> bool {
    state == Canary::Running
}

fn can_discard(state: Canary) -> bool {
    matches!(state, Canary::Building | Canary::Testing | Canary::Running)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_references_are_short_and_titles_ignore_id_placeholders() {
        let id = "delegate:5I6vcoHNj_GAzd5qUD8BI9ZtdO6twyj6CdDvAinIdu4";
        assert_eq!(short_wait_reference(id), "delegate:5I6vcoH…");
        assert_eq!(short_wait_reference("thread-preview"), "thread-preview");
        assert_eq!(short_wait_reference(&"検".repeat(17)), format!("{}…", "検".repeat(16)));
        assert_eq!(wait_title(Some(id), id), None);
        assert_eq!(wait_title(Some("  "), id), None);
        assert_eq!(wait_title(None, id), None);
        assert_eq!(wait_title(Some(" サーバーテスト "), id), Some("サーバーテスト"));
    }

    #[test]
    fn self_update_canary_controls_follow_the_state() {
        for (state, promote, discard) in [
            (Canary::Building, false, true),
            (Canary::Testing, false, true),
            (Canary::Running, true, true),
            (Canary::Promoting, false, false),
            (Canary::Discarded, false, false),
            (Canary::Failed, false, false),
        ] {
            assert_eq!(can_promote(state), promote);
            assert_eq!(can_discard(state), discard);
        }
    }
}
