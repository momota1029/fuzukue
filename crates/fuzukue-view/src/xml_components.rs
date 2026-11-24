//! Read-only XML component vocabulary for Markdown chat messages.

use fuzukue_proto::MarkdownComponentCapability;
use markdown::mdast::Node;

const FENCE_LANGUAGE: &str = "fuzukue-v1";
const VERSION: u32 = 1;
const TAGS: &[&str] = &["callout", "table", "finding"];
const MAX_BLOCK: usize = 16 * 1024;
const MAX_ATTRIBUTE: usize = 512;
const MAX_BODY: usize = 2048;
const MAX_ROWS: usize = 24;
const MAX_COLUMNS: usize = 6;
const MAX_CELL: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Component {
    Callout {
        severity: Severity,
        title: String,
        body: String,
    },
    /// The first row contains column headings.
    Table(Vec<Vec<String>>),
    Finding {
        deck: String,
        id: String,
    },
}

pub fn capabilities() -> Vec<MarkdownComponentCapability> {
    TAGS.iter()
        .map(|&tag| MarkdownComponentCapability {
            tag: tag.into(),
            version: VERSION,
        })
        .collect()
}

/// Recognize a complete, top-level component fence in a Markdown AST. Markdown
/// itself supplies the code-block fallback for every declined node.
pub fn parse_block(node: &Node, source: &str) -> Option<Component> {
    let Node::Code(code) = node else { return None };
    let position = code.position.as_ref()?;
    if code.lang.as_deref() != Some(FENCE_LANGUAGE)
        || code.meta.is_some()
        || position.start.column != 1
        || code.value.len() > MAX_BLOCK
    {
        return None;
    }
    let literal = source.get(position.start.offset..position.end.offset)?;
    let after = source
        .get(position.end.offset..)?
        .trim_start_matches([' ', '\t']);
    let (opening, rest) = literal.split_once('\n')?;
    let (_, closing) = rest.rsplit_once('\n')?;
    // A closing fence at the end of a delta can still grow into a different
    // line. Commit only after the newline establishes a closing delimiter.
    if opening.trim_end_matches('\r').strip_prefix("~~~") != Some(FENCE_LANGUAGE)
        || closing.trim_end_matches([' ', '\t', '\r']) != "~~~"
        || !(after.starts_with('\n') || after.starts_with("\r\n"))
    {
        return None;
    }
    parse_component(&code.value)
}

fn parse_component(source: &str) -> Option<Component> {
    let mut xml = Xml {
        rest: source.trim(),
    };
    let (tag, attrs, empty) = xml.open()?;
    let component = match (tag, empty) {
        ("callout", false) if attrs.len() == 2 => {
            let severity = match attribute(&attrs, "severity")? {
                "info" => Severity::Info,
                "warn" => Severity::Warn,
                "error" => Severity::Error,
                _ => return None,
            };
            let title = attribute(&attrs, "title")?.trim();
            if title.is_empty() || title.contains(['\n', '\r', '\t']) {
                return None;
            }
            let body = xml.text(MAX_BODY)?.trim().to_owned();
            xml.close("callout")?;
            Component::Callout {
                severity,
                title: title.to_owned(),
                body,
            }
        }
        ("table", false) if attrs.is_empty() => Component::Table(xml.table()?),
        ("finding", true) if attrs.len() == 2 => {
            let deck = attribute(&attrs, "deck")?;
            let id = attribute(&attrs, "id")?;
            if !valid_deck(deck) || !valid_finding_id(id) {
                return None;
            }
            Component::Finding {
                deck: deck.into(),
                id: id.into(),
            }
        }
        _ => return None,
    };
    xml.rest.trim().is_empty().then_some(component)
}

fn attribute<'a>(attrs: &'a [(&str, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.as_str())
}

fn valid_deck(deck: &str) -> bool {
    deck == "."
        || deck.split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && !segment.trim().is_empty()
                && segment.chars().all(|ch| {
                    !matches!(
                        ch,
                        ':' | '\\' | '%' | '?' | '#' | '<' | '>' | '\n' | '\r' | '\t'
                    )
                })
        })
}

fn valid_finding_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

struct Xml<'a> {
    rest: &'a str,
}

impl<'a> Xml<'a> {
    fn take(&mut self, prefix: &str) -> Option<()> {
        self.rest = self.rest.strip_prefix(prefix)?;
        Some(())
    }

    fn whitespace(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
    }

