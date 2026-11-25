//! Import literal, multi-file LaTeX into an editable book deck.
mod reading;
mod scan;
mod verify;

pub use reading::manuscript_sections;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    reading_sync::{SourceFile, SourceRange, Theorem},
    workspace_paths, DeckConfig,
};
use scan::{argument, optional, starred, tokens, whitespace};

pub use verify::Engine;

#[derive(Debug, Serialize)]
pub struct Imported {
    pub cards: usize,
    pub report: PathBuf,
    pub verification: String,
}

#[derive(Deserialize, Serialize)]
struct Book {
    title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    before_sections: String,
    sections: Vec<Chapter>,
}

#[derive(Deserialize, Serialize)]
struct Chapter {
    #[serde(flatten)]
    heading: Heading,
    items: Vec<Item>,
}

#[derive(Deserialize, Serialize)]
struct Heading {
    title: String,
    level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    tex: String,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Item {
    Card(String),
    Heading { heading: Heading },
    Proof { proof: String },
}

#[derive(Serialize)]
struct Front<'a> {
    id: &'a str,
    kind: &'a str,
    title: &'a str,
    status: &'static str,
    source: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    additional_sources: &'a [String],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    proof_sources: Vec<String>,
    source_sha256: BTreeMap<String, String>,
}

struct Span {
    range: Range<usize>,
    file: PathBuf,
    line: usize,
    source_start: Option<usize>,
}

#[derive(Default)]
struct Expanded {
    tex: String,
    spans: Vec<Span>,
    sources: BTreeMap<PathBuf, Rc<SourceFile>>,
    assets: BTreeMap<PathBuf, PathBuf>,
    graphics_dirs: Vec<PathBuf>,
    warnings: BTreeSet<String>,
    reading: Option<reading::Reading>,
}

impl Expanded {
    fn push(&mut self, source: &str, file: &Path, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let start = self.tex.len();
        self.tex.push_str(&source[range.clone()]);
        self.spans.push(Span {
            range: start..self.tex.len(),
            file: file.to_owned(),
            line: 1 + source[..range.start]
                .bytes()
                .filter(|&b| b == b'\n')
                .count(),
            source_start: Some(range.start),
        });
    }

    fn boundary(&mut self, tex: &str, source: &str, file: &Path, offset: usize) {
        let start = self.tex.len();
        self.tex.push_str(tex);
        let line = 1 + source[..offset].bytes().filter(|&b| b == b'\n').count();
        self.spans.push(Span {
            range: start..self.tex.len(),
            file: file.to_owned(),
            line,
            source_start: None,
        });
    }

