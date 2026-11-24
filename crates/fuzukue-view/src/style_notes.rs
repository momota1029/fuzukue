//! Presentation and navigation for prose diagnostics.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
};

use fuzukue_proto::CardStyleDiagnostic;

#[derive(Default)]
pub struct StyleNoteDisclosure {
    expanded: HashMap<String, HashSet<String>>,
}

impl StyleNoteDisclosure {
    pub fn is_expanded(&self, workspace: &str, path: &str) -> bool {
        self.expanded
            .get(workspace)
            .is_some_and(|cards| cards.contains(path))
    }

    pub fn toggle(&mut self, workspace: String, path: String) {
        let cards = self.expanded.entry(workspace).or_default();
        if !cards.remove(&path) {
            cards.insert(path);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct JumpTarget {
    pub line: usize,
    pub line_range: Range<usize>,
    pub range: Range<usize>,
}

/// Resolve a body range to its first source line and its precise jump position.
pub fn jump_target(body: &str, range: Range<usize>) -> Option<JumpTarget> {
    body.get(range.clone()).filter(|text| !text.is_empty())?;
    let before = &body[..range.start];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line_end = body[range.start..]
        .find('\n')
        .map_or(body.len(), |i| range.start + i);
    Some(JumpTarget {
        line: before.bytes().filter(|byte| *byte == b'\n').count() + 1,
        line_range: line_start..line_end,
        range,
    })
}

/// Map a body range into an editor buffer when the two share an exact source
/// suffix. A folder card may include TeX before the text in its Markdown file.
pub fn editor_range(
    card_body: &str,
    file: &str,
    file_body: &str,
    range: Range<usize>,
) -> Option<Range<usize>> {
    let card_prefix = card_body.strip_suffix(file_body)?.len();
    let file_prefix = file.strip_suffix(file_body)?.len();
    let start = range.start.checked_sub(card_prefix)? + file_prefix;
    let end = range.end.checked_sub(card_prefix)? + file_prefix;
    file.get(start..end).filter(|text| !text.is_empty())?;
    Some(start..end)
}

pub struct StyleNoteRow<'a> {
    pub excerpt: String,
    pub findings: Vec<&'a CardStyleDiagnostic>,
    pub ranges: Vec<Range<usize>>,
    pub count: usize,
}

pub fn rule_id(style: &CardStyleDiagnostic) -> &str {
    if style.code == "style/textlint" {
        style.textlint_rule.as_deref().unwrap_or(&style.code)
    } else {
        &style.code
    }
}

pub fn rows<'a>(
    body: &str,
    notes: impl IntoIterator<Item = &'a CardStyleDiagnostic>,
) -> Vec<StyleNoteRow<'a>> {
    let mut groups = Vec::<StyleNoteRow>::new();
    let mut slots = HashMap::new();
    let mut seen = HashSet::new();
    let mut excerpts = Vec::new();
    for style in notes
        .into_iter()
        .filter(|style| style.code != "style/textlint-status")
    {
        let note = style.note.as_ref().filter(|note| {
            body.get(note.range.clone())
                .is_some_and(|text| !text.is_empty())
                && body
                    .get(note.excerpt.clone())
                    .is_some_and(|text| !text.is_empty())
                && note.excerpt.start <= note.range.start
                && note.range.end <= note.excerpt.end
        });
        let (source, legacy) = match note {
            Some(note) => (Some(note.excerpt.clone()), ""),
            None => (None, style.detail.as_str()),
        };
        let range = note.map(|note| note.range.clone());
        let rule = rule_id(style);
        if !seen.insert((rule, source.clone(), legacy, range.clone())) {
            continue;
        }
        let index = *slots.entry((source.clone(), legacy)).or_insert_with(|| {
            excerpts.push((source, legacy));
            groups.push(StyleNoteRow {
                excerpt: String::new(),
                findings: Vec::new(),
                ranges: Vec::new(),
                count: 0,
            });
            groups.len() - 1
        });
        let row = &mut groups[index];
        if !row
            .findings
            .iter()
            .any(|existing| rule_id(existing) == rule)
        {
            row.findings.push(style);
        }
        row.ranges.extend(range);
        row.count += 1;
    }
    for (row, (source, legacy)) in groups.iter_mut().zip(excerpts) {
        let unique: BTreeSet<_> = row
            .ranges
            .drain(..)
            .map(|range| (range.start, range.end))
            .collect();
        row.ranges = unique.into_iter().map(|(start, end)| start..end).collect();
        row.excerpt = match source {
            Some(source) => excerpt(&body[source.clone()], row.ranges[0].start - source.start),
            None => excerpt(
                legacy
                    .strip_prefix('「')
                    .and_then(|text| text.split_once('」'))
                    .map_or("", |(quote, _)| quote),
                0,
            ),
        };
    }
    groups.sort_by_key(|row| row.ranges.first().map_or(usize::MAX, |range| range.start));
    groups
}

