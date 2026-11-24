use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    LedgerEntry, LedgerKind, LedgerLocation as Location, LedgerLocationKind as Kind,
    LedgerProposal, LedgerSource,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::{changed, Ledger};
use crate::{references, Workspace};

const MAX_TEXT: usize = 256 * 1024;
const MAX_PROPOSALS: usize = 128;

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?ix)
    (?P<url>(?:https?|ftp)://[^\s<>{}"\\\[\]]+) |
    (?P<doi>\b10\.\d{4,}/[^\s<>{}"\\\[\]]+) |
    (?:\barxiv\s*:\s*|\beprint\s*=\s*[{"]\s*)(?P<arxiv>[a-z][a-z.-]*/\d{7}(?:v\d+)?|\d{4}\.\d{4,5}(?:v\d+)?)\b |
    (?P<bare_arxiv>\b\d{2}(?:0[1-9]|1[0-2])\.\d{4,5}(?:v[1-9]\d*)?\b) |
    (?P<pmc>\bPMC\d+(?:\.\d+)?\b) |
    (?P<geo>\bGS[EM]\d+\b|\bGPL\d+\b|\bGDS\d+\b) |
    \bzenodo[.:\s]+(?P<zenodo>\d+)\b |
    (?P<github>\bgithub\.com/[a-z0-9_.-]+/[a-z0-9_.-]+(?:/[^\s<>{}"\\\[\]]+)?)
    "#
).unwrap()
});
static ARXIV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:[a-z][a-z.-]*/\d{7}|\d{4}\.\d{4,5})(?:v[1-9]\d*)?$").unwrap()
});
static BIB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?im)^\s*@\w+\s*[{(]").unwrap());
static NUMBERED_CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*(?:\[[0-9]+\]|[0-9]+\.)[ \t]+").unwrap());
static BIBLIOGRAPHY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?im)^\s*(?:\d+\.?\s+)?(?:references|bibliography|literature cited|参考文献|引用文献)\s*$",
).unwrap()
});

fn location(kind: Kind, value: impl Into<String>) -> Location {
    Location {
        kind,
        value: value.into(),
    }
}

