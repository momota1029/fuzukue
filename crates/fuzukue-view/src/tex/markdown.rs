//! Serialization for GPUI and the corresponding TeX source map.

use super::{marker, plain_runs, Block, Flow, Inline, Range, Run, Style};

/// Prefix of the inline code span used for a hanging list label in the paper view.
pub const ITEM_LABEL_PREFIX: &str = "fuzukue-item:";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TexMarkdown {
    pub markdown: String,
    spans: Vec<SourceSpan>,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceSpan {
    markdown: Range<usize>,
    tex: Range<usize>,
    exact: bool,
}

impl TexMarkdown {
    pub fn unmapped(markdown: String) -> Self {
        Self { markdown, spans: Vec::new() }
    }

    pub(crate) fn push_mapped(&mut self, text: &str, source: Range<usize>, exact: bool) {
        if text.is_empty() {
            return;
        }
        let start = self.markdown.len();
        self.markdown.push_str(text);
        self.spans.push(SourceSpan {
            markdown: start..self.markdown.len(),
            tex: source,
            exact,
        });
    }

    /// Combine card sections while keeping ranges relative to the full card body.
    pub fn append(&mut self, other: Self, tex_offset: usize) {
        if other.markdown.is_empty() {
            return;
        }
        if !self.markdown.is_empty() {
            self.markdown.push_str("\n\n");
        }
        let offset = self.markdown.len();
        self.markdown.push_str(&other.markdown);
        self.spans
            .extend(other.spans.into_iter().map(|span| SourceSpan {
                markdown: (span.markdown.start + offset)..(span.markdown.end + offset),
                tex: (span.tex.start + tex_offset)..(span.tex.end + tex_offset),
                exact: span.exact,
            }));
    }

    /// Convert a selection in the GPUI Markdown source to a range in the card.
    pub fn tex_range(&self, markdown: Range<usize>) -> Option<Range<usize>> {
        self.project(markdown, |span| (&span.markdown, &span.tex))
    }

    /// Find where a saved card source range appears in the current GPUI Markdown source.
    pub fn markdown_range(&self, tex: Range<usize>) -> Option<Range<usize>> {
        self.project(tex, |span| (&span.tex, &span.markdown))
    }

    fn project(
        &self,
        range: Range<usize>,
        select: impl Fn(&SourceSpan) -> (&Range<usize>, &Range<usize>),
    ) -> Option<Range<usize>> {
        self.spans
            .iter()
            .filter_map(|span| {
                let (from, to) = select(span);
                let start = from.start.max(range.start);
                let end = from.end.min(range.end);
                (start < end).then(|| {
                    if span.exact {
                        to.start + (start - from.start)..to.start + (end - from.start)
                    } else {
                        to.clone()
                    }
                })
            })
            .reduce(|a, b| a.start.min(b.start)..a.end.max(b.end))
    }
}

struct Markdown<'a> {
    tex: &'a str,
    output: TexMarkdown,
    prefix: String,
}

