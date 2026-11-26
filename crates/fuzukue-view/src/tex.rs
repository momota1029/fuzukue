//! Reader for LaTeX in cards, with Markdown code passed through as literal text.

mod markdown;

pub use markdown::{math_markdown, TexMarkdown, ITEM_LABEL_PREFIX};

use std::{collections::BTreeMap, ops::Range};

use fuzukue_proto::{DocReference, DocReferences};

use crate::markdown_code::{self, Code, CodeRange};
use crate::{encode_file_path, expand_macros, paper::theorem_kind, parse_file_ref};

#[derive(Debug)]
struct Spanned<T> {
    value: T,
    source: Range<usize>,
}

type Run = Spanned<Inline>;
type Block = Spanned<Flow>;

#[derive(Debug)]
enum Inline {
    Text(String),
    Styled(Style, Vec<Run>),
    Code(String),
    MarkdownCode(String),
    Math(String),
    Link(String, String),
    Break,
}

#[derive(Debug)]
enum Style {
    Italic,
    Bold,
    Footnote,
}

#[derive(Debug)]
enum Flow {
    Paragraph(Vec<Run>),
    Math(String),
    List {
        start: Option<usize>,
        items: Vec<Item>,
        bibliography: bool,
    },
    Code {
        info: String,
        text: String,
    },
    Quote(Vec<Block>),
    Abstract(Vec<Block>),
    Center(Vec<Block>),
    Theorem {
        heading: Vec<Run>,
        body: Vec<Block>,
    },
    Environment {
        name: String,
        body: Vec<Block>,
    },
}

#[derive(Debug)]
struct Item {
    label: Option<Vec<Run>>,
    body: Vec<Block>,
}

#[derive(Clone, Copy)]
struct Input<'a> {
    tex: &'a str,
    code: &'a [CodeRange],
    pos: usize,
    end: usize,
}

impl<'a> Input<'a> {
    fn rest(self) -> &'a str {
        &self.tex[self.pos..self.end]
    }

    fn next_code(self) -> Option<&'a CodeRange> {
        let ix = self.code.partition_point(|code| code.range.start < self.pos);
        self.code.get(ix).filter(|code| code.range.end <= self.end)
    }

    fn code_at_start(self) -> Option<&'a CodeRange> {
        self.next_code().filter(|code| code.range.start == self.pos)
    }

    fn take(&mut self, text: &str) -> bool {
        if self.rest().starts_with(text) {
            self.pos += text.len();
            true
        } else {
            false
        }
    }

    fn char(&mut self) -> Option<char> {
        let c = self.rest().chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn spaces(&mut self) -> &'a str {
        let rest = self.rest();
        let len = rest.len()
            - rest
                .trim_start_matches(|c: char| c.is_ascii_whitespace())
                .len();
        self.pos += len;
        &rest[..len]
    }

    fn control(&mut self) -> Option<&'a str> {
        if !self.take("\\") {
            return None;
        }
        let start = self.pos;
        let len = self
            .rest()
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .count();
        if len == 0 {
            self.char();
        } else {
            self.pos += len;
        }
        Some(&self.tex[start..self.pos])
    }

    fn arg(&mut self, open: char) -> Option<Self> {
        let mut next = *self;
        next.skip_ignored();
        if next.char()? != open {
            return None;
        }
        let start = next.pos;
        let close = if open == '{' { '}' } else { ']' };
        let mut depth = 0usize;
        while !next.rest().is_empty() {
            if let Some(code) = next.code_at_start() {
                if code.is_fenced() {
                    return None;
                }
                next.pos = code.range.end;
                continue;
            }
            let at = next.pos;
            if next.control().is_some() {
                continue;
            }
            match next.char()? {
                '%' => next.comment(),
                c if c == close && depth == 0 => {
                    self.pos = next.pos;
                    return Some(Self {
                        pos: start,
                        end: at,
                        ..*self
                    });
                }
                '{' => depth += 1,
                '}' => depth = depth.checked_sub(1)?,
                _ => {}
            }
        }
        None
    }

    fn comment(&mut self) {
        self.pos += self.rest().find('\n').map_or(self.rest().len(), |i| i + 1);
        self.pos += self
            .rest()
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    }

    fn skip_ignored(&mut self) {
        loop {
            self.spaces();
            if !self.take("%") {
                break;
            }
            self.comment();
        }
    }

    fn environment_end(&mut self, name: &str) -> bool {
        let mut next = *self;
        if next.control() == Some("end") && next.arg('{').is_some_and(|arg| arg.rest() == name) {
            *self = next;
            true
        } else {
            false
        }
    }
}

