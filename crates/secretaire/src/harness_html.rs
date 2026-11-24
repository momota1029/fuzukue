use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{ensure, Context, Result};
use fuzukue_cards::{section_range, workspace_paths, Card, DeckConfig, HtmlLink};
use regex::Regex;
use serde_yaml::Value;
use url::{ParseError, Url};

use super::{environment, item_card, label, sequence, Renderer, MATH_SPAN};

const MATHJAX: &str = "https://cdn.jsdelivr.net/npm/mathjax@3.2.2/es5/tex-svg-full.js";
const READER_MATHJAX: &str = "https://cdn.jsdelivr.net/npm/mathjax@3.2.2/es5/tex-chtml-full.js";
const CSS: &str = "
body{max-width:72rem;margin:2rem auto;padding:0 1rem;font:1rem/1.65 system-ui,sans-serif}
h1,h2,h3{line-height:1.2} nav{display:flex;gap:1rem;flex-wrap:wrap}
article{margin:1.5rem 0;padding:1rem;border:1px solid #8886;border-radius:.4rem}
article:target,h2:target,h3:target{outline:2px solid #2981d0;scroll-margin-top:1rem}
article h3{margin-top:0} summary,label,input{cursor:pointer} small{display:block}
.contents ul{margin-top:.3rem} .reader{margin-inline:auto;font-family:Georgia,serif}
.reader article{padding:0;border:0} .reader details{padding-inline-start:1em;border-inline-start:2px solid #8886}
mjx-container[display=true]{overflow-x:auto;overflow-y:hidden}
:root{color-scheme:light dark}
";

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn anchor(href: &str, text: &str) -> String {
    format!("<a href=\"{}\">{}</a>", escape(href), escape(text))
}

fn browser_url(value: &str) -> Result<&str> {
    let value = value.trim();
    match Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https" | "mailto") => Ok(value),
        Err(ParseError::RelativeUrlWithoutBase) => Ok(value),
        _ => anyhow::bail!("unsupported HTML URL: {value}"),
    }
}

fn navigation(links: &[HtmlLink], other: Option<(&str, &str)>) -> Result<String> {
    let mut nav = String::new();
    for link in links {
        nav.push_str(&anchor(browser_url(&link.href)?, &link.text));
    }
    if let Some((href, text)) = other.filter(|(href, _)| !links.iter().any(|l| l.href == *href)) {
        nav.push_str(&anchor(href, text));
    }
    Ok(format!("<nav>{nav}</nav>"))
}

fn page(title: &str, language: &str, head: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"{}\"><head><meta charset=\"utf-8\">\n\
        <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
        <title>{}</title><style>{CSS}</style>{head}</head><body>{body}</body></html>\n",
        escape(language),
        escape(title),
    )
}

fn mathjax(cfg: &DeckConfig, reader: bool) -> Result<String> {
    let source =
        cfg.html
            .mathjax
            .as_deref()
            .unwrap_or(if reader { READER_MATHJAX } else { MATHJAX });
    let source = escape(browser_url(source)?);
    let macros: BTreeMap<_, _> = cfg
        .meta
        .macros
        .iter()
        .map(|(key, body)| (key, serde_json::json!(body)))
        .chain(
            cfg.meta
                .macros_with_args
                .iter()
                .map(|(key, (body, args))| (key, serde_json::json!([body, args]))),
        )
        .collect();
    let macros = serde_json::to_string(&macros)?.replace('<', "\\u003c");
    let tags = if reader { "tags: 'ams'," } else { "" };
    Ok(format!(
        r#"<script>window.MathJax = {{
  tex: {{inlineMath: [['$','$'],['\\(','\\)']], displayMath: [['\\[','\\]'],['$$','$$']],
    processEnvironments: true, {tags} macros: {macros}}},
  options: {{skipHtmlTags: ['script','noscript','style','textarea','code']}}
}};</script>
<script defer src="{source}"></script>"#
    ))
}

static INLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\[\[([^\]|\r\n]+)(?:\|([^\]\r\n]*))?\]\]|",
        r"\\(?:[Cc]ref|(?:auto|eq|page)?ref)\*?\{([^{}]+)\}|",
        r"\\(emph|textbf|paragraph)\{([^{}]*)\}|",
        r"\\cite(?:\[([^\]]*)\])?\{([^{}]+)\}|",
        r"`(?P<code>[^`\r\n]+)`|(?P<escaped>\\[_%#&{}])|",
        r"\\[A-Za-z]+|[A-Za-z0-9_:-]+|~",
    ))
    .unwrap()
});
static PARAGRAPHS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").unwrap());

type Targets<'a> = BTreeMap<String, (&'a str, String)>;

