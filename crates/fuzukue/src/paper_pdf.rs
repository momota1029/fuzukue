//! PDF mode for the paper column and its card popup.

use std::collections::{HashMap, HashSet};

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    text::TextView,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{
    anchored, deferred, div, point, prelude::*, px, Deferred, Entity, Pixels, Point, SharedString,
};
use gpui_pdf::{Highlight, NormRect, PdfEvent, PdfView};
use fuzukue_proto::{ClientMsg, PdfCardRegion};
use sha2::{Digest, Sha256};

use super::{
    icons::Symbol, paper::{theorem_frame, theorem_heading}, CommentTarget, Context, ReadPart, RightPane, Window,
    Workbench,
};

#[derive(Default)]
pub(super) struct PaperPdf {
    decks: HashSet<String>,
    maps: HashMap<String, (String, Vec<PdfCardRegion>)>,
    selected_paths: HashMap<String, String>,
    requested: HashSet<String>,
    pub(super) popup: Option<PdfPopup>,
    pub(super) selection_comment: Option<(CommentTarget, Point<Pixels>)>,
}

pub(super) struct PdfPopup {
    pub(super) card: String,
    proof: bool,
    position: Point<Pixels>,
}

impl Workbench {
    pub(super) fn paper_pdf_visible(&self) -> bool {
        self.right == RightPane::Read
            && !self.settings_open
            && !self.reviews_visible()
            && self.paper_pdf.decks.contains(&self.deck)
    }

    pub(super) fn set_paper_pdf(&mut self, show: bool, cx: &mut Context<Self>) {
        if show {
            let popup_editor = self.paper_pdf.popup.as_ref().is_some_and(|popup| {
                self.card_editor(&popup.card, false).is_some()
                    || self.card_editor(&popup.card, true).is_some()
            });
            if self.card_editor.is_some() && !popup_editor && !self.paper_pdf_visible() {
                self.notice = Some(tr!("paper-save-or-discard-card-edits-before-switching-views").into());
                cx.notify();
                return;
            }
            self.paper_pdf.decks.insert(self.deck.clone());
            self.notice = None;
        } else {
            self.paper_pdf.decks.remove(&self.deck);
            self.paper_pdf.selection_comment = None;
        }
        cx.notify();
    }

    pub(super) fn open_paper_pdf_artifact(
        &mut self,
        deck: String,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.paper_pdf.selected_paths.insert(deck.clone(), path);
        self.paper_pdf.decks.insert(deck.clone());
        self.open_paper_deck(deck, cx);
    }

    pub(super) fn invalidate_paper_pdf_maps(&mut self) {
        self.paper_pdf.requested.clear();
        self.paper_pdf.maps.clear();
    }

    pub(super) fn refresh_other_paper_pdf(
        &mut self,
        deck: &str,
        rebuilt: &str,
        cx: &mut Context<Self>,
    ) {
        if self.protocol >= 10 && self.deck == deck && self.paper_pdf_visible() {
            if let Some(path) = self.paper_pdf_path().filter(|path| path != rebuilt) {
                self.request_pdf_map(&path, cx);
            }
        }
    }

    pub(super) fn request_current_pdf_map(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.paper_pdf_path().as_deref() == Some(path)
            && !self.paper_jobs.contains_key(&(self.deck.clone(), None))
        {
            self.request_pdf_map(path, cx);
        }
    }

    fn request_pdf_map(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.paper_pdf.requested.insert(path.to_owned())
            && self
                .out
                .send(ClientMsg::GetPaperPdfMap {
                    deck: self.deck.clone(),
                    pdf: path.into(),
                })
                .is_err()
        {
            self.paper_pdf.requested.remove(path);
            cx.notify();
        }
    }

    fn paper_pdf_path(&self) -> Option<String> {
        if let Some(pdf) = self
            .paper_pdf
            .selected_paths
            .get(&self.deck)
            .or_else(|| self.built_pdfs.get(&self.deck))
        {
            return Some(pdf.clone());
        }
        let build = if self.deck.is_empty() {
            "build/".into()
        } else {
            format!("{}/build/", self.deck)
        };
        for name in ["paper.pdf", "full.pdf", "book.pdf"] {
            let path = format!("{build}{name}");
            if self.pdfs.contains(&path) {
                return Some(path);
            }
        }
        self.pdfs
            .iter()
            .find(|path| {
                path.strip_prefix(&build)
                    .is_some_and(|name| !name.contains('/'))
            })
            .cloned()
    }