fn is_literal_environment(name: &str) -> bool {
    matches!(
        name.trim_end_matches('*'),
        "verbatim" | "lstlisting" | "minted"
    )
}

pub(super) fn literal_environment(line: &str) -> Option<&str> {
    let mut input = Input {
        tex: line,
        code: &[],
        pos: 0,
        end: line.len(),
    };
    input.spaces();
    if input.control() != Some("begin") {
        return None;
    }
    let name = input.arg('{')?.rest();
    is_literal_environment(name).then_some(name)
}

struct Reader<'a> {
    refs: Option<&'a DocReferences>,
    cards: BTreeMap<&'a str, &'a DocReference>,
    macros: &'a [(String, String)],
}

impl<'a> Reader<'a> {
    fn new(refs: Option<&'a DocReferences>, macros: &'a [(String, String)]) -> Self {
        let mut cards = BTreeMap::new();
        if let Some(refs) = refs {
            for (key, reference) in &refs.labels {
                if let Some(id) = key.strip_prefix("card:").or(reference.card.as_deref()) {
                    let entry = cards.entry(id).or_insert(reference);
                    if key.starts_with("card:") {
                        *entry = reference;
                    }
                }
            }
        }
        Self {
            refs,
            macros,
            cards,
        }
    }

    fn blocks(&self, input: &mut Input<'_>, end: Option<&str>, item: Option<&str>) -> Vec<Block> {
        let mut blocks = Vec::new();
        let mut paragraph = Vec::new();
        while !input.rest().is_empty() {
            let start = input.pos;
            let mut next = *input;
            let command = next.control();
            if let Some(end) = end {
                let mut closing = *input;
                if closing.environment_end(end) || (item.is_some() && command == item) {
                    break;
                }
            }
            let flow = if let Some(CodeRange {
                range,
                code: Code::Fenced { info, text },
            }) = input.code_at_start()
            {
                input.pos = range.end;
                Some(Flow::Code {
                    info: info.clone(),
                    text: text.clone(),
                })
            } else if input.take("$$") {
                Some(Flow::Math(self.math(input, "$$", false)))
            } else {
                match command {
                    Some("[") => {
                        *input = next;
                        Some(Flow::Math(self.math(input, "\\]", false)))
                    }
                    Some("begin") => next.arg('{').map(|name| {
                        *input = next;
                        self.environment(input, name.rest(), start)
                    }),
                    _ => None,
                }
            };
            if let Some(flow) = flow {
                flush_paragraph(&mut paragraph, &mut blocks);
                blocks.push(Spanned {
                    value: flow,
                    source: start..input.pos,
                });
            } else if command == Some("par") {
                *input = next;
                input.spaces();
                flush_paragraph(&mut paragraph, &mut blocks);
            } else if input.rest().starts_with(|c: char| c.is_ascii_whitespace()) {
                let whitespace = input.spaces();
                if whitespace.bytes().filter(|&b| b == b'\n').count() >= 2 {
                    flush_paragraph(&mut paragraph, &mut blocks);
                } else if !paragraph.is_empty() {
                    paragraph.push(Spanned {
                        value: Inline::Text(" ".into()),
                        source: start..input.pos,
                    });
                }
            } else {
                self.read_inline(input, &mut paragraph);
            }
        }
        flush_paragraph(&mut paragraph, &mut blocks);
        blocks
    }

