//! Assets and structural checks shared by the standalone reading pages.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{ensure, Context, Result};
use base64::Engine;
use html5gum::{DefaultEmitter, Token, Tokenizer};
use regex::Regex;

use crate::review::Language;

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn katex_dir(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    let has_assets =
        |dir: &Path| dir.join("katex.min.css").is_file() && dir.join("katex.min.js").is_file();
    if let Some(dir) = explicit {
        ensure!(
            has_assets(dir),
            "KaTeX assets not found in {}",
            dir.display()
        );
    }
    let mut paths = explicit
        .into_iter()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    if let Some(dir) = std::env::var_os("KATEX_DIR") {
        paths.push(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(&home).join(".cache/fuzukue/katex"));
        paths.push(PathBuf::from(home).join(".cache/labo-skills/katex/0.19.1"));
    }
    let dir = paths.into_iter().find(|dir| has_assets(dir));
    Ok(dir)
}

/// Load locally installed KaTeX and inline available fonts. Empty assets leave
/// the TeX source visible.
pub fn katex(explicit: Option<&Path>) -> Result<(String, String)> {
    let Some(dir) = katex_dir(explicit)? else {
        println!("KaTeX assets unavailable: showing TeX source in the HTML (set KATEX_DIR)");
        return Ok((String::new(), String::new()));
    };
    let css = fs::read_to_string(dir.join("katex.min.css"))?;
    let font = Regex::new(r"url\(fonts/([A-Za-z0-9_-]+)\.woff2\)")?;
    let fallback = Regex::new(r#",url\(fonts/[^)]*\)\s*format\(['"](?:woff|truetype)['"]\)"#)?;
    let old_ie = Regex::new(
        r#"src:url\(fonts/[^)]*\.eot\);src:url\(fonts/[^)]*#iefix\)\s*format\(['"]embedded-opentype['"]\),"#,
    )?;
    let mut missing = BTreeSet::new();
    let css = font.replace_all(&css, |m: &regex::Captures| {
        let path = dir.join("fonts").join(format!("{}.woff2", &m[1]));
        match fs::read(&path) {
            Ok(bytes) => format!(
                "url(data:font/woff2;base64,{})",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
            Err(_) => {
                missing.insert(m[1].to_owned());
                "url()".into()
            }
        }
    });
    let css = old_ie.replace_all(&css, "src:");
    let css = fallback.replace_all(&css, "").into_owned();
    if !missing.is_empty() {
        println!(
            "WARNING: missing KaTeX fonts: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok((css, fs::read_to_string(dir.join("katex.min.js"))?))
}

pub(crate) fn page(
    body: &str,
    title: &str,
    language: Language,
    template: &str,
    katex_path: Option<&Path>,
    extra_script: &str,
) -> Result<String> {
    let (css, js) = katex(katex_path)?;
    let labels = language.labels();
    let title = escape(title);
    let markers = Regex::new(
        r"\{\{(?:BODY|TITLE|LANG|EXTRA_SCRIPT|L:([a-z0-9_]+))\}\}|/\*KATEX_(?:CSS|JS)\*/",
    )?;
    Ok(markers
        .replace_all(template, |m: &regex::Captures<'_>| {
            match &m[0] {
                "{{BODY}}" => body,
                "{{TITLE}}" => &title,
                "{{LANG}}" => language.code(),
                "{{EXTRA_SCRIPT}}" => extra_script,
                "/*KATEX_CSS*/" => &css,
                "/*KATEX_JS*/" => &js,
                marker => {
                    return labels
                        .template_label(&m[1])
                        .map_or_else(|| marker.to_owned(), escape)
                }
            }
            .to_owned()
        })
        .into_owned())
}

/// Findings from checking a saved standalone page.
pub struct Check {
    pub problems: Vec<String>,
    pub math_checked: bool,
}

/// Check HTML ids, links, required review cards, and math/script syntax when
/// the local Node and KaTeX tools are available.
pub fn check(path: &Path, spec: Option<&Path>, katex_path: Option<&Path>) -> Result<Check> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut ids = BTreeMap::new();
    let mut links = BTreeSet::new();
    let mut math = Vec::new();
    let mut scripts = Vec::new();
    let mut capture = None;
    let mut contents = String::new();
    let mut main = false;
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(text.as_bytes(), emitter).flatten() {
        match token {
            Token::StartTag(tag) => {
                if let Some(id) = tag.attributes.get(b"id".as_slice()) {
                    *ids.entry(String::from_utf8_lossy(id).into_owned())
                        .or_insert(0) += 1;
                }
                if let Some(href) = tag.attributes.get(b"href".as_slice()) {
                    let href = String::from_utf8_lossy(href);
                    if let Some(target) = href.strip_prefix('#').filter(|s| !s.is_empty()) {
                        links.insert(target.to_owned());
                    }
                }
                if tag.name == b"main" {
                    main = true;
                }
                let classes = tag
                    .attributes
                    .get(b"class".as_slice())
                    .map(|classes| String::from_utf8_lossy(classes))
                    .unwrap_or_default();
                if capture.is_none() {
                    capture = match tag.name.as_slice() {
                        b"span" if main && classes.split_ascii_whitespace().any(|c| c == "m") => {
                            Some("span")
                        }
                        b"div" if main && classes.split_ascii_whitespace().any(|c| c == "d") => {
                            Some("div")
                        }
                        b"script" if !tag.attributes.contains_key(b"type".as_slice()) => {
                            Some("script")
                        }
                        _ => None,
                    };
                }
            }
            Token::String(text) if capture.is_some() => {
                contents.push_str(&String::from_utf8_lossy(&text))
            }
            Token::EndTag(tag) => {
                if tag.name == b"main" {
                    main = false;
                }
                if let Some(name) = capture.filter(|name| tag.name.as_slice() == name.as_bytes()) {
                    let source = std::mem::take(&mut contents);
                    if name == "script" {
                        scripts.push(source);
                    } else {
                        math.push((source, name == "div"));
                    }
                    capture = None;
                }
            }
            _ => (),
        }
    }

    let mut problems = Vec::new();
    if Regex::new(r"\{\{(?:BODY|TITLE|LANG|EXTRA_SCRIPT|L:[a-z0-9_]+)\}\}|/\*KATEX_(?:CSS|JS)\*/")?
        .is_match(&text)
    {
        problems.push("unreplaced placeholder in page".into());
    }
    for (id, count) in &ids {
        if *count > 1 {
            problems.push(format!("duplicate id {id}"));
        }
    }
    for link in links {
        if !ids.contains_key(&link) {
            problems.push(format!("link #{link} has no target"));
        }
    }
    if let Some(spec) = spec {
        for id in crate::review::card_ids(spec)? {
            if !ids.contains_key(&id) {
                problems.push(format!("no card with id {id}"));
            }
        }
    }

    let mut math_checked = false;
    if let Some(dir) = katex_dir(katex_path)? {
        let js = dir.join("katex.min.js").canonicalize()?;
        let mut node = Command::new("node");
        node.args(["-e", include_str!("assets/check-reading-math.js")])
            .arg(js)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match node.spawn() {
            Ok(mut child) => {
                let input = serde_json::to_vec(&(math, scripts))?;
                let write = child.stdin.take().expect("piped stdin").write_all(&input);
                let output = child.wait_with_output()?;
                write?;
                if !output.status.success() {
                    problems.push(format!(
                        "math/script syntax check failed:\n{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                math_checked = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("run Node for the math check"),
        }
    }
    Ok(Check {
        problems,
        math_checked,
    })
}
