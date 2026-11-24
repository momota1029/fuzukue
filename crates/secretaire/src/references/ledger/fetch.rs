use std::sync::Arc;

use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use fuzukue_proto::{
    LedgerAttempt, LedgerEntry, LedgerLocation, LedgerLocationKind as Kind,
    LedgerOutcome as Outcome, LedgerStatus,
};
use serde_json::Value;
use url::Url;

use super::super::fetch::sources::{Http, NETWORK};
use super::propose::{digits, extract, web_url};
use super::{changed, store, time, Ledger};
use crate::{references, Workspace};

const COOLDOWN_SECONDS: i64 = 15 * 60;
const META_LIMIT: usize = 1024 * 1024;
const FILE_LIMIT: usize = 64 * 1024 * 1024;
const PMC: &str = "https://pmc-oa-opendata.s3.amazonaws.com";

enum Recipe {
    File(Url, String),
    Pmc(String),
    Zenodo(String, Option<String>),
}

fn pin(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn github(url: &Url) -> Option<Recipe> {
    let parts = url.path_segments()?.collect::<Vec<_>>();
    let (owner, repo, remaining) = match parts.as_slice() {
        [owner, repo, remaining @ ..]
            if [owner, repo].into_iter().all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            }) =>
        {
            (*owner, *repo, remaining)
        }
        _ => return None,
    };
    let archive = |revision: &str, suffix: &str| {
        let format = match suffix {
            "zip" => "zip",
            "tar.gz" => "tar.gz",
            _ => return None,
        };
        pin(revision).then(|| {
            Recipe::File(
                Url::parse(&format!(
                    "https://codeload.github.com/{owner}/{repo}/{format}/{revision}"
                ))
                .unwrap(),
                format!("{repo}-{revision}.{suffix}"),
            )
        })
    };
    let raw = |revision: &str, path: &[&str]| {
        if !pin(revision)
            || path
                .iter()
                .any(|part| part.is_empty() || part.contains('%'))
        {
            return None;
        }
        let name = *path.last()?;
        Some(Recipe::File(
            Url::parse(&format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{revision}/{}",
                path.join("/")
            ))
            .ok()?,
            name.into(),
        ))
    };
    match (url.host_str()?, remaining) {
        ("github.com", ["archive", file]) => {
            let (revision, ext) = file.split_once('.')?;
            archive(revision, ext)
        }
        ("codeload.github.com", [format, revision]) => archive(revision, format),
        ("github.com", ["blob" | "raw", revision, path @ ..])
        | ("raw.githubusercontent.com", [revision, path @ ..]) => raw(revision, path),
        _ => None,
    }
}

fn recipe(location: &LedgerLocation) -> Option<Recipe> {
    let value = location.value.as_str();
    match location.kind {
        Kind::Doi => None,
        Kind::Arxiv => {
            let extracted = extract(&format!("arXiv:{value}"));
            (extracted.as_slice() == [location.clone()]).then(|| {
                Recipe::File(
                    Url::parse(&format!("https://arxiv.org/pdf/{value}")).unwrap(),
                    format!("{}.pdf", value.replace('/', "-")),
                )
            })
        }
        Kind::Pmc => {
            let id = value.strip_prefix("PMC")?;
            let (work, version) = id.split_once('.').unwrap_or((id, "1"));
            (digits(work) && digits(version)).then(|| Recipe::Pmc(value.to_owned()))
        }
        Kind::Zenodo => digits(value).then(|| Recipe::Zenodo(value.into(), None)),
        Kind::Geo => {
            let (prefix, id) = (value.get(..3)?, value.get(3..)?);
            if !digits(id) {
                return None;
            }
            let (group, suffix) = match prefix {
                "GSE" => ("series", "_family"),
                "GPL" => ("platforms", "_family"),
                "GDS" => ("datasets", ""),
                _ => return None,
            };
            let range = &id[..id.len().saturating_sub(3)];
            let name = format!("{value}{suffix}.soft.gz");
            let url = Url::parse(&format!(
                "https://ftp.ncbi.nlm.nih.gov/geo/{group}/{prefix}{range}nnn/{value}/soft/{name}"
            ))
            .ok()?;
            Some(Recipe::File(url, name))
        }
        Kind::Github | Kind::Url => {
            let url = web_url(value)?;
            if url.scheme() != "https" || url.port().is_some() {
                return None;
            }
            match url.host_str()? {
                "github.com" | "codeload.github.com" | "raw.githubusercontent.com" => github(&url),
                "zenodo.org" => match url.path_segments()?.collect::<Vec<_>>().as_slice() {
                    ["record" | "records", id, "files", name] if digits(id) && !name.is_empty() => {
                        Some(Recipe::Zenodo((*id).into(), Some((*name).into())))
                    }
                    _ => None,
                },
                "ftp.ncbi.nlm.nih.gov"
                    if url.path().starts_with("/geo/")
                        && url.query().is_none()
                        && url.fragment().is_none() =>
                {
                    let name = url.path_segments()?.next_back()?;
                    (super::safe_name(name) && name.contains('.'))
                        .then(|| Recipe::File(url.clone(), name.into()))
                }
                _ => None,
            }
        }
    }
}