    fn runs(&self, mut input: Input<'_>) -> Vec<Run> {
        let mut runs = Vec::new();
        input.spaces();
        while !input.rest().is_empty() {
            self.read_inline(&mut input, &mut runs);
        }
        trim_runs(&mut runs);
        runs
    }

    fn read_inline(&self, input: &mut Input<'_>, runs: &mut Vec<Run>) {
        let start = input.pos;
        let value = if let Some(CodeRange {
            range,
            code: Code::Inline {
                text,
                preserve_source,
            },
        }) = input.code_at_start()
        {
            input.pos = range.end;
            if *preserve_source {
                Inline::MarkdownCode(text.clone())
            } else {
                Inline::Code(text.clone())
            }
        } else if input.take("%") {
            input.comment();
            return;
        } else if input.take("$") {
            let math = self.math(input, "$", false);
            if input.pos == start + 1 {
                Inline::Text("$".into())
            } else {
                Inline::Math(math)
            }
        } else if input.take("{") {
            input.pos = start;
            if let Some(arg) = input.arg('{') {
                runs.extend(self.runs(arg));
                return;
            }
            input.char();
            Inline::Text("{".into())
        } else if let Some(command) = input.control() {
            match command {
                "(" => Inline::Math(self.math(input, "\\)", false)),
                "%" | "&" | "_" | "{" | "}" | "#" | "$" => Inline::Text(command.into()),
                " " => Inline::Text("\u{a0}".into()),
                "," => Inline::Text("\u{2009}".into()),
                ";" => Inline::Text("\u{2005}".into()),
                "!" | "leavevmode" | "noindent" | "ignorespaces" | "maketitle"
                | "tableofcontents" | "listoffigures" | "listoftables" | "frontmatter"
                | "mainmatter" | "backmatter" | "appendix" | "newpage" | "clearpage"
                | "cleardoublepage" | "begingroup" | "endgroup" => return,
                "\\" | "newline" | "linebreak" => Inline::Break,
                "label" => {
                    input.arg('{');
                    return;
                }
                "emph" | "textit" | "textbf" | "footnote" | "mathrm" | "textrm" | "text"
                | "texttt" => {
                    let Some(arg) = input.arg('{') else {
                        runs.push(marker(command, start..input.pos));
                        return;
                    };
                    let children = self.runs(arg);
                    match command {
                        "texttt" => Inline::Code(plain_runs(&children)),
                        "mathrm" | "textrm" | "text" => {
                            runs.extend(children);
                            return;
                        }
                        "textbf" => Inline::Styled(Style::Bold, children),
                        "footnote" => Inline::Styled(Style::Footnote, children),
                        _ => Inline::Styled(Style::Italic, children),
                    }
                }
                "Cref" | "cref" | "ref" | "eqref" | "autoref" => {
                    input.take("*");
                    if let Some(arg) = input.arg('{') {
                        self.references(command, arg.rest(), start..input.pos, runs);
                    } else {
                        runs.push(marker(command, start..input.pos));
                    }
                    return;
                }
                "cite" => {
                    input.take("*");
                    let first = input.arg('[');
                    let second = first.and_then(|_| input.arg('['));
                    let Some(keys) = input.arg('{') else {
                        runs.push(marker(command, start..input.pos));
                        runs.extend(
                            first
                                .into_iter()
                                .chain(second)
                                .flat_map(|arg| self.runs(arg)),
                        );
                        return;
                    };
                    let trailing = input.arg('[');
                    let (before, after) = match (second, trailing) {
                        (Some(second), _) => (first, Some(second)),
                        (None, Some(after)) => (first, Some(after)),
                        (None, None) => (None, first),
                    };
                    self.citation(keys, before, after, start..input.pos, runs);
                    return;
                }
                "verb" => {
                    input.take("*");
                    let delimiter = input.char();
                    let text = delimiter.and_then(|c| {
                        let end = input.rest().find(c)?;
                        let text = &input.rest()[..end];
                        input.pos += end + c.len_utf8();
                        Some(text)
                    });
                    Inline::Code(text.unwrap_or_default().into())
                }
                "end" => {
                    let name = input.arg('{').map_or("end", Input::rest);
                    runs.push(marker(name, start..input.pos));
                    return;
                }
                "textbackslash" => Inline::Text("\\".into()),
                "LaTeX" => Inline::Text("LaTeX".into()),
                "TeX" => Inline::Text("TeX".into()),
                "ldots" | "dots" => Inline::Text("…".into()),
                _ => {
                    if self.macros.iter().any(|(name, _)| name == command) {
                        let expanded = expand_macros(&format!("\\{command}"), self.macros);
                        Inline::Math(expanded)
                    } else {
                        // Leave groups for the reader, so an unsupported command
                        // still shows its arguments as normal text.
                        runs.push(marker(command, start..input.pos));
                        return;
                    }
                }
            }
        } else if input.take("~") {
            Inline::Text("\u{a0}".into())
        } else if input.take("---") {
            Inline::Text("—".into())
        } else if input.take("--") {
            Inline::Text("–".into())
        } else if let Some(link) = self.wiki_link(input) {
            link
        } else if input.rest().starts_with(|c: char| c.is_ascii_whitespace()) {
            input.spaces();
            Inline::Text(" ".into())
        } else {
            let rest = input.rest();
            let word_len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.'))
                .count();
            let word_len = if word_len != 0 {
                word_len
            } else {
                rest.chars()
                    .take_while(|c| !c.is_ascii())
                    .map(char::len_utf8)
                    .sum()
            };
            if word_len != 0 {
                let word = &rest[..word_len];
                let id = word.trim_end_matches(['.', ':']);
                if let Some(reference) = self.cards.get(id) {
                    input.pos += id.len();
                    Inline::Link(reference_label(reference, false), format!("fuzukue-card:{id}"))
                } else {
                    // TeX dashes can occur inside a word.
                    let len = word.find("--").unwrap_or(word_len);
                    input.pos += len;
                    Inline::Text(word[..len].into())
                }
            } else {
                Inline::Text(input.char().expect("at least one character").into())
            }
        };
        runs.push(Spanned {
            value,
            source: start..input.pos,
        });
    }

