//! Project source TeX into card anchors and lazy read-only excerpts.

use super::{
    candidates, input_name, label, scan, slug, theorem_kinds, whitespace, Expanded, Item, Split,
};
use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    DocItem, DocProse, DocProseSpan, DocProseText, DocSection, DOC_PROSE_SPAN_BYTES,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Part {
    Statement,
    Proof,
}

pub(super) struct HookInput {
    pub(super) local: PathBuf,
    pub(super) name: String,
}

pub(super) struct Reading {
    deck: PathBuf,
    ids: BTreeSet<String>,
    source_bytes: u64,
    pub(super) anchors: BTreeMap<usize, (String, Part)>,
    in_document: bool,
    title: Vec<HookInput>,
    end: Vec<HookInput>,
}

impl Reading {
    fn new(deck: PathBuf, outline: &[DocSection]) -> Self {
        Self {
            deck,
            ids: outline
                .iter()
                .flat_map(|section| &section.items)
                .filter_map(|item| match item {
                    DocItem::Card { id } => Some(id.clone()),
                    DocItem::Subsection { .. } => None,
                })
                .collect(),
            source_bytes: 0,
            anchors: BTreeMap::new(),
            in_document: false,
            title: Vec::new(),
            end: Vec::new(),
        }
    }

    pub(super) fn count_source(&mut self, path: &Path) -> Result<()> {
        // Repeated includes can expand exponentially, even with a depth limit.
        self.source_bytes = self.source_bytes.saturating_add(fs::metadata(path)?.len());
        ensure!(
            self.source_bytes <= 64 * 1024 * 1024,
            "manuscript outline exceeds the source scan limit"
        );
        Ok(())
    }

    pub(super) fn card_input(&self, path: &Path) -> Option<(String, Part)> {
        // The writing harness emits one input file per card part.
        if path.parent()? == self.deck.join("build/cards") {
            let filename = path.file_name()?.to_str()?;
            let (id, part) = filename
                .strip_suffix(".statement.tex")
                .map(|id| (id, Part::Statement))
                .or_else(|| {
                    filename
                        .strip_suffix(".proof.tex")
                        .map(|id| (id, Part::Proof))
                })?;
            return self.ids.contains(id).then(|| (id.to_owned(), part));
        }
        let part = match path.file_name()?.to_str()? {
            "statement.tex" => Part::Statement,
            "proof.tex" => Part::Proof,
            _ => return None,
        };
        let meta = path.with_file_name("card.md").canonicalize().ok()?;
        if !meta.starts_with(&self.deck) || !meta.is_file() {
            return None;
        }
        let raw = fs::read_to_string(meta).ok()?;
        let (front, _) = crate::split_frontmatter(&raw)?;
        let value: serde_yaml::Value = serde_yaml::from_str(front).ok()?;
        let id = value["id"].as_str()?;
        self.ids.contains(id).then(|| (id.to_owned(), part))
    }