    fn name(&mut self) -> Option<&'a str> {
        let end = self
            .rest
            .bytes()
            .take_while(|byte| byte.is_ascii_lowercase() || *byte == b'-')
            .count();
        let (name, rest) = self.rest.split_at(end);
        self.rest = rest;
        (!name.is_empty()).then_some(name)
    }

    fn open(&mut self) -> Option<(&'a str, Vec<(&'a str, String)>, bool)> {
        self.take("<")?;
        let tag = self.name()?;
        let mut attrs = Vec::new();
        loop {
            let before_space = self.rest.len();
            self.whitespace();
            if self.take("/>").is_some() {
                return Some((tag, attrs, true));
            }
            if self.take(">").is_some() {
                return Some((tag, attrs, false));
            }
            if self.rest.len() == before_space || attrs.len() == 2 {
                return None;
            }
            let key = self.name()?;
            if attrs.iter().any(|(name, _)| *name == key) {
                return None;
            }
            self.whitespace();
            self.take("=")?;
            self.whitespace();
            let quote = self
                .rest
                .chars()
                .next()
                .filter(|ch| matches!(ch, '"' | '\''))?;
            let (value, rest) = self.rest[1..].split_once(quote)?;
            attrs.push((key, decode(value, MAX_ATTRIBUTE)?));
            self.rest = rest;
        }
    }

    fn close(&mut self, tag: &str) -> Option<()> {
        self.take("</")?;
        if self.name()? != tag {
            return None;
        }
        self.whitespace();
        self.take(">")
    }

    fn text(&mut self, limit: usize) -> Option<String> {
        let end = self.rest.find('<')?;
        let (text, rest) = self.rest.split_at(end);
        self.rest = rest;
        decode(text, limit)
    }

    fn table(&mut self) -> Option<Vec<Vec<String>>> {
        let mut rows: Vec<Vec<String>> = Vec::new();
        self.whitespace();
        while self.take("<row>").is_some() {
            if rows.len() == MAX_ROWS {
                return None;
            }
            let mut cells = Vec::new();
            self.whitespace();
            while self.take("<cell>").is_some() {
                if cells.len() == MAX_COLUMNS {
                    return None;
                }
                cells.push(self.text(MAX_CELL)?);
                self.close("cell")?;
                self.whitespace();
            }
            self.close("row")?;
            if cells.is_empty()
                || rows
                    .first()
                    .is_some_and(|header| header.len() != cells.len())
            {
                return None;
            }
            rows.push(cells);
            self.whitespace();
        }
        self.close("table")?;
        (!rows.is_empty()).then_some(rows)
    }
}