    fn wiki_link(&self, input: &mut Input<'_>) -> Option<Inline> {
        if !input.rest().starts_with("[[") {
            return None;
        }
        let end = input.next_code().map_or(input.end, |code| code.range.start);
        let text = input.tex.get(input.pos + 2..end)?;
        let (inner, _) = text.split_once("]]")?;
        let (target, alias) = inner
            .split_once('|')
            .map_or((inner, None), |(id, alias)| (id, Some(alias)));
        let (label, url) = if let Some(path) = target.strip_prefix("file:") {
            let (file, _) = parse_file_ref(path);
            (
                alias.unwrap_or(file).into(),
                format!(
                    "fuzukue-file:{}{}",
                    encode_file_path(file),
                    &path[file.len()..]
                ),
            )
        } else {
            if target.is_empty()
                || !target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
            {
                return None;
            }
            let label = alias
                .map(str::to_owned)
                .or_else(|| {
                    self.cards
                        .get(target)
                        .map(|reference| reference_label(reference, false))
                })
                .unwrap_or_else(|| target.into());
            (label, format!("fuzukue-card:{target}"))
        };
        input.pos += inner.len() + 4;
        Some(Inline::Link(label, url))
    }

    fn references(&self, command: &str, labels: &str, source: Range<usize>, runs: &mut Vec<Run>) {
        for (index, key) in labels.split(',').map(str::trim).enumerate() {
            if index != 0 {
                runs.push(text(", ", source.clone()));
            }
            let conventional = key.strip_prefix("card:");
            let reference = self.refs.and_then(|refs| refs.labels.get(key));
            let card = reference
                .and_then(|reference| reference.card.as_deref())
                .or(conventional);
            let label = reference
                .map(|reference| match command {
                    "ref" => reference.number.clone(),
                    _ => reference_label(reference, command == "eqref"),
                })
                .unwrap_or_else(|| {
                    if self.refs.is_some() {
                        format!("??{key}")
                    } else {
                        conventional.unwrap_or(key).into()
                    }
                });
            let value = match card {
                Some(id) => Inline::Link(label, format!("fuzukue-card:{id}")),
                None => Inline::Text(label),
            };
            runs.push(Spanned {
                value,
                source: source.clone(),
            });
        }
    }

