use std::fmt::Write as _;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

mod python_yaml;
mod yaml;

// Python's str.isspace adds these four separators to Unicode White_Space.
const PYTHON_SPACE: &str = r"[\p{White_Space}\x1c-\x1f]";

/// Hash raw Markdown using the translation harness format.
pub fn source_fingerprint(markdown: &str) -> Result<String> {
    static FRONT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)\A---\n(.*?)\n---\n?(.*)\z").unwrap());
    static NOTES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!(r"(?m)^## Notes{PYTHON_SPACE}*$")).unwrap());
    static SPACE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!("{PYTHON_SPACE}+")).unwrap());

    // Path.read_text in sync.py uses universal newlines.
    let markdown = markdown.replace("\r\n", "\n").replace('\r', "\n");
    let parts = FRONT
        .captures(&markdown)
        .context("source card has no front matter")?;
    let mut meta = python_yaml::load(&parts[1])?;
    meta.apply_merge()?;
    let body = &parts[2];
    let body = NOTES
        .find(body)
        .map_or(body, |notes| &body[..notes.start()]);
    let mut normalized = SPACE
        .split(body)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    normalized.push('[');
    for (i, key) in ["kind", "requires", "title"].into_iter().enumerate() {
        if i != 0 {
            normalized.push_str(", ");
        }
        write!(normalized, "('{key}', ").unwrap();
        python_repr(&meta[key], &mut normalized);
        normalized.push(')');
    }
    normalized.push(']');
    let digest = Sha256::digest(normalized.as_bytes());
    Ok(format!("{:x}", digest)[..10].to_owned())
}

fn python_repr(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) if n.is_f64() => write!(out, "{:?}", n.as_f64().unwrap()).unwrap(),
        Value::Number(n) => write!(out, "{n}").unwrap(),
        Value::String(s) => python_string(s, out),
        Value::Sequence(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                python_repr(item, out);
            }
            out.push(']');
        }
        Value::Mapping(items) => {
            out.push('{');
            for (i, (key, value)) in items.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                python_repr(key, out);
                out.push_str(": ");
                python_repr(value, out);
            }
            out.push('}');
        }
        Value::Tagged(tagged) if tagged.tag == python_yaml::REPR_TAG => {
            if let Some(repr) = tagged.value.as_str() {
                out.push_str(repr);
            }
        }
        Value::Tagged(tagged) => python_repr(&tagged.value, out),
    }
}

fn python_string(s: &str, out: &mut String) {
    static NON_PRINTABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{C}\p{Z}]").unwrap());
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if ch != ' ' && NON_PRINTABLE.is_match(ch.encode_utf8(&mut [0; 4])) => {
                match u32::from(ch) {
                    n @ 0..=0xff => write!(out, "\\x{n:02x}").unwrap(),
                    n @ 0x100..=0xffff => write!(out, "\\u{n:04x}").unwrap(),
                    n => write!(out, "\\U{n:08x}").unwrap(),
                }
            }
            ch => out.push(ch),
        }
    }
    out.push(quote);
}