fn card_targets<'a>(cards: impl IntoIterator<Item = &'a Card>) -> Targets<'a> {
    let mut targets = BTreeMap::new();
    for card in cards {
        let id = card.id.as_str();
        let title = if card.title.is_empty() {
            id
        } else {
            &card.title
        };
        targets.insert(id.to_owned(), (id, title.to_owned()));
        targets
            .entry(label(card))
            .or_insert_with(|| (id, title.to_owned()));
    }
    targets
}

fn prose(text: &str, targets: &Targets<'_>, here: &BTreeSet<&str>, bare: bool) -> String {
    let mut html = String::new();
    let mut start = 0;
    let link = |key: &str, caption: Option<&str>| {
        let (id, title) = targets.get(key)?;
        let file = if here.contains(id) { "" } else { "index.html" };
        Some(anchor(&format!("{file}#{id}"), caption.unwrap_or(title)))
    };
    for math in MATH_SPAN.find_iter(text).map(Some).chain([None]) {
        let end = math.as_ref().map_or(text.len(), |m| m.start());
        let mut pos = start;
        for token in INLINE.captures_iter(&text[start..end]) {
            let whole = token.get(0).unwrap();
            let at = start + whole.start();
            html.push_str(&escape(&text[pos..at]));
            let replacement = if let Some(key) = token.get(1) {
                link(key.as_str(), token.get(2).map(|m| m.as_str()))
            } else if let Some(keys) = token.get(3) {
                keys.as_str()
                    .split(',')
                    .map(|key| link(key.trim(), None))
                    .collect::<Option<Vec<_>>>()
                    .map(|links| links.join(", "))
            } else if let Some(cmd) = token.get(4) {
                let tag = if cmd.as_str() == "emph" {
                    "em"
                } else {
                    "strong"
                };
                Some(format!("<{tag}>{}</{tag}>", escape(&token[5])))
            } else if let Some(key) = token.get(7) {
                let note = token
                    .get(6)
                    .map(|m| format!(", {}", escape(m.as_str())))
                    .unwrap_or_default();
                Some(format!("[{}{note}]", escape(key.as_str())))
            } else if let Some(code) = token.name("code") {
                Some(format!("<code>{}</code>", escape(code.as_str())))
            } else if let Some(escaped) = token.name("escaped") {
                Some(escape(&escaped.as_str()[1..]))
            } else if whole.as_str() == "~" {
                Some(" ".into())
            } else if bare
                && !whole.as_str().starts_with('\\')
                && !text[..at].ends_with('{')
                && !text[start + whole.end()..].starts_with('}')
            {
                link(whole.as_str(), None)
            } else {
                None
            };
            html.push_str(&replacement.unwrap_or_else(|| escape(whole.as_str())));
            pos = start + whole.end();
        }
        html.push_str(&escape(&text[pos..end]));
        if let Some(math) = math {
            html.push_str(&escape(math.as_str()).replace('\n', "&#10;"));
            start = math.end();
        }
    }
    PARAGRAPHS
        .split(html.trim())
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{p}</p>"))
        .collect()
}

fn local_sections<'a>(
    structure: &'a Value,
    appendix: &'a str,
) -> impl Iterator<Item = (&'a str, &'a Value)> {
    sequence(&structure["sections"])
        .map(|section| {
            (
                section["title"].as_str().unwrap_or_default(),
                &section["items"],
            )
        })
        .chain((!structure["appendix"].is_null()).then_some((appendix, &structure["appendix"])))
}