    fn citation(
        &self,
        keys: Input<'_>,
        before: Option<Input<'_>>,
        after: Option<Input<'_>>,
        source: Range<usize>,
        runs: &mut Vec<Run>,
    ) {
        runs.push(text("[", source.clone()));
        if let Some(before) = before {
            runs.extend(self.runs(before));
            runs.push(text(" ", source.clone()));
        }
        let labels = keys
            .rest()
            .split(',')
            .map(str::trim)
            .map(|key| {
                self.refs.map_or_else(
                    || key.to_owned(),
                    |refs| {
                        refs.citations
                            .get(key)
                            .cloned()
                            .unwrap_or_else(|| format!("??{key}"))
                    },
                )
            })
            .collect::<Vec<_>>();
        runs.push(text(&labels.join(", "), source.clone()));
        if let Some(after) = after {
            runs.push(text(", ", source.clone()));
            runs.extend(self.runs(after));
        }
        runs.push(text("]", source));
    }

    fn math(&self, input: &mut Input<'_>, close: &str, environment: bool) -> String {
        let mut cursor = *input;
        // A delimiter in code cannot close a formula starting in prose.
        if let Some(code) = cursor.next_code() {
            cursor.end = code.range.start;
        }
        let mut math = String::new();
        while !cursor.rest().is_empty() {
            if if environment {
                cursor.environment_end(close)
            } else {
                cursor.take(close)
            } {
                input.pos = cursor.pos;
                return expand_macros(math.trim(), self.macros);
            }
            if cursor.take("%") {
                cursor.comment();
            } else {
                let start = cursor.pos;
                if let Some(command) = cursor.control() {
                    if command == "label" && cursor.arg('{').is_some() {
                        continue;
                    }
                } else {
                    cursor.char();
                }
                math.push_str(&cursor.tex[start..cursor.pos]);
            }
        }
        if environment {
            input.pos = cursor.pos;
            expand_macros(math.trim(), self.macros)
        } else {
            String::new()
        }
    }