    fn expand(
        &mut self,
        root: &Path,
        file: &Path,
        stack: &mut Vec<PathBuf>,
        subfile: bool,
        limit: &Path,
    ) -> Result<()> {
        ensure!(
            file.starts_with(limit),
            "取り込み元の外を参照している ({})",
            file.display()
        );
        ensure!(
            stack.len() < 64 && !stack.iter().any(|p| p == file),
            "LaTeX の入力が循環しているか、64 階層を超えた ({})",
            file.display()
        );
        stack.push(file.to_owned());
        if let Some(reading) = &mut self.reading {
            reading.count_source(file)?;
        }
        let file_contents = if let Some(contents) = self.sources.get(file) {
            Rc::clone(contents)
        } else {
            let contents = Rc::new(
                SourceFile::read(file).with_context(|| format!("{} を読めない", file.display()))?,
            );
            self.sources.insert(file.to_owned(), Rc::clone(&contents));
            contents
        };
        let source = file_contents
            .text()
            .with_context(|| format!("{} を読めない", file.display()))?;
        let relative = file.strip_prefix(root).unwrap_or(file);
        let bounds = if subfile {
            let (begin, end) = scan::document(source)
                .with_context(|| format!("{} に document 環境がない", file.display()))?;
            whitespace(source, begin.end)..end.start
        } else {
            0..source.len()
        };
        let local = file
            .parent()
            .context("入力ファイルの親ディレクトリがない")?;
        let mut cursor = bounds.start;
        let mut braces = 0usize;
        for token in
            tokens(&source[bounds.clone()]).map(|r| r.start + bounds.start..r.end + bounds.start)
        {
            if token.start < cursor {
                continue;
            }
            let command = &source[token.clone()];
            match command {
                "{" => braces += 1,
                "}" => braces = braces.saturating_sub(1),
                "\\input" | "\\include" | "\\subfile" | "\\subfileinclude" if braces == 0 => {
                    let (name, end) =
                        input_name(source, token.end, command).with_context(|| {
                            format!(
                                "{} の {command} に固定のファイル名が必要",
                                relative.display()
                            )
                        })?;
                    let (first, second) = if subfile {
                        (local, root)
                    } else {
                        (root, local)
                    };
                    let target = if self.reading.is_some() {
                        reading::find_input(first, second, name, limit)
                    } else {
                        find_file(first, name, &["tex"])
                            .or_else(|| find_file(second, name, &["tex"]))
                    }
                        .with_context(|| {
                            format!(
                                "{} の {command}{{{name}}} が見つからない",
                                relative.display()
                            )
                        })?;
                    self.push(source, relative, cursor..token.start);
                    if let Some(reading) = &mut self.reading {
                        if let Some((id, part)) = reading.card_input(&target) {
                            // Keep an outline anchor without reading the card's TeX body.
                            let start = self.tex.len();
                            let env = match part {
                                reading::Part::Statement => "theorem",
                                reading::Part::Proof => "proof",
                            };
                            self.tex.push_str(&format!("\\begin{{{env}}}\\end{{{env}}}"));
                            reading.anchors.insert(start, (id, part));
                            cursor = end;
                            continue;
                        }
                    }
                    let include = matches!(command, "\\include" | "\\subfileinclude");
                    let child_subfile = matches!(command, "\\subfile" | "\\subfileinclude");
                    if include {
                        self.boundary("\\clearpage%\n", source, relative, token.start);
                    }
                    if child_subfile {
                        self.boundary("\\begingroup%\n", source, relative, token.start);
                    }
                    let start = self.tex.len();
                    self.expand(root, &target, stack, child_subfile, limit)?;
                    if self.tex.len() > start && !self.tex.ends_with('\n') {
                        self.tex.push('\n');
                    }
                    if child_subfile {
                        self.boundary("\\endgroup\\ignorespaces%\n", source, relative, token.start);
                    }
                    if include {
                        self.boundary("\\clearpage%\n", source, relative, token.start);
                    }
                    cursor = end;
                }
                "\\includeonly" if braces == 0 => bail!("\\includeonly を外して本全体を取り込む"),
                _ if self.reading.is_some() => {}
                "\\graphicspath" if braces == 0 => {
                    if let Some((paths, _)) = argument(source, token.end, '{', '}') {
                        let mut offset = 0;
                        self.graphics_dirs.clear();
                        while let Some((dir, end)) = argument(paths, offset, '{', '}') {
                            self.graphics_dirs.push(PathBuf::from(dir.trim()));
                            offset = end;
                        }
                    }
                }
                "\\includegraphics"
                | "\\bibliography"
                | "\\addbibresource"
                | "\\bibliographystyle"
                | "\\usepackage"
                | "\\RequirePackage"
                | "\\documentclass" => {
                    let after_options = optional(source, starred(source, token.end));
                    if let Some((names, _)) = argument(source, after_options, '{', '}') {
                        for name in names.split(',').map(str::trim) {
                            self.asset(root, local, limit, name, command)?;
                        }
                    }
                }
                _ => {}
            }
        }
        self.push(source, relative, cursor..bounds.end);
        stack.pop();
        Ok(())
    }

