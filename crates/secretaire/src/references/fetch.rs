//! Search open bibliographic services and import a selected reference.

pub(super) mod sources;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

use self::sources::{Http, NETWORK};
use super::{directory, entries, refresh, write_atomic};
use crate::Workspace;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    Zbmath,
    Arxiv,
    Crossref,
    #[default]
    All,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Zbmath,
    Arxiv,
    Doi,
    Mr,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Zbmath, Self::Arxiv, Self::Doi, Self::Mr];

    fn name(self) -> &'static str {
        match self {
            Self::Zbmath => "zbmath",
            Self::Arxiv => "arxiv",
            Self::Doi => "doi",
            Self::Mr => "mr",
        }
    }

    fn normalize(self, value: &str) -> Option<String> {
        static ZB: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^(?:zbl\s+)?([0-9]+(?:\.[0-9]+)?)$").unwrap());
        static ARXIV: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)^(?:arxiv:)?((?:[a-z]+(?:[.-][a-z]+)*/[0-9]{7}|[0-9]{4}\.[0-9]{4,5})(?:v[1-9][0-9]*)?)$").unwrap()
        });
        static DOI: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^10\.[0-9]+/[^\s\p{Cc}]+$").unwrap());
        static MR: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^(?:mr)?([0-9]{1,7})$").unwrap());
        let value = value.trim();
        match self {
            Self::Zbmath => Some(ZB.captures(value)?[1].into()),
            Self::Arxiv => Some(ARXIV.captures(value)?[1].to_ascii_lowercase()),
            Self::Doi => DOI.is_match(value).then(|| value.to_ascii_lowercase()),
            Self::Mr => Some(format!("MR{:0>7}", &MR.captures(value)?[1])),
        }
    }

    fn id(self, value: &str) -> String {
        format!("{}:{value}", self.name())
    }
}

struct Identifier {
    kind: Kind,
    value: String,
}

impl Identifier {
    fn parse(input: &str) -> Result<Self> {
        ensure!(input.len() <= 1024, "id は 1024 バイト以内で指定する");
        let (prefix, value) = input
            .trim()
            .split_once(':')
            .context("id に出典の接頭辞がない")?;
        let kind = Kind::ALL
            .into_iter()
            .find(|kind| prefix.eq_ignore_ascii_case(kind.name()))
            .context("id の接頭辞は arXiv、doi、zbmath、mr のいずれかを指定する")?;
        let value = kind.normalize(value).context("id の形式が不正")?;
        Ok(Self { kind, value })
    }
}

#[derive(Default, Serialize)]
struct Ids {
    #[serde(skip_serializing_if = "Option::is_none")]
    zbmath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zbmath_record_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arxiv: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mr: Option<String>,
}

impl Ids {
    fn get(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::Zbmath => self.zbmath.as_deref(),
            Kind::Arxiv => self.arxiv.as_deref(),
            Kind::Doi => self.doi.as_deref(),
            Kind::Mr => self.mr.as_deref(),
        }
    }
}

#[derive(Default)]
struct Paper {
    title: Option<String>,
    authors: Vec<String>,
    year: Option<u32>,
    venue: Option<String>,
    ids: Ids,
    urls: Vec<String>,
    open_pdf: bool,
    journal_article: bool,
}

pub(crate) async fn find(query: String, source: Source, limit: usize) -> Result<Value> {
    tokio::task::spawn_blocking(move || find_with(&NETWORK, &query, source, limit)).await?
}