    fn environment(&self, input: &mut Input<'_>, name: &str, begin: usize) -> Flow {
        let base = name.trim_end_matches('*');
        match base {
            "equation" | "align" | "gather" | "multline" => {
                let math = self.math(input, name, true);
                let wrapper = match base {
                    "align" => Some("aligned"),
                    "gather" | "multline" => Some("gathered"),
                    _ => None,
                };
                Flow::Math(wrapper.map_or_else(
                    || math.clone(),
                    |name| format!("\\begin{{{name}}}{math}\\end{{{name}}}"),
                ))
            }
            "enumerate" | "itemize" | "description" | "thebibliography" => {
                self.list(input, name, base == "enumerate")
            }
            _ if is_literal_environment(name) => Self::code(input, name, base, begin),
            "abstract" => {
                let body = self.blocks(input, Some(name), None);
                input.environment_end(name);
                Flow::Abstract(body)
            }
            "quote" | "quotation" | "center" => {
                let body = self.blocks(input, Some(name), None);
                input.environment_end(name);
                if base == "center" {
                    Flow::Center(body)
                } else {
                    Flow::Quote(body)
                }
            }
            _ if theorem_kind(base).is_some() => {
                let heading_source = begin..input.pos;
                let title = input
                    .arg('[')
                    .map_or_else(Vec::new, |title| self.runs(title));
                let mut next = *input;
                next.skip_ignored();
                let label_start = next.pos;
                let label = (next.control() == Some("label"))
                    .then(|| next.arg('{'))
                    .flatten();
                if label.is_some() {
                    *input = next;
                }
                let fallback = theorem_kind(base).unwrap_or(base);
                let heading = label
                    .and_then(|key| self.refs?.labels.get(key.rest()))
                    .map_or_else(
                        || fallback.to_owned(),
                        |reference| {
                            let kind = if reference.kind.is_empty() {
                                fallback
                            } else {
                                &reference.kind
                            };
                            format!("{kind} {}", reference.number).trim_end().to_owned()
                        },
                    );
                let number_source = if label.is_some() {
                    label_start..input.pos
                } else {
                    heading_source.clone()
                };
                let mut heading = vec![text(&heading, number_source)];
                if let Some(first) = title.first() {
                    heading.push(text(" · ", first.source.start..first.source.start));
                    heading.extend(title);
                }
                let body = self.blocks(input, Some(name), None);
                input.environment_end(name);
                Flow::Theorem { heading, body }
            }
            _ => {
                let body = self.blocks(input, Some(name), None);
                input.environment_end(name);
                Flow::Environment {
                    name: name.into(),
                    body,
                }
            }
        }
    }

    fn list(&self, input: &mut Input<'_>, name: &str, ordered: bool) -> Flow {
        let bibliography = name == "thebibliography";
        let item = if bibliography { "bibitem" } else { "item" };
        if bibliography {
            input.arg('{'); // Width sample for the printed labels.
        }
        let options = input.arg('[');
        let option = |name: &str| {
            options?.rest().split(',').find_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                (key.trim() == name).then(|| value.trim().trim_matches(['{', '}']))
            })
        };
        let template = option("label");
        let start = option("start")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n <= 999_999_999)
            .unwrap_or(1);
        let mut items = Vec::new();
        let mut custom_labels = template.is_some();
        let mut number = start;
        let initial = self.blocks(input, Some(name), Some(item));
        if !initial.is_empty() {
            items.push(Item {
                label: None,
                body: initial,
            });
        }
        loop {
            let at = input.pos;
            let mut next = *input;
            if next.control() != Some(item) {
                break;
            }
            *input = next;
            let explicit = input.arg('[');
            let label = if bibliography {
                let key = input.arg('{');
                let resolved = key.and_then(|key| self.refs?.citations.get(key.rest()));
                let value = resolved.cloned()
                    .or_else(|| explicit.map(|label| plain_runs(&self.runs(label))))
                    .unwrap_or_else(|| number.to_string());
                number += 1;
                Some(vec![text(&format!("[{value}]"), at..input.pos)])
            } else if let Some(label) = explicit {
                custom_labels = true;
                Some(self.runs(label))
            } else if ordered || template.is_some() {
                let label = if let Some(template) = template {
                    let label = list_label(template, number);
                    plain_runs(&self.runs(Input {
                        tex: &label,
                        code: &[],
                        pos: 0,
                        end: label.len(),
                    }))
                } else {
                    format!("{number}.")
                };
                number += 1;
                Some(vec![text(&label, at..input.pos)])
            } else {
                None
            };
            let body = self.blocks(input, Some(name), Some(item));
            items.push(Item { label, body });
        }
        input.environment_end(name);
        let start = (ordered && !custom_labels).then_some(start);
        Flow::List { start, items, bibliography }
    }

    fn code(input: &mut Input<'_>, name: &str, base: &str, begin: usize) -> Flow {
        let options = if base == "verbatim" {
            None
        } else {
            input.arg('[')
        };
        let language = match base {
            "minted" => input.arg('{').map(Input::rest),
            "lstlisting" => options.and_then(|opts| {
                opts.rest().split(',').find_map(|entry| {
                    let (key, value) = entry.split_once('=')?;
                    (key.trim() == "language").then(|| value.trim().trim_matches(['{', '}']))
                })
            }),
            _ => None,
        }
        .unwrap_or_default()
        .trim();
        let source = input.rest();
        let close = format!("\\end{{{name}}}");
        let len = source.find(&close).unwrap_or(source.len());
        let raw = &source[..len];
        input.pos += len;
        input.take(&close);
        let raw = raw
            .split_once('\n')
            .filter(|(first, _)| first.trim().is_empty())
            .map_or(raw, |(_, rest)| rest);
        let raw = raw
            .rsplit_once('\n')
            .filter(|(_, last)| last.trim().is_empty())
            .map_or(raw, |(body, _)| body);
        let prefix = input.tex[..begin].rsplit('\n').next().unwrap_or_default();
        let text = if prefix.chars().all(|c| matches!(c, ' ' | '\t')) {
            raw.split('\n')
                .map(|line| line.strip_prefix(prefix).unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            raw.into()
        };
        Flow::Code {
            info: language
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || "+#_-".contains(*c))
                .map(|c| c.to_ascii_lowercase())
                .collect(),
            text,
        }
    }
}

