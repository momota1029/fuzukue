//! The activity details attached to the chat status strip.

use gpui_kit::base::{Align, HoverCard, HoverCardState, Placement, Scrollbar, ScrollbarMode};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{Div, KeyDownEvent, Role, ScrollHandle, Stateful, Subscription, WeakEntity};

use super::*;

const PANEL_MARGIN: Pixels = px(8.);
// Match the optical center of the spinner against the UI font's line box.
pub(crate) const STATUS_OPTICAL_OFFSET: Pixels = px(0.5);

fn status_sep(color: gpui_kit::Hsla) -> Div {
    div()
        .size(px(3.))
        .mx(px(6.))
        .mt(STATUS_OPTICAL_OFFSET * 2.)
        .flex_none()
        .rounded_full()
        .bg(color)
}

pub(crate) fn status_row(label: &str, color: gpui_kit::Hsla) -> Div {
    let mut parts = label.split(STATUS_SEPARATOR).filter(|part| !part.is_empty());
    let mut row = h_flex().min_w_0();
    if let Some(first) = parts.next() {
        row = row.child(div().min_w_0().child(first.to_owned()));
    }
    for part in parts {
        row = row
            .child(status_sep(color))
            .child(div().min_w_0().child(part.to_owned()));
    }
    row
}

pub(crate) fn status_speech(label: &str) -> String {
    label.replace(STATUS_SEPARATOR, ", ")
}

struct ActivityPanel {
    scroll: ScrollHandle,
    selected: Option<String>,
    expanded: Option<String>,
    _escape: Subscription,
}

impl ActivityPanel {
    fn toggle(&mut self, job: &str) {
        self.selected = Some(job.to_owned());
        self.expanded = (self.expanded.as_deref() != Some(job)).then(|| job.to_owned());
    }
}

struct WorkerActivity<'a> {
    title: &'a str,
    elapsed: Option<String>,
    calls: usize,
    last_tool: Option<&'a str>,
    selection: Option<&'a AgentSelection>,
    tier: Option<fuzukue_proto::Tier>,
    failed: bool,
    details: Option<WorkerActivityDetails<'a>>,
}

struct WorkerActivityDetails<'a> {
    last_activity: Option<&'a str>,
    latest_message: Option<&'a str>,
}

impl<'a> WorkerActivity<'a> {
    fn from_summary(summary: &'a JobSummary, expanded: bool, now_ms: u64) -> Self {
        Self {
            title: delegate_activity_name(&summary.job),
            elapsed: summary.elapsed_at(now_ms).map(|ms| activity_time(ms / 1000)),
            calls: summary.tools,
            last_tool: summary.last_tool.as_deref(),
            selection: summary.job.selection.as_ref(),
            tier: summary.job.routing.as_ref().map(|route| route.tier),
            failed: summary.had_tool_error || matches!(summary.status, JobStatus::Failed | JobStatus::Interrupted),
            details: expanded.then(|| WorkerActivityDetails {
                last_activity: summary.last_activity.as_deref(),
                latest_message: summary.latest_message.as_deref(),
            }),
        }
    }

    fn new(block: &'a DelegateBlock, expanded: bool, now_ms: u64) -> Self {
        let (loaded_calls, failed) = delegate_tools(block);
        let calls = block.deferred.as_ref().map_or(loaded_calls, |deferred| deferred.tools.max(loaded_calls));
        let saved_activity = block.report.as_ref().and_then(|(_, report)| report.tool_activity.as_ref());
        let calls = saved_activity.map_or(calls, |activity| activity.count.max(calls));
        let last_tool = block.progress.iter().rev().find_map(|entry| match entry {
            DelegateEntry::Tool(call) => Some(call.summary.as_str()),
            _ => None,
        }).or_else(|| saved_activity.and_then(|activity| activity.last_tool.as_deref()));
        let details = expanded.then(|| WorkerActivityDetails {
            last_activity: block.summary.as_deref().or(last_tool),
            latest_message: block.progress.iter().rev().find_map(|entry| match entry {
                DelegateEntry::Text(text) if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            }),
        });
        let attempt = block.report.as_ref().and_then(|(_, report)| report.attempts.last());
        let elapsed = if let Some((_, report)) = &block.report {
            (!report.interrupted).then_some(report.elapsed_ms / 1000)
        } else {
            delegate_elapsed(&block.job, now_ms)
                .or_else(|| block.job.queued_at_ms.map(|at| now_ms.saturating_sub(at.max(0) as u64) / 1000))
        };
        Self {
            title: delegate_activity_name(&block.job),
            elapsed: elapsed.map(activity_time),
            calls,
            last_tool,
            selection: attempt.map(|attempt| &attempt.selection).or(block.job.selection.as_ref()),
            tier: attempt.map(|attempt| attempt.tier).or(block.job.routing.as_ref().map(|route| route.tier)),
            failed: failed || block.report.as_ref().is_some_and(|(ok, _)| !ok),
            details,
        }
    }
}

