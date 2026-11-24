// Modified by the fuzukue contributors. See VENDORED.md for details.
use gpui::{
    App, IntoElement, ListState, ParentElement as _, SharedString, Styled as _, Window, div,
};

use std::{
    ops::{Range, RangeInclusive},
    sync::Arc,
};

use crate::text::{
    SelectionFormat,
    node::{BlockNode, NodeContext, SelectionStyles, SourceRangeSelection},
};

/// The parsed document AST.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ParsedDocument {
    pub(crate) source: SharedString,
    pub(crate) blocks: Arc<Vec<BlockNode>>,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct NodeRenderOptions {
    pub(crate) ix: usize,
    pub(crate) in_list: bool,
    pub(crate) todo: bool,
    pub(crate) ordered: bool,
    pub(crate) list_start: Option<u32>,
    pub(crate) depth: usize,
    pub(crate) is_last: bool,
    /// Whether this block opens its flow -- the document or a blockquote --
    /// and so takes no gap above it. An HTML block container passes it on to
    /// its own first child.
    pub(crate) is_first: bool,
    /// The previous sibling block, whose own bottom gap counts toward the
    /// gap above a heading or a rule.
    pub(crate) prev: PrevBlock,
}

/// The kind of block before the one being rendered, as far as the gaps of
/// headings and rules care.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PrevBlock {
    #[default]
    Other,
    /// A heading of the given level.
    Heading(u8),
    Rule,
}

/// Where the block at `ix` stands in its flow: whether it is the first block
/// that shows anything, and the visible block before it. Link definitions
/// and other blocks that render nothing are skipped, so a document opening
/// with `[ref]: url` still starts flush.
pub(crate) fn flow_position<B: std::borrow::Borrow<BlockNode>>(
    blocks: &[B],
    ix: usize,
) -> (bool, PrevBlock) {
    let prev = blocks[..ix].iter().rev().map(B::borrow).find(|block| {
        !matches!(
            block,
            BlockNode::Definition { .. } | BlockNode::Break { .. } | BlockNode::Unknown
        )
    });
    match prev {
        None => (true, PrevBlock::Other),
        Some(BlockNode::Heading { level, .. }) => (false, PrevBlock::Heading(*level)),
        Some(BlockNode::HorizontalRule { .. }) => (false, PrevBlock::Rule),
        Some(_) => (false, PrevBlock::Other),
    }
}

impl NodeRenderOptions {
    pub(crate) fn is_last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }
}

impl ParsedDocument {
    pub(super) fn text(&self) -> String {
        self.plain_text(false)
    }

    pub(super) fn plain_text(&self, escape_dollars: bool) -> String {
        let mut text = String::new();
        for block in self.blocks.iter() {
            text.push_str(&block.plain_text(escape_dollars));
        }
        text
    }

    /// The selected text across all blocks, in `format`.
    ///
    /// In [`SelectionFormat::Source`] each block reconstructs its own Markdown
    /// source (inline markup, and block prefixes for headings and lists), and
    /// top-level blocks are joined with a blank line so the result re-renders
    /// with the same block structure.
    ///
    /// Paint and synchronous Copy project the current gesture onto retained
    /// native layouts. In a scrollable (virtualized) view some covered blocks
    /// have never been laid out and report nothing. The selection is continuous,
    /// so its interior blocks are emitted whole rather than dropped. `blocks`
    /// bounds that span; it comes from the selection endpoints, which hold on
    /// to their block index even after scrolling out of view. Without it, fall
    /// back to the blocks with projected selections.
    ///
    /// A standalone image (a paragraph that is only an image) has no selectable
    /// text run, so it never carries a selection of its own. It is therefore
    /// included when it is *enclosed* by the selection — some block before and
    /// some block after it are selected — mirroring how an inline image is
    /// emitted when the selection runs into it. (Select-all returns the source
    /// verbatim, so a leading or trailing image is still copied there.)
    pub(super) fn selected_text(
        &self,
        format: SelectionFormat,
        blocks: Option<RangeInclusive<usize>>,
    ) -> String {
        self.selected_text_with_escapes(format, blocks, false)
    }

    pub(super) fn selected_text_with_escapes(
        &self,
        format: SelectionFormat,
        blocks: Option<RangeInclusive<usize>>,
        escape_dollars: bool,
    ) -> String {
        self.selected_text_with_options(format, blocks, escape_dollars, false)
    }

