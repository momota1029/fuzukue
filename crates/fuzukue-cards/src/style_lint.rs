//! Deterministic prose checks over the shared TeX scanner.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;

use crate::term_lint::tex;
use crate::{closing_backticks, term_lint, DeckConfig, Diagnostic, Graph, Severity};

mod native;
mod external;

const AI: &str = "style/ai-phrase";
const LONG: &str = "style/long-sentence";
const ENDING: &str = "style/repeated-ending";
const COMMA: &str = "style/comma-density";
const NEGATION: &str = "style/rhetorical-negation";
const HEDGING: &str = "style/hedging";

const DEFAULT_EXCLUDES: &[&str] = &["cards/design/**", "memos/**", "inbox/**", "threads/**"];

#[derive(Default, Deserialize)]
struct Workspace {
    #[serde(default)]
    style_lint: WorkspaceStyleLint,
    #[serde(default)]
    style: WorkspaceStyle,
}

#[derive(Default, Deserialize)]
struct WorkspaceStyle {
    textlint: Option<String>,
}

#[derive(Deserialize)]
#[serde(default)]
struct WorkspaceStyleLint {
    enabled: bool,
    exclude: Vec<String>,
}

impl Default for WorkspaceStyleLint {
    fn default() -> Self {
        Self { enabled: true, exclude: Vec::new() }
    }
}

fn exclusion_globs(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.as_ref();
        set.add(GlobBuilder::new(pattern.strip_prefix("./").unwrap_or(pattern))
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid [style_lint].exclude glob: {pattern}"))?);
    }
    Ok(set.build()?)
}

#[derive(Debug, Default, Deserialize)]
pub struct StyleConfig {
    #[serde(default)]
    pub extra_phrases: Vec<String>,
    #[serde(default)]
    pub disable: Vec<String>,
    pub phrases: Option<Vec<String>>,
    pub repeated_phrases: Option<Vec<String>>,
    pub max_ja_chars: Option<usize>,
    pub max_en_words: Option<usize>,
    pub max_ja_commas: Option<usize>,
    pub max_kanji_chars: Option<usize>,
    pub max_ascii_commas: Option<usize>,
    pub period_mark: Option<String>,
    pub prefer_register: Option<native::Register>,
}

impl StyleConfig {
    fn with_local(mut self, root: &Path) -> Result<Self> {
        let path = root.join("style.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(self),
            Err(e) => return Err(e).with_context(|| format!("{} を読めない", path.display())),
        };
        let local = match Self::parse(&text) {
            Ok(local) => local,
            // Preexisting writing harnesses use this filename for a manual rule ledger.
            Err(_) if legacy_ledger(&text) => return Ok(self),
            Err(e) => return Err(e).with_context(|| format!("{} を読めない", path.display())),
        };
        self.overlay(local);
        Ok(self)
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        Self::table(text)?.try_into()
    }

    fn table(text: &str) -> Result<toml::Table, toml::de::Error> {
        #[derive(Deserialize)]
        struct File {
            style: Option<toml::Table>,
            #[serde(flatten)]
            settings: toml::Table,
        }
        let file: File = toml::from_str(text)?;
        Ok(file.style.unwrap_or(file.settings))
    }

    pub(crate) fn overlay_pack(&mut self, text: &str) -> Result<(), toml::de::Error> {
        let table = Self::table(text)?;
        let has_extra = table.contains_key("extra_phrases");
        let has_disable = table.contains_key("disable");
        let later = table.try_into()?;
        if has_extra {
            self.extra_phrases.clear();
        }
        if has_disable {
            self.disable.clear();
        }
        self.overlay(later);
        Ok(())
    }

    pub(crate) fn overlay(&mut self, later: Self) {
        self.extra_phrases.extend(later.extra_phrases);
        self.disable.extend(later.disable);
        self.phrases = later.phrases.or(self.phrases.take());
        self.repeated_phrases = later.repeated_phrases.or(self.repeated_phrases.take());
        self.max_ja_chars = later.max_ja_chars.or(self.max_ja_chars);
        self.max_en_words = later.max_en_words.or(self.max_en_words);
        self.max_ja_commas = later.max_ja_commas.or(self.max_ja_commas);
        self.max_kanji_chars = later.max_kanji_chars.or(self.max_kanji_chars);
        self.max_ascii_commas = later.max_ascii_commas.or(self.max_ascii_commas);
        self.period_mark = later.period_mark.or(self.period_mark.take());
        self.prefer_register = later.prefer_register.or(self.prefer_register);
    }
}

