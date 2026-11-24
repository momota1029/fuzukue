use std::io::Read;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use roxmltree::{Document, Node};
use serde_json::Value;
use url::Url;

use super::{arxiv_work, Identifier, Ids, Kind, Paper, Source};

const ZBMATH: &str = "https://api.zbmath.org/v1/document/";
const ARXIV: &str = "https://export.arxiv.org/api/query";
const CROSSREF: &str = "https://api.crossref.org/works";
const ATOM: &str = "http://www.w3.org/2005/Atom";
const ARXIV_NS: &str = "http://arxiv.org/schemas/atom";
const METADATA_LIMIT: usize = 8 * 1024 * 1024;
const PDF_LIMIT: usize = 64 * 1024 * 1024;

pub(in crate::references) trait Http {
    fn get(&self, url: &Url, accept: &str, max_bytes: usize) -> Result<Option<Vec<u8>>>;
}

pub(in crate::references) struct Network;
pub(in crate::references) static NETWORK: Network = Network;

impl Http for Network {
    fn get(&self, url: &Url, accept: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
            ureq::AgentBuilder::new()
                .user_agent("fuzukue (references fetcher)")
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .redirects(0)
                .build()
        });
        static ZB_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        static ARXIV_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        static CROSSREF_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        static ZENODO_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        static GITHUB_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        static NCBI_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        let (next, delay) = match url.host_str() {
            Some("api.zbmath.org") => (&ZB_NEXT, Duration::from_millis(400)),
            Some("export.arxiv.org" | "arxiv.org") => (&ARXIV_NEXT, Duration::from_secs(3)),
            Some("api.crossref.org") => (&CROSSREF_NEXT, Duration::from_millis(400)),
            Some("zenodo.org") => (&ZENODO_NEXT, Duration::from_secs(1)),
            Some("codeload.github.com" | "raw.githubusercontent.com") => {
                (&GITHUB_NEXT, Duration::from_secs(1))
            }
            Some("pmc-oa-opendata.s3.amazonaws.com" | "ftp.ncbi.nlm.nih.gov") => {
                (&NCBI_NEXT, Duration::from_secs(1))
            }
            _ => bail!("対応していない取得先: {url}"),
        };
        ensure!(url.scheme() == "https", "取得先は HTTPS で指定する");
        let mut next = next
            .lock()
            .map_err(|_| anyhow::anyhow!("書誌 API の通信ロックが壊れた"))?;
        let mut current = url.clone();
        for _ in 0..=3 {
            if let Some(when) = *next {
                std::thread::sleep(when.saturating_duration_since(Instant::now()));
            }
            *next = Some(Instant::now() + delay);
            let response = match AGENT.get(current.as_str()).set("Accept", accept).call() {
                Ok(response) => response,
                Err(ureq::Error::Status(404, _)) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("{current} の取得に失敗")),
            };
            if (300..400).contains(&response.status()) {
                let location = response
                    .header("Location")
                    .context("リダイレクト先がない")?;
                current = current.join(location)?;
                ensure!(
                    current.origin() == url.origin(),
                    "別のオリジンへの転送: {current}"
                );
                continue;
            }
            let mut data = Vec::new();
            response
                .into_reader()
                .take(max_bytes as u64 + 1)
                .read_to_end(&mut data)?;
            ensure!(
                data.len() <= max_bytes,
                "応答のサイズが上限を超えた: {current}"
            );
            return Ok(Some(data));
        }
        bail!("リダイレクト回数が上限を超えた: {url}")
    }
}

fn request(base: &str, parameters: &[(&str, &str)]) -> Url {
    let mut url = Url::parse(base).expect("fixed API URL");
    if !parameters.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(parameters.iter().copied());
    }
    url
}

fn item(base: &str, id: &str) -> Url {
    let mut url = request(base, &[]);
    url.path_segments_mut()
        .expect("fixed HTTP URL")
        .pop_if_empty()
        .push(id);
    url
}

fn json_response(http: &impl Http, url: &Url) -> Result<Option<Value>> {
    http.get(url, "application/json", METADATA_LIMIT)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).with_context(|| format!("{url} の JSON が不正"))
        })
        .transpose()
}