    fn asset(
        &mut self,
        root: &Path,
        local: &Path,
        limit: &Path,
        name: &str,
        command: &str,
    ) -> Result<()> {
        let (extensions, required): (&[&str], bool) = match command {
            "\\includegraphics" => (
                &["pdf", "png", "jpg", "jpeg", "eps", "mps", "jbig2", "jb2"],
                true,
            ),
            "\\bibliography" | "\\addbibresource" => (&["bib"], true),
            "\\bibliographystyle" => (&["bst"], false),
            "\\documentclass" => (&["cls"], false),
            _ => (&["sty"], false),
        };
        if name.is_empty() || name.contains(['\\', '{', '}', '#', '%']) {
            if required {
                self.warnings.insert(format!(
                    "{command}{{{name}}} のファイル名を静的に解決できなかった"
                ));
            }
            return Ok(());
        }
        let requested = Path::new(name);
        if !safe_relative(requested) {
            if required {
                self.warnings.insert(format!(
                    "{command}{{{name}}} はデッキからの相対パスに移して確認する"
                ));
            }
            return Ok(());
        }
        let paths = std::iter::once(PathBuf::new()).chain(
            self.graphics_dirs
                .iter()
                .filter(|p| command == "\\includegraphics" && safe_relative(p))
                .cloned(),
        );
        let mut found = false;
        for dir in paths {
            let path = dir.join(requested);
            for candidate in candidates(&path, extensions) {
                if let Some(source) = [root, local]
                    .into_iter()
                    .filter_map(|base| base.join(&candidate).canonicalize().ok())
                    .find(|source| source.is_file() && source.starts_with(limit))
                {
                    ensure!(
                        !matches!(candidate.components().next(), Some(Component::Normal(name))
                        if matches!(name.to_str(), Some("cards" | "build" | "deck.toml" | "preamble.tex" | "structure.yaml" | ".gitignore"))),
                        "素材のパスがデッキの生成先と重なる ({})",
                        candidate.display()
                    );
                    if let Some(previous) = self.assets.insert(candidate.clone(), source.clone()) {
                        ensure!(
                            previous == source,
                            "同じ相対パスに異なる素材がある ({})",
                            candidate.display()
                        );
                    }
                    found = true;
                }
            }
        }
        if required && !found {
            self.warnings
                .insert(format!("{command}{{{name}}} の素材が見つからなかった"));
        }
        Ok(())
    }
}

fn safe_relative(path: &Path) -> bool {
    path.components()
        .all(|c| matches!(c, Component::CurDir | Component::Normal(_)))
}

fn candidates<'a>(path: &'a Path, extensions: &'a [&str]) -> impl Iterator<Item = PathBuf> + 'a {
    std::iter::once(path.to_owned()).chain(extensions.iter().filter_map(move |ext| {
        // TeX may append .tex to a name containing dots.
        let mut file = path.as_os_str().to_owned();
        file.push(format!(".{ext}"));
        (path.extension().and_then(|e| e.to_str()) != Some(ext)).then(|| PathBuf::from(file))
    }))
}

fn find_file(root: &Path, name: &str, extensions: &[&str]) -> Option<PathBuf> {
    candidates(Path::new(name), extensions)
        .filter_map(|name| root.join(name).canonicalize().ok())
        .find(|path| path.is_file())
}

fn input_name<'a>(source: &'a str, start: usize, command: &str) -> Option<(&'a str, usize)> {
    let (name, end) = argument(source, start, '{', '}').or_else(|| {
        if command != "\\input" {
            return None;
        }
        let start = whitespace(source, start);
        let tail = &source[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '%')
            .unwrap_or(tail.len());
        Some((&tail[..end], start + end))
    })?;
    let name = name.trim();
    (!name.is_empty()
        && !name.contains(['\\', '{', '}', '#', '%'])
        && Path::new(name).is_relative())
    .then_some((name, end))
}

fn label(tex: &str) -> Option<&str> {
    tokens(tex).find_map(|token| {
        (&tex[token.clone()] == "\\label")
            .then(|| argument(tex, token.end, '{', '}').map(|(name, _)| name.trim()))
            .flatten()
    })
}

pub fn statement_head(tex: &str) -> &str {
    let mut start = whitespace(tex, 0);
    if tex[start..].starts_with("\\begin") {
        if let Some((_, end)) = argument(tex, start + "\\begin".len(), '{', '}') {
            start = whitespace(tex, optional(tex, end));
        }
    }
    while tex[start..].starts_with("\\label") {
        let Some((_, end)) = argument(tex, start + "\\label".len(), '{', '}') else {
            break;
        };
        start = whitespace(tex, end);
    }
    &tex[start..]
}

