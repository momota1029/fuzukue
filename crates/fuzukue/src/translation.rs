use fuzukue_proto::{
    CardSaveOutcome, CardView, ClientMsg, TranslationStatus, TRANSLATION_DRIFT_PROTOCOL,
};
use gpui_kit::component::{v_flex, ActiveTheme as _};
use gpui_kit::{div, prelude::*, AnyElement, Context, MouseButton};

use crate::{pill, wire_deck, Workbench};

pub(super) fn diagnostic(card: &CardView) -> Option<&'static str> {
    match card.translation.as_ref()?.status {
        TranslationStatus::Current => None,
        TranslationStatus::Stale => Some(tr!("workbench-translation-stale")),
        TranslationStatus::Unverified => Some(tr!("workbench-translation-unverified")),
        TranslationStatus::SourceUnavailable => {
            Some(tr!("workbench-translation-source-unavailable"))
        }
    }
}

impl Workbench {
    pub(super) fn translation_diagnostic(
        &self,
        card: &CardView,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let message = diagnostic(card)?;
        let mut section = v_flex()
            .id("translation-detail-diagnostic")
            .items_start()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child(message),
            );
        if let Some(revision) = card
            .translation
            .as_ref()
            .filter(|_| self.protocol >= TRANSLATION_DRIFT_PROTOCOL)
            .and_then(|check| check.revision.clone())
        {
            let button = pill(
                "record-source-fingerprint",
                tr!("workbench-translation-record"),
                false,
                cx,
            );
            let card = card.id.clone();
            section = section.child(button.when(
                self.translation_save_request.is_none(),
                |button| {
                    button.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if this.translation_save_request.is_some() {
                                return;
                            }
                            this.card_save_request += 1;
                            let request = this.card_save_request;
                            if this
                                .out
                                .send(ClientMsg::RecordSourceFingerprint {
                                    request,
                                    deck: wire_deck(&this.deck).to_owned(),
                                    card: card.clone(),
                                    revision: revision.clone(),
                                })
                                .is_ok()
                            {
                                this.translation_save_request = Some(request);
                                this.notice = None;
                            } else {
                                this.notice = Some(tr!("paper-cannot-send-to-server").into());
                            }
                            cx.notify();
                        }),
                    )
                },
            ));
        }
        Some(section.into_any_element())
    }

    pub(super) fn on_translation_save_result(&mut self, outcome: CardSaveOutcome) {
        self.translation_save_request = None;
        self.notice = match outcome {
            CardSaveOutcome::Saved => None,
            CardSaveOutcome::TranslationConflict | CardSaveOutcome::Conflict { .. }
            | CardSaveOutcome::TexUnavailable { .. } | CardSaveOutcome::TexInvalid { .. } => {
                Some(tr!("workbench-translation-conflict").into())
            }
            CardSaveOutcome::Error { message } => {
                Some(tr!("paper-cannot-save", message = message.to_string()).into())
            }
        };
    }
}
