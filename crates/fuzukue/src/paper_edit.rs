//! Inline editing of the on-disk sources of paper cards.

use fuzukue_proto::{CardEditPart, CardEditSource, CardSaveOutcome, ClientMsg};
use gpui_kit::base::{
    input::{Enter, Escape},
    Disableable as _,
};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::EditorState,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, Entity, Focusable, Pixels, SharedString};

use super::{
    focused_composition, icons::Symbol, source::editing, App, Context, ReadPart, Window, Workbench,
};

pub(super) struct CardEditor {
    deck: String,
    part: ReadPart,
    input: Entity<EditorState>,
    _observe: gpui_kit::Subscription,
    base: String,
    language: &'static str,
    confirming: bool,
    diff: Option<String>,
    base_hash: String,
    pending: Option<u64>,
    conflict: Option<CardEditSource>,
    error: Option<String>,
}

impl Workbench {
    pub(super) fn card_editor(&self, id: &str, proof: bool) -> Option<&CardEditor> {
        self.card_editor.as_ref().filter(|editor| {
            editor.deck == self.deck && editor.part.card == id && editor.part.proof == proof
        })
    }

    pub(super) fn start_card_edit(
        &mut self,
        part: ReadPart,
        source: CardEditSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.card_editor.is_some() {
            self.notice = Some(tr!("paper-finish-edit-before-opening-card").into());
            cx.notify();
            return;
        }
        let language = if part.proof {
            "tex"
        } else {
            self.cards
                .iter()
                .find(|card| card.id == part.card)
                .map_or("tex", |card| {
                    let path = card
                        .tex
                        .iter()
                        .find(|path| path.ends_with("statement.tex"))
                        .unwrap_or(&card.path);
                    fuzukue_view::source_language(path)
                })
        };
        let input =
            cx.new(|cx| editing::editor_state(language, &source.body, window, cx).folding(false));
        let observe = cx.observe(&input, |_, _, cx| cx.notify());
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.remeasure_card(&part.card);
        self.comment_popup = None;
        self.comment_selection = None;
        self.card_editor = Some(CardEditor {
            deck: self.deck.clone(),
            part,
            input,
            _observe: observe,
            base: source.body,
            language,
            confirming: false,
            diff: None,
            base_hash: source.base_hash,
            pending: None,
            conflict: None,
            error: None,
        });
        cx.notify();
    }

    fn save_card_edit(&mut self, overwrite: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.card_editor else {
            return;
        };
        if editor.deck != self.deck
            || editor.pending.is_some()
            || editor.conflict.is_some() != overwrite
            || focused_composition(&editor.input, window, cx)
        {
            return;
        }
        self.card_save_request += 1;
        let request = self.card_save_request;
        let result = self.out.send(ClientMsg::SaveCard {
            request,
            deck: editor.deck.clone(),
            card: editor.part.card.clone(),
            part: if editor.part.proof {
                CardEditPart::Proof
            } else {
                CardEditPart::Body
            },
            body: editor.input.read(cx).text().to_string(),
            base_hash: editor
                .conflict
                .as_ref()
                .map_or(&editor.base_hash, |latest| &latest.base_hash)
                .clone(),
        });
        if result.is_ok() {
            editor.pending = Some(request);
            editor.error = None;
            editor.confirming = false;
            editor
                .input
                .update(cx, |input, cx| input.set_readonly(true, cx));
        } else {
            editor.error = Some(tr!("paper-cannot-send-to-server").into());
        }
        let id = editor.part.card.clone();
        self.remeasure_card(&id);
        cx.notify();
    }

    pub(super) fn cancel_card_edit(&mut self, cx: &mut Context<Self>) {
        if self
            .card_editor
            .as_ref()
            .is_some_and(|editor| editor.pending.is_none())
        {
            let editor = self.card_editor.take().unwrap();
            self.remeasure_card(&editor.part.card);
            cx.notify();
        }
    }