fn viewer<'a>(
    render: &'a Renderer<'a>,
    structure: &Value,
    title: &str,
    appendix: &str,
    nav: &str,
) -> (String, Targets<'a>) {
    let graph = render.graph;
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for (heading, items) in local_sections(structure, appendix) {
        let ids: Vec<_> = sequence(items)
            .filter(|item| item["proof_of"].is_null())
            .filter_map(item_card)
            .map(|id| graph.cards[id].id.as_str())
            .filter(|id| seen.insert(*id))
            .collect();
        groups.push((heading, ids));
    }
    let remaining: Vec<_> = graph
        .cards
        .values()
        .filter(|card| !card.external)
        .map(|card| card.id.as_str())
        .filter(|id| seen.insert(*id))
        .collect();
    groups.push(("Not in structure", remaining));
    let external: BTreeSet<_> = graph
        .cards
        .values()
        .filter(|card| !card.external)
        .flat_map(|card| &card.links)
        .filter_map(|link| graph.cards.get(&link.to))
        .filter(|card| card.external)
        .map(|card| card.id.as_str())
        .collect();
    seen.extend(external.iter().copied());
    groups.push(("External cards", external.into_iter().collect()));

    let targets = card_targets(seen.iter().map(|id| &graph.cards[*id]));
    let mut contents = String::new();
    let mut articles = String::new();
    for (heading, ids) in groups.into_iter().filter(|(_, ids)| !ids.is_empty()) {
        write!(
            contents,
            "<details open><summary>{}</summary><ul>",
            escape(heading)
        )
        .unwrap();
        for id in ids {
            let card = &graph.cards[id];
            let href = format!("#{id}");
            writeln!(contents, "<li>{}</li>", anchor(&href, id)).unwrap();
            writeln!(
                articles,
                "<article id=\"{}\"><h3>{}</h3><small><code>{}</code> · {} {}</small>",
                escape(id),
                escape(if card.title.is_empty() {
                    id
                } else {
                    &card.title
                }),
                escape(id),
                escape(&card.kind),
                escape(&card.status)
            )
            .unwrap();
            for name in ["Statement", "Proof", "Shortcuts", "Notes"] {
                if let Some(range) = section_range(&card.body, name) {
                    write!(
                        articles,
                        "<h4>{name}</h4>{}",
                        prose(&card.body[range], &targets, &seen, true)
                    )
                    .unwrap();
                }
            }
            if card.statement.is_none() && card.proof.is_none() {
                articles.push_str(&prose(&card.body, &targets, &seen, false));
            }
            let dependencies: Vec<_> = card
                .links
                .iter()
                .filter(|link| link.rel == "requires")
                .map(|link| {
                    if seen.contains(link.to.as_str()) {
                        anchor(&format!("#{}", link.to), &link.to)
                    } else {
                        escape(&link.to)
                    }
                })
                .collect();
            if !dependencies.is_empty() {
                writeln!(articles, "<p>Uses {}</p>", dependencies.join(", ")).unwrap();
            }
            articles.push_str("</article>");
        }
        contents.push_str("</ul></details>");
    }
    let script = r#"<script>document.getElementById('search').addEventListener('input', e => {
  const query = e.target.value.toLowerCase();
  for (const card of document.querySelectorAll('article')) {
    card.hidden = !card.textContent.toLowerCase().includes(query);
  }
  for (const link of document.querySelectorAll('.contents a')) {
    const card = document.getElementById(link.getAttribute('href').slice(1));
    link.parentElement.hidden = card.hidden;
  }
});</script>"#;
    (
        format!(
        "{nav}<main><h1>{}</h1><label>Search cards <input type=\"search\" id=\"search\"></label>\
         <div class=\"contents\">{contents}</div>{articles}</main>{script}", escape(title),
    ),
        targets,
    )
}

fn reader(
    render: &Renderer<'_>,
    structure: &Value,
    title: &str,
    appendix: &str,
    nav: &str,
    cfg: &DeckConfig,
    targets: &Targets<'_>,
) -> String {
    let here: BTreeSet<_> = local_sections(structure, appendix)
        .flat_map(|(_, items)| sequence(items))
        .filter(|item| item["proof_of"].is_null())
        .filter_map(item_card)
        .filter_map(|id| render.graph.cards.get(id))
        .filter(|card| !is_author_proof(card))
        .map(|card| card.id.as_str())
        .collect();
    let mut contents = String::new();
    let mut body = String::new();
    let proof_title = if render.language == "ja" {
        "証明"
    } else {
        "Proof"
    };
    for (n, (heading, items)) in local_sections(structure, appendix).enumerate() {
        if items.as_sequence().is_none_or(Vec::is_empty) {
            continue;
        }
        let id = format!("section:{}", n + 1);
        write!(contents, "<li>{}</li>", anchor(&format!("#{id}"), heading)).unwrap();
        write!(body, "<h2 id=\"{id}\">{}</h2>", escape(heading)).unwrap();
        for item in sequence(items) {
            if let Some(subsection) = item["subsection"].as_str() {
                write!(body, "<h3>{}</h3>", escape(subsection)).unwrap();
                continue;
            }
            let id = item_card(item).expect("validated outline");
            let card = &render.graph.cards[id];
            let author = render.author_proofs.get(id).copied();
            if item["proof_of"].is_string() {
                let link = anchor(
                    &format!("#{id}"),
                    if card.title.is_empty() {
                        id
                    } else {
                        &card.title
                    },
                );
                write!(
                    body,
                    "<article id=\"proof:{}\"><h3>{proof_title} · {link}</h3>",
                    escape(id)
                )
                .unwrap();
                body.push_str(&proof_html(render, card, author, targets, &here));
                body.push_str("</article>");
                continue;
            }
            if is_author_proof(card) {
                continue;
            }
            write!(body, "<article id=\"{}\">", escape(id)).unwrap();
            if environment(&card.kind).is_some() {
                let title = if card.title.is_empty() {
                    id
                } else {
                    &card.title
                };
                write!(body, "<h3>{}</h3>", escape(title)).unwrap();
            }
            if let Some(part) = card.prose.as_ref().or(card.statement.as_ref()) {
                body.push_str(&prose(&part.text, targets, &here, card.prose.is_none()));
            }
            match item["proof"]
                .as_str()
                .unwrap_or(if author.is_some() { "author" } else { "fact" })
            {
                "appendix" => body.push_str("<p><em>Proof in the appendix.</em></p>"),
                "later" => body.push_str(&anchor(&format!("#proof:{id}"), proof_title)),
                mode @ ("author" | "fact") => {
                    let author = author.filter(|_| mode == "author");
                    let proof = proof_html(render, card, author, targets, &here);
                    if !proof.is_empty() {
                        write!(
                            body,
                            "<details open><summary>{proof_title}</summary>{proof}</details>"
                        )
                        .unwrap();
                    }
                }
                _ => (),
            }
            body.push_str("</article>");
        }
    }
    let size = cfg.html.reader_font_px.unwrap_or(21);
    let width = cfg.html.reader_width_em.unwrap_or(34);
    format!(
        "<main class=\"reader\" style=\"font-size:{size}px;max-width:{width}em\">{nav}\
         <h1>{}</h1><ol>{contents}</ol>{body}</main>",
        escape(title),
    )
}