impl Recipe {
    fn identity(&self) -> String {
        match self {
            Self::File(url, _) => url.to_string(),
            Self::Pmc(id) => format!("pmc:{id}"),
            Self::Zenodo(id, name) => {
                format!("zenodo:{id}/{}", name.as_deref().unwrap_or_default())
            }
        }
    }
}

fn cooldown(entries: &[LedgerEntry], target: &str, now: DateTime<Utc>) -> bool {
    entries
        .iter()
        .flat_map(|entry| &entry.attempts)
        .any(|attempt| {
            DateTime::parse_from_rfc3339(&attempt.at).is_ok_and(|at| {
                now.timestamp().saturating_sub(at.timestamp()) < COOLDOWN_SECONDS
                    && recipe(&attempt.location).is_some_and(|recipe| recipe.identity() == target)
            })
        })
}

struct Download {
    name: String,
    bytes: Vec<u8>,
    source: String,
    license: Option<String>,
}

fn get(http: &impl Http, url: &Url, accept: &str, limit: usize) -> Result<Vec<u8>> {
    http.get(url, accept, limit)?
        .ok_or_else(|| FetchFailure(Outcome::NotFound).into())
}

#[derive(Debug)]
struct FetchFailure(Outcome);

impl std::fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for FetchFailure {}