fn legacy_ledger(text: &str) -> bool {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    lines
        .next()
        .is_some_and(|line| line.split('#').next().unwrap().trim() == "[[rule]]")
        && lines
            .filter(|line| line.starts_with('['))
            .all(|line| line.split('#').next().unwrap().trim() == "[[rule]]")
}

#[derive(Deserialize)]
struct Defaults {
    phrases: Vec<String>,
    repeated_phrases: Vec<String>,
    max_ja_chars: usize,
    max_en_words: usize,
    max_ja_commas: usize,
    max_kanji_chars: usize,
    max_ascii_commas: usize,
    period_mark: String,
    prefer_register: native::Register,
}

static DEFAULTS: LazyLock<Defaults> = LazyLock::new(|| {
    toml::from_str(include_str!("../style/default.toml")).expect("embedded style defaults")
});
static JAPANESE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Han}\p{Hiragana}\p{Katakana}]").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}+(?:['’‐‑-]\p{L}+)*").unwrap());
static ENDINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(であった|である|であろう|でない|する|した|しない|される|された|できる|となる|です|ます|でした|ました)$").unwrap()
});
static NEGATION_JA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"だけでなく|のみでなく|ではなく").unwrap());
static NEGATION_EN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bnot\s+[^;:]+?\s*,?\s+\bbut\b\s+[\p{L}\x00]").unwrap());
static HEDGES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)かもしれない(?:という)?可能性があ(?:る|り|った)|(?:と考えられる|と思われる)(?:とも?|ように)(?:考えられる|思われる)|\b(?:might|may|could) (?:possibly|perhaps)\b|\b(?:perhaps|possibly) (?:might|may|could)\b",
    )
    .unwrap()
});
static LIST_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-+*]|[0-9]+[.)]|\([a-zA-Z0-9]+\))\s+").unwrap());
static NOTE_METADATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)^
        (?:
            (?:出典|出所|sources?)[：:]
            | (?:旧[^：:]{0,16})?Lean(?:宣言[：:]|\s*[（(]20\d{2}-)
            | (?:20\d{2}-\d{2}-\d{2}\s*)?著者(?:と|の(?:指示|指摘(?:で短縮)?|決定|コメント|了承))[：:]
        )",
    )
    .unwrap()
});
static ABBREVIATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:e\.g|i\.e|figs?|eqs?|refs?|secs?|thm|lem|def|dr|mr|mrs|prof|vs|al|no|pp)\.$",
    )
    .unwrap()
});

fn rule_named(names: &[String], code: &str) -> bool {
    names
        .iter()
        .any(|name| {
            let name = name.strip_prefix("style/").unwrap_or(name);
            Some(name) == code.strip_prefix("style/") || Some(name) == native::alias(code)
        })
}

