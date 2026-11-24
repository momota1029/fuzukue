//! Whole-file editing in the card detail pane.

use fuzukue_proto::{
    CardView, ClientMsg, HistoryScope, SourceEdit, SourceSaveOutcome, CHANGE_HISTORY_PROTOCOL,
    EDITOR_PROTOCOL,
};
use gpui_kit::{
    base::{input::Escape, Disableable as _},
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::EditorState,
        v_flex, ActiveTheme as _, Sizable as _,
    },
    div,
    prelude::*,
    px, Context, Entity, Subscription, Window,
};
use sha2::{Digest, Sha256};

use crate::{focused_composition, icons::Symbol, RightPane, Workbench};
use crate::style_notes::{EditorStyleNotes, StyleNoteView};

use super::editing;

pub(crate) struct CardFileEditor {
    pub(crate) path: String,
    pub(crate) input: Entity<EditorState>,
    _observe: Subscription,
    language: &'static str,
    style_notes: EditorStyleNotes,
    base: String,
    pending: Option<u64>,
    conflict: Option<String>,
    diff: Option<String>,
    confirming: bool,
    error: Option<String>,
}

impl CardFileEditor {
    fn dirty(&self, cx: &gpui_kit::App) -> bool {
        editing::is_dirty(&self.base, self.input.read(cx).text())
    }
}

