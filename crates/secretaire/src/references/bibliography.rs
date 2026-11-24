mod bibtex;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub(crate) fn check(project: &Path, bib: &str, path: &Path) -> Result<Value> {
    let input = fs::read_to_string(path).with_context(|| format!("{bib} を読めない"))?;
    let bibliography = bibtex::parse(&input).with_context(|| format!("{bib} の BibTeX が不正"))?;
    let root = super::root(project)?;
    let directories = root
        .as_deref()
        .map(super::entries)
        .transpose()?
        .unwrap_or_default();
    let project = project.canonicalize()?;
    let root = root
        .as_deref()
        .map(|path| relative_path(&project, path))
        .transpose()?;

    let bib_keys = normalized_keys(bibliography.keys());
    let directory_keys = normalized_keys(directories.keys());
    let mut matches = Vec::new();
    let mut bib_only = Vec::new();
    let mut directory_only = Vec::new();
    let mut references = Vec::new();
    let mut missing_required = Vec::new();
    let mut unknown_types = Vec::new();

    for (key, entry) in &bibliography {
        if !directories.contains_key(key) {
            bib_only.push(
                json!({ "key": key, "candidates": candidates(&bib_keys[key], &directory_keys) }),
            );
        }
        let Some(required) = required_fields(&entry.kind) else {
            unknown_types.push(json!({ "key": key, "type": entry.kind }));
            continue;
        };
        let parent = entry
            .fields
            .get("crossref")
            .and_then(|key| bibliography.get(key.trim()));
        let missing: Vec<_> = required
            .iter()
            .copied()
            .filter(|group| {
                group.split(" or ").all(|field| {
                    entry
                        .fields
                        .get(field)
                        .or_else(|| parent.and_then(|parent| parent.fields.get(field)))
                        .is_none_or(|value| value.trim().is_empty())
                })
            })
            .collect();
        if !missing.is_empty() {
            missing_required.push(json!({ "key": key, "type": entry.kind, "fields": missing }));
        }
    }

    let (mut missing_reference_md, mut missing_pdf, mut missing_text) = (0, 0, 0);
    for (key, dir) in &directories {
        let files = super::files(dir).with_context(|| format!("{key} のファイル一覧を読めない"))?;
        let path = relative_path(&project, dir)?;
        let has_reference_md = files.iter().any(|file| file == "reference.md");
        let mut has_pdf = false;
        let mut has_text = false;
        for file in &files {
            match super::extension(file).as_str() {
                "pdf" => has_pdf = true,
                "txt" | "tex" => has_text = true,
                _ => {}
            }
        }
        missing_reference_md += usize::from(!has_reference_md);
        missing_pdf += usize::from(!has_pdf);
        missing_text += usize::from(!has_text);
        references.push(json!({ "key": key, "path": path,
            "has_reference_md": has_reference_md, "has_pdf": has_pdf, "has_text": has_text }));
        if bibliography.contains_key(key) {
            matches.push(json!({ "key": key, "path": path }));
        } else {
            directory_only.push(
                json!({ "key": key, "candidates": candidates(&directory_keys[key], &bib_keys) }),
            );
        }
    }

    Ok(json!({
        "bib": bib,
        "reference_root": root,
        "summary": {
            "bib_entries": bibliography.len(), "directories": directories.len(),
            "matches": matches.len(), "bib_only": bib_only.len(), "directory_only": directory_only.len(),
            "missing_reference_md": missing_reference_md, "missing_pdf": missing_pdf, "missing_text": missing_text,
            "missing_required": missing_required.len(), "unknown_types": unknown_types.len(),
        },
        "matches": matches,
        "bib_only": bib_only,
        "directory_only": directory_only,
        "references": references,
        "missing_required": missing_required,
        "unknown_types": unknown_types,
    }))
}

fn relative_path(project: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(project)?
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/"))
}

// Patashnik, BibTeXing, §3.1. An alternative is satisfied by any listed field.
fn required_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "article" => &["author", "title", "journal", "year"],
        "book" => &["author or editor", "title", "publisher", "year"],
        "booklet" | "manual" => &["title"],
        "inbook" => &[
            "author or editor",
            "title",
            "chapter or pages",
            "publisher",
            "year",
        ],
        "incollection" => &["author", "title", "booktitle", "publisher", "year"],
        "conference" | "inproceedings" => &["author", "title", "booktitle", "year"],
        "mastersthesis" | "phdthesis" => &["author", "title", "school", "year"],
        "misc" => &[],
        "proceedings" => &["title", "year"],
        "techreport" => &["author", "title", "institution", "year"],
        "unpublished" => &["author", "title", "note"],
        _ => return None,
    })
}

fn normalized_keys<'a>(keys: impl Iterator<Item = &'a String>) -> BTreeMap<&'a String, Vec<char>> {
    keys.map(|key| {
        let normalized = key
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        (key, normalized)
    })
    .collect()
}

fn candidates<'a>(key: &[char], other: &BTreeMap<&'a String, Vec<char>>) -> Vec<&'a str> {
    let mut ranked = Vec::new();
    for (&candidate, normalized) in other {
        let threshold = (key.len().min(normalized.len()) / 5).min(3);
        if key.is_empty()
            || normalized.is_empty()
            || key.len().abs_diff(normalized.len()) > threshold
        {
            continue;
        }
        let distance = edit_distance(key, normalized);
        if distance <= threshold {
            ranked.push((distance, candidate.as_str()));
        }
    }
    ranked.sort_unstable();
    ranked.into_iter().take(3).map(|(_, key)| key).collect()
}

fn edit_distance(left: &[char], right: &[char]) -> usize {
    let mut row: Vec<_> = (0..=right.len()).collect();
    for (i, a) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(a != b)).min(1 + above.min(row[j]));
            diagonal = above;
        }
    }
    row[right.len()]
}
