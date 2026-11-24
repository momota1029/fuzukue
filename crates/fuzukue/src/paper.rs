use super::{
    icon_pill, icons::Symbol, math, pill, scroll_list_by_key, App, ClientMsg, Context, Entity, MouseButton, PaperJump,
    Pixels, ReadPart, ReadRow, ReadText, RightPane, SharedString, Window, Workbench,
};
use gpui_kit::base::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, RangeHighlight, TextViewState,
};
use gpui_kit::base::{
    Disableable as _, InteractiveElementExt as _, ScrollBounce, StyledExt as _, TextSelection,
};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    text::TextView,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, rems, StyleRefinement};
use fuzukue_proto::{DocItem, PdfAuditStatus};
use fuzukue_view::{card_section_ranges, theorem_heading_with_label, truncate, TexMarkdown};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub(super) fn paper_list() -> gpui_kit::ListState {
    // Measure the full document for the thumb, but only paint visible cards.
    gpui_kit::ListState::new(0, gpui_kit::ListAlignment::Top, px(300.)).measure_all()
}

fn paper_extensions() -> MarkdownExtensions {
    math::extensions()
        .plugin(ItemLabel)
        .block_surface("fuzukue-quote", |window, cx| {
            div()
                .border_l_2()
                .border_color(cx.theme().border)
                .pl(window.text_style().font_size.to_pixels(window.rem_size()) * 0.9)
                .text_color(cx.theme().muted_foreground)
        })
        .block_surface("fuzukue-center", |_, _| div().text_center())
        .block_surface("fuzukue-theorem", |_, cx| theorem_frame(v_flex(), cx))
}

const ITEM_INDENT: f32 = 2.5;

fn comment_quote(id: impl Into<SharedString>, quote: &str) -> TextView {
    TextView::markdown(id.into(), quote.to_owned())
        .w_full()
        .min_w_0()
        .markdown_extensions(math::extensions())
}

/// Card section headings that hold the statement (source document headings).
pub(super) const STATEMENT_HEADINGS: &[&str] = &["", "Statement", "主張"];

pub(super) fn paper_text(view: TextView, cx: &App) -> TextView {
    // List rows measure their children twice. Give the fit-content TextView
    // the column width in both passes, including when a list has a wide item.
    view.w_full()
        .min_w_0()
        .markdown_extensions(paper_extensions())
        .blockquote_style(
            StyleRefinement::default()
                .border_l_0()
                .pl(rems(ITEM_INDENT))
                .text_color(cx.theme().foreground),
        )
}

struct ItemLabel;

impl MarkdownPlugin for ItemLabel {
    fn name(&self) -> &str {
        "fuzukue-item-label"
    }

    fn parse(
        &self,
        node: &markdown::mdast::Node,
        _: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown::mdast::Node::InlineCode(code) = node else {
            return None;
        };
        let label = code.value.strip_prefix(fuzukue_view::ITEM_LABEL_PREFIX)?;
        Some(
            MarkdownNode::new(self.name(), SharedString::from(label.to_owned()))
                .text(format!("{label} ")),
        )
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        _: &mut App,
    ) -> Option<InlineElement> {
        let label = node.data::<SharedString>()?.clone();
        // Match the inset on the quote and leave a small gap before the body.
        let gap = context.font_size() * 0.35;
        let width = context.rem_size() * ITEM_INDENT - gap;
        let line = window.text_system().shape_line(
            label.clone(),
            context.font_size(),
            &[context.text_style().to_run(label.len())],
            None,
        );
        let text = div().child(label);
        let element = if line.width <= width {
            div()
                .relative()
                .w(px(1.))
                .h(context.line_height())
                .child(text.absolute().right(gap).w(width).text_right())
        } else {
            text.pr(gap)
        };
        Some(InlineElement::new(element))
    }
}

pub(super) fn theorem_frame(element: gpui_kit::Div, cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    element
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted.alpha(0.4))
}

const JOB_FADE: Duration = Duration::from_millis(250);
const EMPTY_REVIEW_DISMISS_DELAY: Duration = Duration::from_secs(8);

pub(super) struct PaperJobResult {
    deck: String,
    failed: bool,
    text: String,
    fade_at: Option<Instant>,
    _timer: Option<gpui_kit::Task<()>>,
}

pub(super) struct ReviewProgress {
    pub details: Option<fuzukue_proto::RunDetails>,
    pub message: String,
    pub model: Option<String>,
    pub failed: bool,
    pub started_at: Option<i64>,
    pub count: Option<usize>,
    pub card: Option<String>,
    pub(super) _dismiss_timer: Option<gpui_kit::Task<()>>,
}

fn can_dismiss_review(started_at: Option<i64>) -> bool {
    started_at.is_none()
}

fn should_auto_dismiss_review(state: &str, failed: bool, count: Option<usize>, card: Option<&str>) -> bool {
    state == "done" && !failed && (count == Some(0) || (count.is_none() && card.is_none()))
}

#[cfg(test)]
mod review_tests {
    use super::{can_dismiss_review, should_auto_dismiss_review};

    #[test]
    fn only_non_running_reviews_can_be_dismissed() {
        assert!(can_dismiss_review(None));
        assert!(!can_dismiss_review(Some(1)));
    }

    #[test]
    fn only_completed_reviews_without_findings_auto_dismiss() {
        assert!(should_auto_dismiss_review("done", false, None, None));
        assert!(should_auto_dismiss_review("done", false, Some(0), None));
        assert!(should_auto_dismiss_review("done", false, Some(0), Some("intro")));
        assert!(!should_auto_dismiss_review("done", false, Some(1), Some("intro")));
        assert!(!should_auto_dismiss_review("done", false, Some(1), None));
        assert!(!should_auto_dismiss_review("done", false, None, Some("intro")));
        assert!(!should_auto_dismiss_review("done", true, Some(0), None));
        for state in ["waiting", "running", "failed", "skipped"] {
            assert!(!should_auto_dismiss_review(state, state == "failed", None, None));
        }
    }
}

pub(super) fn theorem_heading(
    kind: &str,
    id: &str,
    title: &str,
    refs: Option<&fuzukue_proto::DocReferences>,
) -> Option<String> {
    let label = match kind {
        "theorem" | "introthm" => tr!("paper-kind-theorem"),
        "lemma" => tr!("paper-kind-lemma"),
        "proposition" => tr!("paper-kind-proposition"),
        "corollary" => tr!("paper-kind-corollary"),
        "definition" | "introdef" => tr!("paper-kind-definition"),
        "remark" => tr!("paper-kind-remark"),
        "example" => tr!("paper-kind-example"),
        "question" => tr!("paper-kind-question"),
        _ => return None,
    };
    Some(theorem_heading_with_label(label, id, title, refs))
}

fn paper_job_name(kind: Option<&str>) -> &'static str {
    match kind {
        Some("lean") => tr!("paper-lean-review"),
        Some("math") => tr!("paper-math-review"),
        Some("output") => tr!("paper-output-review"),
        Some("consistency") => tr!("paper-consistency-review"),
        Some(_) => tr!("paper-writing-review"),
        None => tr!("paper-pdf-build"),
    }
}