    pub(super) fn ensure_paper_pdf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.paper_pdf_visible() {
            return;
        }
        let Some(path) = self.paper_pdf_path() else {
            return;
        };
        if self.pdf_path.as_ref() != Some(&path) {
            self.load_pdf(path.clone(), Some(window), cx);
        }
        if let Some((_, view)) = &self.pdf {
            view.update(cx, |pdf, cx| pdf.set_outline_enabled(false, cx));
        }
        if self.protocol >= 10 && !self.paper_pdf.maps.contains_key(&path) {
            self.request_pdf_map(&path, cx);
        }
    }

    pub(super) fn receive_paper_pdf_map(
        &mut self,
        deck: String,
        pdf: String,
        digest: String,
        regions: Vec<PdfCardRegion>,
        rebuilt: bool,
        cx: &mut Context<Self>,
    ) {
        let requested = self.paper_pdf.requested.remove(&pdf);
        if rebuilt {
            self.built_pdfs.insert(deck, pdf.clone());
        } else if !requested {
            return;
        }
        if self.pdf_path.as_ref() == Some(&pdf) && self.pdf_digest.as_ref() != Some(&digest) {
            if self.pdf.is_some() {
                self.reload_pdf(pdf.clone(), cx);
            } else if rebuilt {
                self.pdf_path = None;
            }
        }
        self.paper_pdf.maps.insert(pdf, (digest, regions));
        self.highlight_pdf_popup(cx);
    }

    pub(super) fn pdf_regions(&self) -> Option<&[PdfCardRegion]> {
        let (digest, regions) = self.paper_pdf.maps.get(self.pdf_path.as_ref()?)?;
        (self.pdf_digest.as_ref()? == digest).then_some(regions.as_slice())
    }

    pub(super) fn on_pdf_event(
        &mut self,
        path: &str,
        view: &Entity<PdfView>,
        event: &PdfEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PdfEvent::LockChanged => {
                self.pdf_digest = view
                    .read(cx)
                    .loaded_bytes()
                    .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
                self.paper_pdf.selection_comment = None;
                self.highlight_pdf_popup(cx);
            }
            PdfEvent::PageClicked {
                page,
                x,
                y,
                position,
                secondary: false,
                click_count: 1,
                ..
            } if self.paper_pdf_visible() => {
                if self.paper_pdf.popup.is_some() {
                    self.dismiss_pdf_popup(cx);
                } else if view.read(cx).page_ready(*page) {
                    self.open_pdf_popup(*page, *x, *y, *position, cx);
                }
            }
            PdfEvent::ViewportClicked if self.paper_pdf_visible() => self.dismiss_pdf_popup(cx),
            PdfEvent::SelectionCleared => {
                if self.paper_pdf.selection_comment.take().is_some() {
                    cx.notify();
                }
            }
            PdfEvent::PageTextSelected {
                page,
                x,
                y,
                position,
                text,
            } if self.paper_pdf_visible() && self.pdf_path.as_deref() == Some(path) => {
                // Preserve the draft if the card editor was already open.
                let editing_popup = self.paper_pdf.popup.as_ref().is_some_and(|popup| {
                    self.card_editor(&popup.card, false).is_some()
                        || self.card_editor(&popup.card, true).is_some()
                });
                if !editing_popup {
                    self.dismiss_pdf_popup(cx);
                }
                if let Some((card, proof)) = self.pdf_card_at(*page, *x, *y) {
                    let quote = text.replace('\\', "\\\\").replace('$', "\\$");
                    let target = CommentTarget { card, proof, quote: Some(quote), source: None };
                    self.paper_pdf.selection_comment = Some((target, *position));
                    cx.notify();
                }
            }
            PdfEvent::PageClicked {
                page,
                x,
                y,
                secondary: true,
                click_count: 1,
                ..
            } if self.right == RightPane::Pdf => {
                let _ = self.out.send(ClientMsg::LocatePdf {
                    pdf: path.into(),
                    page: *page,
                    x: *x,
                    y: *y,
                });
            }
            _ => {}
        }
    }

    fn pdf_card_at(&self, page: usize, x: f32, y: f32) -> Option<(String, bool)> {
        self.pdf_regions()
            .into_iter()
            .flatten()
            .filter(|region| {
                let [left, top, w, h] = region.rect;
                region.page == page
                    && (left..=left + w).contains(&x)
                    && (top..=top + h).contains(&y)
                    && self.cards.iter().any(|card| card.id == region.card)
            })
            .min_by(|a, b| (a.rect[2] * a.rect[3]).total_cmp(&(b.rect[2] * b.rect[3])))
            .map(|region| (region.card.clone(), region.proof))
    }

    fn open_pdf_popup(
        &mut self,
        page: usize,
        x: f32,
        y: f32,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some((card, proof)) = self.pdf_card_at(page, x, y) {
            self.load_card_content(card.clone(), cx);
            self.paper_pdf.popup = Some(PdfPopup { card, proof, position });
            self.highlight_pdf_popup(cx);
            cx.notify();
        }
    }

    pub(super) fn dismiss_pdf_popup(&mut self, cx: &mut Context<Self>) {
        if self.paper_pdf.selection_comment.take().is_some() {
            cx.notify();
        }
        let Some(popup) = self.paper_pdf.popup.take() else {
            return;
        };
        if self.card_editor(&popup.card, false).is_some()
            || self.card_editor(&popup.card, true).is_some()
        {
            self.cancel_card_edit(cx);
        }
        self.highlight_pdf_popup(cx);
        cx.notify();
    }

    fn highlight_pdf_popup(&self, cx: &mut Context<Self>) {
        let Some((_, view)) = &self.pdf else { return };
        let color = cx.theme().link.opacity(0.10);
        let highlights = self
            .paper_pdf
            .popup
            .iter()
            .flat_map(|popup| {
                self.pdf_regions()
                    .into_iter()
                    .flatten()
                    .filter(move |region| region.card == popup.card)
                    .enumerate()
                    .map(move |(id, region)| {
                        let [x, y, w, h] = region.rect;
                        Highlight {
                            id: id as u64,
                            page: region.page,
                            region: Some(NormRect { x, y, w, h }),
                            quote: String::new(),
                            occurrence: 0,
                            color,
                        }
                    })
            })
            .collect();
        view.update(cx, |view, cx| view.set_highlights(highlights, cx));
    }

    pub(super) fn render_paper_pdf(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let path = self.paper_pdf_path();
        let pane = v_flex()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().muted);
        if let Some((_, view)) = self
            .pdf
            .as_ref()
            .filter(|(pdf, _)| Some(pdf) == path.as_ref())
        {
            return pane.child(view.clone());
        }
        let foreground = cx.theme().muted_foreground;
        let pane = pane.p_4().gap_2().text_sm().text_color(foreground);
        if path.is_some() {
            pane.children(self.pdf_status.clone())
        } else {
            pane.child(tr!("paper-no-pdf-has-been-built")).child(
                Button::new("build-paper-pdf")
                    .small()
                    .label(tr!("paper-build-pdf"))
                    .disabled(self.paper_jobs.contains_key(&(self.deck.clone(), None)))
                    .on_click(cx.listener(|this, _, _, _| {
                        let _ = this.out.send(ClientMsg::BuildPaper {
                            deck: this.deck.clone(),
                        });
                    })),
            )
        }
    }

    pub(super) fn render_pdf_popup(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Deferred> {
        if !self.paper_pdf_visible() {
            return None;
        }
        if let Some((target, position)) = &self.paper_pdf.selection_comment {
            let target = target.clone();
            let viewport = window.viewport_size();
            let x = (position.x + px(8.)).clamp(px(8.), (viewport.width - px(128.)).max(px(8.)));
            let y = (position.y + px(8.)).clamp(px(8.), (viewport.height - px(40.)).max(px(8.)));
            return Some(deferred(anchored().position(point(x, y)).child(
                div().id("pdf-selection-comment").occlude().child(
                    Button::new("comment-pdf-selection")
                        .small()
                        .label(tr!("paper-comment"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.paper_pdf.selection_comment = None;
                            this.start_comment(target.clone(), window, cx);
                        })),
                ),
            )).with_priority(1));
        }
        let popup = self.paper_pdf.popup.as_ref()?;
        let id = &popup.card;
        let proof = popup.proof;
        let card = self.cards.iter().find(|card| &card.id == id)?;
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let theorem = theorem_heading(&card.kind, id, &card.title, self.doc_references.as_ref());
        let boxed = theorem.is_some();
        let title = theorem.unwrap_or_else(|| {
            if card.title.is_empty() {
                id.clone()
            } else {
                card.title.clone()
            }
        });
        let title = TextView::markdown(
            SharedString::from(format!("pdf-card-heading-{id}")),
            self.display_card_tex(&title).markdown,
        )
        .markdown_extensions(super::math::extensions_with_macros(&self.doc_math_macros))
        .selectable(false);
        let edit_proof = proof && card.edit_proof.is_some();
        let editor = self.card_editor(id, edit_proof);
        let viewport = window.viewport_size();
        let max_height = viewport.height * 0.65;
        let editor_height = (max_height - px(122.)).clamp(px(96.), px(440.));
        let preview = v_flex()
            .min_w_0()
            .gap_2()
            .when(boxed && !proof && editor.is_none(), |preview| {
                theorem_frame(preview, cx)
            })
            .child(self.appearance.style_paper(div()).font_bold().child(title))
            .when(proof, |preview| {
                preview.child(div().text_sm().child(tr!("paper-proof")))
            })
            .map(|preview| {
                if let Some(editor) = editor {
                    preview.child(self.render_card_editor(editor, &entity, editor_height, cx))
                } else {
                    preview.children(self.render_read_text(id, proof, &entity, cx))
                        .children(self.card_body_status(id, &entity, cx))
                }
            });
        let actions = h_flex().gap_1().flex_wrap()
            .children((!card.external && editor.is_none()).then(|| self.card_history_button(format!("pdf-history-{id}"), id, entity.downgrade())).flatten());
        let source = if edit_proof {
            &card.edit_proof
        } else {
            &card.edit_body
        };
        let actions = actions.children(
            source
                .as_ref()
                .filter(|_| self.protocol >= 9 && editor.is_none())
                .map(|source| {
                    let source = source.clone();
                    let part = ReadPart {
                        card: id.clone(),
                        proof: edit_proof,
                    };
                    Button::new("edit-pdf-card")
                        .small()
                        .label(tr!("paper-edit"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_card_edit(part.clone(), source.clone(), window, cx)
                        }))
                }),
        );
        let target = CommentTarget {
            card: id.clone(),
            quote: None,
            source: None,
            proof,
        };
        let card = id.clone();
        let actions = actions
            .child(
                Button::new("comment-pdf-card")
                    .ghost()
                    .small()
                    .label(tr!("paper-comment"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_comment(target.clone(), window, cx);
                    })),
            )
            .child(
                Button::new("open-pdf-card")
                    .ghost()
                    .small()
                    .label(tr!("paper-open-as-card"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_paper_to_card(card.clone(), proof, cx)
                    })),
            );
        let margin = px(8.);
        let width = (viewport.width * 0.44)
            .clamp(px(300.), px(560.))
            .min(viewport.width - 2. * margin);
        let right = popup.position.x + px(10.);
        let left = popup.position.x - width - px(10.);
        let max_x = viewport.width - width - margin;
        let x = if right <= max_x { right } else { left }.clamp(margin, max_x);
        let y = (popup.position.y + px(10.))
            .clamp(margin, (viewport.height - max_height - margin).max(margin));
        let panel = v_flex()
            .id("pdf-card-popup")
            .occlude()
            .w(width)
            .max_h(max_height)
            .p_3()
            .gap_2()
            .rounded_lg()
            .shadow_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex().justify_between().child(actions).child(
                    Button::new("close-pdf-card")
                        .ghost()
                        .small()
                        .icon(Symbol::X.icon())
                        .accessibility_label(tr!("paper-close"))
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss_pdf_popup(cx))),
                ),
            )
            .child(
                div()
                    .id("pdf-card-content")
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(preview),
            );
        Some(deferred(anchored().position(point(x, y)).child(panel)).with_priority(1))
    }
}
#[cfg(test)]
mod selection_tests;