    /// Recognize literal inputs in the two preamble hooks used by manuscripts.
    pub(super) fn hook(
        &mut self,
        source: &str,
        token: Range<usize>,
        local: &Path,
    ) -> Vec<HookInput> {
        let command = &source[token.clone()];
        if command == "\\begin"
            && scan::argument(source, token.end, '{', '}').is_some_and(|(env, _)| env == "document")
        {
            self.in_document = true;
        }
        if self.in_document {
            return match command {
                "\\maketitle" => std::mem::take(&mut self.title),
                "\\end"
                    if scan::argument(source, token.end, '{', '}')
                        .is_some_and(|(env, _)| env == "document") =>
                {
                    std::mem::take(&mut self.end)
                }
                _ => Vec::new(),
            };
        }

        let body = |at| scan::argument(source, at, '{', '}').map(|(body, _)| body);
        match command {
            "\\newcommand" | "\\renewcommand" | "\\providecommand" => {
                let start = scan::starred(source, token.end);
                let title_end = match scan::argument(source, start, '{', '}') {
                    Some((name, end)) if name.trim() == "\\maketitle" => Some(end),
                    _ => {
                        let start = whitespace(source, start);
                        scan::tokens(&source[start..])
                            .next()
                            .filter(|name| {
                                name.start == 0
                                    && &source[start + name.start..start + name.end]
                                        == "\\maketitle"
                            })
                            .map(|name| start + name.end)
                    }
                };
                if let Some(end) = title_end {
                    let end = match scan::argument(source, end, '[', ']') {
                        None => Some(end),
                        Some(("0", end)) => Some(end),
                        _ => None,
                    };
                    if let Some(body) = end.and_then(body) {
                        self.title = hook_inputs(body, local).collect();
                    }
                }
            }
            "\\AtEndDocument" => {
                if let Some(body) = body(token.end) {
                    self.end.extend(hook_inputs(body, local));
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn hook_inputs<'a>(body: &'a str, local: &'a Path) -> impl Iterator<Item = HookInput> + 'a {
    let mut braces = 0usize;
    let mut skip = 0;
    scan::tokens(body).filter_map(move |token| {
        if token.start < skip {
            return None;
        }
        let command = &body[token.clone()];
        match command {
            "{" => braces += 1,
            "}" => braces = braces.saturating_sub(1),
            "\\input" | "\\include" if braces == 0 => {
                let (name, end) = input_name(body, token.end, command)?;
                skip = end;
                return Some(HookInput {
                    local: local.to_owned(),
                    name: name.to_owned(),
                });
            }
            _ => {}
        }
        None
    })
}

pub(super) fn find_input(root: &Path, local: &Path, name: &str, deck: &Path) -> Option<PathBuf> {
    [root, local].into_iter().find_map(|base| {
        candidates(Path::new(name), &["tex"])
            .filter_map(|path| {
                let path = base.join(path);
                if base == deck.join("build") && !base.exists() {
                    // A fresh writing harness has no build directory yet.
                    crate::workspace_paths::resolve(deck, deck, &path).ok()
                } else {
                    path.canonicalize().ok()
                }
            })
            .find(|path| {
                path.starts_with(deck)
                    && path.is_file()
                    && path.extension().is_some_and(|ext| ext == "tex")
            })
    })
}

/// Project a literal TeX manuscript into headings, existing card anchors and
/// read-only prose. Keep outline cards with unknown positions available for edits.
pub fn manuscript_sections(
    deck: &Path,
    main: &Path,
    outline: &[DocSection],
) -> Result<Vec<DocSection>> {
    ensure!(super::safe_relative(main), "TeX root must be deck-relative");
    let deck = deck.canonicalize()?;
    let main = find_input(
        &deck,
        &deck,
        main.to_str().context("invalid TeX root")?,
        &deck,
    )
    .context("TeX root is outside the deck or unavailable")?;
    let root = main.parent().context("TeX root has no parent")?;
    let mut expanded = Expanded {
        reading: Some(Reading::new(deck.clone(), outline)),
        ..Expanded::default()
    };
    expanded.expand(root, &main, &mut Vec::new(), false, &deck)?;
    let (begin, end) = scan::document(&expanded.tex).context("missing document environment")?;
    let mut matched = BTreeMap::new();
    let mut sections = project(
        &expanded,
        &deck,
        root,
        begin.start,
        begin.end..end.start,
        &mut matched,
    )?;

    for section in outline {
        let items: Vec<_> = section
            .items
            .iter()
            .filter(|item| {
                let DocItem::Card { id } = item else {
                    return false;
                };
                let remaining = matched.entry(id.clone()).or_insert(0);
                if *remaining == 0 {
                    true
                } else {
                    *remaining -= 1;
                    false
                }
            })
            .cloned()
            .collect();
        if !items.is_empty() {
            sections.push(DocSection {
                title: section.title.clone(),
                items,
                prose: Vec::new(),
                unmatched: true,
            });
        }
    }
    Ok(sections)
}

/// The writing harness builds its body from the outline and runs the
/// preamble hooks around it. Read those hooks from source without a prior build.
pub fn harness_manuscript_sections(
    deck: &Path,
    preamble: &Path,
    outline: Vec<DocSection>,
) -> Result<Vec<DocSection>> {
    ensure!(
        super::safe_relative(preamble),
        "TeX preamble must be deck-relative"
    );
    let deck = deck.canonicalize()?;
    let preamble = find_input(
        &deck,
        &deck,
        preamble.to_str().context("invalid TeX preamble")?,
        &deck,
    )
    .context("TeX preamble is outside the deck or unavailable")?;
    let root = deck.join("build");
    let mut expanded = Expanded {
        reading: Some(Reading::new(deck.clone(), &outline)),
        ..Expanded::default()
    };
    expanded.expand(&root, &preamble, &mut Vec::new(), false, &deck)?;
    let preamble_end = expanded.tex.len();
    let reading = expanded.reading.as_mut().unwrap();
    reading.in_document = true;
    let title = std::mem::take(&mut reading.title);
    let end = std::mem::take(&mut reading.end);
    expanded.expand_hook_inputs(&root, &deck, &mut Vec::new(), title)?;
    let body_end = expanded.tex.len();
    expanded.expand_hook_inputs(&root, &deck, &mut Vec::new(), end)?;

    let mut matched = BTreeMap::new();
    let mut sections = project(
        &expanded,
        &deck,
        &root,
        preamble_end,
        preamble_end..body_end,
        &mut matched,
    )?;
    sections.extend(outline);
    sections.extend(project(
        &expanded,
        &deck,
        &root,
        preamble_end,
        body_end..expanded.tex.len(),
        &mut matched,
    )?);
    Ok(sections)
}

fn project(
    expanded: &Expanded,
    deck: &Path,
    root: &Path,
    preamble_end: usize,
    body: Range<usize>,
    matched: &mut BTreeMap<String, usize>,
) -> Result<Vec<DocSection>> {
    let mut split = Split::new(&expanded.tex, String::new());
    split.parse(body, &theorem_kinds(&expanded.tex[..preamble_end]));
    let reading = expanded.reading.as_ref().unwrap();
    let cards: BTreeMap<_, _> = split
        .cards
        .iter()
        .map(|card| (card.id.as_str(), card))
        .collect();
    let mut ids = BTreeMap::new();
    for card in &split.cards {
        let mut inputs = reading
            .anchors
            .range(card.statement.clone())
            .filter_map(|(_, (id, part))| (*part == Part::Statement).then_some(id.as_str()));
        let input = inputs.next();
        let id = if inputs.next().is_some() {
            None
        } else {
            input.or_else(|| {
                let key = label(&expanded.tex[card.statement.clone()])?;
                [
                    key,
                    key.strip_prefix("card:").unwrap_or(key),
                    &slug(key, 80),
                ]
                .into_iter()
                .find_map(|key| reading.ids.get(key).map(String::as_str))
            })
        };
        if let Some(id) = id {
            ids.insert(card.id.as_str(), id);
        }
    }

    let mut sections = Vec::new();
    for section in split.book.sections {
        let mut view = DocSection {
            title: if section.heading.level == "front" {
                String::new()
            } else {
                section.heading.title
            },
            items: Vec::new(),
            prose: Vec::new(),
            unmatched: false,
        };
        for item in section.items {
            match item {
                Item::Heading { heading } => view.items.push(DocItem::Subsection {
                    title: heading.title,
                }),
                Item::Card(id) if ids.contains_key(id.as_str()) => {
                    let id = ids[id.as_str()].to_owned();
                    *matched.entry(id.clone()).or_default() += 1;
                    view.items.push(DocItem::Card { id });
                }
                Item::Card(id) => push_prose(
                    &mut view,
                    expanded,
                    deck,
                    root,
                    cards[id.as_str()].statement.clone(),
                )?,
                Item::Proof { proof } if !ids.contains_key(proof.as_str()) => {
                    if let Some(range) = cards[proof.as_str()].proof.clone() {
                        push_prose(&mut view, expanded, deck, root, range)?;
                    }
                }
                Item::Proof { .. } => {}
            }
        }
        if !view.title.is_empty() || !view.items.is_empty() || !view.prose.is_empty() {
            sections.push(view);
        }
    }
    Ok(sections)
}

fn push_prose(
    section: &mut DocSection,
    expanded: &Expanded,
    deck: &Path,
    root: &Path,
    range: Range<usize>,
) -> Result<()> {
    let first = expanded
        .spans
        .partition_point(|span| span.range.end <= range.start);
    let mut sources = Vec::new();
    let mut previous_end = range.start;
    for span in &expanded.spans[first..] {
        if span.range.start >= range.end {
            break;
        }
        let Some(source_start) = span.source_start else {
            continue;
        };
        let start = range.start.max(span.range.start);
        let end = range.end.min(span.range.end);
        let path = root
            .join(&span.file)
            .strip_prefix(deck)?
            .to_str()
            .context("non-UTF-8 TeX path")?
            .replace('\\', "/");
        let mut cursor = start;
        while cursor < end {
            let mut next = end.min(cursor + DOC_PROSE_SPAN_BYTES);
            while !expanded.tex.is_char_boundary(next) {
                next -= 1;
            }
            let tex = &expanded.tex[cursor..next];
            sources.push(DocProseSpan {
                path: path.clone(),
                start: (source_start + cursor - span.range.start) as u64,
                len: tex.len(),
                sha256: format!("{:x}", Sha256::digest(tex)),
                newline_before: expanded.tex[previous_end..cursor].contains('\n'),
            });
            cursor = next;
            previous_end = next;
        }
    }
    if !sources.is_empty() && whitespace(&expanded.tex, range.start) < range.end {
        section.prose.push(DocProse {
            before_item: section.items.len(),
            text: DocProseText::Source(sources),
        });
    }
    Ok(())
}
