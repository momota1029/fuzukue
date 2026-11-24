//! 用語の宣言、照合、依存と文書順の検査。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_yaml::Value;

use crate::{mask_code, Diagnostic, Graph, LintConfig, Severity};

pub(crate) mod tex;
pub mod prh;

const BEFORE: &str = "undefined-term/used-before-definition";
const DEPENDENCY: &str = "undefined-term/missing-dependency";

pub(super) fn infer_terms(statement_tex: Option<&str>, markdown: &str) -> Vec<String> {
    let mut terms = Vec::new();
    tex::prose(
        statement_tex.unwrap_or_else(|| statement(markdown)),
        |term| {
            let candidate = term.split_whitespace().collect::<Vec<_>>().join(" ");
            // マクロや数式を含む強調、文や箇条書きの見出しを定義語にしない。
            if candidate.chars().any(char::is_alphabetic)
                && candidate
                    .chars()
                    .all(|c| c.is_alphanumeric() || " -‐‑'’".contains(c))
                && !terms.contains(&candidate)
            {
                terms.push(candidate);
            }
        },
    );
    terms
}

fn statement(markdown: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    let mut first_heading = markdown.len();
    let mut named_statement = false;
    for line in markdown.split_inclusive('\n') {
        if line.trim().eq_ignore_ascii_case("## Statement") {
            start = offset + line.len();
            named_statement = true;
        } else if line.trim_start().starts_with("## ") {
            if named_statement {
                return &markdown[start..offset];
            }
            first_heading = first_heading.min(offset);
        }
        offset += line.len();
    }
    if named_statement {
        &markdown[start..]
    } else {
        &markdown[..first_heading]
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

static JAPANESE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Han}\p{Hiragana}\p{Katakana}]").unwrap());
static HYPHENATED_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)[a-zA-Z0-9_]+(?:-[a-zA-Z0-9_]+)+(?-u:\b)").unwrap());

fn is_word(c: char) -> bool {
    c == '_'
        || (c.is_alphanumeric() && (c.is_ascii() || !JAPANESE.is_match(c.encode_utf8(&mut [0; 4]))))
}

struct Term<'a> {
    text: &'a str,
    key: &'a str,
    substring: bool,
    definitions: &'a [&'a str],
}

struct Terms<'a> {
    by_first_char: HashMap<char, Vec<Term<'a>>>,
}

impl<'a> Terms<'a> {
    fn new(terms: &'a BTreeMap<String, (&str, Vec<&str>)>) -> Self {
        let mut by_first_char: HashMap<char, Vec<Term<'_>>> = HashMap::new();
        for (key, (text, definitions)) in terms {
            by_first_char
                .entry(key.chars().next().unwrap())
                .or_default()
                .push(Term {
                    text,
                    key,
                    substring: JAPANESE.is_match(key),
                    definitions,
                });
        }
        for bucket in by_first_char.values_mut() {
            bucket.sort_by_key(|term| std::cmp::Reverse(term.key.len()));
        }
        Self { by_first_char }
    }

    fn used(&self, prose: &str) -> BTreeMap<&str, &Term<'a>> {
        let mut used = BTreeMap::new();
        let mut end = 0;
        let mut previous = None;
        for (start, c) in prose.char_indices() {
            if start >= end {
                let tail = &prose[start..];
                if let Some(bucket) = self.by_first_char.get(&c) {
                    if let Some(term) = bucket.iter().find(|term| {
                        tail.strip_prefix(term.key).is_some_and(|rest| {
                            term.substring
                                || (!previous.is_some_and(is_word)
                                    && !rest.chars().next().is_some_and(is_word))
                        })
                    }) {
                        used.insert(term.key, term);
                        end = start + term.key.len();
                    }
                }
            }
            previous = Some(c);
        }
        used
    }
}

fn positions(root: &Path) -> HashMap<String, usize> {
    let Some(structure) = std::fs::read_to_string(root.join("structure.yaml"))
        .ok()
        .and_then(|text| serde_yaml::from_str::<Value>(&text).ok())
    else {
        return HashMap::new();
    };
    let mut positions = HashMap::new();
    let items = structure["sections"]
        .as_sequence()
        .into_iter()
        .flatten()
        .flat_map(|section| section["items"].as_sequence().into_iter().flatten())
        .chain(structure["appendix"].as_sequence().into_iter().flatten());
    let ids = items.filter_map(|item| item.as_str().or_else(|| item["card"].as_str()));
    for (position, id) in ids.enumerate() {
        positions.entry(id.to_owned()).or_insert(position);
    }
    positions
}

fn card_prose(body: &str) -> String {
    let body = mask_code(body, '\0');
    let mut prose = String::new();
    // These headings include the synthetic ones added for folder cards.
    for section in body.split_inclusive('\n') {
        if matches!(
            section.trim(),
            "## Statement" | "## Proof" | "## Notes" | "## Shortcuts"
        ) {
            prose.push_str("\0\n");
        } else {
            prose.push_str(section);
        }
    }
    tex::prose(&prose, |_| {})
}