fn excerpt(text: &str, focus: usize) -> String {
    const MAX_CHARS: usize = 40;
    let count = text.chars().count();
    let focus = text[..focus].chars().count();
    let start = if count <= MAX_CHARS {
        0
    } else {
        focus
            .saturating_sub(MAX_CHARS / 2)
            .min(count - (MAX_CHARS - 1))
    };
    let leading = start > 0;
    let trailing = count - start > MAX_CHARS - usize::from(leading);
    let take = MAX_CHARS - usize::from(leading) - usize::from(trailing);
    leading
        .then_some('…')
        .into_iter()
        .chain(
            text.chars()
                .skip(start)
                .take(take)
                .map(|ch| if ch.is_whitespace() { ' ' } else { ch }),
        )
        .chain(trailing.then_some('…'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::CardStyleNote;

    fn note(code: &str, range: Range<usize>, excerpt: Range<usize>) -> CardStyleDiagnostic {
        CardStyleDiagnostic {
            code: code.to_owned(),
            textlint_rule: None,
            detail: String::new(),
            note: Some(CardStyleNote {
                range,
                excerpt,
                limit: None,
            }),
        }
    }

    #[test]
    fn merges_rules_by_excerpt_and_counts_only_distinct_findings() {
        let body = "One sentence.\nOne sentence.";
        let notes = [
            note("style/a", 1..2, 0..13),
            note("style/a", 1..2, 0..13),
            note("style/a", 4..5, 0..13),
            note("style/b", 5..6, 0..13),
            note("style/a", 15..16, 14..27),
        ];
        let rows = rows(body, &notes);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].excerpt, "One sentence.");
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[0].findings.len(), 2);
        assert_eq!(rows[0].ranges, [1..2, 4..5, 5..6]);
        assert_eq!(rows[1].ranges, [15..16]);
    }

    #[test]
    fn excerpt_is_at_most_forty_characters_and_keeps_the_hit() {
        let text = format!("{}！{}", "前".repeat(45), "後".repeat(45));
        let middle = excerpt(&text, 45 * "前".len());
        assert_eq!(middle.chars().count(), 40);
        assert!(middle.starts_with('…') && middle.ends_with('…') && middle.contains('！'));
        assert_eq!(excerpt(&text, 0), format!("{}…", "前".repeat(39)));
        assert_eq!(excerpt(&"語".repeat(40), 0), "語".repeat(40));
    }

    #[test]
    fn disclosure_defaults_to_closed_and_remembers_each_card() {
        let mut state = StyleNoteDisclosure::default();
        assert!(!state.is_expanded("first", "cards/a.md"));
        state.toggle("first".into(), "cards/a.md".into());
        assert!(!state.is_expanded("first", "cards/b.md"));
        assert!(!state.is_expanded("second", "cards/a.md"));
        assert!(state.is_expanded("first", "cards/a.md"));
        state.toggle("first".into(), "cards/a.md".into());
        assert!(!state.is_expanded("first", "cards/a.md"));
    }

    #[test]
    fn jump_uses_source_lines_and_utf8_bytes() {
        let body = "First\r\n😀の文。\nLast";
        assert_eq!(
            jump_target(body, 11..14),
            Some(JumpTarget {
                line: 2,
                line_range: 7..20,
                range: 11..14,
            })
        );
        assert!(jump_target(body, 8..14).is_none());
        let file = format!("---\r\n---\r\n{body}");
        assert_eq!(editor_range(body, &file, body, 11..14), Some(21..24));
        assert_eq!(
            editor_range("TeX\nFirst", "---\n---\nFirst", "First", 5..8),
            Some(9..12)
        );
        assert!(editor_range(body, &file, "different", 11..14).is_none());
    }
}