fn decode(text: &str, limit: usize) -> Option<String> {
    if text.len() > limit
        || text.contains(['<', '\u{061c}', '\u{200e}', '\u{200f}'])
        || text.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return None;
    }
    let mut parts = text.split('&');
    let mut decoded = parts.next()?.to_owned();
    for part in parts {
        let (entity, tail) = part.split_once(';')?;
        decoded.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => return None,
        });
        decoded.push_str(tail);
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALLOUT: &str = r#"<callout title="照合 &amp; 確認" severity="warn">Literal **Markdown** and &lt;b&gt;.</callout>"#;
    const FINDING: &str =
        r#"<finding id="cb38ac8b-d670-47be-91e3-9c4ecdb61d26" deck="research/concentration"/>"#;
    const TABLE: &str = "<table>\n<row><cell>Item</cell><cell>Result</cell></row>\n<row><cell>Proof</cell><cell>Ready</cell></row>\n</table>";

    fn fence(xml: &str) -> String {
        format!("~~~fuzukue-v1\n{xml}\n~~~\n")
    }

    fn components(source: &str) -> Vec<Component> {
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
        root.children()
            .unwrap()
            .iter()
            .filter_map(|node| parse_block(node, source))
            .collect()
    }

    #[test]
    fn reads_the_three_components_in_order() {
        let source = format!(
            "Before\n\n{}\n{}\n{}\nAfter",
            fence(CALLOUT),
            fence(TABLE),
            fence(FINDING)
        );
        let expected = vec![
            Component::Callout {
                severity: Severity::Warn,
                title: "照合 & 確認".into(),
                body: "Literal **Markdown** and <b>.".into(),
            },
            Component::Table(vec![
                vec!["Item".into(), "Result".into()],
                vec!["Proof".into(), "Ready".into()],
            ]),
            Component::Finding {
                deck: "research/concentration".into(),
                id: "cb38ac8b-d670-47be-91e3-9c4ecdb61d26".into(),
            },
        ];
        assert_eq!(components(&source), expected);
        assert_eq!(components(&source.replace('\n', "\r\n")), expected);
    }

    #[test]
    fn every_streaming_prefix_stays_code_until_the_closing_line_is_complete() {
        let source = fence(CALLOUT);
        for (end, _) in source.char_indices() {
            assert!(components(&source[..end]).is_empty(), "prefix {end}");
        }
        assert_eq!(components(&source).len(), 1);
        assert!(components(&source.replace("\n~~~\n", "\n~~~continued\n")).is_empty());
        assert_eq!(components(&(source + "\nLater text")).len(), 1);
    }

    #[test]
    fn ordinary_code_nested_fences_and_future_versions_are_literal() {
        let good = fence(CALLOUT);
        assert!(components(&format!("~~~~markdown\n{good}~~~~\n")).is_empty());
        let quoted = good
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect::<String>();
        assert!(components(&quoted).is_empty());
        let listed = format!("- {}", good.replace('\n', "\n  ").trim_end());
        assert!(components(&(listed + "\n")).is_empty());
        assert!(components(&good.replace("fuzukue-v1", "fuzukue-v2")).is_empty());
        assert!(components(CALLOUT).is_empty());
    }

    #[test]
    fn an_unsupported_block_does_not_claim_the_next_component() {
        let bad = fence("<future/>");
        let source = bad + "\n" + &fence(TABLE);
        let root = markdown::to_mdast(&source, &markdown::ParseOptions::gfm()).unwrap();
        let [Node::Code(first), second] = root.children().unwrap().as_slice() else {
            panic!("expected two distinct code blocks");
        };
        assert_eq!(first.value, "<future/>");
        assert!(matches!(
            parse_block(second, &source),
            Some(Component::Table(_))
        ));
        assert_eq!(components(&source).len(), 1);
    }

    #[test]
    fn declines_unknown_markup_without_rendering_any_children() {
        for xml in [
            "<future/>",
            r#"<callout severity="info" title="Example" extra="x">Hello</callout>"#,
            r#"<callout severity="info" severity="warn">Hello</callout>"#,
            r#"<callout severity="info" title="Example"><callout severity="warn" title="Nested">Text</callout></callout>"#,
            r#"<callout severity="info" title="Example">oops</table>"#,
            r#"<callout severity="info" title="Example">&external;</callout>"#,
            r#"<table><row><cell>One</cell></row><row><cell>Two</cell><cell>Three</cell></row></table>"#,
            r#"<table><row class="x"><cell>Text</cell></row></table>"#,
            r#"<table><row><cell><table/></cell></row></table>"#,
            r#"<table><row><cell>One</cell></row></table><future/>"#,
        ] {
            assert!(components(&fence(xml)).is_empty(), "{xml}");
        }
    }

    #[test]
    fn no_script_url_or_external_entity_can_become_a_component_action() {
        for xml in [
            r#"<script>alert(1)</script>"#,
            r#"<callout severity="info" title="Example"><img src="https://example.invalid/x"/></callout>"#,
            r#"<table><row><cell><a href="javascript:example()">Click</a></cell></row></table>"#,
            r#"<finding id="cb38ac8b-d670-47be-91e3-9c4ecdb61d26" deck="javascript:example()"/>"#,
            r#"<finding id="cb38ac8b-d670-47be-91e3-9c4ecdb61d26" deck="../other"/>"#,
            r#"<finding id="javascript:example()" deck="research/concentration"/>"#,
            r#"<!DOCTYPE table SYSTEM "https://example.invalid/x"><table/>"#,
        ] {
            assert!(components(&fence(xml)).is_empty(), "{xml}");
        }
        assert_eq!(
            components(&fence(
                "<table><row><cell>&lt;script&gt; javascript:example()</cell></row></table>"
            )),
            [Component::Table(vec![vec![
                "<script> javascript:example()".into()
            ]])]
        );
    }

    #[test]
    fn bounds_large_attributes_content_and_tables() {
        let huge = "x".repeat(MAX_ATTRIBUTE + 1);
        let xml = format!(r#"<callout severity="error" title="{huge}">Body</callout>"#);
        assert!(components(&fence(&xml)).is_empty());
        assert!(components(&fence(&xml.repeat(MAX_BLOCK / xml.len() + 1))).is_empty());

        let rows = "<row><cell>x</cell></row>".repeat(MAX_ROWS + 1);
        assert!(components(&fence(&format!("<table>{rows}</table>"))).is_empty());
        let columns = "<cell>x</cell>".repeat(MAX_COLUMNS + 1);
        assert!(components(&fence(&format!("<table><row>{columns}</row></table>"))).is_empty());
        let cell = "x".repeat(MAX_CELL + 1);
        assert!(components(&fence(&format!(
            "<table><row><cell>{cell}</cell></row></table>"
        )))
        .is_empty());
    }
}