fn find_with(http: &impl Http, query: &str, source: Source, limit: usize) -> Result<Value> {
    let query = query.trim();
    ensure!(
        !query.is_empty() && query.len() <= 1024,
        "query は 1〜1024 バイトで指定する"
    );
    ensure!(
        (1..=super::MAX_LIMIT).contains(&limit),
        "limit は 1〜{} で指定する",
        super::MAX_LIMIT
    );
    let sources = match source {
        Source::All => &[Source::Zbmath, Source::Arxiv, Source::Crossref][..],
        _ => std::slice::from_ref(&source),
    };
    let mut results = Vec::new();
    let mut errors = BTreeMap::new();
    for &source in sources {
        match sources::search(http, source, query, limit) {
            Ok(papers) => results.push((source, VecDeque::from(papers))),
            Err(error) if sources.len() == 1 => return Err(error),
            Err(error) => {
                errors.insert(
                    format!("{source:?}").to_ascii_lowercase(),
                    format!("{error:#}"),
                );
            }
        }
    }
    ensure!(!results.is_empty(), "書誌検索に失敗: {errors:?}");
    let mut candidates = Vec::new();
    while candidates.len() < limit {
        let before = candidates.len();
        for (source, papers) in &mut results {
            if let Some(paper) = papers.pop_front() {
                let kind = match source {
                    Source::Zbmath => Kind::Zbmath,
                    Source::Arxiv => Kind::Arxiv,
                    Source::Crossref => Kind::Doi,
                    Source::All => unreachable!(),
                };
                candidates.push(json!({
                    "source": source,
                    "id": paper.ids.get(kind).map(|id| kind.id(id)),
                    "title": paper.title,
                    "authors": paper.authors,
                    "year": paper.year,
                    "venue": paper.venue,
                    "ids": paper.ids,
                    "open_pdf": paper.open_pdf,
                }));
            }
            if candidates.len() == limit {
                break;
            }
        }
        if candidates.len() == before {
            break;
        }
    }
    Ok(json!({ "candidates": candidates, "errors": errors }))
}

pub(crate) async fn fetch(app: &Arc<Workspace>, id: &str, key: Option<String>) -> Result<Value> {
    if let Some(key) = &key {
        validate_key(key)?;
    }
    let id = Identifier::parse(id)?;
    let fetched = tokio::task::spawn_blocking(move || download(&NETWORK, &id)).await??;
    super::run(app, move |project| store(project, fetched, key.as_deref())).await
}

pub(super) fn validate_key(key: &str) -> Result<()> {
    ensure!(
        key.len() <= 128
            && key.starts_with(|c: char| c.is_ascii_alphanumeric())
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "資料のキーが不正: {key}"
    );
    Ok(())
}

struct Download {
    paper: Paper,
    source: Option<Source>,
    pdf: Option<(String, Vec<u8>)>,
    warning: Option<String>,
}

fn download(http: &impl Http, id: &Identifier) -> Result<Download> {
    let (mut paper, source, mut warning) = sources::lookup(http, id)?;
    let pdf = if paper.open_pdf {
        let arxiv = paper.ids.arxiv.as_deref().context("arXiv ID がない")?;
        let url = sources::arxiv_url("pdf", arxiv);
        if !paper.urls.contains(&url) {
            paper.urls.push(url.clone());
        }
        match sources::pdf(http, &url) {
            Ok(bytes) => Some((format!("arxiv-{}.pdf", arxiv.replace('/', "-")), bytes)),
            Err(e) => {
                warning = Some(format!("arXiv PDF の取得に失敗: {e:#}"));
                None
            }
        }
    } else {
        None
    };
    Ok(Download {
        paper,
        source,
        pdf,
        warning,
    })
}

fn arxiv_work(value: &str) -> &str {
    match value.rsplit_once('v') {
        Some((work, version)) if version.bytes().all(|b| b.is_ascii_digit()) => work,
        _ => value,
    }
}