fn job_chip(id: SharedString, text: &str, cx: &gpui_kit::App) -> Stateful<Div> {
    let theme = cx.theme();
    let text = text.to_owned();
    div().id(id).min_w_0().max_w_full().px_1().rounded_sm()
        .bg(theme.muted).text_color(theme.muted_foreground).text_xs().truncate().child(text)
}

fn job_metadata(job: &DelegateJob, worker: &WorkerActivity<'_>, index: usize, cx: &gpui_kit::App) -> Div {
    let selection = worker.selection;
    let model = selection.and_then(|selection| selection.model.as_deref())
        .or_else(|| (!job.model.is_empty()).then_some(job.model.as_str()));
    let backend = selection.map(|selection| selection.backend)
        .or_else(|| model.map(|model| AgentBackend::for_model(Some(model))));
    let effort = selection.and_then(|selection| selection.effort.as_deref());
    h_flex().flex_wrap().gap_1().min_w_0()
        .children([
            ("model", model.unwrap_or(tr!("chat-job-default"))),
            ("effort", effort.unwrap_or(tr!("chat-job-default"))),
            ("tier", worker.tier.map(tier_label).unwrap_or("—")),
            ("backend", backend.map(AgentBackend::label).unwrap_or("—")),
        ].into_iter().map(|(key, value)| {
            let accessibility_label = match key {
                "model" => tr!("chat-job-model", value = value),
                "effort" => tr!("chat-job-effort", value = value),
                "tier" => tr!("chat-job-tier", value = value),
                _ => tr!("chat-job-backend", value = value),
            };
            job_chip(activity_id(&format!("job-{key}-{index}")), value, cx)
                .aria_label(accessibility_label.clone())
                .tooltip(move |window, cx| Tooltip::new(accessibility_label.clone()).build(window, cx))
        }))
}

fn panel_max_height(window_height: Pixels, inset: Pixels) -> Pixels {
    (window_height * 0.6)
        .min(px(480.))
        .min((window_height - (PANEL_MARGIN + inset) * 2.).max(px(0.)))
}

fn activity_id(suffix: &str) -> SharedString {
    format!("chat-tail-activity-{suffix}").into()
}

fn command_elapsed(command: &BackgroundCommand, now_ms: u64) -> String {
    activity_time(now_ms.saturating_sub(command.started_at_ms) / 1000)
}

fn command_state(state: BackgroundCommandState) -> &'static str {
    match state {
        BackgroundCommandState::Running => tr!("chat-activity-command-running"),
        BackgroundCommandState::Completed => tr!("chat-activity-command-completed"),
        BackgroundCommandState::Failed => tr!("chat-activity-command-failed"),
        BackgroundCommandState::Stopped => tr!("chat-activity-command-stopped"),
    }
}

