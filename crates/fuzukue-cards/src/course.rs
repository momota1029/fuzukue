//! Lecture notes built from an imported book: section links, `course.toml`, and the
//! plan / next / commit steps.
//!
//! The book deck is never written. State lives only in the lecture deck's `course.toml`.
//! Versions are hashes of captured bytes (`card.md`, the card's `.tex` files and the TeX
//! files they `\input`), not translation `source_fingerprint`s, which only hash normalized
//! Markdown. A review's `version` in `course.toml` is one of these strings.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{split_frontmatter, DeckConfig, FOLLOW_RELS};

pub const FILE: &str = "course.toml";
/// Layout version of `course.toml` itself.
pub const SCHEMA: u32 = 1;
/// Prefix of every version string. A new capture scheme gets a new prefix, so old and new
/// values never compare equal by accident.
pub const VERSION_PREFIX: &str = "bytes-v1:";
const CARD_PREFIX: &str = "lec-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Todo,
    Adopted,
    Skipped,
    Writing,
    Written,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Adopt,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    Clean,
    Findings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// The captured version that was reviewed (`math_version` or `output_version`).
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<String>,
    pub status: ReviewStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<ReviewRequest>,
}

/// Identifies the frozen section and the exact inputs of one reviewer job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub session: String,
    pub reviewed_version: String,
    pub input_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub key: String,
    #[serde(default)]
    pub title: String,
    pub status: Status,
    /// Content version of the book section when writing started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exercises: BTreeMap<String, Choice>,
    /// `math` / `output` → last requested review.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reviews: BTreeMap<String, Review>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Course {
    pub schema: u32,
    /// Book deck, project-relative.
    pub book: String,
    /// The section being written, or the first adopted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(default, rename = "section")]
    pub sections: Vec<Section>,
}

impl Course {
    pub fn path(lecture: &Path) -> PathBuf {
        lecture.join(FILE)
    }

    pub fn read(lecture: &Path) -> Result<Option<Self>> {
        let path = Self::path(lecture);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("{} を読めない", path.display())),
        };
        let course: Self = toml::from_str(&text)
            .with_context(|| format!("{} を解析できない。自動では直さない", path.display()))?;
        ensure!(course.schema == SCHEMA, "{FILE} の schema {} は未対応", course.schema);
        let mut seen = BTreeSet::new();
        for section in &course.sections {
            ensure!(valid_key(&section.key), "{FILE} の節の鍵 `{}` が不正", section.key);
            ensure!(seen.insert(&section.key), "{FILE} に節の鍵 `{}` が重複している", section.key);
        }
        Ok(Some(course))
    }

    /// Write via a temporary file and rename, so a crash leaves the old or the new file.
    /// Skips the write when the bytes would not change.
    pub fn save(&mut self, lecture: &Path) -> Result<bool> {
        self.next = self
            .sections
            .iter()
            .find(|s| s.status == Status::Writing)
            .or_else(|| self.sections.iter().find(|s| s.status == Status::Adopted))
            .map(|s| s.key.clone());
        let text = toml::to_string(self)?;
        let path = Self::path(lecture);
        if fs::read_to_string(&path).is_ok_and(|old| old == text) {
            return Ok(false);
        }
        let temp = lecture.join(format!(".{FILE}.tmp-{}", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&temp);
            return Err(e).with_context(|| format!("{} を書けない", path.display()));
        }
        if let Ok(dir) = fs::File::open(lecture) {
            let _ = dir.sync_all();
        }
        Ok(true)
    }

    fn section(&self, key: &str) -> Result<usize> {
        self.sections
            .iter()
            .position(|s| s.key == key)
            .with_context(|| format!("節 `{key}` は {FILE} にない"))
    }

    /// Compute the math and output freshness versions for a saved section.
    pub fn review_versions(&self, key: &str, snapshot: &Snapshot) -> Result<(String, String)> {
        current_versions(self, self.section(key)?, snapshot)
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        && !key.starts_with('-')
}

