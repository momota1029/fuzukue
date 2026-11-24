use std::{borrow::Cow, ops::Range};

use markdown::{mdast::Node, Constructs, ParseOptions};

/// Keep all literal whitespace while converting line endings to LF.
pub fn normalize_code_text(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Remove one final line ending while preserving blank lines and indentation.
fn clean_code_text(text: &str) -> String {
    let mut text = normalize_code_text(text).into_owned();
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Wrap raw tool output in a fence that cannot terminate within the output.
pub fn fenced_output(text: &str) -> String {
    let text = clean_code_text(text);
    let fence_length = text
        .split(|ch| ch != '~')
        .map(str::len)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(3);
    let fence = "~".repeat(fence_length);
    // The parser removes the newline immediately before the closing fence.
    format!("{fence}\n{text}\n{fence}")
}

/// Whether a Markdown preview needs literal code rendering.
pub fn has_code(source: &str) -> bool {
    !ranges(source).is_empty()
}

pub(super) enum Code {
    Fenced { info: String, text: String },
    Inline { text: String, preserve_source: bool },
}

pub(super) struct CodeRange {
    pub range: Range<usize>,
    pub code: Code,
}

impl CodeRange {
    pub fn is_fenced(&self) -> bool {
        matches!(self.code, Code::Fenced { .. })
    }
}

/// Identify Markdown literals in a TeX card, where indentation is common.
pub(super) fn ranges(source: &str) -> Vec<CodeRange> {
    ranges_with_indented_code(source, false)
}

/// Protect both fenced and indented code when rewriting Markdown links.
pub(super) fn link_ranges(source: &str) -> Vec<CodeRange> {
    ranges_with_indented_code(source, true)
}

fn ranges_with_indented_code(source: &str, code_indented: bool) -> Vec<CodeRange> {
    let has_delimiters = source.contains(['`', '~']);
    let has_indentation = code_indented && (source.contains('\t') || source.contains("    "));
    if !has_delimiters && !has_indentation {
        return Vec::new();
    }
    let options = ParseOptions {
        constructs: Constructs {
            code_indented,
            ..Constructs::gfm()
        },
        ..ParseOptions::default()
    };
    let mut ranges = Vec::new();
    if let Ok(root) = markdown::to_mdast(source, &options) {
        collect(root, &mut ranges, false);
    }
    ranges
}

fn collect(node: Node, ranges: &mut Vec<CodeRange>, in_container: bool) {
    let (position, code) = match node {
        Node::Code(code) => {
            let mut info = code.lang.unwrap_or_default();
            if let Some(meta) = code.meta {
                info.push(' ');
                info.push_str(&meta);
            }
            (
                code.position,
                Code::Fenced {
                    info,
                    text: code.value,
                },
            )
        }
        Node::InlineCode(code) => (
            code.position,
            Code::Inline {
                text: code.value,
                preserve_source: !in_container,
            },
        ),
        mut parent => {
            let in_container =
                in_container || matches!(parent, Node::Blockquote(_) | Node::ListItem(_));
            if let Some(children) = parent.children_mut() {
                for child in std::mem::take(children) {
                    collect(child, ranges, in_container);
                }
            }
            return;
        }
    };
    if let Some(position) = position {
        ranges.push(CodeRange {
            range: position.start.offset..position.end.offset,
            code,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_drops_one_final_newline_and_preserves_literal_lines() {
        let long_line = "wrapped word ".repeat(30);
        let source = format!("\t{long_line}\r\n\r\n\t~~~\r\n\r\n");
        let expected = format!("\t{long_line}\n\n\t~~~\n");
        assert_eq!(clean_code_text(&source), expected);
        assert_eq!(normalize_code_text(&source), format!("{expected}\n"));
        let markdown = fenced_output(&source);
        let [range] = ranges(&markdown).try_into().ok().unwrap();
        let Code::Fenced { text, .. } = range.code else {
            panic!("expected a fence")
        };
        assert_eq!(text, expected);
        for input in ["", "\n", "one", "one\r", "\tone  "] {
            let markdown = fenced_output(input);
            let [range] = ranges(&markdown).try_into().ok().unwrap();
            let Code::Fenced { text, .. } = range.code else {
                panic!("expected a fence")
            };
            assert_eq!(text, input.trim_end_matches(['\r', '\n']));
        }
    }
}