impl Workbench {
    pub(crate) fn start_edit(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editing {
            if editor.path == path {
                editor.input.update(cx, |input, cx| input.focus(window, cx));
            } else {
                self.notice = Some(tr!("paper-finish-edit-before-opening-card").into());
                cx.notify();
            }
            return;
        }
        let epoch = self.connection_epoch;
        let http = self.http.clone();
        let revision_edit = self.protocol >= EDITOR_PROTOCOL;
        cx.spawn_in(window, async move |this, cx| {
            let p = path.clone();
            let got: Result<String, String> = cx
                .background_executor()
                .spawn(async move {
                    let bytes = if revision_edit {
                        http.fetch_editor(&p)
                    } else {
                        http.fetch(&p)
                    }?;
                    let text = String::from_utf8(bytes)
                        .map_err(|_| tr!("editor-a-utf-8-text-file-is-required").to_owned())?;
                    if text.contains('\0') {
                        return Err(tr!("editor-a-utf-8-text-file-is-required").into());
                    }
                    Ok(text)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.connection_epoch != epoch
                    || this.right != RightPane::Card
                    || this.editing.is_some()
                    || !this
                        .cards
                        .iter()
                        .any(|card| Some(&card.id) == this.selected.as_ref() && card.path == path)
                {
                    return;
                }
                match got {
                    Ok(text) => this.install_card_file_editor(path, text, window, cx),
                    Err(e) => {
                        this.notice = Some(
                            tr!(
                                "workbench-cannot-open",
                                path = path.to_string(),
                                e = e.to_string()
                            )
                            .into(),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn install_card_file_editor(
        &mut self,
        path: String,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let language = fuzukue_view::source_language_with_text(&path, &base);
        let input = cx.new(|cx| editing::editor_state(language, &base, window, cx).folding(false));
        let observe = cx.observe(&input, |_, _, cx| cx.notify());
        input.update(cx, |input, cx| input.focus(window, cx));
        self.notice = None;
        self.editing = Some(CardFileEditor {
            path,
            input,
            _observe: observe,
            language,
            style_notes: EditorStyleNotes::default(),
            base,
            pending: None,
            conflict: None,
            diff: None,
            confirming: false,
            error: None,
        });
        self.sync_card_file_style_notes(cx);
        cx.notify();
    }

    pub(crate) fn sync_card_file_style_notes(&mut self, cx: &mut Context<Self>) {
        let Some(mut editor) = self.editing.take() else { return };
        if let Some(card) = self.cards.iter().find(|card| card.path == editor.path) {
            let notes = self.card_style_diagnostics(&editor.path);
            editor.style_notes.install(card, &editor.base, notes, &editor.input, cx);
        } else {
            editor.style_notes.clear(cx);
        }
        self.editing = Some(editor);
    }

    pub(crate) fn update_card_style_note_color(&self, cx: &mut gpui_kit::App) {
        if let Some(editor) = &self.editing {
            editor.style_notes.update_color(cx);
        }
    }

    fn save_card_file(&mut self, overwrite: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.as_mut() else {
            return;
        };
        if editor.pending.is_some()
            || editor.conflict.is_some() != overwrite
            || (!overwrite && !editor.dirty(cx))
            || focused_composition(&editor.input, window, cx)
        {
            return;
        }

        let request = self.source.id();
        let text = editor.input.read(cx).text().to_string();
        let revision_edit = self.protocol >= EDITOR_PROTOCOL;
        let message = if revision_edit {
            let base = editor.conflict.as_ref().unwrap_or(&editor.base);
            let base_hash = format!("{:x}", Sha256::digest(base));
            ClientMsg::SaveSource {
                request,
                edit: SourceEdit {
                    path: editor.path.clone(),
                    base_hash,
                    range: None,
                    text,
                },
            }
        } else {
            ClientMsg::SaveFile {
                path: editor.path.clone(),
                text,
            }
        };

        if self.out.send(message).is_ok() {
            editor.pending = revision_edit.then_some(request);
            editor.error = None;
            editor.confirming = false;
            if revision_edit {
                editor
                    .input
                    .update(cx, |input, cx| input.set_readonly(true, cx));
            }
        } else {
            editor.error = Some(tr!("paper-cannot-send-to-server").into());
        }
        cx.notify();
    }

    pub(crate) fn card_file_save_pending(&self, request: u64) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|editor| editor.pending == Some(request))
    }

    pub(crate) fn on_card_file_saved(
        &mut self,
        request: u64,
        outcome: SourceSaveOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .editing
            .as_mut()
            .filter(|editor| editor.pending == Some(request))
        else {
            return;
        };
        editor.pending = None;
        match outcome {
            SourceSaveOutcome::Saved { .. } => {
                let path = editor.path.clone();
                let _ = self
                    .out
                    .send(ClientMsg::EditorClosed { path: path.clone() });
                self.notice = Some(tr!("workbench-saved", path = path).into());
                self.editing = None;
            }
            SourceSaveOutcome::Conflict { latest } => {
                editor.conflict = Some(latest);
                editor.diff = None;
            }
            SourceSaveOutcome::Error { message } => {
                editor.error = Some(message);
                editor.input.update(cx, |input, cx| {
                    input.set_readonly(editor.conflict.is_some(), cx)
                });
            }
        }
        cx.notify();
    }

    pub(crate) fn card_file_disconnected(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.editing.as_ref().and_then(|editor| editor.pending) {
            self.on_card_file_saved(
                request,
                SourceSaveOutcome::Error {
                    message: tr!("editor-connection-lost-before-the-save-result-arrived").into(),
                },
                cx,
            );
        }
    }

    pub(crate) fn close_card_file_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(editor) = &mut self.editing else {
            return false;
        };
        if self.right != RightPane::Card
            || !self
                .cards
                .iter()
                .any(|card| Some(&card.id) == self.selected.as_ref() && card.path == editor.path)
        {
            return false;
        }
        if editor.pending.is_none() {
            if editor.dirty(cx) {
                editor.confirming = !editor.confirming;
            } else {
                self.editing = None;
            }
            cx.notify();
        }
        true
    }

    fn reload_card_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .editing
            .as_mut()
            .filter(|editor| editor.pending.is_none())
        else {
            return;
        };
        if let Some(latest) = editor.conflict.take() {
            editor.base = latest;
            editor.style_notes.clear(cx);
            editor.diff = None;
            editor.error = None;
            editor.input.update(cx, |input, cx| {
                input.set_readonly(false, cx);
                input.set_value(&editor.base, window, cx);
                input.focus(window, cx);
            });
            self.sync_card_file_style_notes(cx);
        }
        cx.notify();
    }

    pub(crate) fn card_editor_history_button(
        &self,
        id: String,
        card: &CardView,
        workbench: gpui_kit::WeakEntity<Self>,
    ) -> Option<Button> {
        (!card.external && self.protocol >= CHANGE_HISTORY_PROTOCOL).then(|| {
            let scope = HistoryScope::Card {
                deck: self.deck.clone(),
                card: card.id.clone(),
            };
            let label = tr!("history-title");
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(Symbol::FileDiff.icon())
                .accessibility_label(label)
                .tooltip(label)
                .on_click(move |_, _, cx| {
                    let _ = workbench.update(cx, |this, cx| {
                        this.toggle_history(scope.clone(), cx);
                    });
                })
        })
    }

    pub(crate) fn render_card_file_editor(
        &self,
        editor: &CardFileEditor,
        card: &CardView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let dirty = editor.dirty(cx);
        let pending = editor.pending.is_some();
        let input = editor.input.clone();
        let history = self.card_editor_history_button(
            format!("card-history-{}", card.id),
            card,
            cx.weak_entity(),
        );
        let header = h_flex()
            .id("workbench-pane-header")
            .px(crate::pane_resize::PANE_INSET)
            .h(px(34.))
            .gap_1()
            .flex_none()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(div().flex_1().min_w_0().child(self.render_file_header(
                &card.path,
                "card-file-breadcrumb",
                cx,
            )))
            .children(dirty.then(|| editing::dirty_dot(cx)))
            .child(
                Button::new("save-card")
                    .ghost()
                    .xsmall()
                    .label(if pending {
                        tr!("editor-saving")
                    } else {
                        tr!("editor-save")
                    })
                    .tooltip(tr!("editor-save-shortcut"))
                    .disabled(!dirty || pending || editor.conflict.is_some())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.save_card_file(false, window, cx)),
                    ),
            )
            .child(
                Button::new("cancel-edit")
                    .ghost()
                    .xsmall()
                    .label(tr!("editor-stop-editing"))
                    .tooltip(tr!("editor-stop-editing-shortcut"))
                    .disabled(pending)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_card_file_editor(cx);
                    })),
            )
            .children(history)
            .children((self.pane_close_id() == Some("close-pane"))
                .then(|| self.pane_close_button("close-pane", cx)));
        v_flex()
            .size_full()
            .min_h_0()
            .capture_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if editing::is_save_key(event) {
                        this.save_card_file(false, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action({
                let input = input.clone();
                move |_: &Escape, window, cx| {
                    if focused_composition(&input, window, cx) {
                        cx.stop_propagation();
                    }
                }
            })
            .on_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.close_card_file_editor(cx) {
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .children(self.render_card_style_notes(
                card, StyleNoteView::Editing(&editor.input, &editor.style_notes), cx,
            ).map(|notes| div().px_2().child(notes)))
            .when(editor.confirming && dirty, |pane| {
                pane.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .flex_wrap()
                        .text_xs()
                        .child(tr!("editor-unsaved-changes"))
                        .child(
                            Button::new("card-discard")
                                .ghost()
                                .xsmall()
                                .label(tr!("editor-discard"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.editing = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("card-keep-editing")
                                .ghost()
                                .xsmall()
                                .label(tr!("editor-keep-editing"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(editor) = &mut this.editing {
                                        editor.confirming = false;
                                        editor
                                            .input
                                            .update(cx, |input, cx| input.focus(window, cx));
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .children(editor.error.as_ref().map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone())
            }))
            .children(editor.conflict.as_ref().map(|_| {
                editing::conflict_banner(
                    [
                        Button::new("card-editor-reload")
                            .ghost()
                            .xsmall()
                            .label(tr!("editor-reload"))
                            .disabled(pending)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.reload_card_file(window, cx)
                                }),
                            ),
                        Button::new("card-editor-view-diff")
                            .ghost()
                            .xsmall()
                            .label(if editor.diff.is_some() {
                                tr!("editor-hide-diff")
                            } else {
                                tr!("editor-view-diff")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = &mut this.editing {
                                    if let Some(latest) = &editor.conflict {
                                        editor.diff = if editor.diff.is_some() {
                                            None
                                        } else {
                                            Some(editing::conflict_diff(
                                                latest,
                                                &editor.input.read(cx).text().to_string(),
                                            ))
                                        };
                                        cx.notify();
                                    }
                                }
                            })),
                        Button::new("card-editor-overwrite")
                            .ghost()
                            .xsmall()
                            .label(tr!("editor-overwrite"))
                            .disabled(pending)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_card_file(true, window, cx)
                            })),
                    ],
                    editor.diff.clone(),
                    cx,
                )
            }))
            .child(
                div()
                    .id("card-file-editor")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .w_full()
                    .child(editing::surface(
                        &input,
                        &self.appearance,
                        editor.language,
                        tr!("editor-source-editor"),
                        pending || editor.conflict.is_some(),
                    )),
            )
            .child(editing::status(&input, editor.language, dirty, pending, cx))
    }
}
