//! Read the outline of a source-built manuscript. The outline carries byte ranges
//! so the client can fetch prose on demand alongside the existing card bodies.

use super::{candidates, label, scan, slug, theorem_kinds, whitespace, Expanded, Item, Split};
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

pub(super) struct Reading {
    deck: PathBuf,
    ids: BTreeSet<String>,
    source_bytes: u64,
    pub(super) anchors: BTreeMap<usize, (String, Part)>,
}

impl Reading {
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
}

pub(super) fn find_input(root: &Path, local: &Path, name: &str, deck: &Path) -> Option<PathBuf> {
    [root, local].into_iter().find_map(|base| {
        candidates(Path::new(name), &["tex"])
            .filter_map(|path| base.join(path).canonicalize().ok())
            .find(|path| {
                path.starts_with(deck)
                    && path.is_file()
                    && path.extension().is_some_and(|ext| ext == "tex")
            })
    })
}

/// Project a literal TeX manuscript into headings, existing card anchors and
/// read-only prose. Fall back to the caller's outline if card identities cannot
/// all be reconciled. The original manuscript and deck are never written.
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
    let mut expected: Vec<_> = outline
        .iter()
        .flat_map(|section| &section.items)
        .filter_map(|item| match item {
            DocItem::Card { id } => Some(id.clone()),
            DocItem::Subsection { .. } => None,
        })
        .collect();
    let mut expanded = Expanded {
        reading: Some(Reading {
            deck: deck.clone(),
            ids: expected.iter().cloned().collect(),
            source_bytes: 0,
            anchors: BTreeMap::new(),
        }),
        ..Expanded::default()
    };
    expanded.expand(root, &main, &mut Vec::new(), false, &deck)?;
    let (begin, end) = scan::document(&expanded.tex).context("missing document environment")?;
    let mut split = Split::new(&expanded.tex, String::new());
    split.parse(
        begin.end..end.start,
        &theorem_kinds(&expanded.tex[..begin.start]),
    );
    let reading = expanded.reading.as_ref().unwrap();
    let cards: BTreeMap<_, _> = split
        .cards
        .iter()
        .map(|card| (card.id.as_str(), card))
        .collect();
    let mut matched = BTreeMap::new();
    for card in &split.cards {
        let mut inputs = reading
            .anchors
            .range(card.statement.clone())
            .filter_map(|(_, (id, part))| (*part == Part::Statement).then_some(id.as_str()));
        let input = inputs.next();
        ensure!(
            inputs.next().is_none(),
            "multiple card inputs in one statement"
        );
        let id = input.or_else(|| {
            let key = label(&expanded.tex[card.statement.clone()])?;
            [
                key,
                key.strip_prefix("card:").unwrap_or(key),
                &slug(key, 80),
            ]
            .into_iter()
            .find_map(|key| reading.ids.get(key).map(String::as_str))
        });
        if let Some(id) = id {
            matched.insert(card.id.as_str(), id.to_owned());
        }
    }
    expected.sort_unstable();
    let mut actual: Vec<_> = matched.values().cloned().collect();
    actual.sort_unstable();
    ensure!(
        actual == expected,
        "could not align manuscript with all outline cards"
    );

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
        };
        for item in section.items {
            match item {
                Item::Heading { heading } => view.items.push(DocItem::Subsection {
                    title: heading.title,
                }),
                Item::Card(id) if matched.contains_key(id.as_str()) => {
                    view.items.push(DocItem::Card {
                        id: matched[id.as_str()].clone(),
                    });
                }
                Item::Card(id) => push_prose(
                    &mut view,
                    &expanded,
                    &deck,
                    root,
                    cards[id.as_str()].statement.clone(),
                )?,
                Item::Proof { proof } if !matched.contains_key(proof.as_str()) => {
                    if let Some(range) = cards[proof.as_str()].proof.clone() {
                        push_prose(&mut view, &expanded, &deck, root, range)?;
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