fn unique_quote(text: &str, quote: &str) -> Option<std::ops::Range<usize>> {
    if quote.is_empty() {
        return None;
    }
    let mut matches = text.match_indices(quote);
    let (start, _) = matches.next()?;
    matches
        .next()
        .is_none()
        .then_some(start..start + quote.len())
}

impl Workbench {
    pub(super) fn research_origin_link(
        &self,
        origin: &fuzukue_proto::ResearchOrigin,
        id: String,
        entity: gpui_kit::Entity<Self>,
        cx: &gpui_kit::App,
    ) -> gpui_kit::AnyElement {
        let origin = origin.clone();
        div().id(SharedString::from(id))
            .text_xs().text_color(cx.theme().link).cursor_pointer()
            .hover(|d| d.underline())
            .child(tr!("paper-research-source", deck = origin.deck.to_string(), card = origin.card.to_string()))
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                entity.update(cx, |this, cx| this.open_research_origin(origin.clone(), cx));
            })
            .into_any_element()
    }


    /// 通して読む画面の行とカード本文を更新する。
    pub(super) fn rebuild_read(&mut self, cards_changed: bool, cx: &mut Context<Self>) {
        let mut rows = vec![ReadRow::Title(self.doc_title.clone())];
        for sec in &self.doc_sections {
            rows.push(ReadRow::Section(sec.title.clone()));
            for item in &sec.items {
                rows.push(match item {
                    DocItem::Card { id } => ReadRow::Card(id.clone()),
                    DocItem::Subsection { title } => ReadRow::Subsection(title.clone()),
                });
            }
        }
        let ids: HashSet<_> = rows
            .iter()
            .filter_map(|row| match row {
                ReadRow::Card(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let mut views = HashMap::new();
        let mut changed = HashSet::new();
        for card in self
            .cards
            .iter()
            .filter(|card| ids.contains(card.id.as_str()))
        {
            let uses_tex = super::card_reading::uses_tex(card);
            let parts = if uses_tex {
                let sections = card_section_ranges(&card.body);
                [STATEMENT_HEADINGS, &["Proof", "証明"][..]].map(|headings| {
                    let mut display = TexMarkdown::default();
                    for (_, range) in sections
                        .iter()
                        .filter(|(heading, _)| headings.contains(heading))
                    {
                        display.append(
                            self.display_card_tex(&card.body[range.clone()]),
                            range.start,
                        );
                    }
                    display
                })
            } else {
                [self.display_card_markdown(&card.body), TexMarkdown::default()]
            };
            let mut body = None;
            for (proof, display) in [false, true].into_iter().zip(parts) {
                if display.markdown.trim().is_empty() {
                    continue;
                }
                let part = ReadPart {
                    card: card.id.clone(),
                    proof,
                };
                let text = self
                    .read_md
                    .remove(&part)
                    .filter(|text| {
                        text.uses_tex == uses_tex && *text.display == display && *text.body == card.body
                    })
                    .unwrap_or_else(|| {
                        changed.insert(card.id.clone());
                        let view = cx.new(|cx| TextViewState::markdown(&display.markdown, cx));
                        let observed = part.clone();
                        let last = std::cell::RefCell::new(None);
                        cx.observe(&view, move |this, view, cx| {
                            if !this
                                .read_md
                                .get(&observed)
                                .is_some_and(|text| text.view == view)
                            {
                                return;
                            }
                            let rendered = view.read(cx).rendered_text();
                            if last.borrow().as_ref() != Some(&rendered) {
                                last.replace(Some(rendered));
                                this.highlight_quotes(&observed, &view, cx);
                                if observed.proof
                                    && this.has_proof_comment(&observed.card, cx)
                                    && this.proof_comments.insert(observed.card.clone())
                                {
                                    this.open_proofs
                                        .entry(this.deck.clone())
                                        .or_default()
                                        .insert(observed.card.clone());
                                }
                                if !observed.proof || this.proof_is_open(&observed.card) {
                                    this.remeasure_card(&observed.card);
                                    cx.notify();
                                }
                            }
                        })
                        .detach();
                        ReadText { display: std::sync::Arc::new(display), view, uses_tex,
                            body: body.get_or_insert_with(|| std::sync::Arc::<str>::from(card.body.as_str())).clone() }
                    });
                views.insert(part, text);
            }
        }

        changed.extend(self.read_md.keys().map(|part| part.card.clone()));
        if rows == self.read_rows {
            if cards_changed {
                self.read_list.remeasure_items(0..rows.len());
            } else {
                for card in &changed {
                    self.remeasure_card(card);
                }
            }
        } else {
            let mut anchor = self.read_list.logical_scroll_top();
            let previous = self.read_rows.get(anchor.item_ix);
            self.read_list.reset(rows.len() + 1);
            if let Some(index) = previous.and_then(|row| rows.iter().position(|item| item == row)) {
                anchor.item_ix = index;
                self.read_list.scroll_to(anchor);
            }
        }
        self.read_rows = rows;
        self.read_md = views;
        self.open_commented_proofs(cx);
        if self
            .comment_popup
            .as_ref()
            .is_some_and(|(target, _)| changed.contains(&target.card))
        {
            self.comment_popup = None;
        }
        if self
            .comment_selection
            .as_ref()
            .is_some_and(|part| changed.contains(&part.card))
        {
            self.comment_selection = None;
        }
    }

    fn proof_is_open(&self, card: &str) -> bool {
        self.open_proofs
            .get(&self.deck)
            .is_some_and(|open| open.contains(card))
    }

    fn has_proof_comment(&self, card: &str, cx: &App) -> bool {
        let Some(text) = self.read_md.get(&ReadPart {
            card: card.into(),
            proof: true,
        }) else {
            return false;
        };
        let rendered = text.view.read(cx).rendered_text();
        self.comments
            .iter()
            .filter(|comment| comment.card == card)
            .any(|comment| {
                comment.proof.unwrap_or_else(|| {
                    comment
                        .quote
                        .as_deref()
                        .is_some_and(|quote| !quote.is_empty() && rendered.as_str().contains(quote))
                })
            })
    }

    pub(super) fn open_commented_proofs(&mut self, cx: &App) {
        let cards: Vec<_> = self
            .comments
            .iter()
            .filter(|c| self.has_proof_comment(&c.card, cx))
            .map(|c| c.card.clone())
            .collect();
        let open = self.open_proofs.entry(self.deck.clone()).or_default();
        open.extend(
            cards
                .iter()
                .filter(|card| !self.proof_comments.contains(*card))
                .cloned(),
        );
        self.proof_comments = cards.into_iter().collect();
    }

    pub(super) fn remeasure_card(&self, card: &str) {
        for (ix, row) in self.read_rows.iter().enumerate() {
            if matches!(row, ReadRow::Card(id) if id == card) {
                self.read_list.remeasure_items(ix..ix + 1);
            }
        }
    }

    pub(super) fn highlight_quotes(
        &self,
        part: &ReadPart,
        view: &Entity<TextViewState>,
        cx: &mut Context<Self>,
    ) {
        let background = cx.theme().link.alpha(0.18);
        let rendered = view.read(cx).rendered_text();
        let read = self.read_md.get(part);
        let display = read.map(|text| &text.display);
        let whole_body = read.is_some_and(|text| !text.uses_tex);
        let body = self
            .cards
            .iter()
            .find(|card| card.id == part.card)
            .map(|card| card.body.as_str());
        let highlights = self
            .comments
            .iter()
            .filter(|item| {
                item.card == part.card
                    && (whole_body || item.proof.is_none_or(|proof| proof == part.proof))
            })
            .filter_map(|item| {
                let from_source = || {
                    let source = item.source.as_ref()?;
                    let body = body?;
                    let display =
                        display.filter(|display| display.markdown == rendered.source())?;
                    let range = if body.get(source.range.clone()) == Some(&source.text) {
                        source.range.clone()
                    } else {
                        unique_quote(body, &source.text)?
                    };
                    rendered.range_for_source(display.markdown_range(range)?)
                };
                let range = from_source()
                    .or_else(|| unique_quote(rendered.as_str(), item.quote.as_deref()?))?;
                Some(RangeHighlight::new(range, background))
            });
        view.update(cx, |state, cx| {
            let _ = state.set_range_highlights(highlights, cx);
        });
    }

    pub(super) fn render_read_text(
        &self,
        id: &str,
        proof: bool,
        entity: &Entity<Self>,
        cx: &App,
    ) -> Option<impl IntoElement> {
        let part = ReadPart {
            card: id.into(),
            proof,
        };
        let read = self.read_md.get(&part)?;
        let view = &read.view;
        let link = entity.clone();
        let container = div().w_full().min_w_0().relative();
        let container = if read.uses_tex {
            self.appearance.style_paper(container)
        } else {
            self.appearance.style_reading(container)
        };
        let mut text = container
            .capture_any_mouse_down(move |event, window, cx| {
                if event.button == MouseButton::Left
                    && TextSelection::is_selectable_at(event.position, window, cx)
                {
                    link.update(cx, |this, _| this.comment_selection = Some(part.clone()));
                }
            })
            .child(self.card_body_text(TextView::new(view), read.uses_tex, cx));
        if let Some((target, offset)) = self
            .comment_popup
            .as_ref()
            .filter(|(target, _)| target.card == id && target.proof == proof)
        {
            let target = target.clone();
            let link = entity.clone();
            let dismiss = entity.clone();
            text = text.child(
                pill(format!("quote-{id}"), tr!("paper-comment"), true, cx)
                    .absolute()
                    .left(offset.x)
                    .top(offset.y)
                    .shadow_md()
                    .on_mouse_down_out(move |_, _, cx| {
                        dismiss.update(cx, |this, cx| {
                            this.comment_popup = None;
                            cx.notify();
                        });
                    })
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        link.update(cx, |this, cx| {
                            this.start_comment(target.clone(), window, cx)
                        });
                        cx.stop_propagation();
                    }),
            );
        }
        let row_id = SharedString::from(format!("symbol-text-{id}-{proof}"));
        Some(self.symbol_menu(div().id(row_id).w_full().min_w_0().child(text), entity,
            super::symbols::text_lookup(self.deck.clone(), id.to_owned(), read.body.clone(), read.display.clone(), view.clone())))
    }

    pub(super) fn paper_row_ready(&self, index: usize, cx: &App) -> bool {
        let Some(ReadRow::Card(card)) = self.read_rows.get(index) else {
            return true;
        };
        [false, true].into_iter().all(|proof| {
            (proof && !self.proof_is_open(card))
                || self
                    .read_md
                    .get(&ReadPart {
                        card: card.clone(),
                        proof,
                    })
                    .is_none_or(|text| {
                        text.view.read(cx).rendered_text().source() == text.display.markdown
                    })
        })
    }

    pub(super) fn render_read_pane(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        use crate::pane_resize::PANE_INSET;
        let theme = cx.theme().clone();
        let entity = cx.entity();
        // A jump to the last screen needs enough space below its target on
        // the first layout. Once rows are measured, keep only the shortfall.
        let tail = self.paper_jump.as_ref().map_or(px(0.), |jump| {
            if jump.pending {
                window.viewport_size().height
            } else {
                jump.tail
            }
        });
        let reader = gpui_kit::list(self.read_list.clone(), move |ix, _window, cx| {
            let theme = cx.theme().clone();
            let this = entity.read(cx);
            let Some(row) = this.read_rows.get(ix) else {
                return div().h(tail).into_any_element();
            };
            match row {
                ReadRow::Title(t) => div()
                    .px(PANE_INSET)
                    .pt_6()
                    .pb_2()
                    .text_xl()
                    .child(t.clone())
                    .into_any_element(),
                ReadRow::Section(t) => div()
                    .px(PANE_INSET)
                    .pt_6()
                    .pb_1()
                    .text_lg()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(t.clone())
                    .into_any_element(),
                ReadRow::Subsection(t) => div()
                    .px(PANE_INSET)
                    .pt_4()
                    .text_base()
                    .child(t.clone())
                    .into_any_element(),
                ReadRow::Card(id) => {
                    let card = this.cards.iter().find(|c| &c.id == id);
                    let theorem = card.and_then(|card| {
                        theorem_heading(&card.kind, id, &card.title, this.doc_references.as_ref())
                    });
                    let boxed = theorem.is_some();
                    let header = theorem.unwrap_or_else(|| match card {
                        Some(c) => format!(
                            "{} · {} · {}",
                            if c.title.is_empty() {
                                c.id.clone()
                            } else {
                                c.title.clone()
                            },
                            c.kind,
                            c.status
                        ),
                        None => tr!("paper-missing-card", id = id.to_string()),
                    });
                    let target = id.clone();
                    let link = entity.clone();
                    let heading = if boxed {
                        TextView::markdown(
                            SharedString::from(format!("theorem-heading-{id}")),
                            this.display_card_tex(&header).markdown,
                        )
                        .markdown_extensions(math::extensions())
                        .selectable(false)
                        .into_any_element()
                    } else {
                        div().child(header).into_any_element()
                    };
                    let mut statement = v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .group("paper-statement")
                        .when(boxed, |d| theorem_frame(d, cx))
                        .child(
                            h_flex()
                                .items_start()
                                .gap_2()
                                .child(
                                    div()
                                        .id(SharedString::from(format!("read-head-{id}")))
                                        .flex_1()
                                        .min_w_0()
                                        .when(boxed, |d| this.appearance.style_paper(d).font_bold())
                                        .when(!boxed, |d| {
                                            d.text_xs().text_color(theme.muted_foreground)
                                        })
                                        .cursor_pointer()
                                        .hover(|d| d.text_color(theme.link))
                                        .child(heading)
                                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                            link.update(cx, |this, cx| {
                                                this.select(target.clone(), cx)
                                            });
                                        }),
                                )
                                .children(
                                    card.filter(|card| boxed && !card.status.is_empty()).map(
                                        |card| {
                                            div()
                                                .flex_none()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(card.status.clone())
                                        },
                                    ),
                                )
                                .children(card.and_then(|card| card.edit_body.as_ref())
                                    .filter(|_| this.protocol >= 9 && this.card_editor(id, false).is_none())
                                    .map(|source| Self::card_edit_button(id, false, source, &entity)))
                                .children(card.and_then(|card| card.edit_body.as_ref())
                                    .filter(|_| this.card_editor(id, false).is_none())
                                    .and_then(|source| Self::card_tex_button(id, false, source, &entity)))
                                .children(card.filter(|card| !card.external && this.card_editor(id, false).is_none() && this.card_editor(id, true).is_none())
                                    .and_then(|_| this.card_history_button(format!("paper-history-{id}"), id, entity.downgrade()))),
                        );
                    if let Some(origin) = card.and_then(|card| card.research_origin.as_ref()) {
                        statement = statement.child(this.research_origin_link(origin, format!("read-origin-{id}"), entity.clone(), cx));
                    }
                    // Lean の状態をまとめて一言
                    if let Some(c) = card.filter(|c| !c.lean.is_empty()) {
                        let n = c.lean.len();
                        let ok = c
                            .lean
                            .iter()
                            .filter(|d| this.lean.get(*d).map(String::as_str) == Some("proved"))
                            .count();
                        let color = if ok == n {
                            theme.success
                        } else {
                            theme.warning
                        };
                        statement = statement.child(
                            div()
                                .text_xs()
                                .text_color(color)
                                .child(tr!("paper-lean-proved", ok = ok.to_string(), n = n.to_string())),
                        );
                        if this.protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL {
                            statement = statement.children(c.lean.iter().map(|name| {
                                let name = name.clone();
                                let workbench = entity.clone();
                                div().id(SharedString::from(format!("paper-lean-{id}-{name}")))
                                    .text_xs().text_color(theme.link).cursor_pointer().child(name.clone())
                                    .on_click(move |_, _, cx| {
                                        workbench.update(cx, |this, cx| this.open_card_lean(name.clone(), cx));
                                    })
                            }));
                        }
                    }
                    let statement = if let Some(editor) = this.card_editor(id, false) {
                        statement.child(this.render_card_editor(editor, &entity, px(200.), cx))
                    } else {
                        statement.children(this.render_read_text(id, false, &entity, cx))
                    };
                    let highlighted = this.paper_highlight.as_ref().is_some_and(|(card, _)| card == id);
                    let mut col = v_flex().w_full().min_w_0().px(PANE_INSET).py_4().gap_2()
                        .when(highlighted, |row| row.rounded_lg().bg(theme.link.alpha(0.15)))
                        .child(statement)
                        .children(this.review_markers(id, &entity, cx))
                        .children(card.and_then(|card| {
                            this.card_findings_button(card, format!("paper-review-findings-{id}"), entity.downgrade(), cx)
                        }));
                    if this.read_md.contains_key(&ReadPart { card: id.clone(), proof: true })
                        || (this.protocol >= 9 && card.is_some_and(|card| card.edit_proof.is_some()))
                        || this.card_editor(id, true).is_some() {
                        let open = this.proof_is_open(id);
                        let target = id.clone();
                        let link = entity.clone();
                        col = col.child(
                            h_flex().group("paper-proof").gap_2().child(
                                icon_pill(
                                    format!("proof-{id}"),
                                    if open { Symbol::ChevronDown } else { Symbol::ChevronRight },
                                    tr!("paper-proof"),
                                    cx,
                                )
                                .on_click(move |_, _, cx| {
                                    link.update(cx, |this, cx| {
                                        let cards =
                                            this.open_proofs.entry(this.deck.clone()).or_default();
                                        if !cards.remove(&target) {
                                            cards.insert(target.clone());
                                        }
                                        this.comment_popup = None;
                                        this.remeasure_card(&target);
                                        cx.notify();
                                    });
                                }),
                            ).children(card.and_then(|card| card.edit_proof.as_ref())
                                .filter(|_| this.protocol >= 9 && open && this.card_editor(id, true).is_none())
                                .map(|source| Self::card_edit_button(id, true, source, &entity)))
                              .children(card.and_then(|card| card.edit_proof.as_ref())
                                .filter(|_| open && this.card_editor(id, true).is_none())
                                .and_then(|source| Self::card_tex_button(id, true, source, &entity))),
                        );
                        if open {
                            col = if let Some(editor) = this.card_editor(id, true) {
                                col.child(this.render_card_editor(editor, &entity, px(200.), cx))
                            } else {
                                col.children(this.render_read_text(id, true, &entity, cx))
                            };
                        }
                    }

                    let comments: Vec<_> = this
                        .comments
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| &c.card == id)
                        .collect();
                    let open = this.open_comments.contains(id);
                    if !comments.is_empty() {
                        let target = id.clone();
                        let link = entity.clone();
                        let actions = h_flex().child(
                            icon_pill(
                                format!("comments-{id}"),
                                if open { Symbol::ChevronDown } else { Symbol::ChevronRight },
                                tr!("paper-comment-count", count = comments.len().to_string()),
                                cx,
                            )
                            .on_click(move |_, _, cx| {
                                link.update(cx, |this, cx| {
                                    if !this.open_comments.remove(&target) {
                                        this.open_comments.insert(target.clone());
                                    }
                                    this.read_list.remeasure_items(ix..ix + 1);
                                    cx.notify();
                                });
                            }),
                        );
                        col = col.child(actions);
                    }
                    if open {
                        for (index, comment) in comments {
                            col = col.child(this.render_saved_comment(index, comment, &entity, cx));
                        }
                    }
                    col.into_any_element()
                }
            }
        });
        let n = self.comments.len();
        let mut footer = v_flex()
            .gap_3()
            .p_4()
            .border_t_1()
            .border_color(theme.border);
        if let Some(target) = &self.comment_target {
            footer =
                footer
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(tr!("paper-comment-on-card", card = target.card.to_string())),
                            )
                            .child(pill("cancel-comment", tr!("paper-close"), false, cx).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.comment_target = None;
                                    this.comment_input
                                        .update(cx, |input, cx| input.clean(window, cx));
                                    cx.notify();
                                }),
                            )),
                    )
                    .children(target.quote.as_ref().map(|quote| {
                        div()
                            .id("draft-comment-quote-scroll")
                            .border_l_2()
                            .border_color(theme.link)
                            .pl_3()
                            .text_sm()
                            .max_h(rems(5.))
                            .overflow_y_scroll()
                            .text_color(theme.muted_foreground)
                            .child(comment_quote("draft-comment-quote", quote))
                    }))
                    .child(Self::composer(
                        &self.comment_input,
                        "add-comment",
                        tr!("paper-add"),
                        tr!("paper-write-a-comment"),
                        Self::add_comment,
                        cx,
                    ));
        }
        let here = self.appearance.send_comments_here() || self.protocol < 6;
        let destination = if here { tr!("paper-send-to-current-thread") } else { tr!("paper-apply-in-a-new-thread") };
        let mut actions = h_flex().gap_2().flex_wrap().child(
            Button::new("send-comments").primary().small().disabled(n == 0)
                .label(tr!("paper-send-comments", n = n.to_string()))
                .tooltip(destination)
                .on_click(cx.listener(move |this, _, _, _| this.send_paper_comments(here))),
        );
        if self.protocol >= 6 && n > 0 {
            let workbench = cx.entity();
            actions = actions.child(
                Button::new("comment-destination").ghost().small()
                    .icon(Symbol::ChevronDown.icon())
                    .tooltip(destination)
                    .accessibility_label(tr!("paper-comment-destination"))
                    .dropdown_menu(move |mut menu, _, _| {
                        for (here, label) in [(true, tr!("paper-send-to-current-thread")), (false, tr!("paper-apply-in-a-new-thread"))] {
                            let workbench = workbench.clone();
                            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                workbench.update(cx, |this, cx| {
                                    if let Err(error) = this.appearance.remember_comment_destination(here) {
                                        this.notice = Some(tr!("paper-cannot-save-destination", error = error.to_string()).into());
                                    }
                                    this.send_paper_comments(here);
                                    cx.notify();
                                });
                            }));
                        }
                        menu
                    }),
            );
        }
        footer = footer.child(actions).children(
            self.notice
                .clone()
                .map(|n| div().text_xs().text_color(theme.muted_foreground).child(n)),
        );
        let workbench = cx.entity().downgrade();
        let jump = self.paper_jump.as_ref().and_then(|jump| {
            let index = self.read_card_index(&jump.card)?;
            let anchor = self.read_list.logical_scroll_top();
            (jump.pending || (anchor.item_ix == index && anchor.offset_in_item == px(0.)))
                .then(|| (jump.card.clone(), index))
        });
        let proofs: Vec<_> = self
            .read_md
            .keys()
            .filter(|part| part.proof)
            .map(|part| part.card.clone())
            .collect();
        let all_open = proofs.iter().all(|card| self.proof_is_open(card));
        let reviews = self.reviews_visible();
        let pdf = self.paper_pdf_visible();
        let toolbar = h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .flex_wrap()
            .child(pill("paper-show-cards", tr!("paper-card-view"), !pdf && !reviews, cx).on_click(
                cx.listener(|this, _, _, cx| { this.close_reviews(); this.set_paper_pdf(false, cx); }),
            ))
            .child(pill("paper-show-pdf", tr!("paper-pdf-view"), pdf, cx).on_click(
                cx.listener(|this, _, _, cx| { this.close_reviews(); this.set_paper_pdf(true, cx); }),
            ));
        let toolbar = toolbar.when(self.review_reports_available(), |bar| bar.child(
            pill("paper-show-reviews", tr!("review-title"), reviews, cx).on_click(cx.listener(|this, _, _, cx| {
                this.open_reviews(this.deck.clone(), None, cx);
            })),
        ));
        let toolbar = toolbar.children(self.deck_findings_control("paper-review-findings", cx));
        let return_to_pdf = self.pdf.as_ref().filter(|_| self.from_pdf && !pdf && !reviews).map(|(path, _)| {
            let path = path.clone();
            icon_pill("return-source-pdf", Symbol::ArrowLeft, tr!("paper-back-to-pdf"), cx)
                .on_click(cx.listener(move |this, _, window, cx| this.open_pdf(path.clone(), Some(window), cx)))
        });
        let toolbar = toolbar.children(return_to_pdf).when(!proofs.is_empty() && !reviews, |toolbar| {
            toolbar.child(
                pill("all-proofs", if all_open { tr!("paper-collapse-all") } else { tr!("paper-expand-all") }, false, cx)
                    .when(pdf, |button| button.opacity(0.45))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.paper_pdf_visible() {
                            return;
                        }
                        let open = this.open_proofs.entry(this.deck.clone()).or_default();
                        if all_open {
                            open.clear();
                        } else {
                            open.extend(proofs.iter().cloned());
                        }
                        this.comment_popup = None;
                        this.read_list.remeasure_items(0..this.read_rows.len());
                        cx.notify();
                    })),
            )
        });
        let breadcrumb = self.files.paper_file.as_ref()
            .filter(|(deck, _)| deck == &self.deck && self.file_browser_supported());
        let close = self.pane_close_id()
            .filter(|id| matches!(*id, "close-paper" | "close-paper-pdf" | "close-review"))
            .map(|id| self.pane_close_button(id, cx));
        let toolbar_row = h_flex().px(PANE_INSET).py_2().gap_2()
            .border_b_1().border_color(theme.sidebar_border)
            .child(toolbar);
        let pane = v_flex()
            .size_full()
            .min_w_0()
            .overflow_hidden();
        let pane = if let Some((_, path)) = breadcrumb {
            pane.child(
                h_flex().id("paper-pane-header").px(PANE_INSET).pt_1().gap_2()
                    .child(div().flex_1().min_w_0().child(self.render_file_header(path, "paper-file-breadcrumb", cx)))
                    .children(close),
            ).child(toolbar_row)
        } else {
            pane.child(toolbar_row.id("paper-pane-header").children(close))
        };
        pane.children(self.render_card_tex_build(cx)).child(if reviews {
                self.render_reviews(cx).into_any_element()
            } else if pdf {
                self.render_paper_pdf(cx).into_any_element()
            } else {
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("paper-viewport")
                            .track_focus(&self.paper_focus)
                            .tab_index(0)
                            .relative()
                            .size_full()
                            .on_key_down(cx.listener(|this, event, window, cx| {
                                if scroll_list_by_key(&this.read_list, event, window) {
                                    if this.paper_jump.take().is_some() {
                                        let end = this.read_rows.len();
                                        this.read_list.remeasure_items(end..end + 1);
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }))
                            .child(ScrollBounce::new(
                                "paper-scroll",
                                &self.read_list,
                                reader.size_full(),
                            ))
                            .vertical_scrollbar(&self.read_list),
                    )
                    .on_children_prepainted(move |_, window, cx| {
                        let (Some((card, index)), Some(entity)) = (&jump, workbench.upgrade())
                        else {
                            return;
                        };
                        let Some(tail) = entity.read(cx).paper_jump_tail(*index) else {
                            return;
                        };
                        let card = card.clone();
                        let index = *index;
                        let workbench = workbench.clone();
                        window.defer(cx, move |_, cx| {
                            let _ = workbench.update(cx, |this, cx| {
                                this.settle_paper_jump(&card, index, tail, cx)
                            });
                        });
                    }).into_any_element()
            })
            .children((!reviews).then_some(footer))
    }

    fn render_saved_comment(
        &self, index: usize, comment: &fuzukue_proto::CommentView, entity: &Entity<Self>, cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let link = entity.clone();
        let deck = self.deck.clone();
        let draft = comment.clone();
        v_flex()
            .p_3()
            .gap_2()
            .rounded_md()
            .bg(theme.secondary)
            .children(comment.quote.as_ref().map(|quote| {
                div().border_l_2().border_color(theme.link).pl_3()
                    .text_color(theme.muted_foreground)
                    .child(comment_quote(format!("saved-comment-quote-{index}"), quote))
            }))
            .child(h_flex().items_start().gap_3()
                .child(div().flex_1().min_w_0().child(comment.text.clone()))
                .child(pill(format!("rm-comment-{index}"), tr!("paper-delete"), false, cx)
                    .on_click(move |_, window, cx| {
                        link.update(cx, |this, cx| {
                            let command = if this.protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL {
                                ClientMsg::Trash { action: fuzukue_proto::TrashAction::Delete {
                                    target: fuzukue_proto::TrashTarget::Comment { deck: deck.clone(), draft: draft.clone() },
                                } }
                            } else {
                                ClientMsg::RemoveComment { deck: deck.clone(), index }
                            };
                            this.confirm_removal(command, draft.text.clone(), window, cx);
                        });
                    })))
    }

    pub(super) fn render_deck_comments(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entity = cx.entity();
        v_flex().size_full().min_w_0()
            .child(self.pane_heading(
                div().child(tr!("library-deck-comments", deck = self.deck_title(if self.deck.is_empty() { "." } else { &self.deck }))), cx,
            ).px(crate::pane_resize::PANE_INSET).py_2().border_b_1().border_color(theme.sidebar_border))
            .child(div().id("deck-comments").flex_1().min_h_0().overflow_y_scrollbar()
                .child(v_flex().gap_3().p_4().children(self.comments.iter().enumerate().map(|(index, comment)| {
                    v_flex().gap_1().child(div().text_xs().child(comment.card.clone()))
                        .child(self.render_saved_comment(index, comment, &entity, cx))
                }))))
    }

    fn send_paper_comments(&self, here: bool) {
        if self.comments.is_empty() {
            return;
        }
        let deck = self.deck.clone();
        let session = self.active.clone();
        let command = if here {
            ClientMsg::SendComments { deck, session, confirm: true }
        } else {
            ClientMsg::ApplyComments { deck, origin: session }
        };
        let _ = self.out.send(command);
    }

    pub(super) fn update_paper_job(
        &mut self,
        deck: String,
        kind: Option<String>,
        state: &str,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let name = paper_job_name(kind.as_deref());
        let key = (deck, kind);
        if matches!(state, "cleared" | "discarded") {
            if state == "discarded" {
                self.paper_jobs.remove(&key);
            }
            if self.paper_job_result.as_ref().is_some_and(|result| result.failed && result.deck == key.0) {
                self.paper_job_result = None;
            }
        } else if state == "running" {
            self.paper_jobs.insert(key, message);
        } else {
            self.paper_jobs.remove(&key);
            let deck = if key.0.is_empty() { "." } else { &key.0 };
            let fade_at = (state == "done").then(|| Instant::now() + Duration::from_secs(4));
            let timer = fade_at.map(|fade_at| {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(fade_at.saturating_duration_since(Instant::now()))
                        .await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                    cx.background_executor().timer(JOB_FADE).await;
                    let _ = this.update(cx, |this, cx| {
                        this.paper_job_result = None;
                        cx.notify();
                    });
                })
            });
            self.paper_job_result = Some(PaperJobResult {
                deck: key.0.clone(),
                failed: state == "failed",
                text: format!("{name} · {deck} · {}", truncate(&message, 200)),
                fade_at,
                _timer: timer,
            });
        }
    }

    pub(super) fn update_review_job(
        &mut self, deck: String, kind: String, state: String, mut progress: ReviewProgress,
        cx: &mut Context<Self>,
    ) {
        let key = (deck, kind);
        if matches!(state.as_str(), "superseded" | "cancelled") {
            self.review_jobs.remove(&key);
        } else {
            if state != "running" {
                progress.started_at = None;
            }
            if should_auto_dismiss_review(&state, progress.failed, progress.count, progress.card.as_deref()) {
                let key = key.clone();
                // Replacing or removing the row drops its timer.
                progress._dismiss_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(EMPTY_REVIEW_DISMISS_DELAY).await;
                    let _ = this.update(cx, |this, cx| {
                        this.review_jobs.remove(&key);
                        cx.notify();
                    });
                }));
            }
            self.review_jobs.insert(key, progress);
        }
        let running = self.review_jobs.values().any(|job| job.started_at.is_some());
        if !running {
            self.review_clock = None;
        } else if self.review_clock.is_none() {
            self.review_clock = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }));
        }
    }

    pub(super) fn render_paper_jobs(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if self.paper_jobs.is_empty() && self.paper_job_result.is_none()
            && self.review_jobs.is_empty() && self.pdf_audit.is_empty()
        {
            return None;
        }
        let scroll = window
            .use_keyed_state("paper-jobs-scroll", cx, |_, _| {
                gpui_kit::ScrollHandle::new()
            })
            .read(cx)
            .clone();
        let theme = cx.theme();
        Some(
            div()
                .id("paper-jobs")
                .w_full()
                .flex_none()
                .max_h(px(180.))
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&scroll)
                .vertical_scrollbar(&scroll)
                .border_t_1()
                .border_color(theme.border)
                .children(self.paper_jobs.iter().map(|((deck, kind), message)| {
                    let name = paper_job_name(kind.as_deref());
                    let label = if deck.is_empty() { "." } else { deck };
                    let stop = match kind {
                        Some(kind) => ClientMsg::CancelReview {
                            deck: deck.clone(),
                            kind: kind.clone(),
                        },
                        None => ClientMsg::CancelBuild { deck: deck.clone() },
                    };
                    h_flex()
                        .px_4()
                        .py_1()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .child(format!("{name} · {label} · {message}")),
                        )
                        .when(self.protocol >= 3, |row| {
                            row.child(
                                pill(
                                    format!("stop-{}-{deck}", kind.as_deref().unwrap_or("build")),
                                    tr!("paper-stop"),
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, _| {
                                        let _ = this.out.send(stop.clone());
                                    },
                                )),
                            )
                        })
                }))
                .children(self.review_jobs.iter().map(|((deck, kind), job)| {
                    let name = paper_job_name(Some(kind));
                    let label = if deck.is_empty() { "." } else { deck };
                    let state = if job.failed {
                        "failed"
                    } else if job.started_at.is_some() {
                        "running"
                    } else {
                        "waiting"
                    };
                    let heading = tr!("tiers-review-heading", name = name, state = state, deck = label);
                    let mut row = h_flex().px_4().py_1().gap_2().text_xs().text_color(theme.muted_foreground)
                        .child(crate::chat::chat_activity::status_row(&heading, theme.muted_foreground));
                    let mut details = vec![tr!("tiers-role", role = name)];
                    if let Some(runtime) = &job.details {
                        details.extend(crate::chat::run_details(runtime));
                    } else {
                        details.extend(job.model.iter().cloned());
                    }
                    details.push(job.message.clone());
                    row = row.child(crate::chat::job_details(format!("review-details-{kind}-{deck}"), details));
                    if let Some(started) = job.started_at {
                        let seconds = (super::unix_now() - started).max(0);
                        let stop = ClientMsg::CancelReview { deck: deck.clone(), kind: kind.clone() };
                        row = row.child(format!("{}:{:02}", seconds / 60, seconds % 60))
                            .child(pill(format!("stop-{kind}-{deck}"), tr!("paper-stop"), false, cx)
                                .on_click(cx.listener(move |this, _, _, _| { let _ = this.out.send(stop.clone()); })));
                    } else if let (Some(count), Some(card)) = (job.count, job.card.clone()) {
                        let target = deck.clone();
                        row = row.child(pill(format!("review-comments-{kind}-{deck}"), tr!("paper-findings", count = count), false, cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_paper_target(target.clone(), Some(card.clone()), true, cx);
                            })));
                    }
                    if can_dismiss_review(job.started_at) {
                        let key = (deck.clone(), kind.clone());
                        let label = tr!("paper-dismiss-review");
                        row = row.child(
                            Button::new(SharedString::from(format!("dismiss-review-{kind}-{deck}")))
                                .ghost().xsmall().flex_none().icon(Symbol::X.icon())
                                .tooltip(label).accessibility_label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.review_jobs.remove(&key);
                                    cx.notify();
                                })),
                        );
                    }
                    row
                }))
                .children(self.paper_job_result.as_ref().map(|result| {
                    let opacity = result.fade_at.map_or(1., |start| {
                        let elapsed = Instant::now().saturating_duration_since(start);
                        if elapsed.is_zero() || cx.reduce_motion() {
                            return 1.;
                        }
                        window.request_animation_frame();
                        (1. - elapsed.as_secs_f32() / JOB_FADE.as_secs_f32()).max(0.)
                    });
                    div()
                        .opacity(opacity)
                        .px_4()
                        .py_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(result.text.clone())
                }))
                .when(!self.pdf_audit.is_empty(), |pane| pane.child(self.render_pdf_audit(cx))),
        )
    }

    fn render_pdf_audit(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let warning = theme.warning;
        let dismiss = tr!("paper-audit-dismiss");
        let heading = h_flex()
            .justify_between()
            .child(div().font_semibold().child(tr!("paper-audit-title")))
            .child(Button::new("dismiss-pdf-audit").ghost().xsmall().icon(Symbol::X.icon())
                .tooltip(dismiss).accessibility_label(dismiss)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.pdf_audit.clear();
                    cx.notify();
                })));
        div()
            .id("pdf-audit")
            .border_t_1()
            .border_color(theme.border)
            .px_4()
            .py_1()
            .text_xs()
            .child(heading)
            .children(self.pdf_audit.iter().map(|audit| {
                let checks = [
                    (tr!("paper-audit-links"), &audit.links),
                    (tr!("paper-audit-bookmarks"), &audit.bookmarks),
                    (tr!("paper-audit-synctex"), &audit.synctex),
                ];
                div().pb_1().child(div().font_semibold().child(audit.pdf.clone()))
                    .children(checks.into_iter().map(|(label, check)| {
                        let status = match check.status {
                            PdfAuditStatus::Pass => tr!("paper-audit-pass"),
                            PdfAuditStatus::Fail => tr!("paper-audit-fail"),
                            PdfAuditStatus::Incomplete => tr!("paper-audit-incomplete"),
                        };
                        div().pl_2()
                            .child(div().text_color(if check.status == PdfAuditStatus::Pass { muted } else { warning })
                                .child(format!("{label} · {status}")))
                            .children(check.findings.iter().map(|finding| {
                                div().pl_2().text_color(muted)
                                    .child(format!("{} · {}", finding.location, finding.detail))
                            }))
                    }))
            }))
    }

    pub(super) fn open_paper_from_chat(
        &mut self, deck: String, card: Option<String>, cx: &mut Context<Self>,
    ) {
        self.open_paper_target(deck, card, false, cx);
    }

    pub(super) fn open_paper_target(&mut self, deck: String, card: Option<String>, comments: bool, cx: &mut Context<Self>) {
        self.close_reviews();
        self.files.paper_file = None;
        let deck = super::wire_deck(&deck).to_owned();
        self.settings_open = false;
        self.active_tab = self.tabs.iter()
            .find(|tab| tab.deck.as_deref() == Some(&deck)).map(|tab| tab.id.clone());
        self.right = RightPane::Read;
        self.pending_paper_open = Some((deck.clone(), card, comments));
        if self.deck == deck {
            self.finish_paper_open(cx);
        } else {
            let _ = self.out.send(ClientMsg::SelectPaper { deck });
        }
        cx.notify();
    }

    pub(super) fn finish_paper_open(&mut self, cx: &mut Context<Self>) {
        if self.right != RightPane::Read || !self.pending_paper_open.as_ref()
            .is_some_and(|(deck, _, _)| deck == &self.deck)
        {
            return;
        }
        let (_, card, comments) = self.pending_paper_open.take().unwrap();
        self.paper_highlight = None;
        if let Some(card) = card {
            if self.files.paper_file.is_none() {
                self.files.paper_file = self.cards.iter().find(|entry| entry.id == card)
                    .map(|entry| (self.deck.clone(), entry.path.clone()));
            }
            if self.read_card_index(&card).is_none() {
                if comments {
                    self.right = RightPane::DeckComments;
                } else if self.cards.iter().any(|entry| entry.id == card) {
                    self.select(card, cx);
                }
                return;
            }
            if comments {
                self.open_comments.insert(card.clone());
                if let Some(i) = self.read_card_index(&card) {
                    self.read_list.remeasure_items(i..i + 1);
                }
            }
            self.scroll_paper_to_card(card.clone(), false, cx);
            let timer = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |this, cx| {
                    this.paper_highlight = None;
                    cx.notify();
                });
            });
            self.paper_highlight = Some((card, timer));
        }
    }

    pub(super) fn jump_to_card(&mut self, id: String, cx: &mut Context<Self>) {
        self.scroll_paper_to_card(id, true, cx);
    }

    pub(super) fn scroll_paper_to_card(&mut self, id: String, show_proof: bool, cx: &mut Context<Self>) {
        self.set_paper_pdf(false, cx);
        let Some(index) = self.read_card_index(&id) else {
            self.select(id, cx);
            return;
        };
        if show_proof {
            self.open_proofs
                .entry(self.deck.clone())
                .or_default()
                .insert(id.clone());
            self.remeasure_card(&id);
        }
        self.paper_jump = Some(PaperJump {
            card: id,
            tail: px(0.),
            pending: true,
        });
        self.read_list
            .remeasure_items(self.read_rows.len()..self.read_rows.len() + 1);
        self.read_list.scroll_to(gpui_kit::ListOffset {
            item_ix: index,
            offset_in_item: px(0.),
        });
        self.right = RightPane::Read;
        self.from_pdf = false;
        self.settings_open = false;
        self.comment_popup = None;
        cx.notify();
    }

    fn read_card_index(&self, id: &str) -> Option<usize> {
        self.read_rows
            .iter()
            .position(|row| matches!(row, ReadRow::Card(card) if card == id))
    }

    fn paper_jump_tail(&self, index: usize) -> Option<Pixels> {
        let jump = self.paper_jump.as_ref()?;
        if self.read_list.is_scrollbar_dragging() {
            return None;
        }
        let target = self.read_list.bounds_for_item(index)?;
        let last = self.read_list.bounds_for_item(self.read_rows.len() - 1)?;
        let viewport = self.read_list.viewport_bounds();
        let tail = (viewport.size.height - (last.bottom() - target.top())).max(px(0.));
        (jump.pending || (tail - jump.tail).abs() >= px(1.)).then_some(tail)
    }

    fn settle_paper_jump(
        &mut self,
        card: &str,
        index: usize,
        tail: Pixels,
        cx: &mut Context<Self>,
    ) {
        if self.read_card_index(card) != Some(index) {
            return;
        }
        let Some(jump) = self.paper_jump.as_mut().filter(|jump| jump.card == card) else {
            return;
        };
        jump.tail = tail;
        jump.pending = false;
        let end = self.read_rows.len();
        self.read_list.remeasure_items(end..end + 1);
        self.read_list.scroll_to(gpui_kit::ListOffset {
            item_ix: index,
            offset_in_item: px(0.),
        });
        cx.notify();
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod paper_tests {
    use super::*;
    use gpui_kit::base::text::editor_host::{parse_markdown, BlockNode};
    use gpui_kit::{list, point, size, Bounds, ListOffset, WindowBounds, WindowOptions};

    #[test]
    fn self_update_card_keeps_the_shell_fence_and_its_line_breaks() {
        let card = include_str!("../../../cards/design/self-update.md");
        let start = card.find("## 改名前の監督での入れ替え\n").unwrap();
        let section = &card[start..];
        let close = "\n```\n\n";
        let (before_close, _) = section.split_once(close).unwrap();
        let (_, expected) = before_close.split_once("\n```sh\n").unwrap();
        let section = &section[..before_close.len() + close.len()];
        let sections = card_section_ranges(section);
        let [(heading, range)] = sections.as_slice() else {
            panic!("expected one section, got {sections:?}");
        };
        let mut source = TexMarkdown::unmapped(format!("## {heading}"));
        source.append(fuzukue_view::read_card_tex(&section[range.clone()], None, &[]), range.start);
        let blocks = parse_markdown(&source.markdown, paper_extensions().into()).unwrap();

        let [BlockNode::Heading { .. }, BlockNode::Paragraph(first), BlockNode::Paragraph(second), BlockNode::CodeBlock(code)] =
            blocks.as_slice()
        else {
            panic!("expected a heading, two paragraphs, and one fenced code block, got:\n{}", source.markdown);
        };
        assert_eq!(code.lang().as_deref(), Some("sh"));
        assert_eq!(code.code().as_ref(), expected);
        assert!(first
            .children()
            .iter()
            .chain(second.children())
            .all(|node| node.as_custom().is_none()));
    }

    #[test]
    fn card_and_chat_keep_math_outside_shell_code() {
        let shell = "echo \"$HOME\"\n前**「コード」**後 $((1 + 2))";
        let input = format!("`echo $HOME` と $x^2$\n\n```sh\n{shell}\n```");
        let card = fuzukue_view::read_card_tex(&input, None, &[]);
        let chat = fuzukue_view::link_card_refs(&input);
        for (source, extensions) in [(card.markdown, paper_extensions()), (chat, math::extensions())] {
            let blocks = parse_markdown(&source, extensions.into()).unwrap();
            let [BlockNode::Paragraph(prose), BlockNode::CodeBlock(code)] = blocks.as_slice() else {
                panic!("expected prose and one fenced code block, got {blocks:?}");
            };
            assert_eq!(code.lang().as_deref(), Some("sh"));
            assert_eq!(code.code().as_ref(), shell);
            let math: Vec<_> = prose.children().iter().filter_map(|node| node.as_custom()).collect();
            assert_eq!(math.len(), 1);
            assert_eq!(math[0].name(), "fuzukue-inline-math");
            assert_eq!(math[0].as_text(), "$x^2$");
            assert!(prose.children().iter().any(|node| node.mark_runs().iter().any(|(range, mark)| {
                mark.code && &node.text()[range.clone()] == "echo $HOME"
            })));
        }
    }

    #[test]
    fn card_tex_keeps_math_and_surfaces_in_the_text_view_document() {
        let source = fuzukue_view::read_card_tex(
            r"
            \begin{introthm}[A title]
            \begin{enumerate}
            \item[(c)] A claim $x$.
            \[
            x = y
            \]
            \end{enumerate}
            \end{introthm}
            \begin{center}Centered text.\end{center}
            ",
            None,
            &[],
        );
        let blocks = parse_markdown(&source.markdown, paper_extensions().into()).unwrap();
        let [BlockNode::Surface { name, children, .. }, BlockNode::Surface { name: center, .. }] =
            blocks.as_slice()
        else {
            panic!("expected theorem and centered surfaces, got {blocks:?}");
        };
        assert_eq!(name.as_ref(), "fuzukue-theorem");
        assert_eq!(center.as_ref(), "fuzukue-center");
        assert!(children.iter().any(
            |block| matches!(block, BlockNode::Blockquote { children, .. }
            if children.iter().any(|block| matches!(block, BlockNode::Custom { .. })))
        ));
        assert!(!children
            .iter()
            .any(|block| matches!(block, BlockNode::List { .. })));
    }

    struct PaperRows {
        state: gpui_kit::ListState,
        first_height: f32,
    }

    impl Render for PaperRows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let first_height = self.first_height;
            list(self.state.clone(), move |ix, _, _| {
                div()
                    .h(px(if ix == 0 { first_height } else { 40. }))
                    .into_any_element()
            })
            .size_full()
        }
    }

    #[test]
    fn paper_list_preserves_the_jump_when_an_offscreen_row_grows() {
        gpui_kit::platform::headless().run(|cx| {
            let state = paper_list();
            state.reset(12);
            let anchor = ListOffset {
                item_ix: 5,
                offset_in_item: px(0.),
            };
            state.scroll_to(anchor);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.), px(0.)),
                            size: size(px(320.), px(120.)),
                        })),
                        show: false,
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|_| PaperRows {
                            state: state.clone(),
                            first_height: 40.,
                        })
                    },
                )
                .expect("open a headless paper list");
            for (first_height, max_offset) in [(40., 360.), (90., 410.), (20., 340.)] {
                window
                    .update(cx, |rows, _, cx| {
                        rows.first_height = first_height;
                        rows.state.remeasure_items(0..1);
                        cx.notify();
                    })
                    .expect("update paper list");
                gpui_kit::AnyWindowHandle::from(window)
                    .update(cx, |_, window, cx| {
                        window.draw(cx).clear(cx);
                    })
                    .expect("draw paper list");
                assert_eq!(state.logical_scroll_top(), anchor);
                let viewport = state.viewport_bounds();
                let target = state.bounds_for_item(5).expect("jump target is measured");
                let next = state.bounds_for_item(6).expect("next row is measured");
                assert_eq!(target.top(), viewport.top());
                assert_eq!(next.top(), target.bottom());
                assert_eq!(state.max_offset_for_scrollbar().y, px(max_offset));
            }
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }
}