fn matches_ids(table: &toml::Table, ids: &Ids) -> bool {
    let zbmath_matches = ["zbmath", "zbmath_record_id"].into_iter().any(|field| {
        table
            .get(field)
            .and_then(toml::Value::as_str)
            .and_then(|value| Kind::Zbmath.normalize(value))
            .is_some_and(|value| {
                [ids.zbmath.as_ref(), ids.zbmath_record_id.as_ref()].contains(&Some(&value))
            })
    });
    if zbmath_matches {
        return true;
    }
    Kind::ALL.into_iter().any(|kind| {
        let Some((stored, incoming)) = table
            .get(kind.name())
            .and_then(toml::Value::as_str)
            .and_then(|value| kind.normalize(value))
            .zip(ids.get(kind))
        else {
            return false;
        };
        match kind {
            Kind::Arxiv => arxiv_work(&stored) == arxiv_work(incoming),
            _ => stored == incoming,
        }
    })
}

fn key_base(paper: &Paper) -> String {
    let surname = paper.authors.first().and_then(|name| {
        name.split_once(',')
            .map(|(last, _)| last)
            .or_else(|| name.split_whitespace().last())
    });
    let key = |value: &str| -> String {
        value
            .nfkd()
            .filter(char::is_ascii_alphanumeric)
            .take(80)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    if let Some((surname, year)) = surname
        .map(key)
        .filter(|key| !key.is_empty())
        .zip(paper.year)
    {
        return format!("{surname}{year}");
    }
    Kind::ALL
        .into_iter()
        .find_map(|kind| {
            paper.ids.get(kind).map(|value| match kind {
                Kind::Mr => key(value),
                _ => key(&kind.id(value)),
            })
        })
        .unwrap_or_else(|| "reference".into())
}

fn read_existing(dir: &Path) -> Option<toml::Table> {
    let path = dir.join("ref.toml");
    if !fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    toml::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn select_key(
    root: &Path,
    paper: &Paper,
    requested: Option<&str>,
) -> Result<(String, Option<toml::Table>)> {
    if let Some(key) = requested {
        validate_key(key)?;
        return match directory(root, key, false)? {
            Some(dir) => {
                let metadata = read_existing(&dir)
                    .filter(|table| matches_ids(table, &paper.ids))
                    .with_context(|| format!("{key} に同じ識別子を持つ ref.toml がない"))?;
                Ok((key.into(), Some(metadata)))
            }
            None => Ok((key.into(), None)),
        };
    }
    for (key, dir) in entries(root)? {
        if let Some(table) = read_existing(&dir).filter(|table| matches_ids(table, &paper.ids)) {
            return Ok((key, Some(table)));
        }
    }
    let base = key_base(paper);
    for suffix in 1.. {
        let key = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        match fs::symlink_metadata(root.join(&key)) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok((key, None)),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
    }
    unreachable!()
}

fn store(project: &Path, fetched: Download, key: Option<&str>) -> Result<Value> {
    if let Some(key) = key {
        validate_key(key)?;
    }
    let root = directory(project, "references", true)?.context("references/ がない")?;
    let Download {
        paper,
        source,
        pdf,
        warning,
    } = fetched;
    let (key, existing) = select_key(&root, &paper, key)?;
    let new_entry = existing.is_none();
    let mut table = existing.unwrap_or_default();
    update_metadata(&mut table, &paper, source, &key)?;
    let serialized = toml::to_string_pretty(&table)?;
    let dir = root.join(&key);
    if new_entry {
        fs::create_dir(&dir)?;
    }
    if let Err(e) = write_atomic(&dir, "ref.toml", serialized.as_bytes()) {
        if new_entry {
            let _ = fs::remove_dir(&dir);
        }
        return Err(e);
    }

    let pdf = pdf
        .map(|(name, bytes)| {
            write_pdf(&dir, &name, &bytes).map(|()| format!("references/{key}/{name}"))
        })
        .transpose();
    let corpus = refresh(project);
    let (pdf, warning) = match pdf {
        Ok(pdf) => (pdf, warning),
        Err(e) => (None, Some(format!("PDF の保存に失敗: {e:#}"))),
    };
    Ok(json!({
        "key": key,
        "path": format!("references/{key}/ref.toml"),
        "pdf": pdf,
        "warning": warning,
        "corpus": corpus.with_context(|| format!("references/{key}/ の保存後にコーパスの更新が失敗"))?,
    }))
}

fn write_pdf(dir: &Path, name: &str, contents: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let mut temp = tempfile::Builder::new().prefix(".pdf-").tempfile_in(dir)?;
    temp.write_all(contents)?;
    match temp.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.is_file(),
                "{name} は通常のファイルではない"
            );
            Ok(())
        }
        Err(e) => Err(e.error.into()),
    }
}

