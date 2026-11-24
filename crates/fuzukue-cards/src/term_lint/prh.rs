//! Shared spelling dictionaries in the prh version 1 YAML format.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{ensure, Context, Result};
use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Dictionary {
    version: u32,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many"
    )]
    imports: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rules: Vec<RuleEntry>,
    #[serde(flatten)]
    other: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum RuleEntry {
    Shorthand(String),
    Full(Box<Rule>),
}

#[derive(Debug, Deserialize, Serialize)]
struct Rule {
    expected: String,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many"
    )]
    pattern: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many"
    )]
    patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    specs: Vec<Spec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prh: Option<String>,
    #[serde(rename = "regexpMustEmpty", skip_serializing_if = "Option::is_none")]
    must_be_empty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Options>,
    #[serde(flatten)]
    other: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Options {
    #[serde(default, rename = "wordBoundary")]
    word_boundary: bool,
    #[serde(flatten)]
    other: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Spec {
    from: String,
    to: String,
}

fn one_or_many<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    input: D,
) -> Result<Vec<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Items<T> {
        Many(Vec<T>),
        One(T),
    }
    Ok(match Option::<Items<T>>::deserialize(input)? {
        Some(Items::One(item)) => vec![item],
        Some(Items::Many(items)) => items,
        None => Vec::new(),
    })
}

impl Dictionary {
    /// Read a dictionary and validate the specs in it and its imports.
    pub fn read(path: &Path) -> Result<Self> {
        Ok(Self::load(
            path,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut Vec::new(),
        )?
        .unwrap())
    }

    /// Serialize the dictionary, including its import declarations, as prh YAML.
    /// Relative imports are interpreted from the directory containing the output.
    pub fn write(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut output = tempfile::NamedTempFile::new_in(dir)?;
        serde_yaml::to_writer(output.as_file_mut(), self)?;
        Self::read(output.path())?;
        output.persist_noclobber(path)?;
        Ok(())
    }

    pub(super) fn rules(path: &Path) -> Result<Vec<CompiledRule>> {
        let mut rules = Vec::new();
        Self::load(path, &mut HashSet::new(), &mut HashSet::new(), &mut rules)?;
        Ok(rules)
    }

    fn load(
        path: &Path,
        active: &mut HashSet<PathBuf>,
        loaded: &mut HashSet<PathBuf>,
        rules: &mut Vec<CompiledRule>,
    ) -> Result<Option<Self>> {
        let identity = path
            .canonicalize()
            .with_context(|| format!("Could not find prh dictionary {}", path.display()))?;
        ensure!(
            !active.contains(&identity),
            "Cyclic prh import at {}",
            path.display()
        );
        if !loaded.insert(identity.clone()) {
            return Ok(None);
        }
        active.insert(identity.clone());
        let dictionary: Self = serde_yaml::from_reader(fs::File::open(path)?)
            .with_context(|| format!("Invalid prh dictionary {}", path.display()))?;
        ensure!(
            dictionary.version == 1,
            "Unsupported prh version in {}",
            path.display()
        );
        for entry in &dictionary.rules {
            rules.push(
                CompiledRule::new(entry)
                    .with_context(|| format!("Invalid prh rule in {}", path.display()))?,
            );
        }
        for import in &dictionary.imports {
            Self::load(&path.with_file_name(import), active, loaded, rules)?;
        }
        active.remove(&identity);
        Ok(Some(dictionary))
    }
}

pub(super) struct CompiledRule {
    pub expected: String,
    pub hint: Option<String>,
    patterns: Vec<Regex>,
    literal_replacement: bool,
    must_be_empty: Option<usize>,
}

