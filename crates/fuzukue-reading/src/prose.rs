//! Standalone HTML for prose written in the Reader Markdown dialect.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use pulldown_cmark::{html::push_html, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;

use crate::html::{escape, page};
use crate::review::{valid_slug, Language};

const TEMPLATE: &str = include_str!("assets/prose-page-template.html");
const OPTIONS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_MATH);

#[derive(serde::Deserialize)]
struct Frontmatter {
    title: String,
    slug: String,
    language: Option<String>,
}

/// Render authored Markdown, with figures and optional KaTeX embedded.
pub fn standalone(
    source: &Path,
    language: Option<Language>,
    katex: Option<&Path>,
    template: Option<&Path>,
) -> Result<String> {
    let input = fs::read_to_string(source).with_context(|| format!("read {}", source.display()))?;
    let input = input.replace("\r\n", "\n");
    let (meta, markdown) = input
        .strip_prefix("+++\n")
        .and_then(|text| {
            text.split_once("\n+++\n")
                .or_else(|| text.strip_suffix("\n+++").map(|head| (head, "")))
        })
        .context("missing +++ frontmatter (title, slug, language)")?;
    let meta: Frontmatter = toml::from_str(meta).context("invalid prose frontmatter")?;
    ensure!(!meta.title.trim().is_empty(), "frontmatter needs a title");
    ensure!(
        valid_slug(&meta.slug),
        "invalid Reader slug {:?}",
        meta.slug
    );
    let language = language.map_or_else(
        || Language::parse(meta.language.as_deref().unwrap_or("ja")),
        Ok,
    )?;
    let base = source.parent().unwrap_or(Path::new("."));
    let (markdown, extensions) = preprocess(markdown, base, &meta.slug)?;
    let rendered = render(&markdown, &extensions, false)?;
    let body = format!("<h1>{}</h1>\n{rendered}", escape(&meta.title));
    let custom = template.map(fs::read_to_string).transpose()?;
    page(
        &body,
        &meta.title,
        language,
        custom.as_deref().unwrap_or(TEMPLATE),
        katex,
        "",
    )
}

struct Extensions {
    marker: String,
    blocks: Vec<String>,
}

impl Extensions {
    fn push(&mut self, markdown: &mut String, block: String) {
        use std::fmt::Write as _;
        writeln!(markdown, "\n<!--{}{}-->\n", self.marker, self.blocks.len()).unwrap();
        self.blocks.push(block);
    }

    fn get(&self, marker: &str) -> Option<&str> {
        let index = marker
            .strip_prefix("<!--")?
            .strip_prefix(&self.marker)?
            .trim_end()
            .strip_suffix("-->")?
            .parse::<usize>()
            .ok()?;
        self.blocks.get(index).map(String::as_str)
    }
}

