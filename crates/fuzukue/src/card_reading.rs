use fuzukue_proto::{CardView, MathMacro};
use fuzukue_view::{card_section_ranges, read_card_markdown, TexMarkdown};
use gpui_kit::{
    component::text::{TextView, TextViewStyle},
    App, ElementId, Overflow, ScrollHandle, SharedString, StyleRefinement, Styled as _, Window,
};

use crate::{math, paper, Workbench};

/// Folder cards carry their TeX paths. Formal cards can also store their
/// statements and proofs as TeX directly in the Markdown file.
pub(super) fn uses_tex(card: &CardView) -> bool {
    !card.tex.is_empty()
        || matches!(
            card.kind.as_str(),
            "axiom"
                | "corollary"
                | "definition"
                | "example"
                | "introdef"
                | "introthm"
                | "lemma"
                | "proof"
                | "proposition"
                | "remark"
                | "theorem"
        )
}

impl Workbench {
    pub(super) fn card_detail_scroll(
        &self,
        body_id: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> ScrollHandle {
        let scroll = window
            .use_keyed_state((ElementId::from(body_id), "scroll"), cx, |_, _| ScrollHandle::new())
            .read(cx)
            .clone();
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        if let Some(activity) = &self.shot_activity {
            *activity.card_detail_scroll.borrow_mut() = Some(scroll.clone());
        }
        scroll
    }

    pub(super) fn display_card_markdown(&self, body: &str) -> TexMarkdown {
        read_card_markdown(body, self.doc_references.as_ref())
    }

    pub(super) fn display_card_detail(&self, card: &CardView, uses_tex: bool) -> TexMarkdown {
        if !uses_tex {
            return self.display_card_markdown(&card.body);
        }
        let mut display = TexMarkdown::default();
        for (heading, range) in card_section_ranges(&card.body) {
            if !heading.is_empty() {
                let heading = self.display_card_tex(heading).markdown;
                display.append(TexMarkdown::unmapped(format!("## {heading}")), 0);
            }
            display.append(
                self.display_card_tex(&card.body[range.clone()]),
                range.start,
            );
        }
        display
    }

    pub(super) fn card_body_text(&self, view: TextView, uses_tex: bool, cx: &App) -> TextView {
        let view = if uses_tex {
            paper::paper_text(view, cx)
        } else {
            let mut table = StyleRefinement::default();
            table.overflow.x = Some(Overflow::Scroll);
            let macros: Vec<_> = self
                .doc_macros
                .iter()
                .map(|(name, body)| MathMacro {
                    name: name.clone(),
                    body: body.clone(),
                    arguments: 0,
                })
                .collect();
            self.markdown_code_style(view, TextViewStyle::default().table(table), cx)
                .markdown_extensions(math::extensions_with_macros(&macros))
                .w_full()
                .min_w_0()
        };
        view.selectable(true)
            .on_link_click(self.markdown_link_handler())
    }
}