fn theorem_kinds(preamble: &str) -> BTreeMap<&str, &str> {
    let mut kinds: BTreeMap<_, _> = [
        ("theorem", "theorem"),
        ("thm", "theorem"),
        ("lemma", "lemma"),
        ("lem", "lemma"),
        ("definition", "definition"),
        ("defn", "definition"),
        ("proposition", "proposition"),
        ("prop", "proposition"),
        ("corollary", "corollary"),
        ("cor", "corollary"),
        ("axiom", "axiom"),
        ("claim", "claim"),
        ("remark", "remark"),
        ("example", "example"),
        ("conjecture", "conjecture"),
        ("exercise", "exercise"),
    ]
    .into_iter()
    .collect();
    for token in tokens(preamble) {
        let command = &preamble[token.clone()];
        let start = match command {
            "\\newtheorem" => starred(preamble, token.end),
            "\\declaretheorem" => optional(preamble, token.end),
            _ => continue,
        };
        if let Some((names, end)) = argument(preamble, start, '{', '}') {
            let printed = if command == "\\newtheorem" {
                argument(preamble, optional(preamble, end), '{', '}').map(|(name, _)| name.trim())
            } else {
                let options = &preamble[token.end..start];
                options.split(',').find_map(|option| {
                    let (key, value) = option.trim_start_matches(['[', ' ']).split_once('=')?;
                    (key.trim() == "name").then(|| value.trim().trim_matches(['{', '}', ']']))
                })
            };
            for name in names.split(',').map(str::trim) {
                let kind = kinds
                    .get(name)
                    .or_else(|| {
                        let printed = printed?.to_lowercase();
                        kinds
                            .get(printed.as_str())
                            .or_else(|| match printed.as_str() {
                                "定理" => Some(&"theorem"),
                                "補題" => Some(&"lemma"),
                                "定義" => Some(&"definition"),
                                "系" => Some(&"corollary"),
                                "命題" => Some(&"proposition"),
                                _ => None,
                            })
                    })
                    .copied()
                    .unwrap_or("theorem");
                kinds.insert(name, kind);
            }
        }
    }
    kinds
}

fn slug(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(limit) {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn head(text: &str, fallback: &str) -> String {
    let text = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with(['%', '\\']))
        .unwrap_or(fallback);
    let end = text
        .char_indices()
        .find(|&(at, c)| {
            matches!(c, '。' | '！' | '？')
                || matches!(c, '.' | '!' | '?') && text[at + c.len_utf8()..].starts_with(' ')
        })
        .map_or(text.len(), |(at, c)| at + c.len_utf8());
    text[..end].chars().take(64).collect()
}

struct Card {
    id: String,
    title: String,
    kind: String,
    statement: Range<usize>,
    proof: Option<Range<usize>>,
}

struct Split<'a> {
    tex: &'a str,
    book: Book,
    cards: Vec<Card>,
    ids: BTreeSet<String>,
    numbers: [usize; 4],
    current_theorem: Option<usize>,
    paragraphs: Vec<usize>,
    warnings: BTreeSet<String>,
}

impl<'a> Split<'a> {
    fn new(tex: &'a str, title: String) -> Self {
        Self {
            tex,
            book: Book {
                title,
                before_sections: String::new(),
                sections: Vec::new(),
            },
            cards: Vec::new(),
            ids: BTreeSet::new(),
            numbers: [0; 4],
            current_theorem: None,
            paragraphs: Vec::new(),
            warnings: BTreeSet::new(),
        }
    }

    fn chapter(&mut self) -> &mut Chapter {
        if self.book.sections.is_empty() {
            self.book.sections.push(Chapter {
                heading: Heading {
                    title: "前付け".into(),
                    level: "front".into(),
                    label: None,
                    tex: String::new(),
                },
                items: Vec::new(),
            });
        }
        self.book.sections.last_mut().unwrap()
    }

    fn card(
        &mut self,
        range: Range<usize>,
        kind: &str,
        content: Range<usize>,
        caption: Option<&str>,
    ) {
        if range.is_empty() {
            return;
        }
        let title = caption.map_or_else(|| head(&self.tex[content], kind), str::to_owned);
        let label = label(&self.tex[range.clone()])
            .map(|name| slug(name, 80))
            .filter(|name| !name.is_empty());
        let base = label.unwrap_or_else(|| {
            let [part, chapter, section, sub] = self.numbers;
            let title = slug(&title, 32);
            let scope = if chapter == 0 && part > 0 {
                format!("part{part}")
            } else {
                format!("ch{chapter}")
            };
            format!("{scope}-{section}-{sub}-{kind}-{title}")
                .trim_end_matches('-')
                .to_owned()
        });
        let mut id = base.clone();
        for suffix in 2.. {
            if self.ids.insert(id.clone()) {
                break;
            }
            id = format!("{base}-{suffix}");
        }
        self.chapter().items.push(Item::Card(id.clone()));
        self.cards.push(Card {
            id,
            title,
            kind: kind.into(),
            statement: range,
            proof: None,
        });
    }

