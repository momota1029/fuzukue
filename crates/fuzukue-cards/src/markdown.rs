use std::ops::Range;

use anyhow::{ensure, Context, Result};
use serde::Serialize;

use crate::{split_frontmatter, Front};

#[derive(Debug, Clone, Serialize)]
pub struct CardPart {
    pub text: String,
    /// 1-based line of the first character in `text` in the source file.
    pub line: u64,
}

pub fn section_range(body: &str, name: &str) -> Option<Range<usize>> {
    let mut start = None;
    let mut completed = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if let Some(heading) = line
            .strip_prefix("## ")
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
        {
            if let Some(start) = start.take() {
                completed = Some(start..offset);
            }
            start = (heading == name).then_some(offset + line.len());
        }
        offset += line.len();
    }
    start.map(|start| start..offset).or(completed)
}

fn part(text: &str, range: Range<usize>) -> CardPart {
    let source = &text[range.clone()];
    let start = range.start + source.len() - source.trim_start().len();
    CardPart {
        text: source.trim().to_string(),
        line: 1 + text[..start].bytes().filter(|&b| b == b'\n').count() as u64,
    }
}

pub(crate) fn prose(text: &str, body: &str) -> CardPart {
    part(text, text.len() - body.len()..text.len())
}

pub(crate) fn section(text: &str, body: &str, name: &str) -> Option<CardPart> {
    let start = text.len() - body.len();
    section_range(body, name).map(|range| part(text, start + range.start..start + range.end))
}

/// Merge the paper editor's full Markdown buffer into the current source. Fact
/// cards expose Statement and Proof as editable parts, with their other sections
/// and frontmatter kept byte-for-byte. Explanation cards expose their prose.
pub fn merge_card_edit(current: &str, draft: &str) -> Result<String> {
    let (front, body) = split_frontmatter(current).context("カードの frontmatter がない")?;
    let (draft_front, draft_body) =
        split_frontmatter(draft).context("編集内容に frontmatter がない")?;
    let meta: Front = serde_yaml::from_str(front)?;
    let draft_meta: Front = serde_yaml::from_str(draft_front)?;
    ensure!(meta.id == draft_meta.id, "カードの ID は変更できない");
    let body_start = current.len() - body.len();
    if meta.kind.as_deref() == Some("explanation") {
        return Ok(current[..body_start].to_owned() + draft_body);
    }

    let newline = if current.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut edits = Vec::new();
    let mut new_sections = String::new();
    for name in ["Statement", "Proof"] {
        let Some(draft) = section_range(draft_body, name) else {
            continue;
        };
        let replacement = draft_body[draft].trim();
        let replacement = replacement.replace("\r\n", "\n").replace('\n', newline);
        let Some(old) = section_range(body, name) else {
            if !replacement.is_empty() {
                new_sections.push_str(&format!(
                    "## {name}{newline}{replacement}{newline}{newline}"
                ));
            }
            continue;
        };
        let source = &body[old.clone()];
        let trimmed = source.trim();
        if trimmed == replacement {
            continue;
        }
        let start = body_start + old.start;
        let (range, replacement) = if trimmed.is_empty() {
            let before = if current[..start].ends_with('\n') {
                ""
            } else {
                newline
            };
            (start..start, format!("{before}{replacement}{newline}"))
        } else {
            let start = start + source.len() - source.trim_start().len();
            (start..start + trimmed.len(), replacement)
        };
        edits.push((range, replacement));
    }
    let mut merged = current.to_string();
    edits.sort_by_key(|(range, _)| range.start);
    for (range, replacement) in edits.into_iter().rev() {
        merged.replace_range(range, &replacement);
    }
    if !new_sections.is_empty() {
        if !merged.ends_with('\n') {
            merged.push_str(newline);
        }
        merged.push_str(newline);
        merged.push_str(&new_sections);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_card;
    use std::path::Path;

    const CARD: &str = "---\nid: sample\nkind: theorem\nlabel: thm:sample\n---\n\n## Statement\n\nFirst.\nSecond.\n\n## Proof\nProof.\n\n## Shortcuts\nShortcut.\n## Notes\nNote.";

    #[test]
    fn extracts_parts_and_original_lines_with_either_line_ending() {
        for newline in ["\n", "\r\n"] {
            let text = CARD.replace('\n', newline);
            let card = parse_card(Path::new(""), Path::new("sample.md"), &text).unwrap();
            assert_eq!(card.label.as_deref(), Some("thm:sample"));
            let statement = card.statement.unwrap();
            assert_eq!(statement.text, format!("First.{newline}Second."));
            assert_eq!(statement.line, 9);
            let proof = card.proof.unwrap();
            assert_eq!(proof.text, "Proof.");
            assert_eq!(proof.line, 13);
        }
    }

    #[test]
    fn saves_parts_without_touching_other_sections_or_line_endings() {
        let current = CARD.replace('\n', "\r\n");
        let draft = CARD
            .replace("thm:sample", "thm:different")
            .replace("First.", "Updated.")
            .replace("Proof.\n", "\n")
            .replace("Note.", "Other note.");
        let edited = merge_card_edit(&current, &draft).unwrap();
        assert_eq!(
            edited,
            current
                .replace("First.", "Updated.")
                .replace("Proof.\r\n", "\r\n")
        );
        assert_eq!(merge_card_edit(&edited, &edited).unwrap(), edited);
    }
}