pub(super) fn web_url(text: &str) -> Option<Url> {
    let url = Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn trim_token(mut text: &str) -> &str {
    text = text.trim_end_matches(['.', ',', ';', ':', '。', '、']);
    while text.ends_with(')') && text.matches(')').count() > text.matches('(').count() {
        text = text[..text.len() - 1].trim_end_matches(['.', ',', ';', ':']);
    }
    text
}

fn from_url(value: &str) -> Option<Location> {
    if let Some(path) = value.strip_prefix("ftp://ftp.ncbi.nlm.nih.gov/geo/") {
        return from_url(&format!("https://ftp.ncbi.nlm.nih.gov/geo/{path}"));
    }
    let url = web_url(value)?;
    let path = url.path().trim_matches('/');
    let (head, tail) = path.split_once('/').unwrap_or((path, ""));
    let first = tail.split('/').next().unwrap_or("");
    let result = if url.port().is_none() && url.scheme() == "https" {
        match url.host_str()? {
            "doi.org" | "dx.doi.org" if head.starts_with("10.") && !tail.is_empty() => {
                Some(location(Kind::Doi, path.to_ascii_lowercase()))
            }
            "arxiv.org" | "export.arxiv.org" if matches!(head, "abs" | "pdf" | "html") => {
                let id = tail.strip_suffix(".pdf").unwrap_or(tail);
                ARXIV
                    .is_match(id)
                    .then(|| location(Kind::Arxiv, id.to_ascii_lowercase()))
            }
            "pmc.ncbi.nlm.nih.gov" if head == "articles" && tail == first => {
                identifier(first, Kind::Pmc)
            }
            "www.ncbi.nlm.nih.gov" if path.starts_with("pmc/articles/") => {
                identifier(path.strip_prefix("pmc/articles/")?, Kind::Pmc)
            }
            "www.ncbi.nlm.nih.gov" if path == "geo/query/acc.cgi" => url
                .query_pairs()
                .find(|(key, _)| key == "acc")
                .and_then(|(_, value)| identifier(&value, Kind::Geo)),
            "zenodo.org"
                if matches!(head, "record" | "records") && tail == first && digits(first) =>
            {
                Some(location(Kind::Zenodo, first))
            }
            "github.com" | "raw.githubusercontent.com" | "codeload.github.com" => {
                Some(location(Kind::Github, url.as_str()))
            }
            _ => None,
        }
    } else {
        None
    };
    Some(result.unwrap_or_else(|| location(Kind::Url, url.as_str())))
}

fn identifier(value: &str, kind: Kind) -> Option<Location> {
    extract(value)
        .into_iter()
        .find(|loc| loc.kind == kind && loc.value.eq_ignore_ascii_case(value))
}

pub(super) fn digits(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn extract(text: &str) -> Vec<Location> {
    let mut seen = BTreeSet::new();
    TOKENS
        .captures_iter(text)
        .filter_map(|cap| {
            let value = trim_token(cap.get(0)?.as_str());
            let loc = if cap.name("url").is_some() {
                from_url(value)?
            } else if cap.name("github").is_some() {
                from_url(&format!("https://{value}"))?
            } else if cap.name("doi").is_some() {
                location(Kind::Doi, value.to_ascii_lowercase())
            } else if let Some(id) = cap.name("arxiv").or_else(|| cap.name("bare_arxiv")) {
                location(Kind::Arxiv, id.as_str().to_ascii_lowercase())
            } else if let Some(id) = cap.name("zenodo") {
                location(Kind::Zenodo, id.as_str())
            } else {
                let kind = if cap.name("pmc").is_some() {
                    Kind::Pmc
                } else {
                    Kind::Geo
                };
                location(kind, value.to_ascii_uppercase())
            };
            (loc.value.len() <= 2048 && seen.insert(loc.clone())).then_some(loc)
        })
        .collect()
}

pub(super) fn overlap(a: &[Location], b: &[Location]) -> bool {
    a.iter().any(|a| {
        b.iter().any(|b| {
            a.kind == b.kind
                && match a.kind {
                    Kind::Arxiv => same_version(&a.value, &b.value, 'v'),
                    Kind::Pmc => same_version(&a.value, &b.value, '.'),
                    _ => a.value == b.value,
                }
        })
    })
}

fn same_version(a: &str, b: &str, separator: char) -> bool {
    fn version(id: &str, separator: char) -> (&str, Option<&str>) {
        id.rsplit_once(separator)
            .filter(|(_, version)| digits(version))
            .map_or((id, None), |(work, version)| (work, Some(version)))
    }
    let (a, a_version) = version(a, separator);
    let (b, b_version) = version(b, separator);
    a == b && (a_version == b_version || a_version.is_none() || b_version.is_none())
}

fn read_text(path: &Path) -> Result<String> {
    let mut text = String::new();
    super::open_file(path)?
        .take(MAX_TEXT as u64 + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= MAX_TEXT,
        harness_text!("source text is too large", "候補の入力が長すぎる")
    );
    Ok(text)
}

pub(super) fn reference_identifiers(dir: &Path) -> (Vec<Location>, Option<String>, Option<String>) {
    let table = read_text(&dir.join("ref.toml"))
        .ok()
        .and_then(|text| toml::from_str::<serde_json::Value>(&text).ok())
        .or_else(|| {
            let text = read_text(&dir.join("reference.md")).ok()?;
            let (front, _) = fuzukue_cards::split_frontmatter(&text)?;
            serde_yaml::from_str(front).ok()
        })
        .unwrap_or_default();
    let source = table["source"].as_str().map(str::to_owned);
    let license = table["license"].as_str().map(str::to_owned);
    let mut ids = Vec::new();
    for field in [
        "doi",
        "arxiv",
        "pmc",
        "pmcid",
        "geo",
        "zenodo",
        "github",
        "url",
        "urls",
        "source",
        "identifiers",
        "ids",
        "bibtex",
    ] {
        let value = &table[field];
        if let Some(text) = value.as_str() {
            let text = match field {
                "arxiv" | "zenodo" => format!("{field}:{text}"),
                _ => text.to_owned(),
            };
            ids.extend(extract(&text));
        } else if value.is_object() || value.is_array() {
            ids.extend(extract(&value.to_string()));
        }
    }
    if ids.is_empty() {
        if let Ok(meta) = references::metadata(dir) {
            if let Some(source) = meta.source {
                ids.extend(extract(&source.to_string()));
            }
        }
    }
    (ids, source, license)
}

fn source(project: &Path, input: LedgerSource) -> Result<(String, Vec<(String, bool)>)> {
    Ok(match input {
        LedgerSource::Text { text } => {
            ensure!(
                text.len() <= MAX_TEXT,
                harness_text!("source text is too large", "候補の入力が長すぎる")
            );
            ("pasted_text".into(), vec![(text, false)])
        }
        LedgerSource::Card { path } => {
            ensure!(
                references::relative(&path) && references::extension(&path) == "md",
                harness_text!(
                    "expected a project-relative card path",
                    "カードはワークスペース相対パスで指定する"
                )
            );
            let absolute = crate::safe_join(project, &path)?;
            let content = read_text(&absolute)?;
            let (_, body) = fuzukue_cards::split_frontmatter(&content).context(harness_text!(
                "expected a card",
                "カードの frontmatter がない"
            ))?;
            let mut body = body.to_owned();
            if absolute.ends_with("card.md") {
                for name in ["statement.tex", "proof.tex"] {
                    let file = absolute.with_file_name(name);
                    if let Ok(text) = read_text(&file) {
                        body.push('\n');
                        body.push_str(&text);
                    }
                }
            }
            (path, vec![(body, false)])
        }
        LedgerSource::Reference { key } => {
            ensure!(
                references::entry_key(&key),
                harness_text!("invalid reference key", "資料キーが不正")
            );
            let root = references::required_root(project)?;
            let dir = references::directory(&root, &key, false)?
                .context(harness_text!("unknown reference", "資料が見つからない"))?;
            let mut texts = Vec::new();
            if let Ok(text) = read_text(&dir.join("ref.toml")) {
                let table: toml::Table = toml::from_str(&text)?;
                if let Some(bib) = table.get("bibtex").and_then(toml::Value::as_str) {
                    texts.push((bib.to_owned(), false));
                }
            }
            for file in references::files(&dir)? {
                if references::extension(&file) == "bib" {
                    texts.push((read_text(&references::checked_file(&dir, &file)?)?, false));
                }
            }
            if references::files(&dir)?
                .iter()
                .any(|file| references::extension(file) == "pdf")
            {
                references::refresh(project)?;
                for file in references::library_files(&root)?
                    .into_iter()
                    .filter(|file| file.key == key && references::extension(&file.file) == "pdf")
                {
                    let mut bibliography = None::<String>;
                    for (page, _) in file.parts {
                        let text = read_text(&page)?;
                        if let Some(output) = &mut bibliography {
                            output.push('\n');
                            output.push_str(&text);
                        } else if let Some(heading) = BIBLIOGRAPHY.find(&text) {
                            bibliography = Some(text[heading.end()..].to_owned());
                        }
                        if bibliography
                            .as_ref()
                            .is_some_and(|text| text.len() >= MAX_TEXT)
                        {
                            break;
                        }
                    }
                    if let Some(mut bibliography) = bibliography {
                        bibliography.truncate(bibliography.floor_char_boundary(MAX_TEXT));
                        texts.push((bibliography, true));
                    }
                }
            }
            (format!("references/{key}"), texts)
        }
    })
}

fn fallback_kind(location: &Location) -> LedgerKind {
    match location.kind {
        Kind::Doi | Kind::Arxiv | Kind::Pmc => LedgerKind::Paper,
        Kind::Geo | Kind::Zenodo => LedgerKind::Data,
        Kind::Github => LedgerKind::Code,
        Kind::Url => LedgerKind::WebPage,
    }
}

type Draft = (Vec<Location>, String);

fn drafts(texts: Vec<(String, bool)>) -> Vec<Draft> {
    let mut drafts = Vec::<Draft>::new();
    for (text, bibliography) in texts {
        let entry_start = if bibliography {
            &*NUMBERED_CITATION
        } else {
            &*BIB
        };
        let starts = entry_start
            .find_iter(&text)
            .map(|m| m.start())
            .collect::<Vec<_>>();
        if !starts.is_empty() {
            for (i, start) in starts.iter().enumerate() {
                let excerpt = &text[*start..starts.get(i + 1).copied().unwrap_or(text.len())];
                let locations = extract(excerpt);
                if !locations.is_empty() {
                    drafts.push((locations, excerpt.chars().take(1200).collect()));
                }
            }
        } else {
            for line in text.lines() {
                for loc in extract(line) {
                    drafts.push((vec![loc], line.chars().take(1200).collect()));
                }
            }
        }
    }
    let mut unique = Vec::<Draft>::new();
    for (locations, excerpt) in drafts {
        if let Some((prior, _)) = unique
            .iter_mut()
            .find(|(prior, _)| overlap(prior, &locations))
        {
            for loc in locations {
                if !prior.contains(&loc) {
                    prior.push(loc);
                }
            }
        } else if unique.len() < MAX_PROPOSALS {
            unique.push((locations, excerpt));
        }
    }
    unique
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    row: usize,
    title: Option<String>,
    kind: Option<LedgerKind>,
}

pub(crate) async fn propose(
    app: &Arc<Workspace>,
    input: LedgerSource,
) -> Result<Vec<LedgerProposal>> {
    let (reason, drafts) = references::run(app, move |project| {
        let (reason, texts) = source(project, input)?;
        Ok((reason, drafts(texts)))
    })
    .await?;
    if drafts.is_empty() {
        return Ok(Vec::new());
    }
    let data = drafts.iter().enumerate().map(|(row, (locations, excerpt))| {
        json!({"row": row, "locations": locations, "excerpt": excerpt})
    }).collect::<Vec<_>>();
    let instructions = harness_text!(
        include_str!("../../../prompts/ledger-proposal.md"),
        include_str!("../../../prompts/ja/ledger-proposal.md")
    );
    let data = serde_json::to_string(&data)?;
    let prompt = harness_format!("{instructions}\n{data}", "{instructions}\n{data}");
    let cancel = Arc::new(crate::cancellation::Cancellation::default());
    let deadline = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(90)).await;
            cancel.cancel();
        }
    });
    let labels = if app.usage.blocked("claude").is_none() {
        let result = crate::claude::run_ledger_proposal(app, &prompt, &cancel).await;
        result
            .and_then(|answer| Ok(serde_json::from_str::<Vec<Label>>(&answer)?))
            .unwrap_or_else(|e| {
                tracing::warn!("ledger proposal cataloging: {e:#}");
                Vec::new()
            })
    } else {
        Vec::new()
    };
    deadline.abort();

    let workspace = app.clone();
    references::run(app, move |project| {
        let ledger = Ledger::load(project)?;
        let mut state = workspace.ledger.lock().unwrap();
        let mut proposed = Vec::<LedgerProposal>::new();
        let mut keys = occupied_keys(project, &ledger)?;
        keys.extend(state.proposals.iter().map(|item| item.entry.key.clone()));
        for (row, (locations, _)) in drafts.into_iter().enumerate() {
            if state.proposals.len() + proposed.len() >= MAX_PROPOSALS {
                break;
            }
            if ledger
                .entries
                .iter()
                .any(|entry| overlap(&entry.locations, &locations))
                || state
                    .proposals
                    .iter()
                    .any(|item| overlap(&item.entry.locations, &locations))
            {
                continue;
            }
            let primary = &locations[0];
            let label = labels.iter().find(|label| label.row == row);
            let title = label
                .and_then(|label| label.title.as_deref())
                .map(str::trim)
                .filter(|title| {
                    !title.is_empty()
                        && title.chars().count() <= 300
                        && !title.chars().any(char::is_control)
                })
                .unwrap_or(&primary.value)
                .to_owned();
            let kind = label
                .and_then(|label| label.kind)
                .unwrap_or_else(|| fallback_kind(primary));
            let id = state.next_id();
            let key = allocate_key(&keys, primary);
            keys.insert(key.clone());
            proposed.push(LedgerProposal {
                id,
                entry: LedgerEntry {
                    key,
                    title,
                    kind,
                    reason: reason.clone(),
                    locations,
                    status: Default::default(),
                    attempts: Vec::new(),
                    acquired: None,
                },
            });
        }
        if !proposed.is_empty() {
            let mut preview = Ledger {
                entries: proposed.iter().map(|item| item.entry.clone()).collect(),
            };
            preview.reconcile(project)?;
            for (proposal, entry) in proposed.iter_mut().zip(preview.entries) {
                proposal.entry = entry;
            }
            state.proposals.extend(proposed.iter().cloned());
            changed(&workspace);
        }
        Ok(proposed)
    })
    .await
}