    fn prose(&mut self, range: Range<usize>) {
        let mut start = range.start;
        for end in std::mem::take(&mut self.paragraphs) {
            if end.saturating_sub(start) >= 1500 && range.end.saturating_sub(end) >= 1000 {
                self.card(start..end, "expl", start..end, None);
                start = end;
            }
        }
        self.card(start..range.end, "expl", start..range.end, None);
    }

    fn heading(&mut self, level: usize, name: &str, title: &str, range: Range<usize>) {
        self.numbers[level] += 1;
        self.numbers[level + 1..].fill(0);
        self.current_theorem = None;
        let tex = &self.tex[range];
        let heading = Heading {
            title: title.split_whitespace().collect::<Vec<_>>().join(" "),
            level: name.trim_start_matches('\\').into(),
            label: label(tex).map(str::to_owned),
            tex: tex.into(),
        };
        if level < 2 || level == 2 && self.numbers[1] == 0 {
            self.book.sections.push(Chapter {
                heading,
                items: Vec::new(),
            });
        } else {
            self.chapter().items.push(Item::Heading { heading });
        }
    }

    fn parse(&mut self, body: Range<usize>, kinds: &BTreeMap<&str, &str>) {
        let mut cursor = body.start;
        let mut skip = cursor;
        let mut braces = 0usize;
        let mut envs = Vec::new();
        let mut statement = None;
        let tex = self.tex;
        for token in tokens(&tex[body.clone()]).map(|r| r.start + body.start..r.end + body.start) {
            if token.start < skip {
                continue;
            }
            let command = &tex[token.clone()];
            match command {
                "{" => braces += 1,
                "}" => braces = braces.saturating_sub(1),
                "\\begin" | "\\end" if braces == 0 => {
                    let Some((env, end)) = argument(tex, token.end, '{', '}') else {
                        continue;
                    };
                    skip = end;
                    if command == "\\begin" {
                        if envs.is_empty() {
                            if let Some(kind) = kinds
                                .get(env.trim_end_matches('*'))
                                .copied()
                                .or_else(|| matches!(env, "proof" | "proof*").then_some("proof"))
                            {
                                let caption = argument(tex, end, '[', ']');
                                let content = caption.map_or(end, |(_, end)| end);
                                skip = content;
                                statement = Some((
                                    token.start,
                                    content,
                                    kind,
                                    caption.map(|(name, _)| name),
                                ));
                            }
                        }
                        envs.push(env);
                    } else if envs.last() == Some(&env) {
                        envs.pop();
                        if envs.is_empty() {
                            if let Some((start, content, kind, caption)) = statement.take() {
                                if kind == "proof" {
                                    self.proof(cursor, start, end);
                                } else {
                                    self.gap(cursor..start);
                                    self.card(start..end, kind, content..token.start, caption);
                                    self.current_theorem = Some(self.cards.len() - 1);
                                }
                                cursor = end;
                            }
                        }
                    }
                }
                "\\part" | "\\chapter" | "\\section" | "\\subsection"
                    if braces == 0 && envs.is_empty() =>
                {
                    let start = optional(tex, starred(tex, token.end));
                    let Some((title, mut end)) = argument(tex, start, '{', '}') else {
                        continue;
                    };
                    let next = whitespace(tex, end);
                    if let Some(after_label) = tex[next..].strip_prefix("\\label") {
                        if let Some((_, label_end)) =
                            argument(tex, tex.len() - after_label.len(), '{', '}')
                        {
                            end = label_end;
                        }
                    }
                    self.gap(cursor..token.start);
                    let level = ["\\part", "\\chapter", "\\section", "\\subsection"]
                        .iter()
                        .position(|&name| name == command)
                        .unwrap();
                    self.heading(level, command, title, token.start..end);
                    cursor = end;
                    skip = end;
                }
                _ if braces == 0
                    && envs.is_empty()
                    && (command.starts_with('\n') || command.starts_with('\r')) =>
                {
                    self.paragraphs.push(token.end);
                }
                _ => {}
            }
        }
        self.gap(cursor..body.end);
    }