fn is_japanese_char(c: char) -> bool {
    JAPANESE.is_match(c.encode_utf8(&mut [0; 4]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Position {
    line: usize,
    column: usize,
    byte: usize,
    len: usize,
}

#[derive(Default)]
struct MappedText {
    text: String,
    // One source position per UTF-8 byte. Columns count UTF-16 code units,
    // matching textlint's JavaScript offsets.
    positions: Vec<Position>,
}

impl MappedText {
    fn push(&mut self, text: &str, position: Position) {
        self.text.push_str(text);
        self.positions.extend(std::iter::repeat_n(position, text.len()));
    }

    fn push_line(&mut self, text: &str, line: usize, start: usize) {
        let mut column = 1;
        for (offset, c) in text.char_indices() {
            self.push(c.encode_utf8(&mut [0; 4]), Position {
                line, column, byte: start + offset, len: c.len_utf8(),
            });
            column += c.len_utf16();
        }
    }

    fn source(&self, part: &str) -> &[Position] {
        let start = part.as_ptr() as usize - self.text.as_ptr() as usize;
        &self.positions[start..start + part.len()]
    }

    fn copy(&mut self, from: &Self, part: &str) {
        if part.is_empty() {
            return;
        }
        self.text.push_str(part);
        self.positions.extend_from_slice(from.source(part));
    }

    fn span(&self, part: &str) -> RangeInclusive<Position> {
        let positions = self.source(part);
        positions[0]..=positions[positions.len() - 1]
    }
}

// Match the Markdown code masking used by the card parser while retaining the
// locations of visible text after a code span consumes one or more line breaks.
fn mask_code_with_lines(body: &str) -> MappedText {
    let mut markdown = MappedText::default();
    let mut fence = None;
    for (index, line) in body.split_inclusive('\n').enumerate() {
        let start = line.as_ptr() as usize - body.as_ptr() as usize;
        let trimmed = line.trim_start();
        if let Some(c) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let len = trimmed.bytes().take_while(|&byte| byte == c as u8).count();
            if len >= 3 {
                if fence.is_none() {
                    fence = Some((c, len));
                } else if fence.is_some_and(|(open, n)| c == open && len >= n)
                    && trimmed[len..].trim().is_empty()
                {
                    fence = None;
                }
                markdown.push_line("\n", index + 1, start);
                continue;
            }
        }
        markdown.push_line(
            if fence.is_some() || trimmed.is_empty() {
                "\n"
            } else {
                line
            },
            index + 1,
            start,
        );
    }

    let mut out = MappedText::default();
    for paragraph in markdown.text.split_inclusive("\n\n") {
        let mut rest = paragraph;
        while let Some(start) = rest.find('`') {
            out.copy(&markdown, &rest[..start]);
            let remaining = &rest[start..];
            let len = remaining.bytes().take_while(|&c| c == b'`').count();
            let after = &remaining[len..];
            if let Some(end) = closing_backticks(after, len) {
                out.push("\0", markdown.source(remaining)[0]);
                rest = &after[end..];
            } else {
                out.copy(&markdown, &remaining[..len]);
                rest = after;
            }
        }
        out.copy(&markdown, rest);
    }
    out
}

fn prose(body: &str, preserve_markdown: bool) -> MappedText {
    let masked = mask_code_with_lines(body);
    let mut markdown = MappedText::default();
    let mut in_notes = false;
    for line in masked.text.split_inclusive('\n') {
        let source = masked.source(line)[0];
        let trimmed = line.trim();
        if let Some((marks, heading)) = trimmed.split_once(' ') {
            if !marks.is_empty() && marks.chars().all(|c| c == '#') {
                if marks.len() <= 2 {
                    in_notes = matches!(
                        heading.trim().to_ascii_lowercase().as_str(),
                        "notes" | "ノート" | "備考"
                    );
                }
                if preserve_markdown {
                    markdown.copy(&masked, line);
                } else {
                    markdown.push("\n\n", source);
                }
                continue;
            }
        }
        if trimmed.starts_with('>')
            || trimmed.starts_with('＞')
            || (in_notes && (notes_quote(trimmed) || NOTE_METADATA.is_match(trimmed)))
        {
            markdown.push("\n\n", source);
            continue;
        }
        if preserve_markdown {
            markdown.copy(&masked, line.trim_end_matches(['\r', '\n']));
        } else if let Some(item) = LIST_ITEM.find(trimmed) {
            markdown.push("\n", source);
            markdown.copy(&masked, &trimmed[item.end()..]);
        } else {
            markdown.copy(&masked, trimmed);
        }
        markdown.push("\n", *masked.source(line).last().unwrap());
    }
    let mut positions = Vec::new();
    let text = tex::style_prose(&markdown.text, |out, offset| {
        positions.extend(std::iter::repeat_n(markdown.positions[offset], out.len()));
    });
    MappedText { text, positions }
}

fn notes_quote(line: &str) -> bool {
    line.starts_with("「ユーザー」")
        || line
            .strip_prefix("ユーザー")
            .is_some_and(|rest| rest.starts_with([':', '：', '（', '(']) && rest.contains('「'))
        || (line.starts_with('「') && line.ends_with('」'))
}

fn paragraphs(prose: &MappedText) -> impl Iterator<Item = MappedText> + '_ {
    prose.text.split("\n\n").filter_map(|paragraph| {
        let mut text = MappedText::default();
        for line in paragraph
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            // A soft wrap joins Japanese text without introducing a space.
            let join = text.text.chars().next_back().is_some_and(is_japanese_char)
                && line.chars().next().is_some_and(is_japanese_char);
            if !text.text.is_empty() && !join {
                text.push(" ", prose.source(line)[0]);
            }
            for (index, word) in line.split_whitespace().enumerate() {
                if index > 0 {
                    text.push(" ", prose.source(word)[0]);
                }
                text.copy(prose, word);
            }
        }
        (!text.text.is_empty()).then_some(text)
    })
}