fn preprocess(markdown: &str, base: &Path, slug: &str) -> Result<(String, Extensions)> {
    let mut marker = "reading-block-".to_owned();
    while markdown.contains(&marker) {
        marker.push('-');
    }
    let mut extensions = Extensions {
        marker,
        blocks: Vec::new(),
    };
    let anchor = Regex::new(r" \{#([^}]*)\}$")?;
    let anchor_id = Regex::new(r"^[A-Za-z][A-Za-z0-9_-]{0,63}$")?;
    let asset = Regex::new(r"^!\[([^\]]*)\]\(asset:([A-Za-z0-9._-]+\.svg)\)$")?;
    let protected: Vec<_> = Parser::new_ext(markdown, OPTIONS)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(
                event,
                Event::Start(Tag::CodeBlock(_)) | Event::DisplayMath(_)
            )
            .then_some(range)
        })
        .collect();
    let mut protected = protected.iter().peekable();
    let mut offset = 0;
    let mut result = String::new();
    let mut opened = Vec::new();
    let mut anchors = BTreeSet::new();
    for line in markdown.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        while protected.peek().is_some_and(|range| range.end <= start) {
            protected.next();
        }
        if protected.peek().is_some_and(|range| range.start < offset) {
            result.push_str(line);
            continue;
        }
        let trimmed = line.trim();
        if let Some(title) = trimmed.strip_prefix("::: details ") {
            let (title, attr) = if let Some(parts) = anchor.captures(title) {
                let id = parts.get(1).unwrap().as_str();
                ensure!(
                    anchor_id.is_match(id) && !id.starts_with("section-"),
                    "invalid details anchor: {id}"
                );
                ensure!(anchors.insert(id), "duplicate details anchor: {id}");
                (
                    &title[..parts.get(0).unwrap().start()],
                    format!(r#" id="{id}""#),
                )
            } else {
                (title, String::new())
            };
            opened.push(title);
            let summary = render(title, &extensions, true)?;
            extensions.push(
                &mut result,
                format!(r#"<details class="proof-details"{attr}><summary>{summary}</summary>"#),
            );
        } else if trimmed == ":::" {
            ensure!(opened.pop().is_some(), "unexpected details block end");
            extensions.push(&mut result, "</details>".into());
        } else if trimmed.starts_with("::: ") {
            bail!("unsupported block: {trimmed}");
        } else if trimmed.starts_with("$$") {
            bail!("unclosed display mathematics");
        } else if let Some(parts) = asset.captures(trimmed) {
            extensions.push(&mut result, figure(base, slug, &parts[2], &parts[1])?);
        } else {
            result.push_str(line);
        }
    }
    if let Some(title) = opened.last() {
        bail!("unclosed details block: {title}");
    }
    Ok((result, extensions))
}

fn figure(base: &Path, slug: &str, name: &str, alt: &str) -> Result<String> {
    let figures = base.join("figures");
    let shared_slug = slug.strip_suffix("-en").unwrap_or(slug);
    let path = [
        figures.join(slug).join(name),
        figures.join(shared_slug).join(name),
        figures.join(name),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .with_context(|| format!("figure not found: figures/{slug}/{name}"))?;
    let svg = fs::read_to_string(&path)?;
    let svg = Regex::new(r"(?s)<\?xml[^>]*\?>")?.replace_all(&svg, "");
    Ok(format!(
        "<figure>{}<figcaption>{}</figcaption></figure>",
        svg.trim(),
        escape(alt)
    ))
}

fn render(markdown: &str, extensions: &Extensions, summary: bool) -> Result<String> {
    let mut events = Vec::new();
    let mut section = 0;
    for event in Parser::new_ext(markdown, OPTIONS) {
        let event = match event {
            Event::Html(raw) | Event::InlineHtml(raw) => {
                match extensions.get(&raw).filter(|_| !summary) {
                    Some(block) => Event::Html(CowStr::Borrowed(block)),
                    None => Event::Text(raw),
                }
            }
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) if !summary => {
                section += 1;
                Event::Html(format!(r#"<h2 id="section-{section}">"#).into())
            }
            Event::InlineMath(tex) => math(&tex, false),
            Event::DisplayMath(tex) => math(&tex, true),
            Event::Start(Tag::Link { ref dest_url, .. }) => {
                let url = dest_url.as_ref();
                let safe = ["https://", "http://", "mailto:", "#"]
                    .iter()
                    .any(|prefix| url.starts_with(prefix))
                    || (!url.starts_with("//")
                        && url
                            .chars()
                            .all(|c| c.is_alphanumeric() || "./_#-".contains(c)));
                ensure!(safe, "unsupported link target: {url}");
                event
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                bail!("unsupported image target: {dest_url} (use an SVG asset on its own line)")
            }
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) if summary => continue,
            _ => event,
        };
        events.push(event);
    }
    let mut output = String::new();
    push_html(&mut output, events.into_iter());
    Ok(output)
}

fn math(tex: &str, display: bool) -> Event<'static> {
    let (tag, class) = if display { ("div", "d") } else { ("span", "m") };
    let tex = escape(tex.trim());
    Event::InlineHtml(format!(r#"<{tag} class="{class}">{tex}</{tag}>"#).into())
}