    fn gap(&mut self, range: Range<usize>) {
        if !range.is_empty() && whitespace(self.tex, range.start) >= range.end {
            // Keep separators and comments with the adjacent heading or card.
            if let Some(Item::Card(id)) = self.book.sections.last().and_then(|ch| ch.items.last()) {
                if let Some(card) = self.cards.last_mut().filter(|card| &card.id == id) {
                    card.statement.end = range.end;
                }
            } else if let Some(Item::Proof { .. }) =
                self.book.sections.last().and_then(|ch| ch.items.last())
            {
                if let Some(proof) = self
                    .current_theorem
                    .and_then(|i| self.cards[i].proof.as_mut())
                {
                    proof.end = range.end;
                }
            } else if let Some(chapter) = self.book.sections.last_mut() {
                let heading = match chapter.items.last_mut() {
                    Some(Item::Heading { heading }) => heading,
                    _ => &mut chapter.heading,
                };
                heading.tex.push_str(&self.tex[range]);
            } else {
                self.book.before_sections.push_str(&self.tex[range]);
            }
            self.paragraphs.clear();
        } else {
            self.prose(range);
        }
    }

    fn proof(&mut self, cursor: usize, start: usize, end: usize) {
        if let Some(i) = self.current_theorem {
            if whitespace(self.tex, cursor) >= start {
                if let Some(proof) = self.cards[i]
                    .proof
                    .as_mut()
                    .filter(|proof| proof.end == cursor)
                {
                    proof.end = end;
                    self.paragraphs.clear();
                    return;
                }
            }
            if self.cards[i].proof.is_none() {
                let proof_start = if whitespace(self.tex, cursor) >= start {
                    cursor
                } else {
                    self.prose(cursor..start);
                    start
                };
                let id = self.cards[i].id.clone();
                self.cards[i].proof = Some(proof_start..end);
                self.chapter().items.push(Item::Proof { proof: id });
                self.paragraphs.clear();
                return;
            }
        }
        self.warnings
            .insert("対応する主張にまとめられない証明は独立カードにした".into());
        self.gap(cursor..start);
        self.card(start..end, "proof", start..end, None);
    }
}

fn write_tex(path: &Path, tex: &str) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(tex.as_bytes())?;
    // A fragment cut mid-line must not add a TeX end-of-line space.
    if !tex.is_empty() && !tex.ends_with('\n') {
        file.write_all(b"%\n")?;
    }
    Ok(())
}

fn locations(expanded: &Expanded, lines: &[usize], range: &Range<usize>) -> Vec<SourceRange> {
    let line_at = |offset| lines.partition_point(|&at| at < offset);
    let start = expanded
        .spans
        .partition_point(|s| s.range.end <= range.start);
    expanded.spans[start..]
        .iter()
        .take_while(|s| s.range.start < range.end)
        .map(|span| {
            let line = |at| span.line + line_at(at) - line_at(span.range.start);
            let first = range.start.max(span.range.start);
            let last = range.end.min(span.range.end) - 1;
            SourceRange {
                path: span.file.clone(),
                first: line(first),
                last: line(last),
            }
        })
        .collect()
}

pub(crate) fn uncovered_theorems(
    main: &Path,
    workspace: &Path,
    cards: &BTreeMap<String, crate::Card>,
) -> Result<Vec<Theorem>> {
    let root = main.parent().context("主ファイルの親ディレクトリがない")?;
    let mut expanded = Expanded::default();
    expanded.expand(root, main, &mut Vec::new(), false, workspace)?;
    let (begin, end) = scan::document(&expanded.tex).context("主ファイルに document 環境がない")?;
    let mut split = Split::new(&expanded.tex, String::new());
    split.parse(
        begin.end..end.start,
        &theorem_kinds(&expanded.tex[..begin.start]),
    );
    let lines: Vec<_> = expanded.tex.match_indices('\n').map(|(at, _)| at).collect();
    let current_ids: BTreeSet<_> = split.cards.iter().map(|card| card.id.as_str()).collect();
    let covered_locations: BTreeSet<_> = cards
        .values()
        .filter(|card| !current_ids.contains(card.id.as_str()))
        .filter_map(|card| SourceRange::parse(card.source.as_deref()?))
        .map(|source| (source.path, source.first))
        .collect();

    Ok(split
        .cards
        .iter()
        .filter(|card| !matches!(card.kind.as_str(), "expl" | "proof"))
        .filter_map(|card| {
            let source = locations(&expanded, &lines, &card.statement)
                .into_iter()
                .next()?;
            let has_card = cards
                .get(&card.id)
                .and_then(|existing| SourceRange::parse(existing.source.as_deref()?))
                .is_some_and(|existing| existing.path == source.path);
            if has_card || covered_locations.contains(&(source.path.clone(), source.first)) {
                return None;
            }
            Some(Theorem {
                id: card.id.clone(),
                title: card.title.clone(),
                source: source.to_string(),
            })
        })
        .collect())
}