fn sentences(paragraph: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    let mut chars = paragraph.char_indices().peekable();
    std::iter::from_fn(move || {
        while let Some((i, c)) = chars.next() {
            let next = chars.peek().map(|&(_, c)| c);
            let stop = match c {
                '。' | '．' | '！' | '？' | '!' | '?' => true,
                '.' => {
                    // Periods inside numbers and dotted words are not sentence marks.
                    let internal = next.is_some_and(|c| c.is_ascii_alphanumeric())
                        && paragraph[..i].ends_with(|c: char| c.is_ascii_alphanumeric());
                    !internal && !ABBREVIATION.is_match(&paragraph[start..=i])
                }
                _ => false,
            };
            if stop {
                while chars
                    .peek()
                    .is_some_and(|&(_, c)| "。．！？.!?」』）)”’\"".contains(c))
                {
                    chars.next();
                }
                let end = chars.peek().map_or(paragraph.len(), |&(i, _)| i);
                let sentence = paragraph[start..end].trim();
                start = end;
                return Some(sentence);
            }
        }
        let tail = paragraph[start..].trim();
        start = paragraph.len();
        (!tail.is_empty()).then_some(tail)
    })
}

struct Phrases {
    single: Vec<Regex>,
    repeated: Vec<Regex>,
}