pub(super) fn check(
    graph: &Graph,
    lint: &LintConfig,
    project: Option<&Path>,
    includes: impl Fn(&crate::Card) -> bool,
) -> anyhow::Result<Vec<Diagnostic>> {
    let glossary = lint.glossary.clone().or_else(|| {
        let project = project.or_else(|| graph.root.ancestors().find(|path| fuzukue_proto::compat::workspace_config(path).is_file()));
        std::iter::once(graph.root.as_path()).chain(project)
            .flat_map(|root| ["prh.yml", "prh.yaml"].map(|name| root.join(name)))
            .find(|path| path.is_file())
    });
    let glossary = glossary.as_deref().map(prh::Dictionary::rules).transpose()?.unwrap_or_default();
    let mut definitions: BTreeMap<String, (&str, Vec<&str>)> = BTreeMap::new();
    for card in graph.cards.values() {
        for term in &card.defines {
            let key = normalize(term);
            if !key.is_empty() {
                let (_, ids) = definitions.entry(key).or_insert((term.trim(), Vec::new()));
                if ids.last() != Some(&card.id.as_str()) {
                    ids.push(&card.id);
                }
            }
        }
    }
    if definitions.is_empty() && glossary.is_empty() {
        return Ok(Vec::new());
    }
    let terms = Terms::new(&definitions);
    let positions = positions(&graph.root);
    let deck_ignored: HashSet<_> = lint.ignore_terms.iter().map(|s| normalize(s)).collect();
    let mut diagnostics = Vec::new();
    for card in graph.cards.values().filter(|card| !card.external && includes(card)) {
        let prose = card_prose(&card.body);
        let prose = HYPHENATED_ID.replace_all(&prose, |captures: &regex::Captures<'_>| {
            let id = captures.get(0).unwrap().as_str();
            if graph.cards.contains_key(id) && !definitions.contains_key(&id.to_ascii_lowercase()) {
                Cow::Borrowed("\0")
            } else {
                Cow::Owned(id.to_owned())
            }
        });
        let ignored: HashSet<_> = card.lint_ignore.iter().map(|s| normalize(s)).collect();
        for rule in &glossary {
            let key = normalize(&rule.expected);
            if deck_ignored.contains(&key) || ignored.contains(&key) || ignored.contains("term-consistency/prh") {
                continue;
            }
            let mut seen = HashSet::new();
            // A masked construct is a boundary. Even a regex with "." cannot
            // accidentally join words on either side of a formula or citation.
            for part in prose.split('\0').flat_map(|part| part.split("\n\n")) {
                for (start, end, expected) in rule.changes(part)? {
                    let found = &part[start..end];
                    if !seen.insert((found.to_owned(), expected.clone())) {
                        continue;
                    }
                    let mut message = format!("「{found}」の表記を「{expected}」にそろえる。");
                    if let Some(hint) = &rule.hint {
                        message.push(' ');
                        message.push_str(hint);
                    }
                    let mut diagnostic = Diagnostic::new(Severity::Warning, Some("term-consistency/prh"), card.path.clone(), message);
                    diagnostic.textlint_rule = Some("prh".into());
                    diagnostics.push(diagnostic);
                }
            }
        }
        let prose = normalize(&prose);
        let used = terms.used(&prose);
        let mut relevant = used
            .into_values()
            .filter(|term| {
                !deck_ignored.contains(term.key)
                    && !ignored.contains(term.key)
                    && !term.definitions.contains(&card.id.as_str())
            })
            .peekable();
        if relevant.peek().is_none() {
            continue;
        }
        let context: HashSet<_> = graph
            .context(std::slice::from_ref(&card.id))
            .into_iter()
            .collect();
        for term in relevant {
            let reachable: Vec<_> = term
                .definitions
                .iter()
                .copied()
                .filter(|id| context.contains(*id))
                .collect();
            let mut warn = |code, message| {
                diagnostics.push(Diagnostic::new(Severity::Warning, Some(code), card.path.clone(), message))
            };
            if reachable.is_empty() {
                warn(
                    DEPENDENCY,
                    format!(
                        "用語 `{}` の定義カードに依存していない ({})",
                        term.text,
                        term.definitions.join(", ")
                    ),
                );
            }
            let candidates = if reachable.is_empty() {
                term.definitions
            } else {
                &reachable
            };
            if let Some(&position) = positions.get(&card.id) {
                // 到達できる外部定義は現在の文書の順序から独立している。
                if reachable.iter().any(|id| graph.cards[*id].external) {
                    continue;
                }
                let ordered = candidates
                    .iter()
                    .copied()
                    .filter(|id| !graph.cards[*id].external)
                    .filter_map(|id| positions.get(id).map(|&pos| (pos, id)));
                if let Some((def_position, id)) = ordered.min() {
                    if def_position > position {
                        warn(
                            BEFORE,
                            format!(
                                "用語 `{}` を定義カード `{id}` より先に使用している",
                                term.text
                            ),
                        );
                    }
                }
            }
        }
    }
    Ok(diagnostics)
}

#[cfg(test)]
mod tests;
