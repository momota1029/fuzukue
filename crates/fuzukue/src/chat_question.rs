use std::collections::BTreeMap;

use fuzukue_proto::{ClientMsg, Question, QuestionAnswer, QuestionOutcome, SECRET_ANSWER_MASK};
use gpui_kit::base::{Checkbox, CheckboxState, Disableable as _, Radio};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _,
};
use gpui_kit::{
    div, prelude::*, px, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, Render, SharedString,
    Window,
};

use super::appearance::ThemeSurfaces;
use super::conn::Sender;

pub(super) struct QuestionCard {
    request_id: String,
    fields: Vec<QuestionField>,
    outcome: Option<(QuestionOutcome, String)>,
    out: Sender,
    focus: FocusHandle,
}

struct QuestionField {
    question: Question,
    selected: Vec<String>,
    other: bool,
    input: Option<Entity<InputState>>,
}

impl QuestionField {
    fn answer(&self, cx: &App) -> Option<QuestionAnswer> {
        let text = if self.other {
            let text = self.input.as_ref()?.read(cx).value();
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(text.to_owned())
        } else {
            None
        };
        (!self.selected.is_empty() || text.is_some()).then(|| QuestionAnswer {
            selected: self.selected.clone(),
            text,
        })
    }
}

impl QuestionCard {
    pub(super) fn focused_input(&self, window: &Window, cx: &App) -> Option<&Entity<InputState>> {
        self.fields.iter().filter_map(|field| field.input.as_ref())
            .find(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    pub(super) fn new(
        request_id: String,
        questions: Vec<Question>,
        out: Sender,
        cx: &Context<Self>,
    ) -> Self {
        Self {
            request_id,
            outcome: None,
            out,
            focus: cx.focus_handle(),
            fields: questions
                .into_iter()
                .map(|question| QuestionField {
                    other: question.options.is_empty(),
                    question,
                    selected: Vec::new(),
                    input: None,
                })
                .collect(),
        }
    }

    pub(super) fn resolve(
        &mut self,
        outcome: QuestionOutcome,
        answers: BTreeMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        let summary = self
            .fields
            .iter()
            .filter_map(|field| {
                let values = answers.get(&field.question.id)?;
                Some(if field.question.secret {
                    SECRET_ANSWER_MASK.into()
                } else {
                    values
                        .iter()
                        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            })
            .collect::<Vec<_>>()
            .join(fuzukue_i18n::STATUS_SEPARATOR);
        self.fields.clear();
        self.outcome = Some((outcome, summary));
        cx.notify();
    }

    fn submit(&self, dismiss: bool, cx: &App) {
        if self.outcome.is_some() {
            return;
        }
        let answers = if dismiss {
            BTreeMap::new()
        } else {
            let Some(answers) = self
                .fields
                .iter()
                .map(|field| Some((field.question.id.clone(), field.answer(cx)?)))
                .collect()
            else {
                return;
            };
            answers
        };
        let _ = self.out.send(ClientMsg::AnswerQuestion {
            request_id: self.request_id.clone(),
            answers,
            dismiss,
        });
    }

    fn make_input(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let field = &mut self.fields[index];
        field
            .input
            .get_or_insert_with(|| {
                let secret = field.question.secret;
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(secret)
                        .placeholder(tr!("chat-question-text-placeholder"))
                });
                cx.subscribe(&state, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
                .detach();
                state
            })
            .clone()
    }

    fn toggle(
        &mut self,
        index: usize,
        option: Option<&str>,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = &mut self.fields[index];
        if !field.question.multi_select {
            field.selected.clear();
            field.other = false;
        }
        match option {
            Some(id) if checked => field.selected.push(id.into()),
            Some(id) => field.selected.retain(|selected| selected != id),
            None => {
                field.other = checked;
                if checked {
                    let input = self.make_input(index, window, cx);
                    window.defer(cx, move |window, cx| {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    });
                }
            }
        }
        cx.notify();
    }

    fn option(
        &self,
        index: usize,
        option: Option<&fuzukue_proto::QuestionOption>,
        cx: &Context<Self>,
    ) -> gpui_kit::AnyElement {
        let field = &self.fields[index];
        let multi = field.question.multi_select;
        let (id, label, description, checked) = match option {
            Some(option) => (
                format!("question-option-{}-{}", field.question.id, option.id),
                option.label.clone(),
                option.description.clone(),
                field.selected.contains(&option.id),
            ),
            None => (
                format!("question-other-{}", field.question.id),
                tr!("chat-question-other").into(),
                None,
                field.other,
            ),
        };
        let theme = cx.theme();
        let indicator = div()
            .flex_none()
            .size(px(15.))
            .mt(px(2.))
            .border_1()
            .border_color(if checked {
                theme.primary
            } else {
                theme.muted_foreground
            })
            .when(multi, |indicator| indicator.rounded_sm())
            .when(!multi, |indicator| indicator.rounded_full())
            .flex()
            .items_center()
            .justify_center()
            .children(checked.then(|| {
                div()
                    .size(px(7.))
                    .bg(theme.primary)
                    .when(multi, |mark| mark.rounded_sm())
                    .when(!multi, |mark| mark.rounded_full())
            }));
        let text = v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .whitespace_normal()
            .child(
                div()
                    .id(SharedString::from(format!("{id}-label")))
                    .child(label.clone()),
            )
            .children(description.map(|description| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(description)
            }));
        let row = h_flex()
            .w_full()
            .items_start()
            .gap_2()
            .child(indicator)
            .child(text);
        let option = option.map(|option| option.id.clone());
        let card = cx.entity();
        let toggle = move |checked, _: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            card.update(cx, |card, cx| {
                card.toggle(index, option.as_deref(), checked, window, cx)
            });
        };
        if multi {
            Checkbox::new(SharedString::from(id))
                .checked(checked)
                .accessibility_label(label)
                .on_change(move |state, event, window, cx| {
                    toggle(state == CheckboxState::Checked, event, window, cx)
                })
                .w_full()
                .cursor_pointer()
                .child(row)
                .into_any_element()
        } else {
            Radio::new(SharedString::from(id))
                .checked(checked)
                .accessibility_label(label)
                .on_change(toggle)
                .w_full()
                .cursor_pointer()
                .child(row)
                .into_any_element()
        }
    }
}

impl Render for QuestionCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if let Some((outcome, summary)) = &self.outcome {
            let label = match outcome {
                QuestionOutcome::Answered => {
                    tr!("chat-question-answered", answers = summary.as_str())
                }
                QuestionOutcome::Dismissed => tr!("chat-question-dismissed").into(),
                QuestionOutcome::Cancelled => tr!("chat-question-cancelled").into(),
            };
            return div()
                .id("question-summary")
                .w_full()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(label)
                .into_any_element();
        }
        let surface = ThemeSurfaces::current(cx).code;
        let border = theme.border;
        let muted = theme.muted_foreground;
        let mut body = v_flex()
            .id("question-card")
            .w_full()
            .min_w_0()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .track_focus(&self.focus)
            .tab_index(0)
            .on_key_down(cx.listener(|card, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.number_of_modifiers() != 0 {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "enter" => card.submit(false, cx),
                    "escape" => card.submit(true, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }));
        for index in 0..self.fields.len() {
            let input = self.fields[index]
                .other
                .then(|| self.make_input(index, window, cx));
            let field = &self.fields[index];
            let question = &field.question;
            let group = v_flex()
                .w_full()
                .min_w_0()
                .items_start()
                .gap_2()
                .children(
                    question
                        .header
                        .as_ref()
                        .filter(|header| !header.is_empty())
                        .map(|header| {
                            div()
                                .max_w_full()
                                .rounded_sm()
                                .px_2()
                                .bg(surface)
                                .text_xs()
                                .text_color(muted)
                                .whitespace_normal()
                                .child(header.clone())
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .text_sm()
                        .whitespace_normal()
                        .child(question.question.clone()),
                )
                .children(
                    question
                        .options
                        .iter()
                        .map(|option| self.option(index, Some(option), cx)),
                )
                .children(
                    (question.allow_other && !question.options.is_empty())
                        .then(|| self.option(index, None, cx)),
                )
                .children(input.map(|input| {
                    div()
                        .id(SharedString::from(format!(
                            "question-other-input-{}",
                            question.id
                        )))
                        .w_full()
                        .child(Input::new(&input).w_full())
                }));
            body = body.child(group);
        }
        body.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("question-send")
                        .primary()
                        .label(tr!("chat-question-send"))
                        .disabled(!self.fields.iter().all(|field| field.answer(cx).is_some()))
                        .on_click(cx.listener(|card, _, _, cx| card.submit(false, cx))),
                )
                .child(
                    Button::new("question-dismiss")
                        .secondary()
                        .label(tr!("chat-question-dismiss"))
                        .on_click(cx.listener(|card, _, _, cx| card.submit(true, cx))),
                ),
        )
        .into_any_element()
    }
}