fn is_author_proof(card: &Card) -> bool {
    matches!(card.kind.as_str(), "explanation" | "expl") && card.role.as_deref() == Some("proof")
}

fn proof_html(
    render: &Renderer<'_>,
    card: &Card,
    author: Option<&Card>,
    targets: &Targets<'_>,
    here: &BTreeSet<&str>,
) -> String {
    if let Some(author) = author {
        return author
            .prose
            .as_ref()
            .map_or_else(String::new, |p| prose(&p.text, targets, here, false));
    }
    card.proof.as_ref().map_or_else(String::new, |part| {
        let (text, _) = render.fact_proof(card, part);
        prose(&text, targets, here, true)
    })
}

fn site_index(
    workspace: &Path,
    out: &Path,
    cfg: &DeckConfig,
    language: &str,
) -> Result<Option<(PathBuf, String)>> {
    let Some(index) = &cfg.html.html_index else {
        return Ok(None);
    };
    let site = out
        .parent()
        .context("html_index needs a parent directory")?;
    let path = output_file(workspace, site, "index.html")?;
    let mut viewers = BTreeSet::from([out
        .file_name()
        .context("viewer has no name")?
        .to_string_lossy()
        .into_owned()]);
    if site.is_dir() {
        for entry in std::fs::read_dir(site)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .path()
                    .join("index.html")
                    .symlink_metadata()
                    .is_ok_and(|m| m.is_file())
            {
                viewers.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let mut items = String::new();
    for viewer in viewers {
        // Encode a single directory component for a relative browser URL.
        let mut encoded = String::new();
        for byte in viewer.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                encoded.push(byte as char);
            } else {
                write!(encoded, "%{byte:02X}").unwrap();
            }
        }
        writeln!(
            items,
            "<li>{}</li>",
            anchor(&format!("{encoded}/index.html"), &viewer)
        )
        .unwrap();
    }
    let title = index.title.as_deref().unwrap_or("Card harness");
    let nav = navigation(&index.links, None)?;
    let body = format!(
        "<main><h1>{}</h1><ul>{items}</ul>{nav}</main>",
        escape(title)
    );
    Ok(Some((path, page(title, language, "", &body))))
}

fn output_file(workspace: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let path = workspace_paths::resolve(workspace, workspace, &dir.join(name))?;
    ensure!(
        path == dir.join(name) && (!path.exists() || path.is_file()),
        "HTML output must be a regular file: {}",
        dir.join(name).display()
    );
    Ok(path)
}

pub(super) fn export(
    workspace: &Path,
    deck: &Path,
    cfg: &DeckConfig,
    render: &Renderer<'_>,
    structure: &Value,
    title: &str,
    appendix: &str,
) -> Result<Vec<(PathBuf, String)>> {
    let out = cfg.html_dir(workspace, deck)?;
    let index = output_file(workspace, &out, "index.html")?;
    let read = output_file(workspace, &out, "read.html")?;
    let nav = navigation(&cfg.html.html_links, Some(("read.html", "Reading page")))?;
    let read_nav = navigation(&[], Some(("index.html", "Cards")))?;
    let (viewer, targets) = viewer(render, structure, title, appendix, &nav);
    let reader = reader(render, structure, title, appendix, &read_nav, cfg, &targets);
    let language = render.language;
    let mut pages = vec![
        (index, page(title, language, &mathjax(cfg, false)?, &viewer)),
        (read, page(title, language, &mathjax(cfg, true)?, &reader)),
    ];
    pages.extend(site_index(workspace, &out, cfg, language)?);
    Ok(pages)
}