/// Card ID prefix for a section: the first card is `lec-<key>`, extra cards `lec-<key>.<n>`.
/// Keys cannot contain `.`, so one key's cards never match another key.
pub fn card_prefix(key: &str) -> String {
    format!("{CARD_PREFIX}{key}")
}

fn belongs(id: &str, key: &str) -> bool {
    let prefix = card_prefix(key);
    id == prefix || id.strip_prefix(&prefix).is_some_and(|rest| rest.starts_with('.'))
}

/// `from:` value a lecture card must carry.
pub fn from_value(book: &str, key: &str) -> String {
    format!("{book}#{key}")
}

// ---------------------------------------------------------------------------------------
// Captured bytes

#[derive(Deserialize, Default)]
struct Meta {
    id: Option<String>,
    label: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    links: Vec<crate::Link>,
    from: Option<String>,
}

struct CardBytes {
    /// `card.md`, then the card's TeX files, then `\input` targets: (relative name, bytes).
    files: Vec<(String, Vec<u8>)>,
    meta: Meta,
    labels: BTreeSet<String>,
    refs: BTreeSet<String>,
}

/// Bytes of every card in a deck, read once so one version never mixes two states.
pub struct Snapshot {
    cards: BTreeMap<String, CardBytes>,
    deck_files: Vec<(String, Vec<u8>)>,
}

static INPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\(?:input|include|subfile)\s*\{([^}]*)\}").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\label\s*\{([^}]*)\}").unwrap());
static REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\\(?:ref|eqref|cref|Cref|autoref|nameref|vref|pageref)\*?\s*\{([^}]*)\}").unwrap()
});

fn skipped_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "build" | "target" | "node_modules")
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

