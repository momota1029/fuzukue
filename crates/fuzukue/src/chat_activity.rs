//! The activity details attached to the chat status strip.

use gpui_kit::base::{Align, HoverCard, HoverCardState, Placement, Scrollbar, ScrollbarMode};
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
    failed: bool,
    details: Option<WorkerActivityDetails<'a>>,
}

struct WorkerActivityDetails<'a> {
    calls: usize,
    last_activity: Option<&'a str>,
    latest_message: Option<&'a str>,
}

impl<'a> WorkerActivity<'a> {
    fn new(block: &'a DelegateBlock, expanded: bool, now_ms: u64) -> Self {
        let (calls, failed) = delegate_tools(block);
        let details = expanded.then(|| WorkerActivityDetails {
            calls,
            last_activity: block.summary.as_deref().or_else(|| {
                block.progress.iter().rev().find_map(|entry| match entry {
                    DelegateEntry::Tool(call) => Some(call.summary.as_str()),
                    _ => None,
                })
            }),
            latest_message: block.progress.iter().rev().find_map(|entry| match entry {
                DelegateEntry::Text(text) if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            }),
        });
        Self {
            title: delegate_activity_name(&block.job),
            elapsed: delegate_elapsed(&block.job, now_ms).map(activity_time),
            failed,
            details,
        }
    }
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
        let jobs: Vec<_> = chat.pending_jobs().map(|(_, job)| job).collect();
        let (label, metadata) = if compacting {
            (Some(tr!("chat-wait-compacting").to_owned()), None)
        } else if let Some(goal) = self.chat_waiting.static_goal_status(session, chat) {
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
        let status = h_flex()
            .id(activity_id("trigger"))
            .min_w_0()
            .items_center()
            .cursor_pointer()
            .text_size(rems(0.75))
            .line_height(UI_LINE_HEIGHT)
            .whitespace_nowrap()
            .text_color(theme.muted_foreground)
            .child(activity_spinner(activity_id("icon"), theme.link).mr_1())
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
        let mut jobs: Vec<_> = chat
            .pending_jobs()
            .filter_map(|(row, _)| match &chat.rows[row].content {
                Content::Delegate(block) => Some((row, block)),
                _ => None,
            })
            .collect();
        jobs.sort_unstable_by_key(|(row, _)| *row);
        let state = panel.read(cx);
        let selected = jobs
            .iter()
            .position(|(_, job)| Some(job.id.as_str()) == state.selected.as_deref())
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
                    .child(activity_spinner(activity_id("turn-icon"), theme.link))
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
        for (index, &(_, block)) in jobs.iter().enumerate() {
            let expanded = state.expanded.as_deref() == Some(block.id.as_str());
            let worker = WorkerActivity::new(block, expanded, now_ms);
            let toggle = {
                let job = block.id.clone();
                let panel = panel.clone();
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    panel.update(cx, |state, _| {
                        state.toggle(&job);
                        state.scroll.scroll_to_item(first_job + index);
                    });
                    window.refresh();
                }
            };
            let mut actions = h_flex().flex_none().gap_1().child(
                inline_button(activity_id(&format!("job-details-{index}")))
                    .label(tr!("chat-activity-details"))
                    .selected(expanded)
                    .on_click(toggle),
            );
            if self.protocol >= 14 {
                let (target_session, target_job) = (session.to_owned(), block.id.clone());
                let view = view.clone();
                let chosen = chat.worker_target.as_deref() == Some(&block.id);
                let target_id = if jobs.len() == 1 {
                    "chat-role-target".into()
                } else {
                    SharedString::from(format!("worker-target-{}", block.id))
                };
                let command = ClientMsg::StopWorker {
                    session: session.to_owned(),
                    job: block.id.clone(),
                };
                let out = self.out.clone();
                actions = actions
                    .child(
                        inline_button(target_id)
                            .label(tr!("chat-activity-send"))
                            .selected(chosen)
                            .accessibility_label(if chosen {
                                tr!("chat-selected-recipient")
                            } else {
                                tr!("chat-send-to-this-worker")
                            })
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if let Some(chat) = this.chats.get_mut(&target_session) {
                                        if chat.pending_delegates.contains(&target_job) {
                                            chat.worker_target =
                                                (!chosen).then(|| target_job.clone());
                                            cx.notify();
                                        }
                                    }
                                });
                            }),
                    )
                    .child(
                        inline_button(SharedString::from(format!("stop-worker-{}", block.id)))
                            .label(tr!("chat-stop"))
                            .on_click(move |_, _, _| {
                                let _ = out.send(command.clone());
                            }),
                    );
            }
            let accessible_status = if worker.elapsed.is_some() {
                tr!("chat-activity-working")
            } else {
                tr!("chat-activity-waiting")
            };
            let wait_status = block.job.waiting.as_ref().map(|reason| {
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
            let heading = h_flex()
                .id(activity_id(&format!("job-heading-{index}")))
                .role(Role::Group)
                .aria_label(status_speech(&accessible_label))
                .min_w_0()
                .items_center()
                .gap_1()
                .when(width >= px(320.), |heading| heading.flex_1())
                .when_some(worker.elapsed.as_ref(), |heading, _| {
                    heading.child(activity_spinner(
                        activity_id(&format!("job-icon-{index}")),
                        theme.link,
                    ))
                })
                .when(worker.elapsed.is_none(), |heading| {
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
                .child(
                    h_flex()
                        .id(activity_id(&format!("job-label-{index}")))
                        .min_w_0()
                        .child(div().id(activity_id(&format!("job-title-{index}")))
                            .min_w_0().truncate().child(worker.title.to_owned()))
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
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_1()
                        .when(width >= px(320.), |row| {
                            row.items_center().justify_between()
                        })
                        .when(width < px(320.), |row| row.flex_col().items_start())
                        .child(heading)
                        .child(actions),
                )
                .children(wait_status.map(|status| {
                    div()
                        .id(activity_id(&format!("job-wait-{index}")))
                        .min_w_0()
                        .text_color(theme.muted_foreground)
                        .child(status)
                }));
            if let Some(details) = worker.details {
                let status = if worker.elapsed.is_some() {
                    tr!("chat-tool-running")
                } else {
                    tr!("chat-waiting")
                };
                let label = tr!(
                    "chat-worker-tools",
                    worker = tr!("chat-worker"),
                    calls = details.calls,
                    status = status
                );
                let label = if worker.failed {
                    tr!("chat-tool-group-error", label = label)
                } else {
                    label
                };
                item = item
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
                            activity_id(&format!("job-preview-markdown-{}", block.id)),
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
                        block.id.clone(),
                    );
                    item = item
                        .child(preview)
                        .child(
                            inline_button(activity_id(&format!("job-full-{index}")))
                                .self_start()
                                .label(tr!("chat-activity-full-message"))
                                .on_click(move |_, window, cx| {
                                    let _ = card.update(cx, |card, cx| card.dismiss(window, cx));
                                    let _ = view.update(cx, |this, cx| {
                                        let Some(chat) = this.chats.get_mut(&session) else {
                                            return;
                                        };
                                        let Some(&row) = chat.by_item.get(&job) else {
                                            return;
                                        };
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
                        );
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
            .map(|(_, block)| block.id.clone())
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
                started_at_ms: Some(1_000),
                agent: None,
                model: String::new(),
                selection: None,
                routing: None,
                touches: None,
                waiting: None,
            },
            summary: None,
            progress: vec![
                DelegateEntry::Text("Earlier draft".into()),
                DelegateEntry::Text("I checked the README.".into()),
                DelegateEntry::Tool(tool),
                DelegateEntry::Text(" ".into()),
            ],
            text_open: None,
            tools_open: false,
            earlier_tools_open: false,
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
        assert_eq!(details.calls, 1);
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