/// Replace the translation stamp while retaining the rest of the card's source.
pub fn with_source_fingerprint(before: &str, fingerprint: &str) -> Result<String> {
    let (front, _) = super::split_frontmatter(before).context("card has no front matter")?;
    let mut expected: Mapping = serde_yaml::from_str(front)?;
    expected.insert(Value::from("source_fingerprint"), Value::from(fingerprint));
    let newline = if before.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let stamp = format!("'{fingerprint}'");
    let offset = 3 + newline.len();
    let mut depth = 0;
    let mut key = true;
    let mut stamp_key = false;
    let mut stamp_span = None;
    let mut root = None;
    yaml::visit(front, |event, span| {
        if depth == 0 {
            if let yaml::Event::Mapping { flow } = event {
                root = Some((span.clone(), flow));
            }
        } else if depth == 1 && !matches!(event, yaml::Event::End) {
            if key {
                stamp_key = matches!(&event, yaml::Event::Scalar(scalar) if scalar.value == "source_fingerprint");
            } else if stamp_key {
                stamp_span = Some(span.clone());
            }
            key = !key;
        }
        match event {
            yaml::Event::Mapping { .. } | yaml::Event::Sequence => depth += 1,
            yaml::Event::End => depth -= 1,
            _ => {}
        }
        Ok(())
    })?;

    let mut after = before.to_owned();
    if let Some(span) = stamp_span {
        let scalar = &front[span.clone()];
        let leading = if span.is_empty() || front[..span.start].ends_with(':') {
            " "
        } else {
            ""
        };
        let trailing = if scalar.ends_with('\n') { newline } else { "" };
        after.replace_range(
            offset + span.start..offset + span.end,
            &format!("{leading}{stamp}{trailing}"),
        );
    } else {
        let (span, flow) = root.context("card front matter is not a mapping")?;
        if flow {
            after.insert_str(offset + span.end, &format!("source_fingerprint: {stamp}, "));
        } else {
            let indent = front[..span.start].rsplit('\n').next().unwrap();
            after.insert_str(
                offset + span.start,
                &format!("source_fingerprint: {stamp}{newline}{indent}"),
            );
        }
    }
    let (front, _) = super::split_frontmatter(&after).context("cannot update card front matter")?;
    anyhow::ensure!(
        serde_yaml::from_str::<Mapping>(front)? == expected,
        "cannot update source_fingerprint safely"
    );
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_uses_only_harness_inputs() -> Result<()> {
        let base = "---\nid: test\nkind: lemma\ntitle: \"L'été\"\nrequires: [dep]\nsummary: Before\nsource: before.pdf\n---\nBody \ttext\u{1c}here.\n## Notes \t\nPrivate notes\n";
        let original = source_fingerprint(base)?;
        for (from, to) in [
            ("Body \ttext\u{1c}here.", "Body text\u{a0}here."),
            ("Private notes", "Revised notes"),
            ("summary: Before", "summary: After"),
            ("source: before.pdf", "source: after.pdf"),
            ("\n", "\r\n"),
        ] {
            assert_eq!(
                source_fingerprint(&base.replace(from, to))?,
                original,
                "{from:?}"
            );
        }
        for (from, to) in [
            ("Body", "Other"),
            ("lemma", "theorem"),
            ("L'été", "L'hiver"),
            ("[dep]", "[different]"),
            ("## Notes", "## Notes on the proof"),
        ] {
            assert_ne!(
                source_fingerprint(&base.replace(from, to))?,
                original,
                "{from}"
            );
        }
        let missing = base.replace("requires: [dep]\n", "");
        assert_ne!(
            source_fingerprint(&missing)?,
            source_fingerprint(&base.replace("[dep]", "[]"))?
        );
        assert_eq!(
            source_fingerprint(&missing)?,
            source_fingerprint(&base.replace("[dep]", "null"))?
        );
        Ok(())
    }

    #[test]
    fn fingerprint_matches_python_for_yaml_scalars_and_quoting() -> Result<()> {
        for (front, expected) in [
            (
                r#"id: sample
kind: On
title: 012
requires: [off, "no", 0o70, 1e5, 1.0e-05, 2026-01-09, 2026-01-09T01:02:00-05:00, 18446744073709551616]"#,
                "1ed4f46ce5",
            ),
            (
                r#"id: sample
kind: lemma
requires: &same [yes, "Yes"]
title: >-
  One
  Two
summary: *same"#,
                "917eb5c996",
            ),
            (
                r#"id: sample
kind: "lemma"
title: "both \" and ' and \x85"
requires: []"#,
                "c8fe07163a",
            ),
            ("id: sample\nkind: 2026-1-9\ntitle: 1.23e+21", "51e3d8d6b5"),
        ] {
            assert_eq!(
                source_fingerprint(&format!("---\n{front}\n---\nBody"))?,
                expected,
                "{front}"
            );
        }
        Ok(())
    }

    #[test]
    fn recording_a_fingerprint_preserves_yaml_and_markdown_outside_the_field() -> Result<()> {
        let fingerprint = "0123456789";
        for (before, after) in [
            (
                "id: example\nsource_fingerprint: # previous empty value\ntitle: 日本語",
                "id: example\nsource_fingerprint: '0123456789' # previous empty value\ntitle: 日本語",
            ),
            (
                "{id: example, title: 日本語, 'source_fingerprint': \"old\"} # flow",
                "{id: example, title: 日本語, 'source_fingerprint': '0123456789'} # flow",
            ),
            (
                "{id: example, title: 'Test'} # flow",
                "{source_fingerprint: '0123456789', id: example, title: 'Test'} # flow",
            ),
            (
                "id: example\nsource_fingerprint: >-\n  old\n  value\n# Preserve comment\nsummary: text",
                "id: example\nsource_fingerprint: '0123456789'\n# Preserve comment\nsummary: text",
            ),
            (
                "# Comment\n  id: example\n  title: Test",
                "# Comment\n  source_fingerprint: '0123456789'\n  id: example\n  title: Test",
            ),
        ] {
            let wrap = |front| format!("---\n{front}\n---\nBody\n## Notes\nKeep these as well.\n");
            assert_eq!(with_source_fingerprint(&wrap(before), fingerprint)?, wrap(after));
        }
        Ok(())
    }
}