fn json(http: &impl Http, url: &Url) -> Result<Value> {
    let bytes = get(http, url, "application/json", META_LIMIT)?;
    reject_html(&bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn reject_html(bytes: &[u8]) -> Result<()> {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let prefix = prefix.trim_start();
    if prefix.starts_with("<!doctype html") || prefix.starts_with("<html") {
        let result = if prefix.contains("login") || prefix.contains("sign in") {
            Outcome::Login
        } else {
            Outcome::BotWall
        };
        return Err(FetchFailure(result).into());
    }
    Ok(())
}

fn pmc(http: &impl Http, id: &str) -> Result<(Url, String, Option<String>)> {
    let id = if id.contains('.') {
        id.to_owned()
    } else {
        let mut url = Url::parse(PMC)?;
        url.query_pairs_mut()
            .append_pair("list-type", "2")
            .append_pair("prefix", &format!("{id}."))
            .append_pair("delimiter", "/");
        let body = get(http, &url, "application/xml", META_LIMIT)?;
        let text = std::str::from_utf8(&body)?;
        let data = roxmltree::Document::parse(text)?;
        data.descendants()
            .filter(|node| node.has_tag_name("CommonPrefixes"))
            .filter_map(|node| {
                node.children()
                    .find(|child| child.has_tag_name("Prefix"))?
                    .text()
            })
            .filter_map(|prefix| {
                let prefix = prefix.strip_suffix('/')?;
                let version = prefix
                    .strip_prefix(id)?
                    .strip_prefix('.')?
                    .parse::<u64>()
                    .ok()?;
                Some((version, prefix))
            })
            .max_by_key(|(version, _)| *version)
            .map(|(_, prefix)| prefix.to_owned())
            .ok_or(FetchFailure(Outcome::NotFound))?
    };
    let data = json(http, &Url::parse(&format!("{PMC}/{id}/{id}.json"))?)?;
    if data["is_pmc_openaccess"].as_bool() != Some(true) {
        return Err(FetchFailure(Outcome::Manual).into());
    }
    let (url, ext) = [("pdf_url", "pdf"), ("text_url", "txt")]
        .into_iter()
        .find_map(|(key, ext)| {
            let file = format!("{id}.{ext}");
            let expected = format!("{id}/{file}");
            let source = Url::parse(data[key].as_str()?).ok()?;
            (source.scheme() == "s3"
                && source.host_str() == Some("pmc-oa-opendata")
                && source.path().trim_start_matches('/') == expected)
                .then(|| (Url::parse(&format!("{PMC}/{expected}")).unwrap(), ext))
        })
        .ok_or(FetchFailure(Outcome::NotFound))?;
    Ok((
        url,
        format!("{id}.{ext}"),
        data["license_code"].as_str().map(str::to_owned),
    ))
}

fn zenodo(
    http: &impl Http,
    id: &str,
    selected: Option<&str>,
) -> Result<(Url, String, Option<String>)> {
    let data = json(
        http,
        &Url::parse(&format!("https://zenodo.org/api/records/{id}"))?,
    )?;
    let files = data["files"]
        .as_array()
        .ok_or(FetchFailure(Outcome::Manual))?;
    let mut matches = files.iter().filter_map(|file| {
        let name = file["key"].as_str()?;
        // Compare the encoded spelling used in the user's URL.
        let mut url = Url::parse(&format!("https://zenodo.org/api/records/{id}/files/")).ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().push(name);
        if selected.is_some_and(|selected| {
            url.path_segments().and_then(|mut p| p.next_back()) != Some(selected)
        }) {
            return None;
        }
        url.path_segments_mut().ok()?.push("content");
        Some((url, name))
    });
    let (url, name) = matches.next().ok_or(FetchFailure(Outcome::NotFound))?;
    if matches.next().is_some() {
        return Err(FetchFailure(Outcome::Manual).into());
    }
    let license = data["metadata"]["license"]["id"]
        .as_str()
        .or_else(|| data["metadata"]["rights"][0]["id"].as_str())
        .map(str::to_owned);
    Ok((url, name.into(), license))
}

fn download(http: &impl Http, recipe: Recipe) -> Result<Download> {
    let (url, name, license) = match recipe {
        Recipe::File(url, name) => (url, name, None),
        Recipe::Pmc(id) => pmc(http, &id)?,
        Recipe::Zenodo(id, selected) => zenodo(http, &id, selected.as_deref())?,
    };
    ensure!(
        super::safe_name(&name),
        harness_text!(
            "invalid download file name",
            "ダウンロードのファイル名が不正"
        )
    );
    let pdf = references::extension(&name) == "pdf";
    let bytes = get(
        http,
        &url,
        if pdf { "application/pdf" } else { "*/*" },
        FILE_LIMIT,
    )?;
    reject_html(&bytes)?;
    ensure!(
        !bytes.is_empty() && (!pdf || bytes.starts_with(b"%PDF-")),
        harness_text!(
            "unexpected download format",
            "ダウンロードの形式が想定と異なる"
        )
    );
    Ok(Download {
        name,
        bytes,
        source: url.into(),
        license,
    })
}

fn outcome(error: &anyhow::Error) -> Outcome {
    if let Some(failure) = error.downcast_ref::<FetchFailure>() {
        return failure.0;
    }
    match error.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::Status(401, _)) => Outcome::Login,
        Some(ureq::Error::Status(403, _)) => Outcome::Forbidden,
        Some(ureq::Error::Status(404 | 410, _)) => Outcome::NotFound,
        Some(ureq::Error::Status(408 | 504, _)) => Outcome::Timeout,
        Some(ureq::Error::Status(429, _)) => Outcome::BotWall,
        _ if error.chain().any(|err| {
            err.downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::TimedOut)
        }) =>
        {
            Outcome::Timeout
        }
        _ => Outcome::Error,
    }
}

pub(crate) async fn fetch(app: &Arc<Workspace>, key: String) -> Result<LedgerEntry> {
    let app = app.clone();
    // Let the blocking task own the gate until its final attempt is written.
    let gate = app.ledger_fetch.clone().lock_owned().await;
    let workspace = app.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = gate;
        fetch_with(&workspace, &key, &NETWORK)
    })
    .await?
}