fn reference_label(reference: &DocReference, equation: bool) -> String {
    if equation {
        format!("({})", reference.number)
    } else if reference.kind.is_empty() {
        reference.number.clone()
    } else {
        format!("{} {}", reference.kind, reference.number)
    }
}

fn list_label(template: &str, number: usize) -> String {
    let mut label = template.replace("\\arabic*", &number.to_string());
    if let Some(index) = number.checked_sub(1).filter(|&i| i < 26) {
        label = label
            .replace("\\alph*", &char::from(b'a' + index as u8).to_string())
            .replace("\\Alph*", &char::from(b'A' + index as u8).to_string());
    }
    label
}

fn text(value: &str, source: Range<usize>) -> Run {
    Spanned {
        value: Inline::Text(value.into()),
        source,
    }
}

fn marker(name: &str, source: Range<usize>) -> Run {
    let name: String = name
        .chars()
        .filter(|c| *c != '\\' && !c.is_control())
        .collect();
    Spanned {
        value: Inline::Styled(
            Style::Italic,
            vec![text(&format!("⟨{name}⟩"), source.clone())],
        ),
        source,
    }
}

fn trim_runs(runs: &mut Vec<Run>) {
    while runs.last().is_some_and(|run| {
        matches!(&run.value, Inline::Text(s) if s == " ") || matches!(run.value, Inline::Break)
    }) {
        runs.pop();
    }
}

fn flush_paragraph(runs: &mut Vec<Run>, blocks: &mut Vec<Block>) {
    trim_runs(runs);
    if let (Some(first), Some(last)) = (runs.first(), runs.last()) {
        let source = first.source.start..last.source.end;
        blocks.push(Spanned {
            value: Flow::Paragraph(std::mem::take(runs)),
            source,
        });
    }
}

fn plain_runs(runs: &[Run]) -> String {
    runs.iter()
        .map(|run| match &run.value {
            Inline::Text(text)
            | Inline::Link(text, _)
            | Inline::Math(text)
            | Inline::Code(text)
            | Inline::MarkdownCode(text) => text.clone(),
            Inline::Styled(_, children) => plain_runs(children),
            Inline::Break => " ".into(),
        })
        .collect()
}

/// Render a card's TeX and Markdown code as selectable Markdown with source ranges.
pub fn read_card_tex(
    tex: &str,
    refs: Option<&DocReferences>,
    macros: &[(String, String)],
) -> TexMarkdown {
    let code = markdown_code::ranges(tex);
    let blocks = Reader::new(refs, macros).blocks(
        &mut Input {
            tex,
            code: &code,
            pos: 0,
            end: tex.len(),
        },
        None,
        None,
    );
    markdown::serialize(tex, &blocks)
}

#[cfg(test)]
mod tests;