fn occupied_keys(project: &Path, ledger: &Ledger) -> Result<BTreeSet<String>> {
    let mut keys: BTreeSet<_> = ledger
        .entries
        .iter()
        .map(|entry| entry.key.clone())
        .collect();
    if let Some(root) = references::directory(project, "references", false)? {
        for entry in fs::read_dir(root)? {
            if let Some(key) = entry?.file_name().to_str() {
                keys.insert(key.to_owned());
            }
        }
    }
    Ok(keys)
}

fn allocate_key(reserved: &BTreeSet<String>, loc: &Location) -> String {
    let base = loc
        .value
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase();
    let base = base[..base.len().min(64)].trim_end_matches('-');
    let base = if base.is_empty() { "source" } else { base };
    (1..)
        .map(|n| {
            if n == 1 {
                base.into()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|key| !reserved.contains(key))
        .expect("unused ledger key")
}

pub(super) async fn accept(app: &Arc<Workspace>, ids: Vec<u64>) -> Result<()> {
    let workspace = app.clone();
    references::run(app, move |project| {
        let mut ledger = Ledger::load(project)?;
        let mut state = workspace.ledger.lock().unwrap();
        let mut keys = occupied_keys(project, &ledger)?;
        let mut changed_rows = false;
        for proposal in state.proposals.iter().filter(|item| ids.contains(&item.id)) {
            let mut entry = proposal.entry.clone();
            if ledger
                .entries
                .iter()
                .any(|other| overlap(&other.locations, &entry.locations))
            {
                continue;
            }
            if keys.contains(&entry.key) {
                entry.key = allocate_key(&keys, &entry.locations[0]);
            }
            keys.insert(entry.key.clone());
            entry.status = Default::default();
            entry.acquired = None;
            ledger.entries.push(entry);
            changed_rows = true;
        }
        if changed_rows {
            ledger.reconcile(project)?;
            ledger.save(project)?;
        }
        state.proposals.retain(|item| !ids.contains(&item.id));
        changed(&workspace);
        Ok(())
    })
    .await
}
