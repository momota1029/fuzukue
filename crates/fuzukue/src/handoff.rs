use std::collections::HashMap;

use gpui_kit::{
    base::Disableable as _,
    component::{
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        input::{Input, InputState},
        scroll::ScrollableElement as _,
        text::TextView,
        v_flex, ActiveTheme as _, IconName, Sizable as _,
    },
    div,
    prelude::*,
    px, AnyElement, Context, Entity, SharedString,
};
use fuzukue_proto::{
    ClientMsg, HandoffDetail, HandoffIssueSelection, HandoffRequest as Request, HandoffSummary,
    HandoffWorkerMode as Mode, HandoffWorkerStatus as Status,
};

use crate::{RightPane, Workbench};

#[derive(Default)]
pub(super) struct Handoffs {
    pub notes: Vec<HandoffSummary>,
    detail: Option<HandoffDetail>,
    session: Option<String>,
    chosen: HashMap<String, bool>,
    workers: HashMap<(String, String), WorkerDisplay>,
    publishing: bool,
    generating: bool,
    transcript: Option<Entity<InputState>>,
    error: Option<String>,
    generated: Option<String>,
}

#[derive(Default)]
struct WorkerDisplay {
    status: Option<Status>,
    output: String,
    text_open: Option<bool>,
}

impl Handoffs {
    pub(super) fn reset_worker_disclosures(&mut self) {
        for worker in self.workers.values_mut() {
            worker.text_open = None;
        }
    }
}

fn button(id: impl Into<gpui_kit::ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id).small().outline().label(label)
}

impl Workbench {
    pub(super) fn open_handoffs(&mut self, cx: &mut Context<Self>) {
        self.close_sidebar_menu(cx);
        self.settings_open = false;
        self.handoffs.session = None;
        self.handoffs.chosen.clear();
        self.right = RightPane::Handoff;
        cx.notify();
    }

    fn send_handoff(&self, request: Request) {
        let _ = self.out.send(ClientMsg::Handoff { request });
    }

    pub(super) fn handoff_failed(&mut self, message: String) {
        self.handoffs.publishing = false;
        self.handoffs.generating = false;
        self.handoffs.error = Some(message);
    }

    pub(super) fn handoff_generated(&mut self, path: String) {
        self.handoffs.generating = false;
        self.handoffs.transcript = None;
        self.handoffs.error = None;
        self.handoffs.generated = Some(path);
    }

    pub(super) fn handoff_opened(&mut self, session: String, cx: &mut Context<Self>) {
        self.select_thread(session.clone(), cx);
        self.handoffs.session = Some(session);
        self.handoffs.error = None;
        self.handoffs.chosen.clear();
        self.handoffs.publishing = false;
        self.right = RightPane::Handoff;
    }

    pub(super) fn handoff_detail(&mut self, detail: HandoffDetail) {
        if self.handoffs.session.as_ref() == Some(&detail.session) {
            self.handoffs.chosen.retain(|id, _| {
                detail
                    .tasks
                    .iter()
                    .any(|task| &task.id == id && task.issue.is_none())
            });
            self.handoffs.publishing = false;
            self.handoffs.detail = Some(detail);
        }
    }

    pub(super) fn handoff_worker_updated(
        &mut self,
        session: String,
        thread_id: String,
        status: Status,
    ) {
        let worker = self
            .handoffs
            .workers
            .entry((session, thread_id))
            .or_default();
        match status {
            Status::Output(text) => worker.output = text,
            status => {
                worker.output.clear();
                worker.text_open = matches!(status, Status::Viewing).then_some(true);
                worker.status = Some(status);
            }
        }
    }