pub(super) fn search(
    http: &impl Http,
    source: Source,
    query: &str,
    limit: usize,
) -> Result<Vec<Paper>> {
    let limit = limit.to_string();
    match source {
        Source::Zbmath => zb_search(http, query, &limit),
        Source::Arxiv => {
            let query = query
                .split_whitespace()
                .map(|term| format!("all:\"{}\"", term.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" AND ");
            arxiv(http, &[("search_query", &query), ("max_results", &limit)])
        }
        Source::Crossref => {
            let url = request(
                CROSSREF,
                &[("query.bibliographic", query), ("rows", &limit)],
            );
            let data = json_response(http, &url)?.context("Crossref 検索先が見つからない")?;
            let items = data["message"]["items"]
                .as_array()
                .context("Crossref の検索結果が不正")?;
            Ok(items.iter().filter_map(crossref_record).collect())
        }
        Source::All => unreachable!(),
    }
}

pub(super) fn lookup(
    http: &impl Http,
    id: &Identifier,
) -> Result<(Paper, Option<Source>, Option<String>)> {
    let value = &id.value;
    let (paper, source) = match id.kind {
        Kind::Mr => {
            let result = zb_search(http, &format!("en:{value}"), "10");
            let mut warning = None;
            let paper = match result {
                Ok(papers) => papers
                    .into_iter()
                    .find(|paper| paper.ids.mr.as_ref() == Some(value)),
                Err(e) => {
                    warning = Some(format!("zbMATH Open の MR 照合に失敗: {e:#}"));
                    None
                }
            };
            return Ok(match paper {
                Some(paper) => (paper, Some(Source::Zbmath), None),
                None => (
                    Paper {
                        ids: Ids {
                            mr: Some(value.clone()),
                            ..Ids::default()
                        },
                        urls: vec![mr_url(value)],
                        ..Paper::default()
                    },
                    None,
                    warning.or_else(|| Some(format!("zbMATH Open に {value} の相互参照がない"))),
                ),
            });
        }
        Kind::Arxiv => {
            let paper = arxiv(http, &[("id_list", value)])?
                .into_iter()
                .find(|paper| {
                    paper.ids.arxiv.as_ref().is_some_and(|found| {
                        value == found || (arxiv_work(value) == value && arxiv_work(found) == value)
                    })
                });
            (paper, Source::Arxiv)
        }
        Kind::Zbmath if value.contains('.') => {
            let paper = zb_search(http, &format!("an:{value}"), "10")?
                .into_iter()
                .find(|paper| paper.ids.zbmath.as_ref() == Some(value));
            (paper, Source::Zbmath)
        }
        Kind::Zbmath => {
            let data = json_response(http, &item(ZBMATH, value))?;
            let paper = data.as_ref().and_then(|data| {
                (data["result"]["id"].as_u64() == value.parse::<u64>().ok())
                    .then(|| zb_record(&data["result"]))
                    .flatten()
            });
            (paper, Source::Zbmath)
        }
        Kind::Doi => {
            let data = json_response(http, &item(CROSSREF, value))?;
            let paper = data
                .as_ref()
                .and_then(|data| crossref_record(&data["message"]))
                .filter(|paper| paper.ids.doi.as_ref() == Some(value));
            (paper, Source::Crossref)
        }
    };
    Ok((
        paper.with_context(|| format!("資料が見つからない: {}", id.kind.id(value)))?,
        Some(source),
        None,
    ))
}

fn text(value: &Value) -> Option<&str> {
    let value = value.as_str()?.trim();
    (!value.is_empty()).then_some(value)
}

fn array(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn year(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .or_else(|| text(value)?.get(..4)?.parse().ok())
}

fn http_url(value: &Value) -> Option<Url> {
    let url = Url::parse(text(value)?).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(url)
}

fn zb_search(http: &impl Http, query: &str, limit: &str) -> Result<Vec<Paper>> {
    let url = request(
        &format!("{ZBMATH}_search"),
        &[("search_string", query), ("results_per_page", limit)],
    );
    let data = json_response(http, &url)?.context("zbMATH Open 検索先が見つからない")?;
    let results = data["result"]
        .as_array()
        .context("zbMATH Open の検索結果が不正")?;
    Ok(results.iter().filter_map(zb_record).collect())
}

fn zb_record(value: &Value) -> Option<Paper> {
    let url = http_url(&value["zbmath_url"]);
    let record_id = value["id"].as_u64().map(|id| id.to_string());
    let zbl = text(&value["zbl_id"])
        .and_then(|v| Kind::Zbmath.normalize(v))
        .or_else(|| {
            let url = url.as_ref()?;
            url.path()
                .strip_prefix('/')
                .and_then(|v| Kind::Zbmath.normalize(v))
                .or_else(|| {
                    let (_, v) = url.query_pairs().find(|(key, _)| key == "q")?;
                    Kind::Zbmath.normalize(v.strip_prefix("an:")?)
                })
        })
        .or_else(|| record_id.clone())?;
    let mut ids = Ids {
        zbmath: Some(zbl.clone()),
        zbmath_record_id: record_id,
        doi: text(&value["doi"]).and_then(|id| Kind::Doi.normalize(id)),
        mr: text(&value["mr"])
            .or_else(|| text(&value["mr_number"]))
            .and_then(|id| Kind::Mr.normalize(id)),
        ..Ids::default()
    };
    let mut urls = vec![url
        .unwrap_or_else(|| request("https://zbmath.org/", &[("q", &format!("an:{zbl}"))]))
        .into()];
    for link in array(&value["links"]) {
        let url = http_url(&link["url"]);
        let id = text(&link["identifier"]);
        match text(&link["type"]).map(str::to_ascii_lowercase).as_deref() {
            Some("doi") => ids.doi = ids.doi.or_else(|| Kind::Doi.normalize(id?)),
            Some("arxiv") => ids.arxiv = ids.arxiv.or_else(|| Kind::Arxiv.normalize(id?)),
            Some("mr" | "mathscinet") => ids.mr = ids.mr.or_else(|| Kind::Mr.normalize(id?)),
            _ => {}
        }
        if let Some(url) = url {
            if url.host_str() == Some("doi.org") {
                ids.doi = ids
                    .doi
                    .or_else(|| Kind::Doi.normalize(url.path().trim_start_matches('/')));
            }
            ids.arxiv = ids.arxiv.or_else(|| arxiv_id(&url));
            ids.mr = ids.mr.or_else(|| mr_from_url(&url));
            urls.push(url.into());
        }
    }
    let open_pdf = ids.arxiv.is_some();
    let mut paper = Paper {
        title: text(&value["title"]["title"])
            .or_else(|| text(&value["title"]))
            .map(str::to_owned),
        authors: array(&value["contributors"]["authors"])
            .filter_map(|author| text(&author["name"]))
            .map(str::to_owned)
            .collect(),
        year: year(&value["year"]),
        venue: text(&value["source"]["source"])
            .or_else(|| text(&value["source"]))
            .map(str::to_owned),
        ids,
        urls,
        open_pdf,
        journal_article: value["document_type"] == "j",
    };
    add_identifier_urls(&mut paper);
    Some(paper)
}

fn crossref_record(value: &Value) -> Option<Paper> {
    let doi = Kind::Doi.normalize(text(&value["DOI"])?)?;
    let arxiv = arxiv_doi(&doi).or_else(|| {
        array(&value["relation"]["has-preprint"]).find_map(|link| {
            let id = text(&link["id"])?;
            match text(&link["id-type"]) {
                Some("doi") => arxiv_doi(id),
                Some("arxiv") => Kind::Arxiv.normalize(id),
                _ => arxiv_id(&Url::parse(id).ok()?),
            }
        })
    });
    let open_pdf = arxiv.is_some();
    let mut paper = Paper {
        title: array(&value["title"]).find_map(text).map(str::to_owned),
        authors: array(&value["author"])
            .filter_map(|name| match (text(&name["given"]), text(&name["family"])) {
                (Some(given), Some(family)) => Some(format!("{family}, {given}")),
                (given, family) => family
                    .or(given)
                    .or_else(|| text(&name["name"]))
                    .map(str::to_owned),
            })
            .collect(),
        year: ["published", "issued", "published-print", "published-online"]
            .into_iter()
            .find_map(|field| year(&value[field]["date-parts"][0][0])),
        venue: array(&value["container-title"])
            .find_map(text)
            .map(str::to_owned),
        ids: Ids {
            arxiv,
            doi: Some(doi),
            ..Ids::default()
        },
        open_pdf,
        journal_article: value["type"] == "journal-article",
        ..Paper::default()
    };
    add_identifier_urls(&mut paper);
    Some(paper)
}

fn arxiv_doi(doi: &str) -> Option<String> {
    let doi = doi.to_ascii_lowercase();
    Kind::Arxiv.normalize(doi.strip_prefix("10.48550/arxiv.")?)
}

fn arxiv_id(url: &Url) -> Option<String> {
    match url.host_str()? {
        "arxiv.org" | "www.arxiv.org" | "export.arxiv.org" => {
            let id = url
                .path()
                .strip_prefix("/abs/")
                .or_else(|| url.path().strip_prefix("/pdf/"))?;
            Kind::Arxiv.normalize(id.strip_suffix(".pdf").unwrap_or(id))
        }
        _ => None,
    }
}

pub(super) fn arxiv_url(kind: &str, id: &str) -> String {
    format!("https://arxiv.org/{kind}/{id}")
}

fn mr_from_url(url: &Url) -> Option<String> {
    match url.host_str()? {
        "mathscinet.ams.org" | "www.ams.org" | "ams.org" => {
            let (_, id) = url.query_pairs().find(|(key, _)| key == "mr")?;
            Kind::Mr.normalize(&id)
        }
        _ => None,
    }
}

fn mr_url(id: &str) -> String {
    format!("https://mathscinet.ams.org/mathscinet-getitem?mr={id}")
}

fn add_identifier_urls(paper: &mut Paper) {
    let doi = paper.ids.doi.as_ref().map(|id| {
        // Retain the DOI prefix/suffix slash for DOI resolvers and escape the suffix.
        let (prefix, suffix) = id.split_once('/').expect("validated DOI");
        item(&format!("https://doi.org/{prefix}/"), suffix).into()
    });
    let mr = paper.ids.mr.as_deref().map(mr_url);
    let arxiv = paper.ids.arxiv.as_deref().map(|id| arxiv_url("abs", id));
    for url in [doi, arxiv, mr].into_iter().flatten() {
        if !paper.urls.contains(&url) {
            paper.urls.push(url);
        }
    }
}

fn child<'a>(node: Node<'a, '_>, ns: &str, name: &str) -> Option<&'a str> {
    node.children()
        .find(|n| n.has_tag_name((ns, name)))
        .and_then(|n| n.text())
        .map(str::trim)
}

fn arxiv(http: &impl Http, parameters: &[(&str, &str)]) -> Result<Vec<Paper>> {
    let url = request(ARXIV, parameters);
    let bytes = http
        .get(&url, "application/atom+xml", METADATA_LIMIT)?
        .context("arXiv 検索先が見つからない")?;
    let document =
        Document::parse(std::str::from_utf8(&bytes)?).context("arXiv の Atom フィードが不正")?;
    let feed = document.root_element();
    ensure!(feed.has_tag_name((ATOM, "feed")), "arXiv のフィードがない");
    let mut papers = Vec::new();
    for entry in feed.children().filter(|n| n.has_tag_name((ATOM, "entry"))) {
        let id_url = child(entry, ATOM, "id").context("arXiv のエントリに ID がない")?;
        let Some(id) = Url::parse(id_url).ok().as_ref().and_then(arxiv_id) else {
            let error = child(entry, ATOM, "summary").unwrap_or(id_url);
            bail!("arXiv のエントリを取得できない: {error}");
        };
        let open_pdf = entry
            .children()
            .filter(|n| n.has_tag_name((ATOM, "link")))
            .any(|link| {
                (link.attribute("title") == Some("pdf")
                    || link.attribute("type") == Some("application/pdf"))
                    && link
                        .attribute("href")
                        .and_then(|href| arxiv_pdf_id(href).ok())
                        .is_some_and(|linked| arxiv_work(&linked) == arxiv_work(&id))
            });
        let mut paper = Paper {
            title: child(entry, ATOM, "title")
                .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" ")),
            authors: entry
                .children()
                .filter(|n| n.has_tag_name((ATOM, "author")))
                .filter_map(|author| child(author, ATOM, "name"))
                .map(str::to_owned)
                .collect(),
            year: child(entry, ATOM, "published").and_then(|date| date.get(..4)?.parse().ok()),
            venue: Some(
                child(entry, ARXIV_NS, "journal_ref")
                    .unwrap_or("arXiv")
                    .into(),
            ),
            ids: Ids {
                arxiv: Some(id),
                doi: child(entry, ARXIV_NS, "doi").and_then(|doi| Kind::Doi.normalize(doi)),
                ..Ids::default()
            },
            open_pdf,
            ..Paper::default()
        };
        add_identifier_urls(&mut paper);
        papers.push(paper);
    }
    Ok(papers)
}

fn arxiv_pdf_id(url: &str) -> Result<String> {
    let url = Url::parse(url)?;
    ensure!(url.path().starts_with("/pdf/"), "arXiv の PDF URL が不正");
    arxiv_id(&url).context("arXiv の PDF ID が不正")
}

pub(super) fn pdf(http: &impl Http, url: &str) -> Result<Vec<u8>> {
    let bytes = http
        .get(&Url::parse(url)?, "application/pdf", PDF_LIMIT)?
        .context("arXiv PDF が見つからない")?;
    ensure!(
        bytes.starts_with(b"%PDF-"),
        "arXiv の応答が PDF 形式ではない"
    );
    Ok(bytes)
}