    fn request_cancel_paper_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.card_editor else {
            return;
        };
        if editor.pending.is_some() {
            return;
        }
        if editing::is_dirty(&editor.base, editor.input.read(cx).text()) {
            editor.confirming = !editor.confirming;
            let id = editor.part.card.clone();
            self.remeasure_card(&id);
            cx.notify();
        } else if self.paper_pdf_visible() {
            self.dismiss_pdf_popup(cx);
        } else {
            self.cancel_card_edit(cx);
        }
    }

    fn reload_card_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.card_editor else {
            return;
        };
        if editor.pending.is_some() {
            return;
        }
        let Some(latest) = editor.conflict.take() else {
            return;
        };
        editor.base = latest.body;
        editor.input.update(cx, |input, cx| {
            input.set_readonly(false, cx);
            input.set_value(&editor.base, window, cx);
        });
        editor.input.read(cx).focus_handle(cx).focus(window, cx);
        editor.base_hash = latest.base_hash;
        editor.diff = None;
        editor.error = None;
        let id = editor.part.card.clone();
        self.remeasure_card(&id);
        cx.notify();
    }

    pub(super) fn on_card_save_result(
        &mut self,
        request: u64,
        outcome: CardSaveOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .card_editor
            .as_mut()
            .filter(|editor| editor.pending == Some(request))
        else {
            return;
        };
        editor.pending = None;
        match outcome {
            CardSaveOutcome::Saved => self.cancel_card_edit(cx),
            CardSaveOutcome::Conflict { latest } => {
                editor.conflict = Some(latest);
                editor.diff = None;
            }
            CardSaveOutcome::TranslationConflict => {
                editor.error = Some(tr!("workbench-translation-conflict").into());
                editor.input.update(cx, |input, cx| input.set_readonly(false, cx));
            }
            CardSaveOutcome::Error { message } => {
                editor.error = Some(tr!("paper-cannot-save", message = message.to_string()));
                editor.input.update(cx, |input, cx| {
                    input.set_readonly(editor.conflict.is_some(), cx)
                });
            }
        }
        if let Some(editor) = &self.card_editor {
            self.remeasure_card(&editor.part.card);
        }
        cx.notify();
    }

    pub(super) fn disconnect_card_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.card_editor.as_ref().and_then(|editor| editor.pending) {
            self.on_card_save_result(
                request,
                CardSaveOutcome::Error {
                    message: tr!("paper-connection-lost-before-the-save-result-arrived").into(),
                },
                cx,
            );
        }
    }

    pub(super) fn card_edit_button(
        id: &str,
        proof: bool,
        source: &CardEditSource,
        view: &Entity<Self>,
    ) -> Button {
        let part = ReadPart {
            card: id.into(),
            proof,
        };
        let source = source.clone();
        let view = view.clone();
        let group = if proof {
            "paper-proof"
        } else {
            "paper-statement"
        };
        let label = if proof {
            tr!("paper-edit-proof")
        } else {
            tr!("paper-edit-card")
        };
        Button::new(SharedString::from(format!("edit-{group}-{id}")))
            .ghost()
            .xsmall()
            .icon(Symbol::FilePenLine.icon())
            .accessibility_label(label)
            .tooltip(label)
            .opacity(0.)
            .group_hover(group, |style| style.opacity(1.))
            .focus_visible(|style| style.opacity(1.))
            .on_click(move |_, window, cx| {
                view.update(cx, |this, cx| {
                    this.start_card_edit(part.clone(), source.clone(), window, cx)
                });
            })
    }

    pub(super) fn render_card_editor(
        &self,
        editor: &CardEditor,
        view: &Entity<Self>,
        height: Pixels,
        cx: &App,
    ) -> gpui_kit::Div {
        let pending = editor.pending.is_some();
        let conflict = editor.conflict.is_some();
        let dirty = editing::is_dirty(&editor.base, editor.input.read(cx).text());
        let save_key = view.clone();
        let save_action = view.clone();
        let cancel_key = view.clone();
        let escape_input = editor.input.clone();
        let enter_input = editor.input.clone();
        let save = view.clone();
        let cancel = view.clone();
        let reload = view.clone();
        let discard = view.clone();
        let keep = view.clone();
        let show_diff = view.clone();
        let overwrite = view.clone();
        let history = self
            .cards
            .iter()
            .find(|card| card.id == editor.part.card)
            .and_then(|card| {
                self.card_editor_history_button(
                    format!("paper-editor-history-{}", card.id),
                    card,
                    view.downgrade(),
                )
            });
        let theme = cx.theme();
        v_flex()
            .on_key_down(move |event, window, cx| {
                if editing::is_save_key(event) {
                    save_key.update(cx, |this, cx| this.save_card_edit(false, window, cx));
                    cx.stop_propagation();
                }
            })
            .capture_action(move |action: &Enter, window, cx| {
                if focused_composition(&enter_input, window, cx) {
                    cx.stop_propagation();
                } else if action.secondary
                    && !action.shift
                    && enter_input.read(cx).focus_handle(cx).is_focused(window)
                {
                    save_action.update(cx, |this, cx| this.save_card_edit(false, window, cx));
                    cx.stop_propagation();
                }
            })
            .capture_action(move |_: &Escape, window, cx| {
                if focused_composition(&escape_input, window, cx) {
                    cx.stop_propagation();
                }
            })
            .on_action(move |_: &Escape, _, cx| {
                cancel_key.update(cx, |this, cx| this.request_cancel_paper_edit(cx));
                cx.stop_propagation();
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .px_2()
                    .h(gpui_kit::px(32.))
                    .border_b_1()
                    .border_color(theme.border)
                    .children(dirty.then(|| editing::dirty_dot(cx)))
                    .child(
                        Button::new("save-paper-card")
                            .ghost()
                            .xsmall()
                            .label(if pending {
                                tr!("editor-saving")
                            } else {
                                tr!("editor-save")
                            })
                            .tooltip(tr!("editor-save-shortcut"))
                            .disabled(pending || conflict)
                            .on_click(move |_, window, cx| {
                                save.update(cx, |this, cx| this.save_card_edit(false, window, cx));
                            }),
                    )
                    .child(
                        Button::new("cancel-paper-card")
                            .ghost()
                            .xsmall()
                            .label(tr!("editor-stop-editing"))
                            .tooltip(tr!("editor-stop-editing-shortcut"))
                            .disabled(pending)
                            .on_click(move |_, _, cx| {
                                cancel.update(cx, |this, cx| this.request_cancel_paper_edit(cx));
                            }),
                    )
                    .children(history),
            )
            .when(editor.confirming && dirty, |pane| {
                pane.child(
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_1()
                        .flex_wrap()
                        .text_xs()
                        .child(tr!("editor-unsaved-changes"))
                        .child(
                            Button::new("discard-paper-card")
                                .ghost()
                                .xsmall()
                                .label(tr!("editor-discard"))
                                .on_click(move |_, _, cx| {
                                    discard.update(cx, |this, cx| this.cancel_card_edit(cx));
                                }),
                        )
                        .child(
                            Button::new("keep-paper-card")
                                .ghost()
                                .xsmall()
                                .label(tr!("editor-keep-editing"))
                                .on_click(move |_, window, cx| {
                                    keep.update(cx, |this, cx| {
                                        if let Some(editor) = &mut this.card_editor {
                                            editor.confirming = false;
                                            editor
                                                .input
                                                .update(cx, |input, cx| input.focus(window, cx));
                                            let id = editor.part.card.clone();
                                            this.remeasure_card(&id);
                                            cx.notify();
                                        }
                                    });
                                }),
                        ),
                )
            })
            .children(editor.error.as_ref().map(|error| {
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone())
            }))
            .when(conflict, |pane| {
                pane.child(editing::conflict_banner(
                    [
                        Button::new("reload-paper-card")
                            .ghost()
                            .xsmall()
                            .label(tr!("editor-reload"))
                            .disabled(pending)
                            .on_click(move |_, window, cx| {
                                reload.update(cx, |this, cx| this.reload_card_edit(window, cx))
                            }),
                        Button::new("paper-card-diff")
                            .ghost()
                            .xsmall()
                            .label(if editor.diff.is_some() {
                                tr!("editor-hide-diff")
                            } else {
                                tr!("editor-view-diff")
                            })
                            .on_click(move |_, _, cx| {
                                show_diff.update(cx, |this, cx| {
                                    if let Some(editor) = &mut this.card_editor {
                                        if let Some(latest) = &editor.conflict {
                                            editor.diff = if editor.diff.is_some() {
                                                None
                                            } else {
                                                Some(editing::conflict_diff(
                                                    &latest.body,
                                                    &editor.input.read(cx).text().to_string(),
                                                ))
                                            };
                                            let id = editor.part.card.clone();
                                            this.remeasure_card(&id);
                                            cx.notify();
                                        }
                                    }
                                })
                            }),
                        Button::new("overwrite-paper-card")
                            .ghost()
                            .xsmall()
                            .label(tr!("editor-overwrite"))
                            .disabled(pending)
                            .on_click(move |_, window, cx| {
                                overwrite
                                    .update(cx, |this, cx| this.save_card_edit(true, window, cx))
                            }),
                    ],
                    editor.diff.clone(),
                    cx,
                ))
            })
            .child(div().w_full().h(height).min_w_0().child(editing::surface(
                &editor.input,
                &self.appearance,
                editor.language,
                tr!("editor-source-editor"),
                pending || conflict,
            )))
            .child(editing::status(
                &editor.input,
                editor.language,
                dirty,
                pending,
                cx,
            ))
    }
}