fn fetch_with(app: &Workspace, key: &str, http: &impl Http) -> Result<LedgerEntry> {
    let project = &app.project_dir;
    let locations = {
        let _lock = app.reference_lock.blocking_lock();
        let mut ledger = Ledger::load(project)?;
        if ledger.reconcile(project)? {
            ledger.save(project)?;
            changed(app);
        }
        let entry = ledger.entry_mut(key)?;
        if !super::needs_reconciliation(entry) {
            return Ok(entry.clone());
        }
        entry.locations.clone()
    };
    for location in locations {
        let plan = recipe(&location);
        let at = time();
        {
            let _lock = app.reference_lock.blocking_lock();
            let mut ledger = Ledger::load(project)?;
            if plan
                .as_ref()
                .is_some_and(|plan| cooldown(&ledger.entries, &plan.identity(), Utc::now()))
            {
                continue;
            }
            let entry = ledger.entry_mut(key)?;
            if !super::needs_reconciliation(entry) {
                return Ok(entry.clone());
            }
            let outcome = if plan.is_some() {
                Outcome::Started
            } else {
                Outcome::Manual
            };
            if outcome == Outcome::Manual
                && entry.attempts.iter().any(|attempt| {
                    attempt.location == location && attempt.outcome == Outcome::Manual
                })
            {
                continue;
            }
            entry.attempts.push(LedgerAttempt {
                location: location.clone(),
                at: at.clone(),
                outcome,
                note: None,
            });
            ledger.save(project)?;
        }
        let Some(plan) = plan else { continue };
        let acquired = download(http, plan);
        let _lock = app.reference_lock.blocking_lock();
        let mut ledger = Ledger::load(project)?;
        let entry = ledger.entry_mut(key)?;
        let result = match acquired {
            Ok(file) if super::needs_reconciliation(entry) => store(
                project,
                entry,
                &file.name,
                file.bytes.as_slice(),
                file.source,
                file.license,
            ),
            Ok(_) => Err(anyhow::anyhow!(harness_text!(
                "entry changed during fetch",
                "取得中に台帳の行が変わった"
            ))),
            Err(e) => Err(e),
        };
        if let Some(attempt) = entry.attempts.iter_mut().rev().find(|attempt| {
            attempt.location == location && attempt.at == at && attempt.outcome == Outcome::Started
        }) {
            match result {
                Ok(()) => attempt.outcome = Outcome::Ok,
                Err(e) => {
                    attempt.outcome = outcome(&e);
                    attempt.note = Some(format!("{e:#}").chars().take(1000).collect());
                }
            }
        }
        let entry = entry.clone();
        ledger.save(project)?;
        if !super::needs_reconciliation(&entry) {
            changed(app);
            return Ok(entry);
        }
    }
    let _lock = app.reference_lock.blocking_lock();
    let mut ledger = Ledger::load(project)?;
    let entry = ledger.entry_mut(key)?;
    if super::needs_reconciliation(entry) {
        entry.status = LedgerStatus::AskUser;
    }
    let entry = entry.clone();
    ledger.save(project)?;
    changed(app);
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Mock(RefCell<Vec<String>>);

    impl Http for Mock {
        fn get(&self, url: &Url, _accept: &str, _limit: usize) -> Result<Option<Vec<u8>>> {
            self.0.borrow_mut().push(url.to_string());
            match url.host_str() {
                Some("arxiv.org") => Ok(None),
                Some("raw.githubusercontent.com") => Ok(Some(b"Example source code".to_vec())),
                _ => panic!("outbound request to {url}"),
            }
        }
    }

    #[test]
    fn tries_in_order_records_failures_and_keeps_cooldown_after_reload() -> Result<()> {
        let path = tempfile::tempdir()?;
        let root = path.path();
        let app = Workspace::new(
            root.into(),
            String::new(),
            root.join(".state"),
            Arc::new(crate::Hub::new()),
        );
        let mock = Mock(RefCell::default());
        let source = "https://publisher.example/file.pdf arXiv:2301.01234";
        let mut ledger = Ledger {
            entries: vec![entry("paper", source)],
        };
        ledger.save(root)?;
        let row = fetch_with(&app, "paper", &mock)?;
        assert_eq!(row.status, LedgerStatus::AskUser);
        assert_eq!(
            row.attempts.iter().map(|a| a.outcome).collect::<Vec<_>>(),
            [Outcome::Manual, Outcome::NotFound]
        );
        assert_eq!(&*mock.0.borrow(), &["https://arxiv.org/pdf/2301.01234"]);
        let row = fetch_with(&app, "paper", &mock)?;
        assert_eq!(row.attempts.len(), 2);
        assert_eq!(mock.0.borrow().len(), 1);

        let mut ledger = Ledger::load(root)?;
        ledger.entry_mut("paper")?.attempts[1].at =
            (Utc::now() - chrono::Duration::minutes(16)).to_rfc3339();
        ledger.entry_mut("paper")?.locations.extend(extract(
            "https://github.com/org/repo/blob/0123456789012345678901234567890123456789/src/main.rs",
        ));
        ledger.save(root)?;
        let row = fetch_with(&app, "paper", &mock)?;
        assert_eq!(row.status, LedgerStatus::Acquired);
        assert_eq!(row.attempts.last().unwrap().outcome, Outcome::Ok);
        let file = row.acquired.unwrap();
        assert_eq!(file.file, "references/paper/main.rs");
        assert_eq!(
            file.sha256,
            super::super::hash(b"Example source code".as_slice())?
        );
        assert!(file
            .source
            .starts_with("https://raw.githubusercontent.com/"));
        Ok(())
    }

    fn entry(key: &str, source: &str) -> LedgerEntry {
        LedgerEntry {
            key: key.into(),
            title: "Paper".into(),
            kind: fuzukue_proto::LedgerKind::Paper,
            reason: "fixture".into(),
            locations: extract(source),
            status: LedgerStatus::Pending,
            attempts: Vec::new(),
            acquired: None,
        }
    }

    #[test]
    fn pmc_checks_the_open_access_flag_before_downloading() -> Result<()> {
        struct Cloud {
            open_access: bool,
            seen: RefCell<Vec<String>>,
        }
        impl Http for Cloud {
            fn get(&self, url: &Url, _: &str, _: usize) -> Result<Option<Vec<u8>>> {
                assert_eq!(url.host_str(), Some("pmc-oa-opendata.s3.amazonaws.com"));
                self.seen.borrow_mut().push(url.to_string());
                let bytes = match url.path() {
                    "/" => b"<ListBucketResult><CommonPrefixes><Prefix>PMC42.2/</Prefix></CommonPrefixes></ListBucketResult>".to_vec(),
                    "/PMC42.2/PMC42.2.json" => serde_json::to_vec(&serde_json::json!({
                        "is_pmc_openaccess": self.open_access,
                        "license_code": "CC BY",
                        "pdf_url": "s3://pmc-oa-opendata/PMC42.2/PMC42.2.pdf?md5=example",
                    }))?,
                    "/PMC42.2/PMC42.2.pdf" => b"%PDF-1.7".to_vec(),
                    path => panic!("unexpected request: {path}"),
                };
                Ok(Some(bytes))
            }
        }
        let cloud = Cloud {
            open_access: false,
            seen: RefCell::default(),
        };
        let result = download(&cloud, Recipe::Pmc("PMC42".into()));
        assert_eq!(outcome(&result.err().unwrap()), Outcome::Manual);
        assert_eq!(cloud.seen.borrow().len(), 2);
        let cloud = Cloud {
            open_access: true,
            seen: RefCell::default(),
        };
        let result = download(&cloud, Recipe::Pmc("PMC42".into()))?;
        assert_eq!(result.name, "PMC42.2.pdf");
        assert_eq!(result.license.as_deref(), Some("CC BY"));
        assert_eq!(cloud.seen.borrow().len(), 3);
        Ok(())
    }

    #[test]
    fn rejects_publisher_and_unpinned_github_locations() {
        for loc in extract("https://publisher.example/a https://arxiv.org.publisher.example/pdf/2401.10000 \
            https://github.com/org/repo/archive/main.zip https://raw.githubusercontent.com/org/repo/main/a.rs \
            https://pmc.ncbi.nlm.nih.gov/articles/PMC123/pdf/file.pdf https://doi.org/10.1234/article") {
            assert!(recipe(&loc).is_none(), "{loc:?}");
        }
    }
}