    pub(super) fn handoff_thread_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.protocol < fuzukue_proto::HANDOFF_PROTOCOL {
            return None;
        }
        let thread = self
            .threads
            .iter()
            .find(|thread| thread.id == self.active)?;
        if !thread
            .artifacts
            .first()
            .is_some_and(|path| path.starts_with(".fuzukue/handoff/") || path.starts_with(".labo/handoff/"))
        {
            return None;
        }
        Some(
            div()
                .px_5()
                .py_1()
                .child(
                    button("thread-handoff", tr!("handoff-title")).on_click(cx.listener(
                        |this, _, _, cx| {
                            let session = this.active.clone();
                            this.settings_open = false;
                            this.handoffs.session = Some(session.clone());
                            this.handoffs.chosen.clear();
                            this.right = RightPane::Handoff;
                            this.send_handoff(Request::Detail { session });
                            cx.notify();
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_handoffs(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = &self.handoffs;
        let detail = state
            .detail
            .as_ref()
            .filter(|detail| state.session.as_ref() == Some(&detail.session));
        v_flex()
            .id("handoff-view")
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .gap_3()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .when(state.session.is_some(), |row| {
                        row.child(
                            button("handoff-back", tr!("handoff-back"))
                                .on_click(cx.listener(|this, _, _, cx| this.open_handoffs(cx))),
                        )
                    })
                    .child(div().text_lg().flex_1().child(tr!("handoff-title")))
                    .when(
                        state.session.is_none() && state.transcript.is_none(),
                        |row| {
                            row.child(
                                button("handoff-from-transcript", tr!("handoff-from-transcript"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let input = cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(tr!("handoff-transcript-placeholder"))
                                        });
                                        input.update(cx, |input, cx| input.focus(window, cx));
                                        this.handoffs.transcript = Some(input);
                                        this.handoffs.generated = None;
                                        this.handoffs.error = None;
                                        cx.notify();
                                    })),
                            )
                        },
                    )
                    .child(self.pane_close_button("handoff-close", cx)),
            )
            .child(
                div()
                    .id("handoff-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(800.))
                            .mx_auto()
                            .p_5()
                            .gap_4()
                            .children(state.error.as_ref().map(|error| {
                                div()
                                    .text_sm()
                                    .text_color(theme.danger)
                                    .child(tr!("handoff-error", reason = error.clone()))
                            }))
                            .when(state.session.is_none(), |body| {
                                body.children(
                                    state
                                        .transcript
                                        .as_ref()
                                        .map(|input| self.render_handoff_transcript(input, cx)),
                                )
                                .children(
                                    state.generated.as_ref().map(|path| {
                                        div()
                                            .text_sm()
                                            .child(tr!("handoff-generated", path = path.clone()))
                                    }),
                                )
                            })
                            .map(|body| match detail {
                                Some(detail) => body.child(self.render_handoff_detail(detail, cx)),
                                None if state.session.is_some() => {
                                    body.child(tr!("handoff-loading"))
                                }
                                None if state.notes.is_empty() => body.child(tr!("handoff-empty")),
                                None => body.children(state.notes.iter().enumerate().map(
                                    |(index, note)| {
                                        let path = note.path.clone();
                                        v_flex()
                                            .p_4()
                                            .gap_2()
                                            .border_1()
                                            .rounded_md()
                                            .border_color(theme.border)
                                            .child(div().text_lg().child(note.title.clone()))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!(
                                                        "{} · {}",
                                                        note.date, note.source
                                                    )),
                                            )
                                            .child(div().text_sm().child(tr!(
                                                "handoff-counts",
                                                tasks = note.tasks.to_string(),
                                                workers = note.workers.to_string()
                                            )))
                                            .child(
                                                h_flex().child(
                                                    button(
                                                        format!("handoff-receive-{index}"),
                                                        tr!("handoff-receive"),
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, _| {
                                                        this.send_handoff(Request::Receive {
                                                            path: path.clone(),
                                                        })
                                                    })),
                                                ),
                                            )
                                    },
                                )),
                            }),
                    ),
            )
    }

    fn render_handoff_transcript(
        &self,
        input: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let busy = self.handoffs.generating;
        let source = input.clone();
        v_flex()
            .gap_2()
            .child(div().text_sm().child(tr!("handoff-transcript-input")))
            .child(Input::new(input).aria_label(tr!("handoff-transcript-input")))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        button(
                            "handoff-generate",
                            if busy {
                                tr!("handoff-generating")
                            } else {
                                tr!("handoff-generate")
                            },
                        )
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let value = source.read(cx).value();
                            let source = value.trim();
                            if !source.is_empty() {
                                this.handoffs.error = None;
                                this.handoffs.generating = true;
                                this.send_handoff(Request::CreateFromTranscript {
                                    source: source.into(),
                                });
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        button("handoff-cancel-generate", tr!("handoff-cancel"))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.handoffs.transcript = None;
                                this.handoffs.error = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_handoff_detail(
        &self,
        detail: &HandoffDetail,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let session = detail.session.clone();
        let state = &self.handoffs;
        v_flex()
            .id("handoff-checklist")
            .gap_4()
            .min_w_0()
            .child(div().text_xl().child(detail.note.title.clone()))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(detail.note.path.clone()),
            )
            .child(div().text_lg().child(tr!("handoff-tasks")))
            .when(detail.tasks.is_empty(), |body| {
                body.child(tr!("handoff-no-tasks"))
            })
            .children(detail.tasks.iter().enumerate().map(|(index, task)| {
                let choice = state.chosen.get(&task.id).copied();
                let task_id = task.id.clone();
                let ready_id = task.id.clone();
                let select = cx.entity().downgrade();
                let ready = select.clone();
                v_flex()
                    .p_4()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_sm().child(TextView::markdown(
                        SharedString::from(format!("handoff-task-{index}")),
                        task.markdown.trim_end().to_owned(),
                    )))
                    .child(h_flex().gap_4().map(|row| {
                        match task.issue {
                            Some(number) => {
                                row.child(tr!("handoff-issue-created", number = number.to_string()))
                            }
                            None => row
                                .child(
                                    Checkbox::new(format!("handoff-select-{index}"))
                                        .label(tr!("handoff-select"))
                                        .checked(choice.is_some())
                                        .disabled(state.publishing)
                                        .on_change(move |checked, _, cx| {
                                            let _ = select.update(cx, |this, cx| {
                                                if *checked {
                                                    this.handoffs
                                                        .chosen
                                                        .insert(task_id.clone(), false);
                                                } else {
                                                    this.handoffs.chosen.remove(&task_id);
                                                }
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(
                                    Checkbox::new(format!("handoff-ready-{index}"))
                                        .label(tr!("handoff-ready"))
                                        .checked(choice == Some(true))
                                        .disabled(choice.is_none() || state.publishing)
                                        .on_change(move |checked, _, cx| {
                                            let _ = ready.update(cx, |this, cx| {
                                                if let Some(ready) =
                                                    this.handoffs.chosen.get_mut(&ready_id)
                                                {
                                                    *ready = *checked;
                                                }
                                                cx.notify();
                                            });
                                        }),
                                ),
                        }
                    }))
            }))
            .when(!detail.tasks.is_empty(), |body| {
                body.child(
                    h_flex().child(
                        button("handoff-create", tr!("handoff-create"))
                            .disabled(state.chosen.is_empty() || state.publishing)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(detail) = &this.handoffs.detail else {
                                    return;
                                };
                                let tasks = detail
                                    .tasks
                                    .iter()
                                    .filter_map(|task| {
                                        this.handoffs.chosen.get(&task.id).map(|&ready| {
                                            HandoffIssueSelection {
                                                id: task.id.clone(),
                                                ready,
                                            }
                                        })
                                    })
                                    .collect();
                                this.handoffs.publishing = true;
                                this.handoffs.error = None;
                                this.send_handoff(Request::CreateIssues {
                                    session: session.clone(),
                                    tasks,
                                });
                                cx.notify();
                            })),
                    ),
                )
            })
            .when(!detail.workers.is_empty(), |body| {
                body.child(div().text_lg().child(tr!("handoff-workers")))
                    .children(detail.workers.iter().enumerate().map(|(index, worker)| {
                        let display = self
                            .handoffs
                            .workers
                            .get(&(detail.session.clone(), worker.thread_id.clone()));
                        let status = display.and_then(|worker| worker.status.as_ref());
                        let output = match status {
                            Some(Status::Finished(text) | Status::Failed(text)) => Some(text.as_str()),
                            _ => display.map(|worker| worker.output.as_str()),
                        }.filter(|text| !text.is_empty());
                        let text_open = display.and_then(|worker| worker.text_open)
                            .unwrap_or(self.appearance.settings.show_worker_messages);
                        let watching = matches!(status, Some(Status::Viewing | Status::Waiting));
                        let adopting = matches!(status, Some(Status::Adopting | Status::Adopted));
                        let buttons = [
                            (
                                Mode::Adopt,
                                "handoff-adopt",
                                tr!("handoff-adopt"),
                                !watching && !adopting,
                            ),
                            (
                                Mode::Wait,
                                "handoff-wait",
                                tr!("handoff-wait"),
                                !watching && !adopting,
                            ),
                            (
                                Mode::View,
                                "handoff-view-worker",
                                tr!("handoff-view-worker"),
                                !watching && !adopting,
                            ),
                            (Mode::Stop, "handoff-stop", tr!("handoff-stop"), watching),
                        ];
                        v_flex()
                            .p_4()
                            .gap_2()
                            .border_1()
                            .rounded_md()
                            .border_color(theme.border)
                            .child(div().child(worker.role.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} · {}", worker.worktree, worker.branch)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} · {}", worker.thread_id, worker.state)),
                            )
                            .child(
                                h_flex().gap_2().children(
                                    buttons
                                        .into_iter()
                                        .filter(|(_, _, _, visible)| *visible)
                                        .map(|(mode, id, label, _)| {
                                            let session = detail.session.clone();
                                            let thread_id = worker.thread_id.clone();
                                            button((id, index), label).on_click(cx.listener(
                                                move |this, _, _, _| {
                                                    this.send_handoff(Request::Worker {
                                                        session: session.clone(),
                                                        thread_id: thread_id.clone(),
                                                        mode,
                                                    });
                                                },
                                            ))
                                        }),
                                ),
                            )
                            .children(
                                status
                                    .map(worker_status)
                                    .map(|text| div().text_sm().child(text)),
                            )
                            .children(
                                output.map(|text| {
                                    let session = detail.session.clone();
                                    let thread = worker.thread_id.clone();
                                    let label = tr!("chat-worker-messages");
                                    let action = if text_open { tr!("chat-collapse") } else { tr!("chat-expand") };
                                    let tooltip = tr!("chat-toggle-details", label = label.to_string(), action = action.to_string());
                                    v_flex().gap_1()
                                        .child(Button::new(("handoff-worker-messages", index))
                                            .text().small().label(label)
                                            .icon(if text_open { IconName::ChevronDown } else { IconName::ChevronRight })
                                            .accessibility_label(tooltip.clone()).tooltip(tooltip)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(worker) = this.handoffs.workers.get_mut(&(session.clone(), thread.clone())) {
                                                    worker.text_open = Some(!text_open);
                                                    cx.notify();
                                                }
                                            })))
                                        .children(text_open.then(|| div().text_sm().child(text.to_owned())))
                                }),
                            )
                    }))
            })
            .when(!detail.skills.is_empty(), |body| {
                body.child(div().text_lg().child(tr!("handoff-skills")))
                    .children(detail.skills.iter().enumerate().map(|(index, skill)| {
                        let session = detail.session.clone();
                        let name = skill.name.clone();
                        h_flex()
                            .gap_2()
                            .child(
                                v_flex().flex_1().min_w_0().child(skill.name.clone()).child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(skill.path.clone()),
                                ),
                            )
                            .map(|row| {
                                if skill.enabled {
                                    row.child(tr!("handoff-skill-enabled"))
                                } else {
                                    row.child(
                                        button(
                                            ("handoff-enable-skill", index),
                                            tr!("handoff-enable-skill"),
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, _| {
                                                this.send_handoff(Request::EnableSkill {
                                                    session: session.clone(),
                                                    name: name.clone(),
                                                });
                                            }),
                                        ),
                                    )
                                }
                            })
                    }))
            })
    }
}

fn worker_status(status: &Status) -> SharedString {
    match status {
        Status::Adopting => tr!("handoff-adopting").into(),
        Status::Adopted => tr!("handoff-adopted").into(),
        Status::Waiting => tr!("handoff-waiting").into(),
        Status::Viewing => tr!("handoff-viewing").into(),
        Status::Stopped => tr!("handoff-stopped").into(),
        Status::Output(_) => tr!("handoff-viewing").into(),
        Status::Finished(_) => tr!("chat-done").into(),
        Status::Failed(_) => tr!("handoff-worker-error").into(),
    }
}
