use super::{FENCE_LANGUAGE, MAX_BLOCK, parse_component};
use crate::ui_kit::Component;
use markdown::{ParseOptions, mdast::Node};
use std::ops::Range;

const OPEN: &str = "<fuzukue:";
const FENCE: &str = "~~~fuzukue-v1";

pub enum BlockContent {
    Component(Component),
    Pending,
    Literal,
}

pub struct ReplyBlock {
    pub range: Range<usize>,
    pub content: BlockContent,
}

/// Find whole reply blocks before rendering Markdown. CommonMark is used here
/// only to identify protected source contexts, including lazy quote continuation.
pub fn reply_blocks(source: &str, streaming: bool, options: &ParseOptions) -> Vec<ReplyBlock> {
    let mut blocks = Vec::new();
    let mut context = None;
    let mut masked = None;
    let mut offset = 0;
    let mut blank_before = true;
    for line in source.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let starts_block = blank_before;
        blank_before = line.trim_matches([' ', '\t', '\r', '\n']).is_empty();
        if blocks.last().is_some_and(|block: &ReplyBlock| start < block.range.end) {
            continue;
        }
        let legacy = line.trim_end_matches(['\r', '\n']) == FENCE;
        let partial = streaming && offset == source.len() && !line.ends_with('\n')
            && (OPEN.starts_with(line) || FENCE.starts_with(line));
        if !starts_block || !(legacy || partial || line.starts_with(OPEN)) {
            continue;
        }
        let ast = context.get_or_insert_with(|| {
            let input = masked.as_deref().map_or(source, |bytes| {
                std::str::from_utf8(bytes).expect("whole source ranges are masked")
            });
            markdown::to_mdast(input, options).ok()
        });
        let Some(Node::Root(root)) = ast else { continue };
        let i = root.children.partition_point(|node| {
            node.position().is_some_and(|position| position.end.offset <= start)
        });
        let node = root.children.get(i);
        let (end, content) = if legacy {
            let Some(Node::Code(code)) = node else { continue };
            let Some(position) = &code.position else { continue };
            if position.start.offset != start || code.lang.as_deref() != Some(FENCE_LANGUAGE) {
                continue;
            }
            let end = position.end.offset;
            let body = &source[offset..end.max(offset)];
            let closed = body.rsplit('\n').next().is_some_and(|line| {
                line.trim_end_matches([' ', '\t', '\r']) == "~~~"
            });
            let content = if closed && (!streaming || source[end..].contains('\n')) {
                component_or_literal(&code.value)
            } else if streaming && source[end..].trim().is_empty() {
                BlockContent::Pending
            } else {
                BlockContent::Literal
            };
            let end = if matches!(content, BlockContent::Pending) { source.len() } else { end };
            (end, content)
        } else {
            let eligible = match node {
                Some(Node::Paragraph(_)) => true,
                Some(Node::Code(code)) if partial && line.starts_with('~') => {
                    code.position.as_ref().is_some_and(|p| p.start.offset == start)
                }
                Some(node) => node.position().is_none_or(|p| p.start.offset > start),
                None => true,
            };
            if !eligible {
                continue;
            }
            match xml_end(&source[start..]) {
                Some(length) => {
                    let end = start + length;
                    let tail = &source[end..];
                    let line_end = tail.find('\n').unwrap_or(tail.len());
                    let content = if !tail[..line_end].trim().is_empty() {
                        BlockContent::Literal
                    } else if streaming && line_end == tail.len() {
                        BlockContent::Pending
                    } else {
                        component_or_literal(&source[start..end])
                    };
                    (end + line_end, content)
                }
                None if streaming => (source.len(), BlockContent::Pending),
                None => (source.len(), BlockContent::Literal),
            }
        };
        if end <= start {
            continue;
        }
        let range = start..end;
        blank_before = true;
        let bytes = masked.get_or_insert_with(|| source.as_bytes().to_vec());
        mask(&mut bytes[range.clone()]);
        context = None;
        blocks.push(ReplyBlock { range, content });
    }
    blocks
}

fn mask(bytes: &mut [u8]) {
    for byte in bytes {
        if !matches!(*byte, b'\r' | b'\n') {
            *byte = b' ';
        }
    }
}

fn component_or_literal(xml: &str) -> BlockContent {
    if xml.len() <= MAX_BLOCK {
        if let Some(component) = parse_component(xml) {
            return BlockContent::Component(component);
        }
    }
    BlockContent::Literal
}

// Find the root boundary independently of schema validation so malformed
// markup can be presented literally. Attribute quotes can contain > characters.
fn xml_end(source: &str) -> Option<usize> {
    let local = source.strip_prefix(OPEN)?;
    let name_len = local
        .bytes()
        .take_while(|byte| byte.is_ascii_lowercase() || *byte == b'-')
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &source[1..OPEN.len() + name_len];
    let mut offset = tag_end(source)?;
    if source[..offset].ends_with("/>") {
        return Some(offset);
    }

    let mut depth = 1;
    while let Some(relative) = source[offset..].find('<') {
        let start = offset + relative;
        offset = start + tag_end(&source[start..])?;
        let tag = &source[start + 1..offset - 1];
        if tag.strip_prefix('/').is_some_and(|closing| closing.trim_end() == name) {
            depth -= 1;
            if depth == 0 {
                return Some(offset);
            }
        } else if !tag.ends_with('/') && tag.strip_prefix(name).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with([' ', '\t', '\r', '\n'])
        }) {
            depth += 1;
        }
    }
    None
}

fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    tag.bytes().enumerate().find_map(|(i, byte)| {
        match quote {
            Some(open) if byte == open => quote = None,
            None if matches!(byte, b'\'' | b'"') => quote = Some(byte),
            None if byte == b'>' => return Some(i + 1),
            _ => {}
        }
        None
    })
}

/// Plain Markdown projection for copying and clients that cannot render the UI.
pub fn text_fallback(source: &str, streaming: bool) -> String {
    project_text(source, streaming, false)
}

/// Readable, inert Markdown for clients without native components.
pub fn markdown_fallback(source: &str, streaming: bool) -> String {
    project_text(source, streaming, true)
}

fn reply_parse_options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// Whether a streaming reply ends in a block with a deferred text projection.
pub fn has_pending_block(source: &str) -> bool {
    reply_blocks(source, true, &reply_parse_options()).last()
        .is_some_and(|block| matches!(block.content, BlockContent::Pending))
}

/// Resolve Markdown card references without modifying the XML source.
pub fn link_card_refs(source: &str) -> String {
    if !source.contains("[[") {
        return source.to_owned();
    }
    let mut linked = String::new();
    let mut previous = 0;
    for block in reply_blocks(source, false, &reply_parse_options()) {
        linked.push_str(&crate::link_card_refs(&source[previous..block.range.start]));
        previous = block.range.end;
        linked.push_str(&source[block.range]);
    }
    linked.push_str(&crate::link_card_refs(&source[previous..]));
    linked
}

fn project_text(source: &str, streaming: bool, escape: bool) -> String {
    let mut text = String::new();
    let mut previous = 0;
    for block in reply_blocks(source, streaming, &reply_parse_options()) {
        text.push_str(&source[previous..block.range.start]);
        previous = block.range.end;
        match block.content {
            BlockContent::Component(component) => {
                // Encode Markdown punctuation, keeping displayed text inert.
                let plain = component.plain_text();
                if !plain.is_empty() {
                    text.push_str("\n\n");
                    for ch in plain.chars() {
                        if escape {
                            match ch {
                                '<' => { text.push_str("&lt;"); continue; }
                                '>' => { text.push_str("&gt;"); continue; }
                                '\\' | '\x60' | '*' | '_' | '[' | ']' | '#' | '+' | '.' | '~' | '$' | '-' => text.push('\\'),
                                '\n' => text.push_str("  "),
                                _ => {}
                            }
                        }
                        text.push(ch);
                    }
                    text.push_str("\n\n");
                }
            }
            BlockContent::Pending => {}
            BlockContent::Literal => text.push_str(&source[block.range]),
        }
    }
    text.push_str(&source[previous..]);
    text
}


#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = r#"<fuzukue:flowchart>
  <fuzukue:step label="Review"/>
  <fuzukue:decision question="Ready?">
    <fuzukue:branch label="Yes" tone="success">Publish</fuzukue:branch>
    <fuzukue:branch label="No" tone="warn">Revise</fuzukue:branch>
  </fuzukue:decision>
</fuzukue:flowchart>"#;

    #[test]
    fn extracts_only_top_level_blocks() {
        let source = format!(
            "Prose\n\n    {FLOW:?}\n\n{text_fence}\n{FLOW}\n~~~~\n\n{quote}\n\n{FLOW}\n\nAfter",
            text_fence = "~~~~xml",
            quote = FLOW.lines().map(|line| format!("> {line}")).collect::<Vec<_>>().join("\n"),
        );
        let blocks = reply_blocks(&source, false, &ParseOptions::gfm());
        let [block] = blocks.as_slice() else { panic!("got {} blocks", blocks.len()) };
        assert_eq!(&source[block.range.clone()], FLOW);
        let BlockContent::Component(component) = &block.content else { panic!("expected flowchart") };
        assert!(component.plain_text().contains("No → Revise"));
        assert!(component.plain_text().contains("Yes → Publish"));

        for text in [
            "\x60<fuzukue:text>Inline</fuzukue:text>\x60",
            "\x60before\n<fuzukue:text>Inline</fuzukue:text>\nafter\x60",
            "> Quote\n<fuzukue:text>Lazy quote continuation</fuzukue:text>",
            "    <fuzukue:text>Indented</fuzukue:text>",
            "~~~xml\n<fuzukue:text>Fenced</fuzukue:text>\n~~~",
        ] {
            assert!(reply_blocks(text, false, &ParseOptions::gfm()).is_empty(), "{text}");
        }
    }

    #[test]
    fn pending_and_final_blocks_preserve_source_ranges() {
        let options = ParseOptions::gfm();
        let partial = &FLOW[..FLOW.find("Publish").unwrap()];
        for end in partial.char_indices().map(|(i, _)| i).skip(1).chain([partial.len()]) {
            let source = &partial[..end];
            let blocks = reply_blocks(source, true, &options);
            assert_eq!(blocks.len(), 1, "{source}");
            assert!(matches!(blocks[0].content, BlockContent::Pending), "{source}");
        }
        assert!(matches!(reply_blocks(partial, false, &options)[0].content, BlockContent::Literal));
        assert!(matches!(reply_blocks(FLOW, false, &options)[0].content, BlockContent::Component(_)));
        let invalid = FLOW.replace("label=\"Yes\"", "href=\"https://example.com\"");
        assert_eq!(text_fallback(&invalid, false), invalid);
        let nested = r#"<fuzukue:card title="Outer"><fuzukue:card title="Nested > value"/></fuzukue:card>"#;
        assert!(matches!(reply_blocks(nested, false, &options)[0].content, BlockContent::Component(_)));
        let legacy = format!("~~~fuzukue-v1\n{FLOW}\n~~~\n");
        assert!(matches!(reply_blocks(&legacy, false, &options)[0].content, BlockContent::Component(_)));
    }
}