impl Snapshot {
    pub fn read(root: &Path) -> Result<Self> {
        let root = root.canonicalize().with_context(|| format!("{} を開けない", root.display()))?;
        let mut cards = BTreeMap::new();
        let walker = WalkDir::new(&root).sort_by_file_name().into_iter().filter_entry(|e| {
            e.depth() == 0 || !(e.file_type().is_dir() && skipped_dir(&e.file_name().to_string_lossy()))
        });
        for entry in walker {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type().is_file() || path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let bytes = fs::read(path)?;
            let Ok(text) = std::str::from_utf8(&bytes) else { continue };
            let Some((front, _)) = split_frontmatter(text) else { continue };
            let meta: Meta = serde_yaml::from_str(front)
                .with_context(|| format!("{} の frontmatter を読めない", rel(&root, path)))?;
            let Some(id) = meta.id.clone() else { continue };
            let folder = path.file_name().is_some_and(|n| n == "card.md");
            let mut files = vec![(rel(&root, path), bytes)];
            if folder {
                let dir = path.parent().unwrap();
                let mut tex: Vec<_> = fs::read_dir(dir)?
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "tex"))
                    .collect();
                tex.sort();
                for file in tex {
                    files.push((rel(&root, &file), fs::read(&file)?));
                }
            }
            let (mut labels, mut refs) = (BTreeSet::new(), BTreeSet::new());
            labels.extend(meta.label.clone());
            let dir = path.parent().unwrap().to_owned();
            let mut seen: BTreeSet<String> = files.iter().map(|(n, _)| n.clone()).collect();
            let mut at = 1;
            while at <= files.len() {
                let (name, bytes) = files[at - 1].clone();
                at += 1;
                if !name.ends_with(".tex") {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                for cap in LABEL.captures_iter(&text) {
                    labels.insert(cap[1].trim().to_owned());
                }
                for cap in REF.captures_iter(&text) {
                    refs.extend(cap[1].split(',').map(|s| s.trim().to_owned()));
                }
                if files.len() > 64 {
                    continue;
                }
                for cap in INPUT.captures_iter(&text) {
                    let target = cap[1].trim();
                    match resolve_input(&root, &dir, target) {
                        Some(found) => {
                            let name = rel(&root, &found);
                            if seen.insert(name.clone()) {
                                files.push((name, fs::read(&found)?));
                            }
                        }
                        // A missing target is part of the capture: creating it changes the version.
                        None => {
                            let marker = format!("missing:{target}");
                            if seen.insert(marker.clone()) {
                                files.push((marker, Vec::new()));
                            }
                        }
                    }
                }
            }
            ensure!(
                !cards.contains_key(&id),
                "カード ID `{id}` が重複している: {}",
                rel(&root, path)
            );
            cards.insert(id, CardBytes { files, meta, labels, refs });
        }
        let mut deck_files = Vec::new();
        let mut names = vec![PathBuf::from("deck.toml")];
        if let Ok(cfg) = DeckConfig::read(&root) {
            names.extend(cfg.meta.preamble.clone());
            names.extend(cfg.macros_file.clone());
        }
        for name in names {
            if let Ok(found) = root.join(&name).canonicalize() {
                if found.starts_with(&root) && found.is_file() {
                    deck_files.push((rel(&root, &found), fs::read(&found)?));
                }
            }
        }
        Ok(Self { cards, deck_files })
    }

    pub fn has(&self, id: &str) -> bool {
        self.cards.contains_key(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.cards.keys()
    }

    pub fn from_of(&self, id: &str) -> Option<&str> {
        self.cards.get(id)?.meta.from.as_deref()
    }

    fn card(&self, id: &str) -> Result<&CardBytes> {
        self.cards.get(id).with_context(|| format!("カード `{id}` がない"))
    }

    fn feed(&self, hash: &mut Sha256, ids: &[String]) -> Result<()> {
        for id in ids {
            frame(hash, b"card", id.as_bytes());
            for (name, bytes) in &self.card(id)?.files {
                frame(hash, b"file", name.as_bytes());
                frame(hash, b"bytes", bytes);
            }
        }
        Ok(())
    }

    /// Version of the cards' own bytes, in the given order.
    pub fn content_version(&self, ids: &[String]) -> Result<String> {
        let mut hash = Sha256::new();
        frame(&mut hash, b"kind", b"content");
        self.feed(&mut hash, ids)?;
        Ok(finish(hash))
    }

    /// Content plus every prerequisite outside the section: `requires`-style links and
    /// cards whose labels the section's TeX refers to, transitively.
    pub fn math_version(&self, ids: &[String]) -> Result<String> {
        let own: BTreeSet<&String> = ids.iter().collect();
        let mut owners: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
        for (id, card) in &self.cards {
            for label in &card.labels {
                owners.entry(label).or_default().push(id);
            }
        }
        let mut seen: BTreeSet<String> = ids.iter().cloned().collect();
        let mut queue: Vec<String> = ids.to_vec();
        let mut missing = BTreeSet::new();
        while let Some(id) = queue.pop() {
            let card = self.card(&id)?;
            let mut next: Vec<String> = card.meta.requires.clone();
            next.extend(
                card.meta.links.iter().filter(|l| FOLLOW_RELS.contains(&l.rel.as_str())).map(|l| l.to.clone()),
            );
            for reference in &card.refs {
                match owners.get(reference.as_str()) {
                    Some(found) => next.extend(found.iter().map(|id| (*id).clone())),
                    None => {
                        missing.insert(reference.clone());
                    }
                }
            }
            for id in next {
                if self.cards.contains_key(&id) {
                    if seen.insert(id.clone()) {
                        queue.push(id);
                    }
                } else {
                    missing.insert(format!("card:{id}"));
                }
            }
        }
        let mut hash = Sha256::new();
        frame(&mut hash, b"kind", b"math");
        self.feed(&mut hash, ids)?;
        let prerequisites: Vec<String> = seen.into_iter().filter(|id| !own.contains(id)).collect();
        frame(&mut hash, b"prerequisites", b"");
        self.feed(&mut hash, &prerequisites)?;
        // An unresolved reference is part of the version, so defining it later changes it.
        for name in missing {
            frame(&mut hash, b"unresolved", name.as_bytes());
        }
        Ok(finish(hash))
    }

    /// Content plus the deck preamble and the content of all earlier sections, in order.
    pub fn output_version(&self, ids: &[String], earlier: &[Vec<String>]) -> Result<String> {
        let mut hash = Sha256::new();
        frame(&mut hash, b"kind", b"output");
        self.feed(&mut hash, ids)?;
        for (name, bytes) in &self.deck_files {
            frame(&mut hash, b"deck-file", name.as_bytes());
            frame(&mut hash, b"bytes", bytes);
        }
        for section in earlier {
            frame(&mut hash, b"earlier", self.content_version(section)?.as_bytes());
        }
        Ok(finish(hash))
    }
}