/// Import using the main file's directory as the source boundary.
pub fn import(main: &Path, deck: &Path, verify: bool) -> Result<Imported> {
    let main = main.canonicalize()?;
    let limit = main.parent().context("主ファイルの親ディレクトリがない")?;
    import_in(&main, deck, limit, verify)
}

/// All resolved inputs must stay inside the supplied source boundary.
pub fn import_in(main: &Path, deck: &Path, limit: &Path, verify: bool) -> Result<Imported> {
    let main = main.canonicalize()?;
    let source_root = main.parent().context("主ファイルの親ディレクトリがない")?;
    let limit = limit.canonicalize()?;
    let parent = deck
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let name = deck
        .file_name()
        .and_then(|n| n.to_str())
        .context("出力先デッキ名が不正")?;
    let deck = parent.join(name);
    let report = parent.join(format!("{name}.import-report.md"));
    ensure!(
        !deck.exists() && !report.exists(),
        "デッキと取り込み報告には新しい出力先を指定する"
    );

    let mut expanded = Expanded::default();
    expanded.expand(source_root, &main, &mut Vec::new(), false, &limit)?;
    let (begin, end) = scan::document(&expanded.tex).context("主ファイルに document 環境がない")?;
    let preamble = &expanded.tex[..begin.start];
    let title = tokens(preamble)
        .find_map(|token| {
            (&preamble[token.clone()] == "\\title")
                .then(|| {
                    argument(preamble, optional(preamble, token.end), '{', '}')
                        .map(|(name, _)| name)
                })
                .flatten()
        })
        .unwrap_or(name)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut split = Split::new(&expanded.tex, title);
    split.parse(begin.end..end.start, &theorem_kinds(preamble));
    let stage = tempfile::Builder::new()
        .prefix(".fuzukue-book-import-")
        .tempdir_in(&parent)?;
    let staged_deck = stage.path().join("deck");
    fs::create_dir(&staged_deck)?;
    let lines: Vec<_> = expanded.tex.match_indices('\n').map(|(at, _)| at).collect();
    fs::create_dir(staged_deck.join("cards"))?;
    for card in &split.cards {
        let dir = staged_deck.join("cards").join(&card.id);
        fs::create_dir_all(&dir)?;
        let mut sources = locations(&expanded, &lines, &card.statement);
        if sources.is_empty() {
            sources.push(SourceRange {
                path: main.file_name().unwrap_or_default().into(),
                first: 1,
                last: 1,
            });
        }
        let proof_sources = card
            .proof
            .as_ref()
            .map_or_else(Vec::new, |range| locations(&expanded, &lines, range));
        let source_sha256 = sources
            .iter()
            .chain(&proof_sources)
            .map(|range| {
                let path = source_root.join(&range.path);
                let hash = expanded
                    .sources
                    .get(&path)
                    .and_then(|file| file.range_hash(range))
                    .with_context(|| format!("原稿の行範囲が不正 ({range})"))?;
                Ok((range.to_string(), hash))
            })
            .collect::<Result<_>>()?;
        let sources: Vec<_> = sources.iter().map(ToString::to_string).collect();
        let front = Front {
            id: &card.id,
            kind: &card.kind,
            title: &card.title,
            status: "draft",
            source: &sources[0],
            additional_sources: &sources[1..],
            proof_sources: proof_sources.iter().map(ToString::to_string).collect(),
            source_sha256,
        };
        fs::write(
            dir.join("card.md"),
            format!("---\n{}---\n", serde_yaml::to_string(&front)?),
        )?;
        write_tex(
            &dir.join("statement.tex"),
            &expanded.tex[card.statement.clone()],
        )?;
        if let Some(proof) = &card.proof {
            write_tex(&dir.join("proof.tex"), &expanded.tex[proof.clone()])?;
        }
    }
    for (dest, source) in &expanded.assets {
        let dest = staged_deck.join(dest);
        fs::create_dir_all(dest.parent().context("素材の親ディレクトリがない")?)?;
        fs::copy(source, dest)?;
    }
    fs::write(staged_deck.join("preamble.tex"), preamble)?;
    fs::write(
        staged_deck.join("structure.yaml"),
        serde_yaml::to_string(&split.book)?,
    )?;
    let mut settings = toml::Table::new();
    settings.insert("kind".into(), "book".into());
    settings.insert("title".into(), split.book.title.into());
    settings.insert("preamble".into(), "preamble.tex".into());
    settings.insert("imported_latex".into(), true.into());
    let manuscript_main = workspace_paths::relative(&deck, &main).unwrap_or(main.clone());
    settings.insert(
        "manuscript_main".into(),
        manuscript_main.to_string_lossy().into_owned().into(),
    );
    let language = if expanded
        .tex
        .chars()
        .any(|c| matches!(c, '\u{3040}'..='\u{30ff}'))
    {
        "ja"
    } else {
        "en"
    };
    settings.insert("language".into(), language.into());
    fs::write(staged_deck.join("deck.toml"), toml::to_string(&settings)?)?;
    fs::write(staged_deck.join(".gitignore"), "/build/\n")?;

    let mut summary = format!(
        "# LaTeX 取り込み報告\n\n- 入力 {}\n- カード {} 枚、素材 {} 件\n",
        main.file_name().unwrap_or_default().to_string_lossy(),
        split.cards.len(),
        expanded.assets.len()
    );
    for warning in expanded.warnings.union(&split.warnings) {
        summary.push_str(&format!("- {warning}\n"));
    }
    let verification = if verify {
        verify::compare(&main, &staged_deck, &mut summary).unwrap_or_else(|error| {
            summary.push_str(&format!("\n比較エラー\n\n    {error:#}\n"));
            "比較エラー".into()
        })
    } else {
        summary.push_str("\n## PDF の比較\n\n省略した。原本とデッキの PDF を別途確認する。\n");
        "省略".into()
    };
    let mut staged_report = tempfile::NamedTempFile::new_in(&parent)?;
    staged_report.write_all(summary.as_bytes())?;
    fs::create_dir(&deck)?;
    if let Err(error) = staged_report.persist_noclobber(&report) {
        let _ = fs::remove_dir(&deck);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&staged_deck, &deck) {
        let _ = fs::remove_dir(&deck);
        let _ = fs::remove_file(&report);
        return Err(error.into());
    }
    Ok(Imported {
        cards: split.cards.len(),
        report,
        verification,
    })
}

