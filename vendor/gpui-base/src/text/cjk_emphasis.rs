//! Recover literal CJK-adjacent emphasis left in an mdast after parsing.
//!
//! CommonMark restricts a delimiter next to punctuation or a symbol on its
//! content side. Here an adjacent CJK character (including CJK punctuation
//! and fullwidth forms) permits opening or closing at that boundary. An
//! inline node also supplies a punctuation boundary. The other end can
//! have ordinary CommonMark flanking. Only literal `*`, `**` and `~~` runs
//! in the same parent's text participate. The original bytes distinguish
//! these runs from escapes and character references and locate the new nodes.
//!
//! See [markdown-cjk-friendly](https://github.com/tats-u/markdown-cjk-friendly)
//! and [CommonMark issue #650](https://github.com/commonmark/commonmark-spec/issues/650).

use std::collections::HashSet;

use markdown::{
    mdast::{Delete, Emphasis, Node, Strong, Text},
    unist::{Point, Position},
};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

/// Apply CJK flanking to unresolved text in a parsed Markdown document.
///
/// `source` is the original input used to parse `root`.
pub fn apply(root: &mut Node, source: &str) {
    if source.is_ascii() || !source.bytes().any(|b| matches!(b, b'*' | b'~')) {
        return;
    }

    let mut pending = vec![root];
    let mut recovered = HashSet::new();
    while let Some(node) = pending.pop() {
        let is_recovered = node
            .position()
            .is_some_and(|pos| recovered.remove(&pos.start.offset));
        let phrasing = match node {
            Node::Paragraph(_)
            | Node::Heading(_)
            | Node::TableCell(_)
            | Node::Emphasis(_)
            | Node::Strong(_)
            | Node::Delete(_)
            | Node::LinkReference(_)
            | Node::MdxJsxTextElement(_) => true,
            // Autolinks expose their URL as a Text child. Explicit links have labels.
            Node::Link(link) => {
                if !link
                    .position
                    .as_ref()
                    .is_some_and(|pos| source.as_bytes().get(pos.start.offset) == Some(&b'['))
                {
                    continue;
                }
                true
            }
            _ => false,
        };
        if let Some(children) = node.children_mut() {
            // Traverse recovered wrappers to reach their original inline
            // descendants, without scanning the same text for runs again.
            if phrasing && !is_recovered {
                recover(children, source, &mut recovered);
            }
            pending.extend(children.iter_mut().filter(|node| node.children().is_some()));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Italic,
    Bold,
    Strike,
}

impl Kind {
    fn from_run(byte: u8, len: usize) -> Option<Self> {
        match (byte, len) {
            (b'*', 1) => Some(Self::Italic),
            (b'*', 2) => Some(Self::Bold),
            (b'~', 2) => Some(Self::Strike),
            _ => None,
        }
    }

    fn wrap(self, children: Vec<Node>, position: Position) -> Node {
        let position = Some(position);
        match self {
            Self::Italic => Node::Emphasis(Emphasis { children, position }),
            Self::Bold => Node::Strong(Strong { children, position }),
            Self::Strike => Node::Delete(Delete { children, position }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flank {
    Neither,
    Ordinary,
    Cjk,
}

#[derive(Clone, Copy, Default)]
struct Boundary {
    whitespace: bool,
    punctuation: bool,
    cjk: bool,
}

impl Boundary {
    fn character(ch: char) -> Self {
        Self {
            whitespace: ch.is_whitespace(),
            punctuation: ch.is_ascii_punctuation()
                || matches!(
                    ch.general_category_group(),
                    GeneralCategoryGroup::Punctuation | GeneralCategoryGroup::Symbol
                ),
            cjk: matches!(ch as u32,
                0x1100..=0x11ff | 0x2e80..=0x2fff | 0x3000..=0x30ff
                | 0x3100..=0x312f | 0x3130..=0x318f | 0x3190..=0x31bf
                | 0x31c0..=0x31ff | 0x3200..=0x33ff | 0x3400..=0x4dbf
                | 0x4e00..=0x9fff | 0xa960..=0xa97f | 0xac00..=0xd7ff
                | 0xf900..=0xfaff | 0xfe10..=0xfe1f | 0xfe30..=0xfe4f
                | 0xff01..=0xffef | 0x16fe0..=0x16fff | 0x1aff0..=0x1b16f
                | 0x20000..=0x2fa1f | 0x30000..=0x3347f
            ),
        }
    }

    fn at_sibling(node: Option<&Node>, first: bool) -> Self {
        match node {
            Some(Node::Text(text)) => {
                let mut chars = text.value.chars();
                let ch = if first {
                    chars.next()
                } else {
                    chars.next_back()
                };
                ch.map_or(Self::SPACE, Self::character)
            }
            None | Some(Node::Break(_)) => Self::SPACE,
            Some(_) => Self {
                punctuation: true,
                ..Self::default()
            },
        }
    }

    const SPACE: Self = Self {
        whitespace: true,
        punctuation: false,
        cjk: false,
    };

    // Swap the arguments for right-flanking.
    fn flank(inside: Self, outside: Self) -> Flank {
        if inside.whitespace {
            Flank::Neither
        } else if !inside.punctuation || outside.whitespace || outside.punctuation {
            Flank::Ordinary
        } else if inside.cjk || outside.cjk {
            Flank::Cjk
        } else {
            Flank::Neither
        }
    }
}

struct Run {
    child: usize,
    start: usize,
    end: usize,
    position: Position,
    kind: Kind,
    open: Flank,
    close: Flank,
}

/// Scan text and source in lockstep, skipping only syntax mdast can omit
/// from text. A mismatch leaves this text node untouched.
struct SourceText<'a> {
    raw: &'a str,
    point: Point,
    continuation: bool,
}

impl SourceText<'_> {
    fn advance(&mut self, len: usize) {
        // Advance by one token: a character, an escape/reference, a
        // delimiter run, or a line ending (including CRLF).
        match self.raw.as_bytes()[0] {
            b'\n' | b'\r' => {
                self.point.line += 1;
                self.point.column = 1;
            }
            b'\t' => self.point.column += 4 - (self.point.column - 1) % 4,
            _ => self.point.column += len,
        }
        self.raw = &self.raw[len..];
        self.point.offset += len;
    }

    // (Rendered bytes, original bytes, literal). Escapes and references
    // consume both streams atomically, even if they decode to a marker.
    fn next(&mut self, rendered: &str) -> Option<(usize, usize, bool)> {
        let ch = rendered.chars().next()?;
        if self.continuation {
            while !self.raw.starts_with(ch) && self.raw.starts_with([' ', '\t', '>']) {
                self.advance(1);
            }
            self.continuation = false;
        }
        if let Some(escaped) = self.raw.strip_prefix('\\')
            && ch.is_ascii_punctuation()
            && escaped.starts_with(ch)
        {
            return Some((1, 2, false));
        }
        if let Some((decoded, len)) = entity(self.raw)
            && rendered.starts_with(&decoded)
        {
            return Some((decoded.len(), len, false));
        }
        if matches!(ch, '\n' | '\r') {
            let source_break = line_ending(self.raw)?;
            self.continuation = true;
            return Some((line_ending(rendered)?, source_break, false));
        }
        self.raw
            .starts_with(ch)
            .then_some((ch.len_utf8(), ch.len_utf8(), true))
    }
}

fn line_ending(text: &str) -> Option<usize> {
    if text.starts_with("\r\n") {
        Some(2)
    } else {
        text.starts_with(['\r', '\n']).then_some(1)
    }
}

fn entity(raw: &str) -> Option<(String, usize)> {
    let raw = raw.strip_prefix('&')?;
    // CommonMark's longest named reference is at most 31 characters.
    let end = raw
        .bytes()
        .take(33)
        .position(|b| !b.is_ascii_alphanumeric() && b != b'#')?;
    if raw.as_bytes()[end] != b';' {
        return None;
    }
    let name = &raw[..end];
    let decoded = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        (hex.len() <= 6 && u32::from_str_radix(hex, 16).is_ok())
            .then(|| markdown::decode_numeric(hex, 16))?
    } else if let Some(decimal) = name.strip_prefix('#') {
        (decimal.len() <= 7 && decimal.bytes().all(|b| b.is_ascii_digit()) && !decimal.is_empty())
            .then(|| markdown::decode_numeric(decimal, 10))?
    } else {
        markdown::decode_named(name, true)?
    };
    Some((decoded, end + 2))
}

fn collect_runs(
    children: &[Node],
    child: usize,
    text: &Text,
    source: &str,
    runs: &mut Vec<Run>,
) -> Option<()> {
    if !text.value.bytes().any(|b| matches!(b, b'*' | b'~')) {
        return Some(());
    }
    let position = text.position.as_ref()?;
    let raw = source.get(position.start.offset..position.end.offset)?;
    let starts_escaped = source.as_bytes()[..position.start.offset]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
        % 2
        != 0;
    let mut source_text = SourceText {
        raw,
        point: position.start.clone(),
        continuation: false,
    };
    let mut index = 0;
    while index < text.value.len() {
        let suffix = &text.value[index..];
        let byte = suffix.as_bytes()[0];
        let (rendered, raw, literal) = source_text.next(suffix)?;
        if literal && (index != 0 || !starts_escaped) && matches!(byte, b'*' | b'~') {
            // An escape or an entity can render the same marker next to
            // this run, but only the consecutive source bytes are eligible.
            let width = source_text.raw.bytes().take_while(|&b| b == byte).count();
            if suffix.get(..width)? != &source_text.raw[..width] {
                return None;
            }
            let end = index + width;
            let start_point = source_text.point.clone();
            source_text.advance(width);
            if let Some(kind) = Kind::from_run(byte, width) {
                let before = text.value[..index].chars().next_back().map_or_else(
                    || {
                        Boundary::at_sibling(
                            child.checked_sub(1).and_then(|i| children.get(i)),
                            false,
                        )
                    },
                    Boundary::character,
                );
                let after = text.value[end..].chars().next().map_or_else(
                    || Boundary::at_sibling(children.get(child + 1), true),
                    Boundary::character,
                );
                let open = Boundary::flank(after, before);
                let close = Boundary::flank(before, after);
                if open != Flank::Neither || close != Flank::Neither {
                    runs.push(Run {
                        child,
                        start: index,
                        end,
                        position: Position {
                            start: start_point,
                            end: source_text.point.clone(),
                        },
                        kind,
                        open,
                        close,
                    });
                }
            }
            index = end;
        } else {
            source_text.advance(raw);
            index += rendered;
        }
    }
    source_text.raw.is_empty().then_some(())
}

fn recover(children: &mut Vec<Node>, source: &str, recovered: &mut HashSet<usize>) {
    let mut runs = Vec::new();
    for (child, node) in children.iter().enumerate() {
        if let Node::Text(text) = node {
            let start = runs.len();
            if collect_runs(children, child, text, source, &mut runs).is_none() {
                runs.truncate(start);
            }
        }
    }

    // Index the stack by delimiter kind and opener flank, so an unmatched
    // closer never scans the stack. Discarded openers are each popped once.
    let mut stack: Vec<usize> = Vec::new();
    let mut openers: [[Vec<usize>; 2]; 3] = Default::default();
    let mut matched = vec![None; runs.len()];
    for (i, run) in runs.iter().enumerate() {
        let by_flank = &openers[run.kind as usize];
        let cjk = by_flank[1].last().copied();
        let opener = match run.close {
            Flank::Neither => None,
            Flank::Ordinary => cjk,
            Flank::Cjk => cjk.max(by_flank[0].last().copied()),
        };
        if let Some(opener) = opener
            && runs[opener].position.end.offset < run.position.start.offset
        {
            while let Some(top) = stack.pop() {
                let run = &runs[top];
                openers[run.kind as usize][usize::from(run.open == Flank::Cjk)].pop();
                if top == opener {
                    break;
                }
            }
            matched[opener] = Some(true);
            matched[i] = Some(false);
        } else if run.open != Flank::Neither {
            stack.push(i);
            openers[run.kind as usize][usize::from(run.open == Flank::Cjk)].push(i);
        }
    }
    let mut delimiters = runs
        .into_iter()
        .zip(matched)
        .filter_map(|(run, open)| open.map(|open| (run, open)))
        .peekable();
    if delimiters.peek().is_none() {
        return;
    }

    let mut output = Vec::new();
    let mut nesting: Vec<(Run, Vec<Node>)> = Vec::new();
    for (child, node) in std::mem::take(children).into_iter().enumerate() {
        if !delimiters.peek().is_some_and(|(run, _)| run.child == child) {
            output.push(node);
            continue;
        }
        let Node::Text(text) = node else {
            unreachable!()
        };
        let position = text.position.expect("only positioned text yields runs");
        let mut index = 0;
        let mut point = position.start;
        while delimiters.peek().is_some_and(|(run, _)| run.child == child) {
            let (run, open) = delimiters.next().unwrap();
            push_text(
                &mut output,
                &text.value[index..run.start],
                point,
                run.position.start.clone(),
            );
            index = run.end;
            point = run.position.end.clone();
            if open {
                nesting.push((run, std::mem::take(&mut output)));
            } else {
                let (opener, mut parent) = nesting.pop().unwrap();
                let position = Position {
                    start: opener.position.start,
                    end: run.position.end,
                };
                recovered.insert(position.start.offset);
                parent.push(run.kind.wrap(output, position));
                output = parent;
            }
        }
        push_text(&mut output, &text.value[index..], point, position.end);
    }
    *children = output;
}

fn push_text(children: &mut Vec<Node>, value: &str, start: Point, end: Point) {
    if !value.is_empty() {
        children.push(Node::Text(Text {
            value: value.to_owned(),
            position: Some(Position { start, end }),
        }));
    }
}