    pub(super) fn quote_text(&self, blocks: Option<RangeInclusive<usize>>, all: bool) -> String {
        if all {
            return self.blocks.iter().map(|block| block.quote_text(true))
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>().join("\n");
        }
        self.selected_text_with_options(SelectionFormat::Plain, blocks, false, true)
    }

    /// Plain selections that include code keep literal whitespace at their edges.
    pub(super) fn plain_clipboard_with_code(
        &self,
        blocks: Option<RangeInclusive<usize>>,
        escape_dollars: bool,
        all: bool,
    ) -> Option<String> {
        let painted: Vec<_> = self.blocks.iter().map(BlockNode::has_selection).collect();
        let range = if all {
            0..=self.blocks.len().checked_sub(1)?
        } else {
            self.selection_blocks(blocks.as_ref(), &painted)?
        };
        let mut text = String::new();
        let mut contains_code = false;
        for ix in range {
            let interior = blocks
                .as_ref()
                .is_some_and(|range| *range.start() < ix && ix < *range.end());
            contains_code |= self.blocks[ix].append_plain_clipboard(
                all || interior || !painted[ix],
                escape_dollars,
                &mut text,
            );
        }
        if contains_code {
            text.pop(); // The last leaf's separator.
            Some(text)
        } else {
            None
        }
    }

    fn selection_blocks(
        &self,
        blocks: Option<&RangeInclusive<usize>>,
        painted: &[bool],
    ) -> Option<RangeInclusive<usize>> {
        let painted_first = painted.iter().position(|painted| *painted)?;
        let painted_last = painted.iter().rposition(|painted| *painted)?;
        let last = self.blocks.len() - 1;
        Some(match blocks {
            Some(blocks) => *blocks.start().min(&last)..=*blocks.end().min(&last),
            None => painted_first..=painted_last,
        })
    }

