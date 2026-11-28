//! Window close and the server shutdown it can trigger.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuzukue_proto::{
    ClientMsg, DrainProgress, DrainReason, DrainStatus, ShutdownWork, DRAIN_PROTOCOL,
    SAFE_CLOSE_PROTOCOL,
};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, WindowExt as _,
};
use gpui_kit::{div, prelude::*, px, Context, Task, Window};

use super::{ConnectionStatus, Workbench};

const DEFAULT_DEADLINE_SECS: u64 = 60;
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_THREADS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    Window,
    StopServer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    Close,
    Review,
    Drain,
}

fn close_plan(intent: Intent, last_window: bool, managed: bool, save: bool, jobs: usize) -> Plan {
    if intent == Intent::Window && (!last_window || !managed) {
        Plan::Close
    } else if jobs > 0 {
        Plan::Review
    } else if save || intent == Intent::StopServer {
        Plan::Drain
    } else {
        Plan::Close
    }
}

#[derive(Default)]
pub(super) struct SafeClose {
    pub(super) packaged_peer: Option<u64>,
    work: Option<ShutdownWork>,
    latest_drain: Option<DrainStatus>,
    flow: Option<Flow>,
    tick: Option<Task<()>>,
}

impl SafeClose {
    pub(super) fn update_draining(&self) -> Option<bool> {
        self.latest_drain.as_ref().map(|status| {
            status.reason == DrainReason::Update
                && matches!(status.progress, DrainProgress::Draining | DrainProgress::Forcing)
        })
    }
}

#[derive(Default)]
struct RunningWork {
    jobs: usize,
    threads: Vec<String>,
}

struct Flow {
    intent: Intent,
    packaged_peer: Option<u64>,
    work: RunningWork,
    visible: bool,
    phase: Phase,
    error: Option<String>,
    confirm_force: bool,
}

enum Phase {
    Review,
    Starting {
        since: Instant,
        previous_id: Option<i64>,
        cancel: bool,
        force: bool,
    },
    Draining {
        status: DrainStatus,
        observed_at: Instant,
        elapsed: Duration,
        // Only send CancelDrain for a shutdown started by this flow.
        started_here: bool,
        pending: Option<(Control, Instant)>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Cancel,
    Force,
}

enum Effect {
    None,
    Send(ClientMsg),
    Dismiss,
    Close,
}

impl Flow {
    fn new(intent: Intent, work: RunningWork, visible: bool) -> Self {
        Self {
            intent,
            packaged_peer: None,
            work,
            visible,
            phase: Phase::Review,
            error: None,
            confirm_force: false,
        }
    }

    fn begin(&mut self, previous: Option<&DrainStatus>, now: Instant, force: bool) -> Effect {
        if previous.is_some_and(|status| {
            status.reason == DrainReason::Shutdown
                && matches!(
                    status.progress,
                    DrainProgress::Ready | DrainProgress::TimedOut
                )
        }) {
            return Effect::Close;
        }
        if let Some(status) = previous.filter(|status| active_shutdown(status)) {
            self.observe(status.clone(), now, false);
            return if force {
                self.control(Control::Force, now)
            } else {
                Effect::None
            };
        }
        self.error = None;
        self.phase = Phase::Starting {
            since: now,
            previous_id: previous.map(|status| status.started_at),
            cancel: false,
            force,
        };
        Effect::Send(if force {
            ClientMsg::ForceShutdown { started_at: None }
        } else {
            ClientMsg::RequestDrain {
                reason: DrainReason::Shutdown,
            }
        })
    }

    fn observe(&mut self, status: DrainStatus, now: Instant, started_here: bool) {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_sub(Duration::from_millis(status.started_at.max(0) as u64));
        self.phase = Phase::Draining {
            status,
            observed_at: now,
            elapsed,
            started_here,
            pending: None,
        };
    }

