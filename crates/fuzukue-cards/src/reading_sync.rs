//! Read-only comparison of reading cards and recorded manuscript sources.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{latex_book, workspace_paths, DeckConfig, Diagnostic, Graph};

pub(crate) struct SourceRange {
    pub path: PathBuf,
    pub first: usize,
    pub last: usize,
}

impl SourceRange {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (path, lines) = text.rsplit_once(':')?;
        let (first, last) = lines.split_once('-')?;
        Some(Self {
            path: path.into(),
            first: first.parse().ok()?,
            last: last.parse().ok()?,
        })
    }
}

impl fmt::Display for SourceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}-{}", self.path.display(), self.first, self.last)
    }
}

pub(crate) struct SourceFile {
    bytes: Vec<u8>,
    line_starts: Vec<usize>,
}

impl SourceFile {
    pub(crate) fn read(path: &Path) -> std::io::Result<Self> {
        ensure_regular_file(path)?;
        let bytes = fs::read(path)?;
        let mut line_starts = vec![0];
        line_starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter_map(|(i, &b)| (b == b'\n').then_some(i + 1)),
        );
        if line_starts.last() != Some(&bytes.len()) {
            line_starts.push(bytes.len());
        }
        Ok(Self { bytes, line_starts })
    }

    pub(crate) fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }

    pub(crate) fn range_hash(&self, range: &SourceRange) -> Option<String> {
        let first = range.first.checked_sub(1)?;
        if range.first > range.last {
            return None;
        }
        let bytes =
            &self.bytes[*self.line_starts.get(first)?..*self.line_starts.get(range.last)?];
        Some(format!("{:x}", Sha256::digest(bytes)))
    }
}

fn ensure_regular_file(path: &Path) -> std::io::Result<()> {
    if fs::metadata(path)?.is_file() {
        return Ok(());
    }
    Err(std::io::Error::new(
        ErrorKind::InvalidInput,
        "expected a regular file",
    ))
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub ok: bool,
    pub checked_ranges: usize,
    pub checked_files: usize,
    pub stale_cards: BTreeMap<String, Vec<Issue>>,
    pub untracked_cards: BTreeMap<String, Vec<String>>,
    pub stale_files: Vec<Issue>,
    pub uncovered_theorems: Vec<Theorem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub card_diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct Issue {
    source: String,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Theorem {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) source: String,
}

/// Compare recorded source hashes with current files inside the workspace.
pub fn check(workspace: &Path, deck: &Path) -> Result<Report> {
    let workspace = workspace.canonicalize()?;
    let deck = workspace_paths::resolve(&workspace, &workspace, &deck.canonicalize()?)?;
    let cfg = DeckConfig::read_in_workspace(&deck, &workspace)?;
    let graph = Graph::load_cards_scoped(&deck, &cfg.dirs, Some(&workspace))?;
    let mut report = Report {
        card_diagnostics: graph.diagnostics,
        ..Report::default()
    };
    let main = cfg
        .meta
        .manuscript_main
        .as_deref()
        .map(|path| workspace_paths::resolve(&workspace, &deck, path))
        .transpose()?;
    let source_root = main.as_deref().and_then(Path::parent).unwrap_or(&deck);
    let mut files = BTreeMap::new();
    for (id, card) in &graph.cards {
        let locations: BTreeSet<_> = card
            .source
            .iter()
            .filter(|source| SourceRange::parse(source).is_some())
            .chain(&card.additional_sources)
            .chain(&card.proof_sources)
            .chain(card.source_sha256.keys())
            .collect();
        for source in locations {
            let Some(expected) = card.source_sha256.get(source) else {
                report
                    .untracked_cards
                    .entry(id.clone())
                    .or_default()
                    .push(source.clone());
                continue;
            };
            report.checked_ranges += 1;
            if let Err(reason) =
                compare_range(&workspace, source_root, source, expected, &mut files)
            {
                report
                    .stale_cards
                    .entry(id.clone())
                    .or_default()
                    .push(Issue {
                        source: source.clone(),
                        reason,
                    });
            }
        }
    }
    check_manifest(&workspace, &deck, &mut report)?;
    if let Some(main) = main {
        match latex_book::uncovered_theorems(&main, &workspace, &graph.cards) {
            Ok(theorems) => report.uncovered_theorems = theorems,
            Err(error) => report.coverage_error = Some(format!("{error:#}")),
        }
    }
    report.ok = report.checked_ranges + report.checked_files > 0
        && report.stale_cards.is_empty()
        && report.untracked_cards.is_empty()
        && report.stale_files.is_empty()
        && report.uncovered_theorems.is_empty()
        && report.coverage_error.is_none()
        && report.card_diagnostics.is_empty();
    Ok(report)
}

fn compare_range(
    workspace: &Path,
    root: &Path,
    source: &str,
    expected: &str,
    files: &mut BTreeMap<PathBuf, std::io::Result<SourceFile>>,
) -> Result<(), &'static str> {
    if !valid_hash(expected) {
        return Err("invalid_hash");
    }
    let range = SourceRange::parse(source).ok_or("invalid_range")?;
    if range.path.as_os_str().is_empty() {
        return Err("invalid_path");
    }
    let path = workspace_paths::resolve(workspace, workspace, &root.join(&range.path))
        .map_err(|_| "invalid_path")?;
    let file = files
        .entry(path.clone())
        .or_insert_with(|| SourceFile::read(&path))
        .as_ref()
        .map_err(read_error)?;
    let actual = file.range_hash(&range).ok_or("invalid_range")?;
    compare_hash(&actual, expected)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn compare_hash(actual: &str, expected: &str) -> Result<(), &'static str> {
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err("changed")
    }
}

fn read_error(error: &std::io::Error) -> &'static str {
    if error.kind() == ErrorKind::NotFound {
        "missing"
    } else {
        "unreadable"
    }
}

fn check_manifest(workspace: &Path, deck: &Path, report: &mut Report) -> Result<()> {
    let manifest = workspace_paths::resolve(workspace, deck, Path::new("source-sha256.txt"))?;
    let text = match ensure_regular_file(&manifest).and_then(|_| fs::read_to_string(&manifest)) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", manifest.display()))
        }
    };
    for (index, line) in text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let (hash, source) = line
            .split_once(' ')
            .filter(|(hash, _)| valid_hash(hash))
            .and_then(|(hash, rest)| rest.strip_prefix([' ', '*']).map(|source| (hash, source)))
            .filter(|(_, source)| !source.is_empty())
            .with_context(|| {
                format!(
                    "invalid SHA-256 entry in {} at line {}",
                    manifest.display(),
                    index + 1
                )
            })?;
        let result = workspace_paths::resolve(workspace, workspace, Path::new(source))
            .map_err(|_| "invalid_path")
            .and_then(|path| {
                ensure_regular_file(&path)
                    .and_then(|_| fs::read(path))
                    .map_err(|error| read_error(&error))
            })
            .and_then(|bytes| compare_hash(&format!("{:x}", Sha256::digest(bytes)), hash));
        report.checked_files += 1;
        if let Err(reason) = result {
            report.stale_files.push(Issue {
                source: source.into(),
                reason,
            });
        }
    }
    Ok(())
}