    fn selected_text_with_options(
        &self,
        format: SelectionFormat,
        blocks: Option<RangeInclusive<usize>>,
        escape_dollars: bool,
        quote: bool,
    ) -> String {
        let requested_blocks = blocks.clone();
        let painted = self
            .blocks
            .iter()
            .map(|block| block.has_selection())
            .collect::<Vec<_>>();
        let Some(range) = self.selection_blocks(blocks.as_ref(), &painted) else {
            return String::new();
        };
        let (first, last) = (*range.start(), *range.end());

        // A selection within one style copies as the text it renders; one
        // that is only partly painted cannot tell, and keeps its source.
        let format = match format {
            SelectionFormat::SourceAcrossStyles
                if first == last
                    && painted[first]
                    && self.blocks[first].selection_styles() != SelectionStyles::Mixed =>
            {
                SelectionFormat::Plain
            }
            SelectionFormat::SourceAcrossStyles => SelectionFormat::Source,
            format => format,
        };

        if format == SelectionFormat::Plain {
            let mut text = String::new();
            for (ix, block) in self.blocks.iter().enumerate().take(last + 1).skip(first) {
                let is_virtual_endpoint = requested_blocks
                    .as_ref()
                    .is_some_and(|blocks| ix == *blocks.start() || ix == *blocks.end());
                let interior = requested_blocks.is_some() && !is_virtual_endpoint;
                if quote {
                    let block = block.quote_text(interior || !painted[ix]);
                    if !block.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&block);
                    }
                } else if interior {
                    text.push_str(&block.plain_text(escape_dollars));
                } else {
                    let selected = block.selected_plain_text(escape_dollars);
                    if !selected.is_empty() || painted[ix] {
                        text.push_str(&selected);
                    } else {
                        // An unpainted endpoint has no projected selection.
                        text.push_str(&block.plain_text(escape_dollars));
                    }
                }
            }
            return text;
        }

        let mut out: Vec<String> = Vec::new();
        for (ix, block) in self.blocks.iter().enumerate().take(last + 1).skip(first) {
            // The selection is one continuous range, so only the block it
            // starts in and the block it ends in can be partly selected.
            // Everything between them is covered whole, and so is any block
            // that reports nothing — it either scrolled past without painting,
            // or renders no selectable text run at all (a rule, a break, a
            // custom node, a standalone image).
            let source = if (ix == first || ix == last) && painted[ix] {
                block.selected_text(format)
            } else {
                self.whole_source(block)
            };

            let trimmed = source.trim_end_matches('\n');
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        out.join("\n\n")
    }

    /// The whole source of a block the selection covers.
    ///
    /// Copied straight out of the original text, which the Markdown parser
    /// locates per block. That keeps whatever the author wrote — `_italic_`
    /// stays `_italic_`, a reference link keeps its `[ref]` form, a table keeps
    /// its column padding — and it needs no rule of its own per block type.
    /// Blocks the parser could not locate fall back to reconstruction.
    fn whole_source(&self, block: &BlockNode) -> String {
        if let Some(span) = block.span()
            && let Some(source) = self.source.get(span.start..span.end)
        {
            return source.to_string();
        }

        block.selected_text(SelectionFormat::Source)
    }

    pub(super) fn selected_source_range(&self) -> Option<Range<usize>> {
        let mut selected = SourceRangeSelection::Unselected;
        for block in self.blocks.iter() {
            selected.merge(block.selected_source_range());
        }
        selected.into_range()
    }

    /// Synchronously clear the selection stored in every inline state.
    ///
    /// This mirrors the [`selected_text`](Self::selected_text) traversal so the
    /// stored selection can be cleared without relying on a repaint. Offscreen
    /// (virtualized) views do not repaint, so their `InlineState.selection`
    /// would otherwise retain stale values from the last painted frame.
    pub(super) fn clear_selection(&self) {
        for block in self.blocks.iter() {
            block.clear_selection();
        }
    }

    /// Converts the node to markdown format.
    ///
    /// This is used to generate markdown for test.
    #[allow(dead_code)]
    pub(crate) fn to_markdown(&self) -> String {
        self.blocks
            .iter()
            .map(|child| child.to_markdown())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub(super) fn render_root(
        &self,
        list_state: Option<ListState>,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let Some(list_state) = list_state else {
            let blocks_len = self.blocks.len();
            return div().children(self.blocks.iter().enumerate().map(move |(ix, node)| {
                let is_last = ix + 1 == blocks_len;
                let (is_first, prev) = flow_position(&self.blocks, ix);
                node.render_block(
                    NodeRenderOptions {
                        ix,
                        is_last,
                        is_first,
                        prev,
                        ..Default::default()
                    },
                    node_cx,
                    window,
                    cx,
                )
            }));
        };

        let options = NodeRenderOptions {
            is_last: true,
            ..Default::default()
        };

        let blocks = &self.blocks;
        if list_state.item_count() != blocks.len() {
            list_state.reset(blocks.len());
        }

        div().size_full().child(
            gpui::list(list_state, {
                let node_cx = node_cx.clone();
                let blocks = blocks.clone();
                move |ix, window, cx| {
                    let is_last = ix + 1 == blocks.len();
                    let (is_first, prev) = flow_position(&blocks, ix);
                    blocks[ix]
                        .render_block(
                            NodeRenderOptions {
                                ix,
                                is_last,
                                is_first,
                                prev,
                                ..options
                            },
                            &node_cx,
                            window,
                            cx,
                        )
                        .into_any_element()
                }
            })
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{PrevBlock, flow_position};
    use crate::text::{format::markdown, node::NodeContext};

    fn positions(source: &str) -> Vec<(bool, PrevBlock)> {
        let document = markdown::parse(source, &mut NodeContext::default()).unwrap();
        (0..document.blocks.len())
            .map(|ix| flow_position(&document.blocks, ix))
            .collect()
    }

    #[test]
    fn flow_position_skips_blocks_that_render_nothing() {
        // The definition renders nothing, so the heading still opens the flow.
        let flow = positions("[ref]: https://example.com\n\n# Title\n\ntext");
        assert_eq!(flow[1], (true, PrevBlock::Other));
        assert_eq!(flow[2], (false, PrevBlock::Heading(1)));
    }

    #[test]
    fn flow_position_reports_the_previous_heading_or_rule() {
        let flow = positions("text\n\n## A\n\n---\n\n### B");
        assert_eq!(
            flow,
            [
                (true, PrevBlock::Other),
                (false, PrevBlock::Other),
                (false, PrevBlock::Heading(2)),
                (false, PrevBlock::Rule),
            ]
        );
    }
}
