use std::{fmt::Write as _, sync::LazyLock};

use html5gum::{DefaultEmitter, Token, Tokenizer};
use regex::Regex;

static EXTRA_NEWLINES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid regex"));

pub(super) fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
}

pub(super) fn present(text: &Option<String>) -> Option<&str> {
    text.as_deref().filter(|text| !text.is_empty())
}

pub(super) fn details(summary: &str, anchor: Option<&str>, body: &str) -> String {
    let mut head = format!("::: details {summary}");
    if let Some(anchor) = anchor {
        write!(head, " {{#{anchor}}}").expect("writing to a string");
    }
    format!("{head}\n\n{}\n\n:::", body.trim_matches(whitespace))
}

pub(super) fn table<const N: usize, S: AsRef<str>>(
    headers: [&str; N],
    rows: impl IntoIterator<Item = [S; N]>,
) -> String {
    let mut output = format!("| {} |\n|{}|", headers.join(" | "), ["---"; N].join("|"));
    for row in rows {
        output.push_str("\n|");
        for entry in row {
            let text = escape(entry.as_ref());
            let text = text
                .split(whitespace)
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
                .replace('|', "\\|");
            let cell = if text.is_empty() { "—" } else { &text };
            write!(output, " {cell} |").expect("writing to a string");
        }
    }
    output
}

fn whitespace(c: char) -> bool {
    // Include the additional separators recognized by Python's strip and \s.
    c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}')
}

#[derive(Default)]
struct Markdown {
    out: String,
    math: Option<Math>,
    lists: Vec<List>,
}

struct Math {
    display: bool,
    text: String,
}

struct List {
    ordered: bool,
    items: usize,
}

impl Markdown {
    fn paragraph_break(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            self.out.push('\n');
        }
    }

    fn start(&mut self, tag: &[u8], class: &[u8]) {
        match (tag, class) {
            (b"span", b"m") | (b"div", b"d") => {
                let display = tag == b"div";
                if display {
                    self.paragraph_break();
                }
                self.math = Some(Math {
                    display,
                    text: String::new(),
                });
            }
            (b"p" | b"div", _) => self.paragraph_break(),
            (b"b" | b"strong", _) => self.out.push_str("**"),
            (b"i" | b"em", _) => self.out.push('*'),
            (b"code", _) => self.out.push('`'),
            (b"ul" | b"ol", _) => {
                self.paragraph_break();
                self.lists.push(List {
                    ordered: tag == b"ol",
                    items: 0,
                });
            }
            (b"li", _) => {
                if !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                self.out
                    .push_str(&"   ".repeat(self.lists.len().saturating_sub(1)));
                if let Some(list) = self.lists.last_mut().filter(|list| list.ordered) {
                    list.items += 1;
                    write!(self.out, "{}. ", list.items).expect("writing to a string");
                } else {
                    self.out.push_str("- ");
                }
            }
            (b"br", _) => self.out.push_str("  \n"),
            _ => {}
        }
    }

    fn end(&mut self, tag: &[u8]) {
        if let Some(math) = self.math.as_ref() {
            let text = math.text.trim_matches(whitespace);
            match (tag, math.display) {
                (b"span", false) => {
                    write!(self.out, "${text}$").expect("writing to a string");
                    self.math = None;
                    return;
                }
                (b"div", true) => {
                    write!(self.out, "$$\n{text}\n$$\n\n").expect("writing to a string");
                    self.math = None;
                    return;
                }
                _ => {}
            }
        }

        match tag {
            b"p" | b"div" => self.paragraph_break(),
            b"b" | b"strong" => self.out.push_str("**"),
            b"i" | b"em" => self.out.push('*'),
            b"code" => self.out.push('`'),
            b"ul" | b"ol" => {
                self.lists.pop();
                self.paragraph_break();
            }
            b"li" => self.out.push('\n'),
            _ => {}
        }
    }
}

/// Convert the small HTML subset used by the deck's prose and proof bodies.
pub(super) fn markdown(fragment: Option<&str>) -> String {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut markdown = Markdown::default();
    for token in Tokenizer::new_with_emitter(fragment.unwrap_or_default(), emitter).flatten() {
        match token {
            Token::StartTag(tag) => {
                let class = tag
                    .attributes
                    .get(b"class".as_slice())
                    .map(|value| value.as_slice())
                    .unwrap_or_default();
                markdown.start(&tag.name, class);
                if tag.self_closing {
                    markdown.end(&tag.name);
                }
            }
            Token::EndTag(tag) => markdown.end(&tag.name),
            Token::String(data) => {
                let data = String::from_utf8_lossy(&data);
                if let Some(math) = &mut markdown.math {
                    math.text.push_str(&data);
                } else {
                    markdown.out.push_str(&escape(&data));
                }
            }
            _ => {}
        }
    }
    EXTRA_NEWLINES
        .replace_all(&markdown.out, "\n\n")
        .trim_matches(whitespace)
        .to_owned()
}