fn update_metadata(
    table: &mut toml::Table,
    paper: &Paper,
    source: Option<Source>,
    key: &str,
) -> Result<()> {
    if let Some(title) = &paper.title {
        table.insert("title".into(), title.clone().into());
    }
    if !paper.authors.is_empty() {
        table.insert("authors".into(), toml::Value::try_from(&paper.authors)?);
        // The corpus reader accepts author as an alias for authors.
        table.remove("author");
    }
    if let Some(year) = paper.year {
        table.insert("year".into(), i64::from(year).into());
    }
    if let Some(venue) = &paper.venue {
        table.insert("venue".into(), venue.clone().into());
    }
    for kind in Kind::ALL {
        if let Some(id) = paper.ids.get(kind) {
            table.insert(kind.name().into(), id.into());
        }
    }
    if let Some(id) = &paper.ids.zbmath_record_id {
        table.insert("zbmath_record_id".into(), id.clone().into());
    }
    if let Some(source) = source {
        if !table.contains_key("source") {
            table.insert("source".into(), toml::Value::try_from(source)?);
        }
    }
    let mut urls = table
        .get("urls")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for url in &paper.urls {
        let url = url.clone().into();
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    table.insert("urls".into(), urls.into());
    let bib_key = ["bib_key", "bibkey", "bib", "key"]
        .iter()
        .find_map(|key| table.get(*key)?.as_str())
        .unwrap_or(key);
    if source.is_some() || !table.contains_key("bibtex") {
        table.insert(
            "bibtex".into(),
            bibtex(table, bib_key, paper.journal_article).into(),
        );
    }
    Ok(())
}

pub(crate) fn bibtex(table: &toml::Table, key: &str, journal_article: bool) -> String {
    let field = |name: &str| table.get(name).and_then(toml::Value::as_str);
    let authors = table
        .get("authors")
        .or_else(|| table.get("author"))
        .map(|value| match value {
            toml::Value::Array(names) => names
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()
                .join(" and "),
            _ => value.as_str().unwrap_or_default().into(),
        });
    let year = table.get("year").map(|value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    });
    let url = table.get("urls")
        .and_then(toml::Value::as_array)
        .and_then(|urls| urls.first())
        .and_then(toml::Value::as_str);
    let kind = if journal_article { "article" } else { "misc" };
    let venue = if journal_article {
        "journal"
    } else {
        "howpublished"
    };
    let mut bib = format!("@{kind}{{{key},\n");
    for (field, value) in [
        ("title", field("title")),
        ("author", authors.as_deref()),
        ("year", year.as_deref()),
        (venue, field("venue")),
        ("doi", field("doi")),
        ("eprint", field("arxiv")),
        (
            "mrnumber",
            field("mr").map(|mr| mr.trim_start_matches("MR")),
        ),
        ("url", url),
    ] {
        if let Some(value) = value {
            bib.push_str(&format!("  {field} = {{"));
            for c in value.chars() {
                match c {
                    '\\' => bib.push_str(r"\textbackslash{}"),
                    '{' | '}' | '%' | '&' | '#' | '_' | '$' => {
                        bib.push('\\');
                        bib.push(c);
                    }
                    '\r' | '\n' => bib.push(' '),
                    _ => bib.push(c),
                }
            }
            bib.push_str("},\n");
        }
    }
    bib.push_str("}\n");
    bib
}