impl Markdown<'_> {
    fn write(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            if self.output.markdown.is_empty() || self.output.markdown.ends_with('\n') {
                self.output.markdown.push_str(&self.prefix);
            }
            self.output.markdown.push_str(line);
        }
    }

    fn mapped(&mut self, value: &str, source: Range<usize>, exact: bool) {
        if value.is_empty() {
            return;
        }
        let start = self.output.markdown.len();
        let has_prefix = self.output.markdown.is_empty() || self.output.markdown.ends_with('\n');
        self.write(value);
        if source.is_empty() {
            return;
        }
        let start = start + if has_prefix { self.prefix.len() } else { 0 };
        self.output.spans.push(SourceSpan {
            markdown: start..self.output.markdown.len(),
            tex: source,
            exact: exact && (self.prefix.is_empty() || !value.contains('\n')),
        });
    }

    fn literal(&mut self, value: &str, source: Range<usize>) {
        let exact = self.tex.get(source.clone()) == Some(value);
        let mut start = 0;
        for (index, c) in value
            .char_indices()
            .filter(|(_, c)| c.is_ascii_punctuation())
        {
            self.literal_chunk(value, start..index, &source, exact, false);
            let end = index + c.len_utf8();
            self.literal_chunk(value, index..end, &source, exact, true);
            start = end;
        }
        self.literal_chunk(value, start..value.len(), &source, exact, false);
    }

    fn literal_chunk(
        &mut self,
        value: &str,
        chunk: Range<usize>,
        source: &Range<usize>,
        exact: bool,
        escape: bool,
    ) {
        if chunk.is_empty() {
            return;
        }
        let range = if exact {
            source.start + chunk.start..source.start + chunk.end
        } else {
            source.clone()
        };
        if escape {
            self.mapped(&format!("\\{}", &value[chunk]), range, false);
        } else {
            self.mapped(&value[chunk], range, exact);
        }
    }

    fn runs(&mut self, runs: &[Run]) {
        for run in runs {
            let source = run.source.clone();
            match &run.value {
                Inline::Text(value) => self.literal(value, source),
                Inline::Link(label, url) => {
                    self.write("[");
                    self.literal(label, source);
                    self.write("](<");
                    self.write(url);
                    self.write(">)");
                }
                Inline::Math(math) if !math.is_empty() => {
                    self.mapped(&math_markdown(math, false), source, false);
                }
                Inline::Math(_) => {}
                Inline::Code(code) => {
                    let fence = fence(code, '`', 1);
                    let pad = if !code.is_empty() && code.trim_matches(' ').is_empty() {
                        ""
                    } else {
                        " "
                    };
                    self.mapped(&format!("{fence}{pad}{code}{pad}{fence}"), source, false);
                }
                Inline::MarkdownCode(_) => self.mapped(&self.tex[source.clone()], source, true),
                Inline::Styled(style, children) => {
                    let (open, close) = match style {
                        Style::Italic => ("<em>", "</em>"),
                        Style::Bold => ("<strong>", "</strong>"),
                        Style::Footnote => ("〔注 ", "〕"),
                    };
                    if !children.is_empty() {
                        self.write(open);
                        self.runs(children);
                        self.write(close);
                    }
                }
                Inline::Break => self.mapped("\\\n", source, false),
            }
        }
    }

    fn blocks(&mut self, blocks: &[Block]) {
        for (index, block) in blocks.iter().enumerate() {
            if index != 0 {
                self.write("\n\n");
            }
            self.block(block);
        }
    }

    fn block(&mut self, block: &Block) {
        let source = block.source.clone();
        match &block.value {
            Flow::Paragraph(runs) => self.runs(runs),
            Flow::Math(math) if !math.is_empty() => {
                self.mapped(&math_markdown(math, true), source, false);
            }
            Flow::Math(_) => {}
            Flow::Code { info, text } => {
                let delimiter = if info.contains('`') { '~' } else { '`' };
                let fence = fence(text, delimiter, 3);
                let space = if info.starts_with(delimiter) { " " } else { "" };
                let value = format!("{fence}{space}{info}\n{text}\n{fence}");
                let exact = self.tex.get(source.clone()) == Some(&value);
                self.mapped(&value, source, exact);
            }
            Flow::Center(body) if self.prefix.is_empty() && !body.is_empty() => {
                self.blockquote(Some("fuzukue-center"), |out| out.blocks(body));
            }
            Flow::Center(body) => self.blocks(body),
            Flow::Quote(body) => self.blockquote(Some("fuzukue-quote"), |out| out.blocks(body)),
            Flow::Theorem { heading, body } => {
                self.blockquote(Some("fuzukue-theorem"), |out| {
                    out.write("<strong>");
                    out.runs(heading);
                    out.write("</strong>");
                    if !body.is_empty() {
                        out.write("\n\n");
                        out.blocks(body);
                    }
                });
            }
            Flow::Environment { name, body } => {
                self.runs(&[marker(name, source)]);
                if !body.is_empty() {
                    self.write("\n\n");
                    self.blocks(body);
                }
            }
            Flow::List { start, items } => {
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        self.write("\n\n");
                    }
                    if let (None, Some(label)) = (start, &item.label) {
                        // A quote gives the entire item a shared indent, including
                        // continuation paragraphs, nested lists and display math.
                        self.blockquote(None, |out| {
                            let value = plain_runs(label).replace(['\n', '\r'], " ");
                            let fence = fence(&value, '`', 1);
                            let source = label
                                .first()
                                .zip(label.last())
                                .map_or(0..0, |(first, last)| first.source.start..last.source.end);
                            out.mapped(
                                &format!("{fence} {ITEM_LABEL_PREFIX}{value} {fence}"),
                                source,
                                false,
                            );
                            if item
                                .body
                                .first()
                                .is_some_and(|block| !matches!(block.value, Flow::Paragraph(_)))
                            {
                                out.write("\n\n");
                            }
                            out.blocks(&item.body);
                        });
                    } else {
                        let marker = start
                            .map_or_else(|| "- ".into(), |start| format!("{}. ", start + index));
                        self.write(&marker);
                        let old = self.prefix.len();
                        self.prefix.push_str(&" ".repeat(marker.len()));
                        self.blocks(&item.body);
                        self.prefix.truncate(old);
                    }
                }
            }
        }
    }

    fn blockquote(&mut self, surface: Option<&str>, body: impl FnOnce(&mut Self)) {
        let old = self.prefix.len();
        if !self.output.markdown.is_empty() && !self.output.markdown.ends_with('\n') {
            self.write("> ");
        }
        self.prefix.push_str("> ");
        // GPUI surfaces keep their children in the selectable document.
        if let Some(surface) = surface.filter(|_| old == 0) {
            self.write(&format!("[!{surface}]\n"));
        }
        body(self);
        self.prefix.truncate(old);
    }
}

/// Serialize a formula as standalone Markdown, even when it was parsed inside
/// a list or blockquote. Use a fence longer than any dollar run in the TeX.
pub fn math_markdown(latex: &str, display: bool) -> String {
    let fence = fence(latex, '$', if display { 2 } else { 1 });
    let pad = if display {
        "\n"
    } else if fence.len() > 1 {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{latex}{pad}{fence}")
}

fn fence(value: &str, delimiter: char, minimum: usize) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in value.chars() {
        run = if c == delimiter { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    delimiter.to_string().repeat(minimum.max(longest + 1))
}

pub(super) fn serialize(tex: &str, blocks: &[Block]) -> TexMarkdown {
    let mut markdown = Markdown {
        tex,
        output: TexMarkdown::default(),
        prefix: String::new(),
    };
    markdown.blocks(blocks);
    markdown.output
}
