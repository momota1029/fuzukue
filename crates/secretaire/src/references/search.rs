use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use super::{
    cache_dir, cache_part, checked_file, directory, manifest, required_root, stamp, Location,
};

pub(crate) const DEFAULT_LIMIT: usize = 8;
pub(crate) const MAX_LIMIT: usize = 50;

static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{Latin}\p{M}0-9_]+").unwrap());
static JAPANESE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Hiragana}\p{Katakana}\p{Han}ー]+").unwrap());

#[derive(Serialize)]
struct Hit<'a> {
    key: &'a str,
    file: &'a str,
    #[serde(flatten)]
    location: Location,
    score: f64,
    snippet: String,
}

pub(crate) fn search(project: &Path, query: &str, limit: usize) -> Result<Value> {
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "limit は 1〜{MAX_LIMIT} で指定する"
    );
    let query = Query::new(query)?;

    let root = required_root(project)?;
    let Some(corpus) = directory(&root, ".corpus", false)? else {
        return Ok(json!({ "hits": [] }));
    };
    let manifest = manifest(&corpus)?;
    let mut hits = Vec::new();
    let mut document_freq = vec![0; query.len()];
    let mut documents = 0;
    for (key, files) in &manifest.entries {
        let Some(dir) = directory(&root, key, false)? else {
            continue;
        };
        for (file, cached) in files {
            let current = checked_file(&dir, file).and_then(|path| stamp(&path));
            if !current.is_ok_and(|stamp| stamp == cached.stamp) {
                continue;
            }
            let out = cache_dir(&corpus, key, cached.id)?.context("抽出ファイルがない")?;
            for part in 1..=cached.parts {
                let text = fs::read_to_string(cache_part(&out, part)?)?;
                documents += 1;
                if let Some((freq, first)) = query.scan(&text) {
                    count_document(&mut document_freq, &freq);
                    hits.push((
                        Hit {
                            key,
                            file,
                            location: cached.location(part),
                            score: 0.0,
                            snippet: snippet(&text, first),
                        },
                        freq,
                    ));
                }
            }
        }
    }
    for (hit, freq) in &mut hits {
        hit.score = score(freq, &document_freq, documents);
    }
    hits.sort_by(|(a, _), (b, _)| b.score.total_cmp(&a.score));
    let hits: Vec<_> = hits.into_iter().take(limit).map(|(hit, _)| hit).collect();
    Ok(json!({ "hits": hits }))
}

pub(crate) fn snippet(text: &str, first: usize) -> String {
    excerpt(text, first).0
}

fn excerpt(text: &str, first: usize) -> (String, Range<usize>, usize) {
    let start = text[..first]
        .char_indices()
        .rev()
        .nth(99)
        .map_or(0, |(i, _)| i);
    let end = text[start..]
        .char_indices()
        .nth(300)
        .map_or(text.len(), |(i, _)| start + i);
    let before = if start > 0 { "…" } else { "" };
    let after = if end < text.len() { "…" } else { "" };
    let part = &text[start..end];
    let trimmed_start = end - part.trim_start().len();
    let trimmed = part.trim();
    (
        format!("{before}{trimmed}{after}"),
        trimmed_start..trimmed_start + trimmed.len(),
        before.len(),
    )
}

pub(crate) struct Query<'a> {
    words: BTreeSet<String>,
    phrases: BTreeSet<&'a str>,
}

pub(crate) type Words = HashMap<String, (usize, usize)>;

pub(crate) fn index_words(text: &str) -> Words {
    let mut words = Words::new();
    for word in WORDS.find_iter(text) {
        words
            .entry(word.as_str().to_lowercase())
            .or_insert((0, word.start()))
            .0 += 1;
    }
    words
}

impl<'a> Query<'a> {
    pub(crate) fn new(query: &'a str) -> Result<Self> {
        ensure!(
            query.len() <= 1024,
            harness_text!(
                "Use at most 1024 UTF-8 bytes for query.",
                "query は UTF-8 で 1024 バイト以内で指定する"
            )
        );
        let query = Self {
            words: WORDS
                .find_iter(query)
                .map(|m| m.as_str().to_lowercase())
                .collect(),
            phrases: JAPANESE.find_iter(query).map(|m| m.as_str()).collect(),
        };
        ensure!(
            query.len() > 0,
            harness_text!("Provide a search term.", "query に検索する語を指定する")
        );
        Ok(query)
    }

    pub(crate) fn len(&self) -> usize {
        self.words.len() + self.phrases.len()
    }

    pub(crate) fn matches(&self, text: &str, words: &Words) -> Option<(Vec<usize>, usize)> {
        let mut first = text.len();
        let mut freq = Vec::with_capacity(self.len());
        for word in &self.words {
            let (count, start) = words.get(word).copied().unwrap_or((0, text.len()));
            first = first.min(start);
            freq.push(count);
        }
        for phrase in &self.phrases {
            let mut matches = text.match_indices(phrase);
            if let Some((start, _)) = matches.next() {
                first = first.min(start);
                freq.push(1 + matches.count());
            } else {
                freq.push(0);
            }
        }
        (first < text.len()).then_some((freq, first))
    }

    pub(crate) fn scan(&self, text: &str) -> Option<(Vec<usize>, usize)> {
        let mut words = Words::new();
        if !self.words.is_empty() {
            for word in WORDS.find_iter(text) {
                let term = word.as_str().to_lowercase();
                if self.words.contains(&term) {
                    words.entry(term).or_insert((0, word.start())).0 += 1;
                }
            }
        }
        self.matches(text, &words)
    }

    pub(crate) fn highlighted_snippet(
        &self,
        text: &str,
        first: usize,
    ) -> (String, Vec<[usize; 2]>) {
        let (snippet, range, prefix) = excerpt(text, first);
        let matches = WORDS
            .find_iter(text)
            .take_while(|m| m.start() < range.end)
            .filter(|m| m.end() > range.start && self.words.contains(&m.as_str().to_lowercase()))
            .map(|m| m.range())
            .chain(self.phrases.iter().flat_map(|phrase| {
                text.match_indices(phrase)
                    .take_while(|(start, _)| *start < range.end)
                    .map(|(start, term)| start..start + term.len())
            }));
        let mut highlights = matches
            .filter_map(|m| {
                let start = m.start.max(range.start);
                let end = m.end.min(range.end);
                (start < end).then(|| [prefix + start - range.start, prefix + end - range.start])
            })
            .collect::<Vec<_>>();
        highlights.sort_unstable();
        let mut merged: Vec<[usize; 2]> = Vec::new();
        for [start, end] in highlights {
            match merged.last_mut() {
                Some(last) if start <= last[1] => last[1] = last[1].max(end),
                _ => merged.push([start, end]),
            }
        }
        (snippet, merged)
    }
}

pub(crate) fn count_document(document_freq: &mut [usize], freq: &[usize]) {
    for (df, &count) in document_freq.iter_mut().zip(freq) {
        *df += usize::from(count > 0);
    }
}

pub(crate) fn score(freq: &[usize], document_freq: &[usize], documents: usize) -> f64 {
    freq.iter()
        .zip(document_freq)
        .filter(|(&tf, _)| tf > 0)
        .map(|(&tf, &df)| {
            let idf = 1.0 + ((documents + 1) as f64 / (df + 1) as f64).ln();
            (1.0 + (tf as f64).ln()) * idf
        })
        .sum()
}