fn resolve_input(root: &Path, dir: &Path, target: &str) -> Option<PathBuf> {
    if target.is_empty() {
        return None;
    }
    for base in [dir, root] {
        for name in [target.to_owned(), format!("{target}.tex")] {
            if let Ok(found) = base.join(&name).canonicalize() {
                if found.is_file() && found.starts_with(root) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn frame(hash: &mut Sha256, tag: &[u8], bytes: &[u8]) {
    hash.update((tag.len() as u64).to_le_bytes());
    hash.update(tag);
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn finish(hash: Sha256) -> String {
    format!("{VERSION_PREFIX}{:x}", hash.finalize())
}

// ---------------------------------------------------------------------------------------
// Book sections

#[derive(Debug, Clone)]
pub struct BookSection {
    pub key: String,
    pub title: String,
    pub cards: Vec<String>,
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

/// Sections of a book deck, from its `structure.yaml` (the format `import_latex_book`
/// writes). A section's key is an explicit `key:`, else its label, else `s<N>-<title>`.
/// Nothing is added to the book.
pub fn book_sections(book: &Path) -> Result<Vec<BookSection>> {
    let structure = DeckConfig::read(book)
        .map(|cfg| cfg.structure)
        .unwrap_or_else(|_| PathBuf::from("structure.yaml"));
    book_sections_from(&book.join(structure))
}

/// Read a book's ordered sections from a caller-resolved structure file.
pub fn book_sections_from(path: &Path) -> Result<Vec<BookSection>> {
    let text = fs::read_to_string(path).with_context(|| format!("{} を読めない", path.display()))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text)?;
    let sections = value["sections"].as_sequence().context("structure.yaml に sections がない")?;
    let mut out: Vec<BookSection> = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        let title = section["title"].as_str().unwrap_or("").to_owned();
        let base = section["key"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| section["label"].as_str().map(|l| slug(l, 60)).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| {
                let tail = slug(&title, 24);
                if tail.is_empty() { format!("s{}", index + 1) } else { format!("s{}-{tail}", index + 1) }
            });
        ensure!(valid_key(&base), "節の鍵 `{base}` が不正（英数字、-、_ だけ）");
        let mut key = base.clone();
        for n in 2.. {
            if !out.iter().any(|s| s.key == key) {
                break;
            }
            key = format!("{base}-{n}");
        }
        let mut cards = Vec::new();
        for item in section["items"].as_sequence().into_iter().flatten() {
            let id = item.as_str().or_else(|| item["proof"].as_str());
            if let Some(id) = id {
                if !cards.iter().any(|c| c == id) {
                    cards.push(id.to_owned());
                }
            }
        }
        out.push(BookSection { key, title, cards });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Operations

/// What a caller asks `plan` to do to one section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub key: String,
    pub status: Option<Status>,
    #[serde(default)]
    pub exercises: BTreeMap<String, Choice>,
}

#[derive(Debug, Serialize)]
pub struct SectionReport {
    pub key: String,
    pub title: String,
    pub status: Status,
    pub cards: Vec<String>,
    /// The book section changed since writing started (state is not moved).
    pub stale: bool,
    /// Reviews whose recorded version no longer matches the lecture cards.
    pub stale_reviews: Vec<String>,
    /// In `course.toml` but no longer in the book.
    pub missing_in_book: bool,
}

#[derive(Debug, Serialize)]
pub struct PlanReport {
    pub book: String,
    pub next: Option<String>,
    pub changed: bool,
    pub sections: Vec<SectionReport>,
}

pub fn plan(
    project: &Path,
    lecture: &Path,
    book: Option<&str>,
    decisions: &[Decision],
) -> Result<PlanReport> {
    plan_with_review_check(project, lecture, book, decisions, |_, _, _| true)
}

/// Apply a plan with an additional freshness check for captured reviewer inputs.
pub fn plan_with_review_check(
    project: &Path,
    lecture: &Path,
    book: Option<&str>,
    decisions: &[Decision],
    mut receipt_current: impl FnMut(&Section, &str, &Review) -> bool,
) -> Result<PlanReport> {
    let mut course = match Course::read(lecture)? {
        Some(course) => {
            if let Some(book) = book {
                ensure!(book == course.book, "この講義の原本は `{}`。別の本には変えない", course.book);
            }
            course
        }
        None => {
            let book = book.context("最初の course_plan には book（原本デッキ）を指定する")?;
            Course { schema: SCHEMA, book: book.to_owned(), next: None, sections: Vec::new() }
        }
    };
    let book_root = project.join(&course.book);
    ensure!(book_root.join("deck.toml").is_file() || book_root.join("structure.yaml").is_file(),
        "原本 `{}` がデッキではない", course.book);
    ensure!(
        book_root.canonicalize()? != lecture.canonicalize()?,
        "講義デッキと原本デッキは別にする"
    );
    let book_sections = book_sections(&book_root)?;
    // New book sections join as `todo`; existing entries are never touched by a refresh.
    for section in &book_sections {
        if !course.sections.iter().any(|s| s.key == section.key) {
            course.sections.push(Section {
                key: section.key.clone(),
                title: section.title.clone(),
                status: Status::Todo,
                book_version: None,
                cards: Vec::new(),
                exercises: BTreeMap::new(),
                reviews: BTreeMap::new(),
            });
        }
    }
    let lecture_snapshot = Snapshot::read(lecture)?;
    // Both the done guard and the report use the same freshness check.
    let mut stale_by_section: BTreeMap<_, _> = course.sections.iter().map(|section| {
        let stale = stale_reviews(&course, section, &lecture_snapshot, &mut receipt_current);
        (section.key.clone(), stale)
    }).collect();
    for decision in decisions {
        let at = course.section(&decision.key)?;
        let current = course.sections[at].status;
        if let Some(target) = decision.status {
            if target != current {
                use Status::*;
                match (current, target) {
                    (Todo | Adopted | Skipped, Todo | Adopted | Skipped) => {}
                    (Written, Done) => check_done(&course.sections[at], &stale_by_section[&decision.key])?,
                    _ => bail!(
                        "節 `{}` は {current:?} から {target:?} へは変えられない（書き始めた節は course_next / course_commit で進める）",
                        decision.key
                    ),
                }
                course.sections[at].status = target;
            } else if target == Status::Done {
                check_done(&course.sections[at], &stale_by_section[&decision.key])?;
            }
        }
        if !decision.exercises.is_empty() {
            ensure!(
                matches!(course.sections[at].status, Status::Todo | Status::Adopted | Status::Skipped),
                "書き始めた節 `{}` の演習の採否は変えない", decision.key
            );
            course.sections[at].exercises.extend(decision.exercises.clone());
        }
    }
    let changed = course.save(lecture)?;
    let book_snapshot = Snapshot::read(&book_root)?;
    let sections = course
        .sections
        .iter()
        .map(|section| {
            let listed = book_sections.iter().find(|b| b.key == section.key);
            let stale = match (&section.book_version, listed) {
                (Some(old), Some(listed)) => {
                    book_snapshot.content_version(&listed.cards).map_or(true, |now| &now != old)
                }
                _ => false,
            };
            SectionReport {
                key: section.key.clone(),
                title: section.title.clone(),
                status: section.status,
                cards: section.cards.clone(),
                stale,
                stale_reviews: stale_by_section.remove(&section.key).unwrap_or_default(),
                missing_in_book: listed.is_none(),
            }
        })
        .collect();
    Ok(PlanReport { book: course.book.clone(), next: course.next.clone(), changed, sections })
}

fn current_versions(
    course: &Course,
    at: usize,
    snapshot: &Snapshot,
) -> Result<(String, String)> {
    let ids = &course.sections[at].cards;
    let earlier: Vec<Vec<String>> = course.sections[..at]
        .iter()
        .filter(|s| matches!(s.status, Status::Written | Status::Done))
        .map(|s| s.cards.clone())
        .collect();
    Ok((snapshot.math_version(ids)?, snapshot.output_version(ids, &earlier)?))
}

fn stale_reviews(
    course: &Course,
    section: &Section,
    snapshot: &Snapshot,
    receipt_current: &mut impl FnMut(&Section, &str, &Review) -> bool,
) -> Vec<String> {
    let Ok(at) = course.section(&section.key) else { return Vec::new() };
    if section.reviews.is_empty() {
        return Vec::new();
    }
    let Ok((math, output)) = current_versions(course, at, snapshot) else {
        return section.reviews.keys().cloned().collect();
    };
    section
        .reviews
        .iter()
        .filter(|(kind, review)| {
            let current = match kind.as_str() {
                "math" => &math,
                "output" => &output,
                _ => return true,
            };
            review.version != *current
                || (review.status != ReviewStatus::Pending
                    && review.request.is_some()
                    && !receipt_current(section, kind, review))
        })
        .map(|(kind, _)| kind.clone())
        .collect()
}

fn check_done(section: &Section, stale: &[String]) -> Result<()> {
    for kind in ["math", "output"] {
        let review = section
            .reviews
            .get(kind)
            .with_context(|| format!("節 `{}` は {kind} の確認がまだなので done にできない", section.key))?;
        ensure!(
            !stale.iter().any(|name| name == kind),
            "節 `{}` の {kind} の確認は古い（確認後にカードや前提が変わった）。確認し直す",
            section.key
        );
        ensure!(
            review.status == ReviewStatus::Clean,
            "節 `{}` の {kind} の確認は clean ではない", section.key
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Next {
    /// `None` when no adopted section is left.
    pub section: Option<NextSection>,
}

#[derive(Debug, Serialize)]
pub struct NextSection {
    pub key: String,
    pub title: String,
    pub book: String,
    /// The `from:` value every lecture card carries.
    pub from: String,
    pub card_prefix: String,
    pub book_cards: Vec<String>,
    pub book_version: String,
    pub exercises: BTreeMap<String, Choice>,
    /// Lecture cards of this section already on disk (progress kept across a restart).
    pub existing_cards: Vec<String>,
    pub resumed: bool,
}

/// Problems that make the saved state unreliable. `next` stops on them and fixes nothing.
fn inconsistencies(course: &Course, lecture: &Snapshot) -> Vec<String> {
    let mut problems = Vec::new();
    let writing: Vec<_> = course.sections.iter().filter(|s| s.status == Status::Writing).collect();
    if writing.len() > 1 {
        problems.push(format!(
            "書き込み中の節が複数ある: {}",
            writing.iter().map(|s| s.key.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    for section in &course.sections {
        if matches!(section.status, Status::Written | Status::Done) {
            if section.cards.is_empty() {
                problems.push(format!("節 `{}` は {:?} なのに cards が空", section.key, section.status));
            }
            for id in &section.cards {
                if !lecture.has(id) {
                    problems.push(format!("節 `{}` のカード `{id}` がない", section.key));
                }
            }
            for id in lecture.ids().filter(|id| belongs(id, &section.key)) {
                if !section.cards.contains(id) {
                    problems.push(format!("節 `{}` の cards にないカード `{id}` が残っている", section.key));
                }
            }
        }
    }
    problems
}

pub fn next(project: &Path, lecture: &Path) -> Result<Next> {
    let mut course = Course::read(lecture)?.context("course.toml がない。先に course_plan で計画を作る")?;
    let snapshot = Snapshot::read(lecture)?;
    let problems = inconsistencies(&course, &snapshot);
    ensure!(
        problems.is_empty(),
        "{FILE} が不整合。自動では直さない:\n- {}",
        problems.join("\n- ")
    );
    let (at, resumed) = match course.sections.iter().position(|s| s.status == Status::Writing) {
        Some(at) => (at, true),
        None => match course.sections.iter().position(|s| s.status == Status::Adopted) {
            Some(at) => (at, false),
            None => return Ok(Next { section: None }),
        },
    };
    let key = course.sections[at].key.clone();
    let book_root = project.join(&course.book);
    let listed = book_sections(&book_root)?
        .into_iter()
        .find(|s| s.key == key)
        .with_context(|| format!("節 `{key}` は原本 `{}` にもうない", course.book))?;
    if !resumed {
        // Capture once, when writing starts; a resumed run keeps the original capture.
        let version = Snapshot::read(&book_root)?.content_version(&listed.cards)?;
        let section = &mut course.sections[at];
        section.status = Status::Writing;
        section.book_version = Some(version);
        section.title = listed.title.clone();
        course.save(lecture)?;
    }
    let section = &course.sections[at];
    Ok(Next {
        section: Some(NextSection {
            key: key.clone(),
            title: listed.title,
            book: course.book.clone(),
            from: from_value(&course.book, &key),
            card_prefix: card_prefix(&key),
            book_cards: listed.cards,
            book_version: section.book_version.clone().unwrap_or_default(),
            exercises: section.exercises.clone(),
            existing_cards: snapshot.ids().filter(|id| belongs(id, &key)).cloned().collect(),
            resumed,
        }),
    })
}

#[derive(Debug, Serialize)]
pub struct Committed {
    pub key: String,
    pub status: Status,
    pub cards: Vec<String>,
    /// False when the same commit had already been recorded.
    pub changed: bool,
}

/// Record a finished section. `check` runs on the cards' IDs after the structure checks
/// (the caller passes the style check). Sending the same commit again changes nothing.
pub fn commit(
    lecture: &Path,
    key: &str,
    cards: &[String],
    check: impl FnOnce(&[String]) -> Result<()>,
) -> Result<Committed> {
    let mut course = Course::read(lecture)?.context("course.toml がない。先に course_plan で計画を作る")?;
    let at = course.section(key)?;
    let mut wanted: Vec<String> = cards.to_vec();
    wanted.sort();
    wanted.dedup();
    let section = &course.sections[at];
    match section.status {
        Status::Written | Status::Done => {
            let mut have = section.cards.clone();
            have.sort();
            ensure!(
                have == wanted,
                "節 `{key}` は別のカード一覧で記録済み: {}",
                section.cards.join(", ")
            );
            return Ok(Committed { key: key.into(), status: section.status, cards: section.cards.clone(), changed: false });
        }
        Status::Writing => {}
        other => bail!("節 `{key}` は {other:?}。course_next で書き始めてから commit する"),
    }
    ensure!(!wanted.is_empty(), "cards が空");
    let snapshot = Snapshot::read(lecture)?;
    let from = from_value(&course.book, key);
    for id in &wanted {
        ensure!(belongs(id, key), "カード `{id}` の ID は `{}` か `{}.<n>` にする", card_prefix(key), card_prefix(key));
        ensure!(snapshot.has(id), "カード `{id}` がない。書き終えてから commit する");
        ensure!(
            snapshot.from_of(id) == Some(from.as_str()),
            "カード `{id}` の from を `{from}` にする"
        );
    }
    let extra: Vec<_> = snapshot.ids().filter(|id| belongs(id, key) && !wanted.contains(id)).collect();
    ensure!(extra.is_empty(), "節 `{key}` のカードが cards に入っていない: {}",
        extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    check(&wanted)?;
    let section = &mut course.sections[at];
    section.status = Status::Written;
    section.cards = wanted.clone();
    course.save(lecture)?;
    Ok(Committed { key: key.into(), status: Status::Written, cards: wanted, changed: true })
}

#[cfg(test)]
mod tests;