    fn on_status(&mut self, next: &DrainStatus, now: Instant) -> Effect {
        if next.reason != DrainReason::Shutdown {
            return Effect::None;
        }
        let queued = match &mut self.phase {
            Phase::Starting {
                previous_id,
                cancel,
                force,
                ..
            } if *previous_id != Some(next.started_at) => {
                let force = *force;
                let queued = if force {
                    Some(Control::Force)
                } else if *cancel {
                    Some(Control::Cancel)
                } else {
                    None
                };
                self.observe(next.clone(), now, !force);
                queued
            }
            Phase::Draining { status, .. } if status.started_at == next.started_at => {
                status.clone_from(next);
                None
            }
            _ => return Effect::None,
        };
        match next.progress {
            DrainProgress::Ready | DrainProgress::TimedOut => Effect::Close,
            DrainProgress::Cancelled => Effect::Dismiss,
            DrainProgress::Failed => {
                self.phase = Phase::Review;
                self.error = Some(
                    next.message
                        .clone()
                        .unwrap_or_else(|| tr!("safe-close-failed").into()),
                );
                Effect::None
            }
            DrainProgress::Draining | DrainProgress::Forcing => match queued {
                Some(Control::Cancel) if next.can_cancel => self.control(Control::Cancel, now),
                Some(Control::Force) => {
                    if let Phase::Draining { pending, .. } = &mut self.phase {
                        *pending = Some((Control::Force, now));
                    }
                    Effect::None
                }
                _ => Effect::None,
            },
        }
    }

    fn control(&mut self, action: Control, now: Instant) -> Effect {
        match (&mut self.phase, action) {
            (Phase::Review, Control::Cancel) => Effect::Dismiss,
            (
                Phase::Starting {
                    cancel,
                    force: false,
                    ..
                },
                Control::Cancel,
            ) => {
                *cancel = true;
                Effect::None
            }
            (
                Phase::Draining {
                    pending: None,
                    started_here: false,
                    ..
                },
                Control::Cancel,
            ) => Effect::Dismiss,
            (
                Phase::Draining {
                    status, pending, ..
                },
                action,
            ) if pending.is_none() && (action == Control::Force || status.can_cancel) => {
                *pending = Some((action, now));
                self.error = None;
                self.confirm_force = false;
                let started_at = status.started_at;
                Effect::Send(match action {
                    Control::Cancel => ClientMsg::CancelDrain { started_at },
                    Control::Force => ClientMsg::ForceShutdown {
                        started_at: Some(started_at),
                    },
                })
            }
            _ => Effect::None,
        }
    }

