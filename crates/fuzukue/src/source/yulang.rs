use std::ops::Range;

use gpui_kit::{
    component::input::{
        EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
    },
    Context, HighlightStyle, SharedString, Window,
};

#[derive(Default)]
pub(super) struct Highlighter {
    spans: Vec<(Range<usize>, &'static str)>,
}

impl InputHighlighter for Highlighter {
    fn language(&self) -> SharedString {
        "yulang".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        self.spans = spans(&text.to_string());
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if range.is_empty() {
            return Vec::new();
        }
        let mut runs = Vec::new();
        let mut offset = range.start;
        let first = self.spans.partition_point(|(span, _)| span.end <= offset);
        for (span, name) in &self.spans[first..] {
            if span.start >= range.end {
                break;
            }
            let start = span.start.max(range.start);
            if offset < start {
                runs.push((offset..start, HighlightStyle::default()));
            }
            offset = span.end.min(range.end);
            runs.push((start..offset, resolver.style(name).unwrap_or_default()));
        }
        if offset < range.end {
            runs.push((offset..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

fn spans(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while let Some(first) = text[offset..].chars().next() {
        let rest = &text[offset..];
        let (len, style) = if rest.starts_with("//") {
            (
                rest.find(['\r', '\n']).unwrap_or(rest.len()),
                Some("comment"),
            )
        } else if rest.starts_with("/*") {
            (block_comment_len(rest), Some("comment"))
        } else if first == '"' {
            (string_len(rest), Some("string"))
        } else if first.is_alphanumeric() || first == '_' {
            let mut len = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            if rest[len..].starts_with(['?', '!']) {
                len += 1;
            }
            let keyword = matches!(
                &rest[..len],
                "my" | "our"
                    | "pub"
                    | "use"
                    | "mod"
                    | "struct"
                    | "type"
                    | "enum"
                    | "role"
                    | "impl"
                    | "effect"
                    | "operator"
                    | "do"
                    | "if"
                    | "else"
                    | "case"
                    | "catch"
                    | "for"
                    | "in"
                    | "sub"
                    | "rule"
                    | "true"
                    | "false"
            );
            (len, keyword.then_some("keyword"))
        } else {
            (first.len_utf8(), None)
        };
        if let Some(style) = style {
            spans.push((offset..offset + len, style));
        }
        offset += len;
    }
    spans
}

fn block_comment_len(text: &str) -> usize {
    let mut depth = 1;
    let mut offset = 2;
    while let Some(next) = text[offset..].find(['/', '*']) {
        offset += next;
        let delimiter = &text[offset..];
        if delimiter.starts_with("/*") {
            depth += 1;
            offset += 2;
        } else if delimiter.starts_with("*/") {
            depth -= 1;
            offset += 2;
            if depth == 0 {
                return offset;
            }
        } else {
            offset += 1;
        }
    }
    text.len()
}

fn string_len(text: &str) -> usize {
    let quotes = text.bytes().take_while(|&byte| byte == b'"').count();
    if quotes >= 3 {
        let mut offset = quotes;
        while let Some(next) = text[offset..].find('"') {
            offset += next;
            let len = text[offset..]
                .bytes()
                .take_while(|&byte| byte == b'"')
                .count();
            offset += len;
            if len == quotes {
                return offset;
            }
        }
        return text.len();
    }
    let mut escaped = false;
    for (offset, ch) in text.char_indices().skip(1) {
        match ch {
            '\r' | '\n' => return offset,
            '"' if !escaped => return offset + 1,
            _ => escaped = ch == '\\' && !escaped,
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_words_and_keeps_comments_and_strings_opaque() {
        let text = r#"my 名前 = "if \" //"
/* outer /* if */ comment */ pub iffy if?
// struct
"""mod
"""
for"#;
        let tokens: Vec<_> = spans(text)
            .into_iter()
            .map(|(range, style)| (&text[range], style))
            .collect();
        assert_eq!(
            tokens,
            [
                ("my", "keyword"),
                (r#""if \" //""#, "string"),
                ("/* outer /* if */ comment */", "comment"),
                ("pub", "keyword"),
                ("// struct", "comment"),
                ("\"\"\"mod\n\"\"\"", "string"),
                ("for", "keyword"),
            ]
        );
    }
}