/// Render a master that is built from the deck directory.
pub fn render(deck: &Path, cfg: &DeckConfig) -> Result<(Engine, String)> {
    let path = cfg
        .meta
        .preamble
        .as_ref()
        .context("プリアンブルを指定する")?;
    let preamble = fs::read_to_string(deck.join(path))?;
    let engine = match cfg.meta.build.as_ref().and_then(|build| build.engine) {
        Some(engine) => engine,
        None => Engine::detect(&preamble, cfg.meta.language.as_deref())?,
    };
    let book: Book = serde_yaml::from_str(&fs::read_to_string(deck.join(&cfg.structure))?)?;
    let mut master = preamble;
    master.push_str("\\begin{document}");
    master.push_str(&book.before_sections);
    for chapter in book.sections {
        master.push_str(&chapter.heading.tex);
        for item in chapter.items {
            let (id, file) = match item {
                Item::Heading { heading } => {
                    master.push_str(&heading.tex);
                    continue;
                }
                Item::Card(id) => (id, "statement.tex"),
                Item::Proof { proof } => (proof, "proof.tex"),
            };
            ensure!(
                !id.is_empty() && id.chars().all(|ch| ch.is_alphanumeric() || ch == '-'),
                "取り込んだカードの ID が不正 ({id})"
            );
            master.push_str(&format!("\\input{{cards/{id}/{file}}}%\n"));
        }
    }
    master.push_str("\\end{document}\n");
    Ok((engine, master))
}

pub fn prepare_build(deck: &Path, cfg: &DeckConfig) -> Result<Engine> {
    let (engine, text) = render(deck, cfg)?;
    fs::create_dir_all(deck.join("build"))?;
    fs::write(deck.join("build/paper.tex"), text)?;
    Ok(engine)
}

pub fn build(deck: &Path) -> Result<PathBuf> {
    let cfg = DeckConfig::read(deck)?;
    ensure!(
        cfg.meta.imported_latex,
        "LaTeX から取り込んだデッキを指定する"
    );
    let engine = prepare_build(deck, &cfg)?;
    verify::build_deck(deck, engine)?;
    Ok(deck.join("build/paper.pdf"))
}