    fn on_error(&mut self, message: String) {
        self.error = Some(message);
        match &mut self.phase {
            Phase::Starting { .. } => self.phase = Phase::Review,
            Phase::Draining { pending, .. } => *pending = None,
            Phase::Review => {}
        }
    }
}

fn active_shutdown(status: &DrainStatus) -> bool {
    status.reason == DrainReason::Shutdown
        && matches!(
            status.progress,
            DrainProgress::Draining | DrainProgress::Forcing
        )
}

impl Workbench {
    pub(super) fn track_window_close(window: &Window, cx: &Context<Self>) {
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| this.window_can_close(window, cx))
                .unwrap_or(true)
        });
    }

    pub(super) fn window_can_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.can_begin_close(cx) {
            return false;
        }
        let last = cx.windows().len() <= 1;
        let save = self.save_server_on_close(cx);
        let managed = last
            && (self.connection.manages_server(&self.http, cx)
                || (save && self.safe_close.packaged_peer.is_some()));
        if managed {
            let work = self.running_work();
            match close_plan(Intent::Window, last, managed, save, work.jobs) {
                Plan::Review => {
                    self.begin_close(Intent::Window, work, true, window, cx);
                    return false;
                }
                Plan::Drain if self.can_request_shutdown() => {
                    self.begin_close(Intent::Window, work, false, window, cx);
                    return false;
                }
                _ => {}
            }
        }
        self.appearance.save_window(window);
        if last {
            super::conn::Connection::close_parked_servers(cx);
        } else {
            self.connection.park_server(cx);
        }
        true
    }

    pub(super) fn can_request_shutdown(&self) -> bool {
        self.protocol >= DRAIN_PROTOCOL && matches!(self.status, ConnectionStatus::Connected)
    }

    pub(super) fn stop_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_begin_close(cx) || !self.can_request_shutdown() {
            return;
        }
        let work = self.running_work();
        let visible =
            close_plan(Intent::StopServer, false, false, false, work.jobs) == Plan::Review;
        self.begin_close(Intent::StopServer, work, visible, window, cx);
    }

    fn can_begin_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.safe_close.flow.is_some() {
            return false;
        }
        if self.source.has_unsaved() {
            self.workspace_error(tr!("menu-finish-edits-before-closing").into());
            cx.notify();
            return false;
        }
        true
    }

    fn running_work(&self) -> RunningWork {
        let mut work = RunningWork::default();
        let mut ids = std::collections::HashSet::new();
        let active_drain = self.safe_close.latest_drain.as_ref().filter(|status| {
            matches!(
                status.progress,
                DrainProgress::Draining | DrainProgress::Forcing
            )
        });
        work.jobs = self
            .safe_close
            .work
            .as_ref()
            .map_or(0, |work| work.jobs_running);
        if let Some(drain) = active_drain {
            work.jobs = work.jobs.max(drain.jobs_running);
        }
        let snapshots = active_drain
            .into_iter()
            .flat_map(|drain| &drain.blocking.threads)
            .chain(self.safe_close.work.iter().flat_map(|work| &work.threads));
        for thread in snapshots.filter(|thread| thread.running || thread.worker) {
            if ids.insert((thread.workspace.as_str(), thread.id.as_str())) {
                work.threads
                    .push(thread.title.clone().unwrap_or_else(|| thread.id.clone()));
            }
        }
        let workspace = self.workspaces.active.as_deref().unwrap_or("");
        let mut running: Vec<_> = self
            .chats
            .iter()
            .filter(|(_, chat)| chat.busy)
            .map(|(id, _)| id)
            .collect();
        running.sort();
        for id in running {
            if ids.insert((workspace, id)) {
                work.threads.push(
                    self.threads
                        .iter()
                        .find(|thread| thread.id == *id)
                        .map(super::thread_title)
                        .unwrap_or(id)
                        .to_owned(),
                );
            }
        }
        work.jobs = work.jobs.max(work.threads.len());
        work
    }

    fn begin_close(
        &mut self,
        intent: Intent,
        work: RunningWork,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut flow = Flow::new(intent, work, visible);
        flow.packaged_peer = self.safe_close.packaged_peer;
        self.safe_close.flow = Some(flow);
        if visible {
            self.show_close_dialog(window, cx);
        } else {
            self.send_shutdown(false, window, cx);
        }
    }

    fn send_shutdown(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flow) = &mut self.safe_close.flow else {
            return;
        };
        let effect = flow.begin(self.safe_close.latest_drain.as_ref(), Instant::now(), force);
        self.apply_close_effect(effect, window, cx);
        if self.safe_close.flow.is_some() && self.safe_close.tick.is_none() {
            self.safe_close.tick = Some(cx.spawn_in(window, async |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if !this
                    .update_in(cx, |this, window, cx| {
                        this.tick_safe_close(window, cx);
                        this.safe_close.flow.is_some()
                    })
                    .unwrap_or(false)
                {
                    break;
                }
            }));
        }
    }

    fn tick_safe_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flow) = &mut self.safe_close.flow else {
            return;
        };
        if !matches!(flow.phase, Phase::Review)
            && !matches!(self.status, ConnectionStatus::Connected)
            && self.connection.launched_server_stopped(&self.http, cx)
        {
            self.finish_close(window, cx);
            return;
        }
        let deadline = self
            .safe_close
            .work
            .as_ref()
            .map_or(DEFAULT_DEADLINE_SECS, |work| work.deadline_secs);
        let expired = match &flow.phase {
            Phase::Starting { since, .. } => since.elapsed() >= ACK_TIMEOUT,
            Phase::Draining {
                pending: Some((_, since)),
                ..
            } => since.elapsed() >= ACK_TIMEOUT,
            Phase::Draining {
                elapsed,
                observed_at,
                ..
            } if !matches!(self.status, ConnectionStatus::Connected) => {
                if elapsed.saturating_add(observed_at.elapsed())
                    >= Duration::from_secs(deadline).saturating_add(ACK_TIMEOUT)
                {
                    flow.phase = Phase::Review;
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if expired {
            self.safe_close_error(tr!("safe-close-no-response").into(), window, cx);
        }
        cx.notify();
    }

    pub(super) fn shutdown_work(&mut self, work: ShutdownWork) {
        self.safe_close.work = Some(work);
    }

    pub(super) fn safe_close_reconnected(&mut self) {
        if self.safe_close.flow.is_none() {
            self.safe_close.latest_drain = None;
        }
    }

    pub(super) fn reset_safe_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_close_effect(Effect::Dismiss, window, cx);
        self.safe_close = SafeClose::default();
    }

    pub(super) fn drain_progress(
        &mut self,
        status: DrainStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(flow) = self.safe_close.flow.as_mut().filter(|flow| {
            flow.packaged_peer.is_none() || flow.packaged_peer == self.safe_close.packaged_peer
        }) {
            let effect = flow.on_status(&status, Instant::now());
            if !flow.visible
                && (flow.error.is_some()
                    || matches!(flow.phase, Phase::Draining { .. })
                        && active_shutdown(&status)
                        && status.jobs_running > 0)
            {
                flow.visible = true;
                self.show_close_dialog(window, cx);
            }
            self.apply_close_effect(effect, window, cx);
        }
        self.safe_close.latest_drain = Some(status);
    }

    pub(super) fn owned_server_stopped(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .safe_close
            .flow
            .as_ref()
            .is_some_and(|flow| !matches!(flow.phase, Phase::Review))
        {
            self.apply_close_effect(Effect::Close, window, cx);
        }
    }

    pub(super) fn safe_close_error(
        &mut self,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(flow) = &mut self.safe_close.flow {
            flow.on_error(message);
            if !flow.visible {
                flow.visible = true;
                self.show_close_dialog(window, cx);
            }
            cx.notify();
        }
    }

    fn cancel_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flow) = &mut self.safe_close.flow else {
            return;
        };
        let effect = flow.control(Control::Cancel, Instant::now());
        self.apply_close_effect(effect, window, cx);
    }

    fn force_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.protocol < SAFE_CLOSE_PROTOCOL || !self.can_request_shutdown() {
            match self.connection.stop_launched_server(&self.http, cx) {
                Ok(()) => self.finish_close(window, cx),
                Err(error) => self.safe_close_error(
                    tr!("connection-cannot-stop-server", err = format!("{error:#}")),
                    window,
                    cx,
                ),
            }
        } else if let Some(flow) = &mut self.safe_close.flow {
            match flow.phase {
                Phase::Review => self.send_shutdown(true, window, cx),
                _ => {
                    let effect = flow.control(Control::Force, Instant::now());
                    self.apply_close_effect(effect, window, cx);
                }
            }
        }
    }

    fn apply_close_effect(&mut self, effect: Effect, window: &mut Window, cx: &mut Context<Self>) {
        match effect {
            Effect::None => {}
            Effect::Send(message) => {
                let sent = match self.safe_close.flow.as_ref().and_then(|flow| flow.packaged_peer) {
                    Some(peer) => self.safe_close.packaged_peer == Some(peer)
                        && self.connection.send_packaged_shutdown(peer, message),
                    None => self.out.send(message).is_ok(),
                };
                if !sent {
                    self.safe_close_error(tr!("safe-close-no-response").into(), window, cx);
                }
            }
            Effect::Dismiss => {
                if self.safe_close.flow.take().is_some_and(|flow| flow.visible) {
                    window.close_dialog(cx);
                }
                self.safe_close.tick = None;
            }
            Effect::Close => self.finish_close(window, cx),
        }
        cx.notify();
    }

    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flow) = self.safe_close.flow.take() else {
            return;
        };
        self.safe_close.tick = None;
        self.connection.reap_stopping_server(&self.http, cx);
        self.appearance.save_window(window);
        super::conn::Connection::close_parked_servers(cx);
        if flow.intent == Intent::StopServer && cx.windows().len() > 1 {
            cx.quit();
        } else {
            window.remove_window();
        }
    }

    fn show_close_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some((title, body)) = view
                .update(cx, |this, cx| this.render_safe_close(cx))
                .ok()
                .flatten()
            else {
                return dialog;
            };
            let view = view.clone();
            dialog
                .title(title)
                .w(px(500.))
                .overlay_closable(false)
                .close_button(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this
                            .safe_close
                            .flow
                            .as_ref()
                            .is_some_and(|flow| flow.confirm_force)
                        {
                            this.confirm_force(false, cx);
                        } else {
                            this.cancel_close(window, cx);
                        }
                    });
                    false
                })
                .child(body)
        });
    }

    fn render_safe_close(&self, cx: &mut Context<Self>) -> Option<(String, gpui_kit::Div)> {
        let flow = self.safe_close.flow.as_ref()?;
        let can_save = self.can_request_shutdown()
            && flow.packaged_peer.is_none_or(|peer| self.safe_close.packaged_peer == Some(peer));
        let can_force = (can_save && self.protocol >= SAFE_CLOSE_PROTOCOL)
            || self.connection.can_stop_launched_server(&self.http, cx);
        let colors = cx.theme();
        let button = |id, label| Button::new(id).label(label);
        let mut body = v_flex().gap_3();
        let mut buttons = h_flex().justify_end().gap_2().flex_wrap();
        let title;

        match &flow.phase {
            Phase::Review => {
                title = if flow.work.jobs == 0 {
                    tr!("menu-stop-server").into()
                } else {
                    tr!("safe-close-running-title", count = flow.work.jobs as u64)
                };
                for (index, thread) in flow.work.threads.iter().take(MAX_THREADS).enumerate() {
                    body = body.child(
                        div()
                            .id(("safe-close-thread", index))
                            .truncate()
                            .child(thread.clone()),
                    );
                }
                let more = flow.work.threads.len().saturating_sub(MAX_THREADS);
                if more > 0 {
                    body = body.child(
                        div()
                            .id("safe-close-more")
                            .child(tr!("safe-close-more", count = more as u64)),
                    );
                }
                if flow.intent == Intent::Window {
                    body = body.child(div().text_sm().child(tr!("safe-close-managed-warning")));
                }
                if !can_save || !can_force {
                    body = body.child(div().text_sm().child(tr!("safe-close-action-unavailable")));
                }
                buttons = buttons
                    .child(
                        button("safe-close-cancel", tr!("safe-close-cancel")).on_click(
                            cx.listener(|this, _, window, cx| this.cancel_close(window, cx)),
                        ),
                    )
                    .child(
                        button("safe-close-now", tr!("safe-close-now"))
                            .disabled(!can_force)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.force_close(window, cx)),
                            ),
                    )
                    .child(
                        button("safe-close-save", tr!("safe-close-save"))
                            .primary()
                            .disabled(!can_save)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_shutdown(false, window, cx)
                            })),
                    );
            }
            Phase::Starting { cancel, force, .. } => {
                title = tr!("safe-close-progress-title").into();
                body = body.child(if *force {
                    tr!("safe-close-stopping")
                } else if *cancel {
                    tr!("safe-close-cancelling")
                } else {
                    tr!("safe-close-starting")
                });
                buttons = buttons.child(
                    button("safe-close-cancel", tr!("safe-close-cancel"))
                        .disabled(*cancel || *force)
                        .on_click(cx.listener(|this, _, window, cx| this.cancel_close(window, cx))),
                );
            }
            Phase::Draining {
                status,
                observed_at,
                elapsed,
                started_here,
                pending,
            } => {
                title = tr!("safe-close-progress-title").into();
                let elapsed = elapsed.saturating_add(observed_at.elapsed()).as_secs();
                let deadline = self
                    .safe_close
                    .work
                    .as_ref()
                    .map_or(DEFAULT_DEADLINE_SECS, |work| work.deadline_secs);
                body = body.child(div().id("safe-close-progress").child(tr!(
                    "safe-close-progress",
                    jobs = status.jobs_running as u64,
                    elapsed = elapsed,
                    remaining = deadline.saturating_sub(elapsed)
                )));
                if let Some(message) = status.message.as_ref() {
                    body = body.child(
                        div()
                            .text_sm()
                            .max_h(px(96.))
                            .id("safe-close-detail")
                            .overflow_y_scroll()
                            .child(message.clone()),
                    );
                }
                if flow.confirm_force {
                    body = body.child(tr!("safe-close-force-confirmation"));
                    buttons = buttons
                        .child(
                            button("safe-close-force-back", tr!("safe-close-back")).on_click(
                                cx.listener(|this, _, _, cx| this.confirm_force(false, cx)),
                            ),
                        )
                        .child(
                            button("safe-close-force-confirm", tr!("safe-close-force"))
                                .danger()
                                .disabled(!can_force)
                                .on_click(cx.listener(
                                    |this, event: &gpui_kit::ClickEvent, window, cx| {
                                        if event.click_count() == 1 {
                                            this.force_close(window, cx);
                                        }
                                    },
                                )),
                        );
                } else {
                    if let Some((control, _)) = pending {
                        body = body.child(match control {
                            Control::Cancel => tr!("safe-close-cancelling"),
                            Control::Force => tr!("safe-close-stopping"),
                        });
                    }
                    buttons =
                        buttons
                            .child(
                                button("safe-close-cancel", tr!("safe-close-cancel"))
                                    .disabled(
                                        pending.is_some() || (*started_here && !status.can_cancel),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_close(window, cx)
                                    })),
                            )
                            .child(
                                button("safe-close-force", tr!("safe-close-force"))
                                    .danger()
                                    .disabled(pending.is_some() || !can_force)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.confirm_force(true, cx)),
                                    ),
                            );
                }
            }
        }
        if let Some(message) = &flow.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(colors.danger)
                    .child(message.clone()),
            );
        }
        Some((title, body.child(buttons)))
    }

    fn confirm_force(&mut self, confirm: bool, cx: &mut Context<Self>) {
        if let Some(flow) = &mut self.safe_close.flow {
            flow.confirm_force = confirm;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_only_applies_to_the_last_managed_window_or_an_explicit_stop() {
        for save in [true, false] {
            assert_eq!(
                close_plan(Intent::Window, true, false, save, 5),
                Plan::Close
            );
            assert_eq!(
                close_plan(Intent::Window, false, true, save, 5),
                Plan::Close
            );
            assert_eq!(
                close_plan(Intent::Window, true, true, save, 5),
                Plan::Review
            );
        }
        assert_eq!(
            close_plan(Intent::Window, true, true, false, 0),
            Plan::Close
        );
        assert_eq!(close_plan(Intent::Window, true, true, true, 0), Plan::Drain);
        assert_eq!(
            close_plan(Intent::StopServer, false, false, false, 5),
            Plan::Review
        );
        assert_eq!(
            close_plan(Intent::StopServer, false, false, false, 0),
            Plan::Drain
        );
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn safe_close_packaged_server_reviews_work_and_binds_shutdown_to_the_verified_socket() {
        use gpui_kit::AppContext as _;

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            crate::math::init(cx);
            let (window, view) = gpui_kit::open_window(
                gpui_kit::component::TitleBar::window_options(),
                cx,
                |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Fixture")))),
            ).unwrap();
            window.update(cx, |_, window, cx| view.update(cx, |view, cx| {
                let requests = view.connection.packaged_shutdown_fixture();
                view.status = ConnectionStatus::Connected;
                view.protocol = SAFE_CLOSE_PROTOCOL;
                view.shutdown_work(ShutdownWork { jobs_running: 1, threads: vec![], deadline_secs: 60 });

                // The loopback address alone confers no authority to stop a server.
                cx.global_mut::<crate::appearance::SaveServerOnClose>().0 = true;
                assert!(view.window_can_close(window, cx));

                view.safe_close.packaged_peer = Some(42);
                cx.global_mut::<crate::appearance::SaveServerOnClose>().0 = false;
                assert!(view.window_can_close(window, cx));

                cx.global_mut::<crate::appearance::SaveServerOnClose>().0 = true;
                assert!(!view.window_can_close(window, cx));
                assert!(matches!(view.safe_close.flow.as_ref().unwrap().phase, Phase::Review));
                assert!(requests.try_recv().is_err());
                view.cancel_close(window, cx);

                view.shutdown_work(ShutdownWork { jobs_running: 0, threads: vec![], deadline_secs: 60 });
                assert!(!view.window_can_close(window, cx));
                assert!(matches!(
                    requests.try_recv().unwrap(),
                    (42, ClientMsg::RequestDrain { reason: DrainReason::Shutdown })
                ));
                view.safe_close.packaged_peer = Some(43);
                view.apply_close_effect(Effect::Send(ClientMsg::ForceShutdown { started_at: None }), window, cx);
                assert!(requests.try_recv().is_err());
                assert!(view.safe_close.flow.as_ref().unwrap().error.is_some());
            })).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }

    #[test]
    fn cancel_before_ack_waits_for_the_shutdown_id_and_ignores_stale_status() {
        let mut flow = Flow::new(Intent::Window, RunningWork::default(), true);
        let mut status = DrainStatus {
            started_at: 1,
            reason: DrainReason::Shutdown,
            progress: DrainProgress::Cancelled,
            jobs_running: 2,
            blocking: Default::default(),
            can_cancel: false,
            message: None,
        };
        let now = Instant::now();
        assert!(matches!(
            flow.begin(Some(&status), now, false),
            Effect::Send(ClientMsg::RequestDrain {
                reason: DrainReason::Shutdown
            })
        ));
        assert!(matches!(flow.control(Control::Cancel, now), Effect::None));
        assert!(matches!(flow.on_status(&status, now), Effect::None));
        status.started_at = 2;
        status.reason = DrainReason::Update;
        assert!(matches!(flow.on_status(&status, now), Effect::None));
        status.reason = DrainReason::Shutdown;
        status.progress = DrainProgress::Draining;
        status.can_cancel = true;
        assert!(matches!(
            flow.on_status(&status, now),
            Effect::Send(ClientMsg::CancelDrain { started_at: 2 })
        ));
        status.progress = DrainProgress::Cancelled;
        assert!(matches!(flow.on_status(&status, now), Effect::Dismiss));
    }
}