impl Workbench {
    fn show_job_report(
        view: WeakEntity<Self>, session: String, name: String, ok: bool, report: DelegateReport,
        window: &mut Window, cx: &mut gpui_kit::App,
    ) {
        let title = if ok { tr!("chat-final-report") } else { tr!("chat-failure-details") };
        let text = if report.interrupted { tr!("chat-worker-recovery").to_owned() } else { report.text };
        let changes = report.changes;
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(view) = view.upgrade() else { return dialog };
            let this = view.read(cx);
            let Some(chat) = this.chats.get(&session) else { return dialog };
            dialog.title(title).w(px(620.))
                .child(v_flex().id("job-report-dialog").gap_2()
                    .child(div().font_bold().child(name.clone()))
                    .child(this.chat_markdown("job-report-text".into(), fuzukue_view::link_card_refs(&text), chat, cx))
                    .children((!changes.is_empty()).then(|| div().whitespace_normal().child(changes.clone()))))
        });
    }

    pub(crate) fn finish_job_report(
        &mut self, session: &str, job: &str, request: u64, result: Option<(bool, DelegateReport)>,
        window: &mut Window, cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chats.get_mut(session) else { return };
        if chat.jobs.report_request.as_ref().is_none_or(|(id, item)| *id != request || item != job) {
            return;
        }
        chat.jobs.report_request = None;
        if self.active != session {
            return;
        }
        if let Some((ok, report)) = result {
            let name = chat.jobs.get(job).map(|summary| delegate_activity_name(&summary.job))
                .or_else(|| chat.delegate_block(job).map(|block| delegate_activity_name(&block.job)))
                .unwrap_or(job).to_owned();
            Self::show_job_report(cx.weak_entity(), session.into(), name, ok, report, window, cx);
        }
    }

    pub(super) fn rename_job_button(&self, session: &str, job_id: &str, metadata: &DelegateJob, id: SharedString) -> Button {
        let session = session.to_owned();
        let job = job_id.to_owned();
        let name = metadata.display.label_source.as_deref()
            .unwrap_or_else(|| delegate_activity_name(metadata)).to_owned();
        let out = self.out.clone();
        inline_button(id).label(tr!("chat-job-rename")).tab_stop(true)
            .on_click(move |_, window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).default_value(name.clone()));
                let (out, session, job) = (out.clone(), session.clone(), job.clone());
                let focus = input.read(cx).focus_handle(cx);
                window.open_dialog(cx, move |dialog, _, _| {
                    let (out, session, job, input) = (out.clone(), session.clone(), job.clone(), input.clone());
                    let read = input.clone();
                    let save = move |window: &mut Window, cx: &mut gpui_kit::App| {
                        let value = read.read(cx).value();
                        let label = value.trim();
                        if !label.is_empty() {
                            let _ = out.send(ClientMsg::RenameJob {
                                session: session.clone(), job: job.clone(), label: label.to_owned(),
                            });
                            window.close_dialog(cx);
                        }
                    };
                    let on_enter = save.clone();
                    dialog.title(tr!("chat-job-rename")).w(px(400.))
                        .child(v_flex().id("rename-job-dialog").gap_2()
                            .on_action(move |_: &Enter, window, cx| on_enter(window, cx))
                            .child(Input::new(&input).aria_label(tr!("chat-job-name")))
                            .child(div().text_sm().child(tr!("chat-job-name-help", columns = fuzukue_proto::JOB_LABEL_COLUMNS)))
                            .child(h_flex().justify_end().gap_1()
                                .child(Button::new("rename-job-cancel").ghost().tab_stop(true).label(tr!("chat-job-cancel"))
                                    .on_click(|_, window, cx| window.close_dialog(cx)))
                                .child(Button::new("rename-job-save").primary().tab_stop(true).label(tr!("chat-job-save"))
                                    .on_click(move |_, window, cx| save(window, cx)))))
                });
                focus.focus(window, cx);
            })
    }

    pub(crate) fn background_command_activity(&self, session: &str, now_ms: u64) -> Option<(String, String)> {
        let running = || self.chat_waiting.background_commands(session).iter()
            .filter(|command| command.state == BackgroundCommandState::Running);
        let oldest = running().min_by_key(|command| command.started_at_ms)?;
        Some((
            tr!("chat-activity-background-commands", count = running().count()),
            command_elapsed(oldest, now_ms),
        ))
    }

    pub(super) fn render_chat_activity(
        &self,
        session: &str,
        entity: WeakEntity<Self>,
        cx: &gpui_kit::App,
    ) -> Stateful<Div> {
        let chat = &self.chats[session];
        let waiting = chat.visible_wait(self.chat_waiting.reason(session));
        let now_ms = unix_millis();
        let commands = self.background_command_activity(session, now_ms);
        let compacting = waiting.is_some_and(|wait| wait.reason == ChatWaitReason::Compacting);
        if compacting && commands.is_none() {
            return h_flex().id("chat-tail-activity")
                .text_size(rems(0.75)).line_height(UI_LINE_HEIGHT).text_color(cx.theme().muted_foreground)
                .child(div().id(activity_id("label")).child(tr!("chat-wait-compacting")));
        }
        let turn = chat.turn_activity();
        let static_goal = self.chat_waiting.static_goal_status(session, chat);
        let jobs: Vec<_> = chat.pending_jobs().map(|(_, _, job)| job).collect();
        let (label, metadata) = if compacting {
            (Some(tr!("chat-wait-compacting").to_owned()), None)
        } else if let Some(goal) = static_goal {
            (Some(goal_tail_label(goal)), None)
        } else if chat.has_activity(waiting) {
            let (label, metadata) = activity_summary(turn.as_ref(), &jobs, waiting, now_ms);
            (Some(label), metadata)
        } else {
            (None, None)
        };
        let show_phase = label.is_some();
        let status_label = status_speech(&join_status_parts(
            label.as_deref().into_iter().chain(metadata.as_deref())
                .chain(commands.as_ref().into_iter().flat_map(|(label, elapsed)| [label.as_str(), elapsed.as_str()])),
        ));
        let theme = cx.theme();
        let idle_jobs = label.is_none() && commands.is_none();
        let status_label = if idle_jobs { tr!("chat-job-list").to_owned() } else { status_label };
        let spinning = !idle_jobs && (static_goal.is_none() || commands.is_some());
        let status = h_flex()
            .id(activity_id("trigger"))
            .min_w_0()
            .items_center()
            .cursor_pointer()
            .text_size(rems(0.75))
            .line_height(UI_LINE_HEIGHT)
            .whitespace_nowrap()
            .text_color(theme.muted_foreground)
            .when(idle_jobs, |status| status.child(tr!("chat-job-list")))
            .when(spinning, |status| status.child(activity_spinner(activity_id("icon"), theme.ring).mr_1()))
            .when_some(label, |status, label| status.child(
                status_row(&label, theme.muted_foreground)
                    .id(activity_id("label"))
                    .truncate(),
            ))
            .when_some(metadata, |status, metadata| {
                status
                    .child(status_sep(theme.muted_foreground).id(activity_id("separator-metadata")))
                    .child(
                        status_row(&metadata, theme.muted_foreground)
                            .id(activity_id("metadata"))
                            .when(commands.is_none(), |row| row.w(rems(8.)))
                            .when(commands.is_some(), |row| row.max_w(rems(8.)))
                            .truncate(),
                    )
            })
            .when_some(commands, |status, (label, elapsed)| {
                status
                    .when(show_phase, |status| status.child(status_sep(theme.muted_foreground)))
                    .child(div().id(activity_id("command-label")).min_w_0().truncate().child(label))
                    .child(status_sep(theme.muted_foreground))
                    .child(div().id(activity_id("command-time")).flex_none().child(elapsed))
            });
        let session = session.to_owned();
        h_flex()
            .id("chat-tail-activity")
            .min_w_0()
            .max_w_full()
            .child(
                HoverCard::new(activity_id(&session))
                    .placement(Placement::Top, Align::Start)
                    .open_delay(Duration::from_millis(300))
                    .pin_on_click(tr!("chat-status-details", status = status_label))
                    .focus_on_pin()
                    .min_w_0()
                    .max_w_full()
                    .trigger(status)
                    .content(move |card, window, cx| {
                        let card_handle = cx.entity().downgrade();
                        let window_id = window.window_handle().window_id();
                        let panel = window.use_keyed_state(
                            activity_id(&format!("panel-{session}")),
                            cx,
                            |_, cx| {
                                let card = card_handle.clone();
                                ActivityPanel {
                                    scroll: ScrollHandle::new(),
                                    selected: None,
                                    expanded: None,
                                    // A hover-open card leaves focus in the transcript or composer.
                                    _escape: cx.intercept_keystrokes(move |event, window, cx| {
                                        if event.keystroke.key != "escape"
                                            || event.keystroke.modifiers.modified()
                                            || window.window_handle().window_id() != window_id
                                            || window.has_active_dialog(cx)
                                        {
                                            return;
                                        }
                                        let dismissed = card
                                            .update(cx, |card, cx| {
                                                let open = card.is_open();
                                                if open {
                                                    card.dismiss(window, cx);
                                                }
                                                open
                                            })
                                            .unwrap_or(false);
                                        if dismissed {
                                            cx.stop_propagation();
                                        }
                                    }),
                                }
                            },
                        );
                        let Some(view) = entity.upgrade() else {
                            return div().id(activity_id("details"));
                        };
                        view.read(cx).render_activity_panel(
                            &session,
                            &entity,
                            card.is_pinned(),
                            card_handle,
                            &panel,
                            window,
                            cx,
                        )
                    }),
            )
    }

    fn render_activity_panel(
        &self,
        session: &str,
        view: &WeakEntity<Self>,
        pinned: bool,
        card: WeakEntity<HoverCardState>,
        panel: &Entity<ActivityPanel>,
        window: &Window,
        cx: &gpui_kit::App,
    ) -> Stateful<Div> {
        let chat = &self.chats[session];
        let waiting = chat.visible_wait(self.chat_waiting.reason(session));
        let turn = chat.turn_activity();
        let now_ms = unix_millis();
        let commands = self.chat_waiting.background_commands(session);
        let goal = self.chat_waiting.static_goal_status(session, chat);
        let theme = cx.theme();
        let mut jobs: Vec<_> = chat.jobs.iter()
            .filter(|summary| summary.status.active() || !summary.job.dismissed_from_panel)
            .map(|summary| (summary.position, summary.id.as_str(), Some(summary), chat.delegate_block(&summary.id)))
            .collect();
        if !chat.jobs.received {
            jobs.extend(chat.pending_jobs().filter_map(|(position, id, _)| {
                let block = chat.delegate_block(id)?;
                chat.jobs.get(id).is_none().then_some((position, id, block.wire_summary.as_ref(), Some(block)))
            }));
            if self.protocol >= fuzukue_proto::JOB_LABEL_PROTOCOL {
                jobs.extend(chat.finished_panel_jobs().filter(|(_, block)| chat.jobs.get(&block.id).is_none())
                    .map(|(position, block)| (position, block.id.as_str(), block.wire_summary.as_ref(), Some(block))));
            }
        }
        jobs.sort_unstable_by_key(|(position, _, summary, block)| {
            if summary.map_or_else(|| block.is_some_and(|block| block.report.is_none()), |summary| summary.status.active()) {
                (false, *position)
            } else {
                (true, usize::MAX - *position)
            }
        });
        let state = panel.read(cx);
        let selected = jobs
            .iter()
            .position(|(_, id, _, _)| Some(*id) == state.selected.as_deref())
            .unwrap_or(0);
        let scroll = state.scroll.clone();
        let inset = window.client_inset().unwrap_or_default();
        let viewport = window.viewport_size();
        let width = (viewport.width - (PANEL_MARGIN + inset) * 2.).clamp(px(0.), px(448.));
        let height = panel_max_height(viewport.height, inset);
        let first_job = usize::from(turn.is_some()) + usize::from(waiting.is_some())
            + usize::from(goal.is_some()) + commands.len();
        let mut rows = v_flex()
            .id(activity_id("job-scroll"))
            .w_full()
            .max_h((height - px(2.)).max(px(0.)))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .gap_2()
            .p_2()
            .pr_4()
            .children(turn.as_ref().map(|turn| {
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(activity_spinner(activity_id("turn-icon"), theme.ring))
                    .child(status_row(&turn_activity_detail(turn), theme.muted_foreground))
            }))
            .children(waiting.map(|wait| status_row(&waiting_label(wait, now_ms), theme.muted_foreground)))
            .children(goal.map(|goal| status_row(&goal_tail_label(goal), theme.muted_foreground)))
            .children(commands.iter().enumerate().map(|(index, command)| {
                let label = if command.summary.is_empty() {
                    tr!("chat-activity-command").to_owned()
                } else {
                    command.summary.clone()
                };
                let state = command_state(command.state);
                let elapsed = command_elapsed(command, now_ms);
                h_flex().id(activity_id(&format!("command-{index}")))
                    .min_w_0()
                    .role(Role::Group)
                    .aria_label(format!("{label}, {state}, {elapsed}"))
                    .whitespace_nowrap()
                    .child(div().min_w_0().truncate().child(label))
                    .child(status_sep(theme.muted_foreground))
                    .child(div().flex_none().text_color(theme.muted_foreground).child(state))
                    .child(status_sep(theme.muted_foreground))
                    .child(div().flex_none().text_color(theme.muted_foreground).child(elapsed))
            }));
        let pending_jobs = chat.pending_jobs().count();
        for (index, &(_, id, summary, block)) in jobs.iter().enumerate() {
            let expanded = state.expanded.as_deref() == Some(id);
            let report = block.and_then(|block| block.report.as_ref());
            let job = summary.map_or_else(|| &block.unwrap().job, |summary| &summary.job);
            let worker = match summary {
                Some(summary) => WorkerActivity::from_summary(summary, expanded, now_ms),
                None => WorkerActivity::new(block.unwrap(), expanded, now_ms),
            };
            let toggle = {
                let job = id.to_owned();
                let panel = panel.clone();
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    panel.update(cx, |state, _| {
                        state.toggle(&job);
                        state.scroll.scroll_to_item(first_job + index);
                    });
                    window.refresh();
                }
            };
            let running = summary.map_or(report.is_none(), |summary| summary.status.active());
            let mut actions = h_flex().flex_none().flex_wrap().gap_1().child(
                inline_button(activity_id(&format!("job-details-{index}")))
                    .tab_stop(true).label(tr!("chat-activity-details"))
                    .selected(expanded)
                    .on_click(toggle),
            );
            if running && self.protocol >= 14 {
                let (target_session, target_job) = (session.to_owned(), id.to_owned());
                let view = view.clone();
                let chosen = chat.worker_target.as_deref() == Some(id);
                let target_id = if pending_jobs == 1 {
                    "chat-role-target".into()
                } else {
                    SharedString::from(format!("worker-target-{id}"))
                };
                let command = ClientMsg::StopWorker {
                    session: session.to_owned(),
                    job: id.to_owned(),
                };
                let out = self.out.clone();
                actions = actions
                    .child(
                        inline_button(target_id)
                            .tab_stop(true).label(tr!("chat-activity-send"))
                            .selected(chosen)
                            .accessibility_label(if chosen {
                                tr!("chat-selected-recipient")
                            } else {
                                tr!("chat-send-to-this-worker")
                            })
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if let Some(chat) = this.chats.get_mut(&target_session) {
                                        if chat.pending_jobs().any(|(_, id, _)| id == target_job) {
                                            chat.worker_target =
                                                (!chosen).then(|| target_job.clone());
                                            cx.notify();
                                        }
                                    }
                                });
                            }),
                    )
                    .child(
                        inline_button(SharedString::from(format!("stop-worker-{id}")))
                            .tab_stop(true).label(tr!("chat-stop"))
                            .on_click(move |_, _, _| {
                                let _ = out.send(command.clone());
                            }),
                    );
            }
            if running && self.protocol >= fuzukue_proto::JOB_LABEL_PROTOCOL {
                actions = actions.child(self.rename_job_button(session, id, job, activity_id(&format!("job-rename-{index}"))));
            }
            let job_id = id.to_owned();
            actions = actions.child(
                inline_button(activity_id(&format!("job-copy-{index}"))).tab_stop(true)
                    .label(tr!("chat-job-copy-id"))
                    .on_click(move |_, _, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(job_id.clone()))),
            );
            let has_report = summary.is_some_and(|summary|
                summary.elapsed_ms.is_some() || summary.status == JobStatus::Interrupted);
            if !running && (report.is_some() || has_report) {
                let name = worker.title.to_owned();
                let report = report.filter(|(_, report)| summary.is_none() || report.interrupted || !report.text.is_empty()).cloned();
                let (view, card, session) = (view.clone(), card.clone(), session.to_owned());
                let job_id = id.to_owned();
                actions = actions.child(
                    inline_button(activity_id(&format!("job-report-{index}"))).tab_stop(true)
                        .label(tr!("chat-job-open-report"))
                        .on_click(move |_, window, cx| {
                            let _ = card.update(cx, |card, cx| card.dismiss(window, cx));
                            if let Some((ok, report)) = &report {
                                Self::show_job_report(view.clone(), session.clone(), name.clone(), *ok, report.clone(), window, cx);
                            } else {
                                let _ = view.update(cx, |this, _| {
                                    let request = chat_history::next_request();
                                    if let Some(chat) = this.chats.get_mut(&session) {
                                        if this.out.send(ClientMsg::GetThreadJobReport {
                                            session: session.clone(), job: job_id.clone(), request,
                                        }).is_ok() {
                                            chat.jobs.report_request = Some((request, job_id.clone()));
                                        }
                                    }
                                });
                            }
                        }),
                );
            }
            if !running && self.protocol >= fuzukue_proto::JOB_DISMISS_PROTOCOL {
                let command = ClientMsg::DismissJob { session: session.to_owned(), job: id.to_owned() };
                let out = self.out.clone();
                actions = actions.child(
                    inline_button(activity_id(&format!("job-dismiss-{index}"))).tab_stop(true)
                        .label(tr!("chat-job-dismiss"))
                        .on_click(move |_, _, _| {
                            let _ = out.send(command.clone());
                        }),
                );
            }
            let accessible_status = match (summary.map(|summary| summary.status), report) {
                (Some(JobStatus::Running), _) => tr!("chat-activity-working"),
                (Some(JobStatus::Queued), _) => tr!("chat-activity-waiting"),
                (Some(status), _) => job_status_label(status),
                (None, Some((_, report))) if report.interrupted => tr!("agents-interrupted"),
                (None, Some((true, _))) => tr!("chat-done"),
                (None, Some((false, _))) => tr!("chat-failed"),
                (None, None) if job.started_at_ms.is_some() => tr!("chat-activity-working"),
                (None, None) => tr!("chat-activity-waiting"),
            };
            let wait_status = job.waiting.as_ref().map(|reason| {
                format!("{}: {reason}", tr!("chat-waiting"))
            });
            let accessible_label = worker_heading(
                wait_status.as_deref().unwrap_or(accessible_status.as_ref()),
                Some(worker.title),
                worker.elapsed.as_deref(),
            );
            let accessible_label = if worker.failed {
                tr!("chat-tool-group-error", label = accessible_label)
            } else {
                accessible_label
            };
            let title_tip = job.title.clone()
                .or_else(|| job.display.label_source.clone()).unwrap_or_else(|| worker.title.to_owned());
            let heading = h_flex()
                .id(activity_id(&format!("job-heading-{index}")))
                .role(Role::Group)
                .aria_label(status_speech(&accessible_label))
                .min_w_0()
                .items_center()
                .gap_1()
                .when(running && job.started_at_ms.is_some(), |heading| {
                    heading.child(activity_spinner(
                        activity_id(&format!("job-icon-{index}")),
                        theme.ring,
                    ))
                })
                .when(running && job.started_at_ms.is_none(), |heading| {
                    heading.child(
                        div()
                            .size(px(14.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(px(6.)).rounded_full().bg(theme.muted_foreground)),
                    )
                })
                .children(worker.failed.then(|| {
                    div()
                        .id(activity_id(&format!("job-error-{index}")))
                        .size(px(6.))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.danger)
                }))
                .when(!running, |row| row.child(job_chip(activity_id(&format!("job-state-{index}")), accessible_status, cx)))
                .children(job.display.card_id.as_ref().map(|id| {
                    job_chip(activity_id(&format!("job-card-{index}")), id, cx)
                }))
                .child(
                    h_flex()
                        .id(activity_id(&format!("job-label-{index}")))
                        .min_w_0()
                        .child(div().id(activity_id(&format!("job-title-{index}")))
                            .min_w_0().whitespace_normal().line_clamp(2).text_ellipsis()
                            .child(worker.title.to_owned())
                            .tooltip(move |window, cx| Tooltip::new(title_tip.clone()).build(window, cx)))
                        .when_some(worker.elapsed.as_ref(), |row, elapsed| {
                            row.child(status_sep(theme.muted_foreground)
                                .id(activity_id(&format!("job-separator-{index}"))))
                                .child(div().id(activity_id(&format!("job-time-{index}")))
                                    .flex_none().whitespace_nowrap().child(elapsed.clone()))
                        }),
                );
            let mut item = v_flex()
                .id(activity_id(&format!("job-{index}")))
                .min_w_0()
                .flex_none()
                .gap_1()
                .px_1()
                .rounded_md()
                .when(pinned && selected == index, |item| item.bg(theme.accent))
                .child(heading)
                .child(job_metadata(job, &worker, index, cx))
                .child(h_flex().min_w_0().gap_2().text_color(theme.muted_foreground)
                    .child(div().flex_none().child(tr!("chat-job-tool-count", count = worker.calls)))
                    .children(worker.last_tool.or_else(|| summary.and_then(|summary| summary.last_activity.as_deref())).map(|tool| {
                        let tool = tool.to_owned();
                        div().id(activity_id(&format!("job-last-tool-{index}"))).min_w_0().truncate()
                            .child(tool.clone())
                            .tooltip(move |window, cx| Tooltip::new(tool.clone()).build(window, cx))
                    })))
                .child(actions)
                .children(wait_status.map(|status| {
                    div()
                        .id(activity_id(&format!("job-wait-{index}")))
                        .min_w_0()
                        .text_color(theme.muted_foreground)
                        .child(status)
                }));
            if let Some(details) = worker.details {
                let status = accessible_status;
                let label = tr!(
                    "chat-worker-tools",
                    worker = tr!("chat-worker"),
                    calls = worker.calls,
                    status = status
                );
                let label = if worker.failed {
                    tr!("chat-tool-group-error", label = label)
                } else {
                    label
                };
                item = item
                    .children(delegate_details(job, report.map(|(_, report)| report))
                        .into_iter().map(|line| div().whitespace_normal().child(line)))
                    .child(
                        status_row(&label, theme.muted_foreground)
                            .id(activity_id(&format!("job-tool-status-{index}")))
                            .text_color(if worker.failed {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            }),
                    )
                    .children(details.last_activity.map(|activity| {
                        div()
                            .truncate()
                            .child(tr!("chat-activity-last", activity = activity))
                    }))
                    .children(worker.elapsed.map(|elapsed| {
                        div().child(tr!("chat-activity-elapsed", elapsed = elapsed))
                    }));
                if let Some(message) = details.latest_message {
                    let preview = div()
                        .id(activity_id(&format!("job-preview-{index}")))
                        .w_full()
                        .min_w_0();
                    let preview = if fuzukue_view::markdown_code::has_code(message) {
                        preview.bg(theme.background).child(self.chat_markdown(
                            activity_id(&format!("job-preview-markdown-{id}")),
                            fuzukue_view::link_card_refs(message),
                            chat,
                            cx,
                        ).max_lines(6))
                    } else {
                        preview.whitespace_normal().line_clamp(6).text_ellipsis().child(message.to_owned())
                    };
                    let (view, card, session, job) = (
                        view.clone(),
                        card.clone(),
                        session.to_owned(),
                        id.to_owned(),
                    );
                    item = item
                        .child(preview)
                        .when(block.is_some(), |item| item.child(
                            inline_button(activity_id(&format!("job-full-{index}")))
                                .self_start()
                                .tab_stop(true).label(tr!("chat-activity-full-message"))
                                .on_click(move |_, window, cx| {
                                    let _ = card.update(cx, |card, cx| card.dismiss(window, cx));
                                    let _ = view.update(cx, |this, cx| {
                                        let Some(chat) = this.chats.get(&session) else { return };
                                        let Some(&row) = chat.by_item.get(&job) else { return };
                                        if chat.delegate_block(&job).and_then(|block| block.deferred.as_ref())
                                            .is_some_and(|deferred| !deferred.open)
                                        {
                                            this.toggle_thread_job(&session, row, cx);
                                        }
                                        let chat = this.chats.get_mut(&session).unwrap();
                                        if let Some(Row {
                                            content: Content::Delegate(block),
                                            ..
                                        }) = chat.rows.get_mut(row)
                                        {
                                            block.text_open = Some(true);
                                            chat.remeasure(row, cx);
                                            chat.reveal_delegate(&job);
                                            this.chat_focus.focus(window, cx);
                                        }
                                    });
                                }),
                        ));
                }
            }
            rows = rows.child(item);
        }
        if let Some((until, _, queued)) = &chat.paused {
            if !waiting
                .is_some_and(|wait| matches!(wait.reason, ChatWaitReason::UsagePaused { .. }))
            {
                let remaining = ((*until).max(0) as u64).saturating_sub(now_ms / 1000);
                rows = rows.child(if remaining > 0 {
                    tr!(
                        "chat-wait-usage-resume",
                        remaining = activity_time(remaining)
                    )
                } else {
                    tr!("chat-wait-usage").into()
                });
            }
            let command = ClientMsg::ResumeNow {
                session: session.to_owned(),
            };
            let out = self.out.clone();
            rows = rows.child(
                h_flex()
                    .gap_1()
                    .children(
                        (*queued > 0)
                            .then(|| tr!("chat-wait-queued-count", count = queued.to_string())),
                    )
                    .child(
                        inline_button("chat-status-action")
                            .label(tr!("chat-resume-now"))
                            .on_click(move |_, _, _| {
                                let _ = out.send(command.clone());
                            }),
                    ),
            );
        }
        let jobs: Vec<_> = jobs
            .into_iter()
            .map(|(_, id, _, _)| id.to_owned())
            .collect();
        let panel = panel.clone();
        div()
            .id(activity_id("details"))
            .relative()
            .w(width)
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_md()
            .text_sm()
            .line_height(UI_LINE_HEIGHT)
            .child(rows)
            .child(Scrollbar::vertical(&scroll).mode(if pinned {
                ScrollbarMode::Always
            } else {
                ScrollbarMode::Hover
            }))
            .when(pinned, |content| {
                content.on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    let key = event.keystroke.key.as_str();
                    if !jobs.is_empty() && matches!(key, "up" | "down" | "enter") {
                        let next = match key {
                            "up" => selected.saturating_sub(1),
                            "down" => (selected + 1).min(jobs.len() - 1),
                            _ => selected,
                        };
                        panel.update(cx, |state, _| {
                            if key == "enter" {
                                state.toggle(&jobs[next]);
                            } else {
                                state.selected = Some(jobs[next].clone());
                            }
                            state.scroll.scroll_to_item(first_job + next);
                        });
                        window.refresh();
                    } else {
                        return;
                    }
                    cx.stop_propagation();
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_activity_reads_job_summary_without_a_transcript_row() {
        let summary: JobSummary = serde_json::from_value(serde_json::json!({
            "id":"delegate:test", "position":20, "status":"running",
            "job":{
                "title":"Check references", "model":"gpt-5.5", "started_at_ms":1000,
                "selection":{"backend":"codex","model":"gpt-5.5","effort":"high"},
                "routing":{"tier":"heavy","reason":"test"},
            },
            "tools":512, "had_tool_error":true, "last_tool":"Read README.md",
            "last_activity":"Reviewing notes", "latest_message":"The links resolve.",
        })).unwrap();
        let mut jobs = chat_history::JobSummaries::default();
        jobs.update(11, summary.clone());
        jobs.snapshot(10, true, vec![JobSummary { tools: 0, ..summary }]);
        let worker = WorkerActivity::from_summary(jobs.get("delegate:test").unwrap(), true, 226_000);
        assert_eq!(worker.calls, 512);
        assert_eq!(worker.elapsed.as_deref(), Some("3:45"));
        assert_eq!(worker.tier, Some(fuzukue_proto::Tier::Heavy));
        assert_eq!(worker.selection.unwrap().effort.as_deref(), Some("high"));
        assert_eq!(worker.last_tool, Some("Read README.md"));
        assert_eq!(worker.details.unwrap().last_activity, Some("Reviewing notes"));
    }

    #[test]
    fn worker_activity_keeps_the_error_visible_when_details_are_collapsed() {
        let mut tool = ToolCall::new("Read", "Read README.md".into(), Value::Null);
        tool.result = Some(ToolResult {
            ok: false,
            output: String::new(),
        });
        let mut block = DelegateBlock {
            id: "delegate:test".into(),
            job: DelegateJob {
                title: Some("Check references".into()),
                display: Default::default(),
                queued_at_ms: None,
                started_at_ms: Some(1_000),
                agent: None,
                model: String::new(),
                selection: None,
                routing: None,
                touches: None,
                waiting: None,
                dismissed_from_panel: false,
            },
            summary: None,
            wire_summary: None,
            progress: vec![
                DelegateEntry::Text("Earlier draft".into()),
                DelegateEntry::Text("I checked the README.".into()),
                DelegateEntry::Tool(tool),
                DelegateEntry::Text(" ".into()),
            ],
            text_open: None,
            tools_open: false,
            earlier_tools_open: false,
            deferred: None,
            report: None,
        };
        let compact = WorkerActivity::new(&block, false, 226_000);
        assert_eq!(compact.title, "Check references");
        assert_eq!(compact.elapsed.as_deref(), Some("3:45"));
        assert!(compact.failed);
        assert!(compact.details.is_none());

        let expanded = WorkerActivity::new(&block, true, 226_000);
        assert!(expanded.failed);
        let details = expanded.details.unwrap();
        assert_eq!(expanded.calls, 1);
        assert_eq!(compact.last_tool, Some("Read README.md"));
        assert_eq!(details.last_activity, Some("Read README.md"));
        assert_eq!(details.latest_message, Some("I checked the README."));

        block.summary = Some("Checked file paths".into());
        block.progress.clear();
        block.job.started_at_ms = None;
        let queued = WorkerActivity::new(&block, true, 226_000);
        assert!(!queued.failed);
        assert!(queued.elapsed.is_none());
        let details = queued.details.unwrap();
        assert_eq!(details.last_activity, Some("Checked file paths"));
        assert_eq!(details.latest_message, None);
    }

    #[test]
    fn activity_panel_caps_the_scroll_viewport() {
        assert_eq!(panel_max_height(px(600.), px(0.)), px(360.));
        assert_eq!(panel_max_height(px(1200.), px(0.)), px(480.));
        assert_eq!(panel_max_height(px(120.), px(20.)), px(64.));
    }
}