impl CompiledRule {
    fn new(entry: &RuleEntry) -> Result<Self> {
        let (expected, patterns, specs, hint, boundary, must_be_empty) = match entry {
            RuleEntry::Shorthand(expected) => (expected, &[][..], &[][..], None, false, None),
            RuleEntry::Full(rule) => (
                &rule.expected,
                if rule.pattern.is_empty() {
                    &rule.patterns[..]
                } else {
                    &rule.pattern[..]
                },
                &rule.specs[..],
                rule.prh.clone(),
                rule.options
                    .as_ref()
                    .is_some_and(|options| options.word_boundary),
                rule.must_be_empty.as_deref(),
            ),
        };
        let must_be_empty = must_be_empty
            .map(|group| {
                group
                    .strip_prefix('$')
                    .and_then(|n| n.parse::<usize>().ok())
                    .context("regexpMustEmpty requires a capture such as $1")
            })
            .transpose()?;

        let literal_replacement = patterns.is_empty();
        let patterns = if patterns.is_empty() {
            ensure!(!expected.is_empty(), "Empty implicit prh pattern");
            vec![compile(&implicit_pattern(expected), "mu", boundary)?]
        } else {
            patterns
                .iter()
                .map(|pattern| {
                    if let Some((body, flags)) = pattern
                        .strip_prefix('/')
                        .and_then(|rest| rest.rsplit_once('/'))
                    {
                        compile(body, flags, boundary)
                    } else {
                        compile(&regex::escape(pattern), "mu", boundary)
                    }
                })
                .collect::<Result<_>>()?
        };
        let rule = Self {
            expected: expected.clone(),
            hint,
            patterns,
            literal_replacement,
            must_be_empty,
        };
        for spec in specs {
            let mut changed = String::new();
            let mut offset = 0;
            for (start, end, replacement) in rule.changes(&spec.from)? {
                changed.push_str(&spec.from[offset..start]);
                changed.push_str(&replacement);
                offset = end;
            }
            changed.push_str(&spec.from[offset..]);
            ensure!(
                changed == spec.to,
                "prh spec for {expected:?}: {:?} should become {:?}, got {changed:?}",
                spec.from,
                spec.to
            );
        }
        Ok(rule)
    }

    pub(super) fn changes(&self, text: &str) -> Result<Vec<(usize, usize, String)>> {
        let mut candidates = Vec::new();
        for pattern in &self.patterns {
            for capture in pattern.captures_iter(text) {
                let capture = capture?;
                let m = capture.get(0).unwrap();
                if self
                    .must_be_empty
                    .and_then(|i| capture.get(i))
                    .is_some_and(|m| !m.as_str().is_empty())
                {
                    continue;
                }
                let replacement = if self.literal_replacement {
                    self.expected.clone()
                } else {
                    static SUBSTITUTION: LazyLock<regex::Regex> =
                        LazyLock::new(|| regex::Regex::new(r"\$([$&1-9])").unwrap());
                    SUBSTITUTION
                        .replace_all(&self.expected, |parts: &regex::Captures<'_>| {
                            match &parts[1] {
                                "$" => "$".to_owned(),
                                "&" => m.as_str().to_owned(),
                                digit => {
                                    let i = (digit.as_bytes()[0] - b'0') as usize;
                                    if i < capture.len() {
                                        capture.get(i).map_or("", |m| m.as_str()).to_owned()
                                    } else {
                                        parts[0].to_owned()
                                    }
                                }
                            }
                        })
                        .into_owned()
                };
                candidates.push((m.start(), m.end(), replacement));
            }
        }
        // Stable ordering gives the first declared pattern priority at the same offset.
        candidates.sort_by_key(|candidate| candidate.0);
        let mut end = None;
        candidates.retain(|(start, stop, _)| {
            if end.is_some_and(|end| *start < end) {
                return false;
            }
            end = Some(if start == stop { stop + 1 } else { *stop });
            true
        });
        candidates.retain(|(start, end, replacement)| &text[*start..*end] != replacement);
        Ok(candidates)
    }
}

fn compile(body: &str, flags: &str, boundary: bool) -> Result<Regex> {
    ensure!(!body.is_empty(), "Empty prh pattern");
    let mut seen = HashSet::new();
    ensure!(
        flags.chars().all(|c| "gimsu".contains(c) && seen.insert(c)),
        "Unsupported prh regex flags: {flags}"
    );
    let switches: String = "ims"
        .chars()
        .filter(|c| flags.contains(*c) || *c == 'm')
        .collect();
    let body = body.replace(r"\/", "/");
    let body = if boundary {
        // JavaScript word boundaries use the ASCII word class.
        let word = "[A-Za-z0-9_]";
        let edge = format!("(?:(?<!{word})(?={word})|(?<={word})(?!{word}))");
        format!("{edge}(?:{body}){edge}")
    } else {
        body
    };
    Ok(Regex::new(&format!("(?{switches}){body}"))?)
}

fn implicit_pattern(expected: &str) -> String {
    let mut pattern = String::new();
    for c in expected.chars() {
        if c.is_ascii_alphanumeric() {
            pattern.push('[');
            for variant in [c.to_ascii_lowercase(), c.to_ascii_uppercase()] {
                pattern.push(variant);
                pattern.push(char::from_u32(variant as u32 + 0xfee0).unwrap());
            }
            pattern.push(']');
        } else {
            pattern.push_str(&regex::escape(&c.to_string()));
        }
    }
    pattern
}