impl Phrases {
    fn compile(config: &StyleConfig) -> Self {
        fn compile(phrases: impl Iterator<Item = impl AsRef<str>>) -> Vec<Regex> {
            let mut seen = std::collections::HashSet::new();
            phrases
                .map(|phrase| {
                    phrase
                        .as_ref()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|phrase| !phrase.is_empty() && seen.insert(phrase.to_lowercase()))
                .filter_map(|phrase| {
                    let literal = regex::escape(&phrase);
                    let pattern = if JAPANESE.is_match(&phrase) {
                        format!("(?i){literal}")
                    } else {
                        let start = if phrase.starts_with(char::is_alphanumeric) {
                            r"\b"
                        } else {
                            ""
                        };
                        let end = if phrase.ends_with(char::is_alphanumeric) {
                            r"\b"
                        } else {
                            ""
                        };
                        format!("(?i){start}{literal}{end}")
                    };
                    Regex::new(&pattern).ok()
                })
                .collect()
        }
        Self {
            single: compile(config.phrases.as_ref().unwrap_or(&DEFAULTS.phrases).iter().chain(&config.extra_phrases)),
            repeated: compile(config.repeated_phrases.as_ref().unwrap_or(&DEFAULTS.repeated_phrases).iter()),
        }
    }
}

fn ending(sentence: &str) -> Option<&str> {
    let text = sentence.trim_end_matches(['」', '』', '）', '”', '"']);
    let stem = text.strip_suffix(['。', '．', '！', '？', '.', '!', '?'])?;
    ENDINGS.find(stem.trim_end()).map(|found| found.as_str())
}

fn negated_contrast(sentence: &str) -> bool {
    NEGATION_EN.is_match(sentence)
        || NEGATION_JA.find_iter(sentence).any(|found| {
            let rest = &sentence[found.end()..];
            // Exclude a change of state (ではなくなる) and an unfinished clause.
            !["なっ", "なら", "なり", "なる", "ない"]
                .iter()
                .any(|suffix| rest.starts_with(suffix))
                && rest
                    .trim_end_matches(['。', '．', '！', '？', '.', '!', '?'])
                    .chars()
                    .any(|c| c.is_alphanumeric() || c == '\0')
        })
}

fn quoted(text: &str) -> String {
    format!("「{}」", text.trim().replace('\0', "…"))
}

fn inspect(
    prose: &MappedText,
    config: &StyleConfig,
    phrases: &Phrases,
    enabled: impl Fn(&str) -> bool,
    mut report: impl FnMut(&'static str, String, RangeInclusive<Position>, RangeInclusive<Position>),
) {
    let mut repeated_phrases = vec![0; phrases.repeated.len()];
    let max_ja_chars = config.max_ja_chars.unwrap_or(DEFAULTS.max_ja_chars);
    let max_en_words = config.max_en_words.unwrap_or(DEFAULTS.max_en_words);
    let max_ja_commas = config.max_ja_commas.unwrap_or(DEFAULTS.max_ja_commas);
    let paragraphs: Vec<_> = paragraphs(prose).collect();
    let mut register = native::RegisterUse::default();
    if enabled("style/no-mix-dearu-desumasu") {
        for sentence in paragraphs.iter().flat_map(|p| sentences(&p.text)) {
            register.observe(sentence);
        }
    }
    let preferred = register.preferred(config.prefer_register.unwrap_or(DEFAULTS.prefer_register));
    for paragraph in paragraphs {
        let mut run = Vec::new();
        let mut previous = None;
        for sentence in sentences(&paragraph.text) {
            let excerpt = paragraph.span(sentence);
            let mut warn = |code, message, span: RangeInclusive<Position>| {
                let context = if span.start() < excerpt.start() { span.clone() } else { excerpt.clone() };
                report(code, message, span, context);
            };
            if enabled(AI) {
                for phrase in &phrases.single {
                    if let Some(found) = phrase.find(sentence) {
                        warn(
                            AI,
                            format!(
                                "{} は定型句。対象や働きを具体的に述べられるか見直す。",
                                quoted(found.as_str())
                            ),
                            paragraph.span(found.as_str()),
                        );
                    }
                }
                for (phrase, count) in phrases.repeated.iter().zip(&mut repeated_phrases) {
                    for found in phrase.find_iter(sentence) {
                        *count += 1;
                        if *count == 2 && !phrases.single.iter().any(|p| p.is_match(found.as_str()))
                        {
                            warn(AI, format!("{} がカード内で繰り返されている。文どうしの関係を具体的に示す。", quoted(found.as_str())), paragraph.span(found.as_str()));
                        }
                    }
                }
            }
            native::inspect_sentence(sentence, config, preferred, &enabled, &mut |code, message, part| {
                warn(code, message, paragraph.span(part));
            });
            let japanese = JAPANESE.is_match(sentence);
            if enabled(LONG) {
                let (count, limit, unit) = if japanese {
                    (
                        sentence
                            .chars()
                            .filter(|c| !c.is_whitespace() && *c != '\0')
                            .count(),
                        max_ja_chars,
                        "字",
                    )
                } else {
                    (WORD.find_iter(sentence).count(), max_en_words, "語")
                };
                if count > limit {
                    warn(LONG, format!("{} は {count}{unit}（目安 {limit}{unit}）。条件と結論や、独立した主張を分けられるか見直す。", quoted(sentence)), paragraph.span(sentence));
                }
            }
            if japanese && enabled(COMMA) {
                let count = sentence.matches(['、', '，']).count();
                if count > max_ja_commas {
                    warn(COMMA, format!("{} に読点が {count} 個ある（目安 {max_ja_commas} 個）。列挙や従属節の構成を見直す。", quoted(sentence)), paragraph.span(sentence));
                }
            }
            if enabled(ENDING) {
                let current = japanese.then(|| ending(sentence)).flatten();
                if current.is_none() || current != previous {
                    run.clear();
                }
                if let Some(current) = current.filter(|_| run.len() < 3) {
                    run.push(sentence);
                    if run.len() == 3 {
                        let span = paragraph.source(run[0])[0]..=*excerpt.end();
                        warn(
                            ENDING,
                            format!(
                                "{} で文末「{current}」が続く。文の役割に合わせて構成を見直す。",
                                quoted(&run.join(" "))
                            ),
                            span,
                        );
                    }
                }
                previous = current;
            }
            if enabled(NEGATION) && negated_contrast(sentence) {
                warn(NEGATION, format!("{} の対比は、必要なら残す。強調が目的なら肯定側の主張を直接述べることも検討する。", quoted(sentence)), paragraph.span(sentence));
            }
            if enabled(HEDGING) {
                for found in HEDGES.find_iter(sentence) {
                    warn(
                        HEDGING,
                        format!(
                            "{} に留保が重なっている。不確かな点を絞り、確度を一度で示す。",
                            quoted(found.as_str())
                        ),
                        paragraph.span(found.as_str()),
                    );
                }
            }
        }
    }
}

pub(super) fn check(
    graph: &Graph,
    config: StyleConfig,
    project: Option<&Path>,
    includes: impl Fn(&crate::Card) -> bool,
) -> Result<Vec<Diagnostic>> {
    check_ranges(graph, config, project, includes, None)
}

impl Graph {
    /// Run term lint on touched cards and report style diagnostics intersecting the
    /// given 1-based line ranges in each card's assembled `Card::body`.
    pub fn lint_edited_cards_with_rules(
        root: &Path,
        ids: &BTreeSet<String>,
        edited: &BTreeMap<String, Vec<RangeInclusive<usize>>>,
        rules: &[PathBuf],
        project: Option<&Path>,
    ) -> Result<Vec<Diagnostic>> {
        let workspace = project.unwrap_or_else(|| crate::workspace_paths::workspace_for(root));
        let cfg = DeckConfig::read_config(root, rules, Some(workspace))?;
        let graph = Self::load_deck(root, &cfg, workspace)?;
        let mut diagnostics = term_lint::check(&graph, &cfg.lint, project, |card| ids.contains(&card.id))?;
        diagnostics.extend(check_ranges(
            &graph,
            cfg.style,
            project,
            |card| ids.contains(&card.id),
            Some(edited),
        )?);
        Ok(diagnostics)
    }
}

fn in_ranges(ranges: Option<&[RangeInclusive<usize>]>, span: &RangeInclusive<Position>) -> bool {
    ranges.is_none_or(|ranges| {
        ranges.iter().any(|range| *range.start() <= span.end().line && span.start().line <= *range.end())
    })
}

fn check_ranges(
    graph: &Graph,
    config: StyleConfig,
    project: Option<&Path>,
    includes: impl Fn(&crate::Card) -> bool,
    edited: Option<&BTreeMap<String, Vec<RangeInclusive<usize>>>>,
) -> Result<Vec<Diagnostic>> {
    let root = project.unwrap_or_else(|| {
        graph.root.ancestors()
            .find(|p| fuzukue_proto::compat::workspace_config(p).is_file())
            .unwrap_or(&graph.root)
    });
    let settings = match std::fs::read_to_string(fuzukue_proto::compat::workspace_config(root)) {
        Ok(contents) => toml::from_str::<Workspace>(&contents)
            .context("Invalid style lint settings in fuzukue.toml")?,
        Err(err) if err.kind() == ErrorKind::NotFound => Workspace::default(),
        Err(err) => return Err(err).context("Could not read fuzukue.toml"),
    };
    if !settings.style_lint.enabled {
        return Ok(Vec::new());
    }
    static DEFAULT_GLOBS: LazyLock<GlobSet> =
        LazyLock::new(|| exclusion_globs(DEFAULT_EXCLUDES).expect("built-in style lint globs"));
    let excludes = exclusion_globs(&settings.style_lint.exclude)?;
    let prefix = graph.root.strip_prefix(root).unwrap_or(Path::new(""));
    let cards: Vec<_> = graph.cards.values()
        .filter(|card| !card.external && includes(card))
        .filter(|card| !matches!(card.kind.as_str(), "plan" | "memo" | "design" | "design-log"))
        .filter(|card| {
            let workspace_path = prefix.join(&card.path);
            !DEFAULT_GLOBS.is_match(&card.path)
                && !DEFAULT_GLOBS.is_match(&workspace_path)
                && !excludes.is_match(&workspace_path)
        })
        .filter(|card| edited.is_none_or(|map| map.get(&card.id).is_some_and(|ranges| !ranges.is_empty())))
        .collect();
    if cards.is_empty() {
        return Ok(Vec::new());
    }
    let config = config.with_local(&graph.root)?;
    static DEFAULT_PHRASES: LazyLock<Phrases> = LazyLock::new(|| Phrases::compile(&StyleConfig::default()));
    let custom;
    let phrases = if config.extra_phrases.is_empty() && config.phrases.is_none() && config.repeated_phrases.is_none() {
        &*DEFAULT_PHRASES
    } else {
        custom = Phrases::compile(&config);
        &custom
    };
    let mut diagnostics = Vec::new();
    let mut locations = BTreeMap::<_, Vec<_>>::new();
    for card in &cards {
        let ranges = edited.map(|map| map[&card.id].as_slice());
        let enabled = |code: &str| !rule_named(&config.disable, code) && !rule_named(&card.lint_ignore, code);
        inspect(
            &prose(&card.body, false),
            &config,
            phrases,
            enabled,
            |code, message, span, excerpt| {
                if in_ranges(ranges, &span) {
                    let alias = native::alias(code);
                    if let Some(alias) = alias {
                        locations.entry((&card.id, alias)).or_default().push(span.clone());
                    }
                    diagnostics.push(style_diagnostic(card, code, alias, message, span, excerpt, &config));
                }
            },
        );
    }
    match external::check(root, &settings.style, &cards) {
        Ok(findings) => {
            let mut seen = std::collections::HashSet::new();
            let mut excerpts = BTreeMap::new();
            for finding in findings {
                let card = finding.card;
                let rule = finding.rule.rsplit('/').next().unwrap_or(&finding.rule);
                let external_code = format!("style/{rule}");
                // Allow either the upstream rule ID or its native alias in suppressions.
                let disabled = [&config.disable, &card.lint_ignore].into_iter().any(|names| {
                    rule_named(names, &external_code)
                        || names.iter().any(|name| {
                            let code = format!("style/{}", name.strip_prefix("style/").unwrap_or(name));
                            native::alias(&code) == Some(rule) || name == &finding.rule
                        })
                });
                let ranges = edited.map(|map| map[&card.id].as_slice());
                if disabled
                    || !in_ranges(ranges, &(finding.position..=finding.position))
                    || locations.get(&(&card.id, rule)).is_some_and(|spans| spans.iter().any(|span| span.contains(&finding.position)))
                    || !seen.insert((&card.id, finding.rule.clone(), finding.position))
                {
                    continue;
                }
                let sentences = excerpts.entry(&card.id).or_insert_with(|| {
                    paragraphs(&prose(&card.body, false))
                        .flat_map(|paragraph| {
                            sentences(&paragraph.text)
                                .map(|sentence| paragraph.span(sentence))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                });
                let span = finding.position..=finding.position;
                let excerpt = sentences.get(sentences.partition_point(|range| range.end() < span.start()))
                    .filter(|range| range.contains(span.start()))
                    .cloned()
                    .unwrap_or_else(|| span.clone());
                diagnostics.push(style_diagnostic(card, "style/textlint", Some(&finding.rule), finding.message, span, excerpt, &config));
            }
        }
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                Some("style/textlint-status"),
                ".".into(),
                format!("{err:#}"),
            ));
        }
    }
    Ok(diagnostics)
}

fn style_diagnostic(
    card: &crate::Card,
    code: &'static str,
    alias: Option<&str>,
    message: String,
    span: RangeInclusive<Position>,
    excerpt: RangeInclusive<Position>,
    config: &StyleConfig,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(Severity::Warning, Some(code), card.path.clone(), message);
    diagnostic.textlint_rule = alias.map(str::to_owned);
    diagnostic.line = Some(span.start().line);
    diagnostic.column = Some(span.start().column);
    diagnostic.fix_hint = Some(fuzukue_i18n::Locale::JAPANESE.style_fix_hint(code, alias));
    let bytes = |span: RangeInclusive<Position>| span.start().byte..span.end().byte + span.end().len;
    diagnostic.style_note = Some(fuzukue_proto::CardStyleNote {
        range: bytes(span),
        excerpt: bytes(excerpt),
        limit: match code {
            COMMA => Some(config.max_ja_commas.unwrap_or(DEFAULTS.max_ja_commas)),
            "style/max-comma" => Some(config.max_ascii_commas.unwrap_or(DEFAULTS.max_ascii_commas)),
            _ => None,
        },
    });
    diagnostic
}

#[cfg(test)]
mod tests;
