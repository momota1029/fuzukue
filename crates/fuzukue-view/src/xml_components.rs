//! Fenced XML transport for the canonical UI tree.

use crate::ui_kit::{
    self, Component, ListItem, ListKind, Segment, Severity, Span, Step, StepStatus, TextKind, Tone,
    MAX_CELLS, MAX_CHILDREN, MAX_DEPTH, MAX_LABEL, MAX_ROWS, MAX_TEXT,
};
use fuzukue_proto::MarkdownComponentCapability;
use markdown::mdast::Node;

const FENCE_LANGUAGE: &str = "fuzukue-v1";
const NAMESPACE: &str = "urn:fuzukue:ui:v1";
const PREFIX: &str = "fuzukue:";
const TAGS: &[&str] = &[
    "callout",
    "table",
    "finding",
    "card",
    "grid",
    "stat",
    "progress",
    "text",
    "badge",
    "list",
    "steps",
    "details",
    "accordion",
    "divider",
    "kbd",
    "code",
];
const MAX_BLOCK: usize = 16 * 1024;

pub fn capabilities() -> Vec<MarkdownComponentCapability> {
    TAGS.iter()
        .map(|&tag| MarkdownComponentCapability {
            tag: tag.into(),
            version: 1,
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
    let mut root = xml.open()?;
    if let Some(index) = root
        .attrs
        .iter()
        .position(|(key, _)| *key == "xmlns:fuzukue")
    {
        if !root.name.starts_with(PREFIX) || root.attrs.remove(index).1 != NAMESPACE {
            return None;
        }
    }
    let component = xml.component(root, 1)?;
    component.validate().ok()?;
    xml.rest.trim().is_empty().then_some(component)
}

struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, String)>,
    empty: bool,
}

impl Tag<'_> {
    fn allowed(&self, keys: &[&str]) -> Option<()> {
        self.attrs
            .iter()
            .all(|(key, _)| keys.contains(key))
            .then_some(())
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }

    fn local(&self) -> &str {
        self.name.strip_prefix(PREFIX).unwrap_or(self.name)
    }

    fn tone(&self) -> Option<Option<Tone>> {
        match self.get("tone") {
            None => Some(None),
            Some("info") => Some(Some(Tone::Info)),
            Some("success") => Some(Some(Tone::Success)),
            Some("warn") => Some(Some(Tone::Warn)),
            Some("error") => Some(Some(Tone::Error)),
            Some("accent") => Some(Some(Tone::Accent)),
            Some("muted") => Some(Some(Tone::Muted)),
            _ => None,
        }
    }
}

struct Xml<'a> {
    rest: &'a str,
}

impl<'a> Xml<'a> {
    fn component(&mut self, tag: Tag<'_>, depth: usize) -> Option<Component> {
        if depth > MAX_DEPTH
            || (!tag.name.starts_with(PREFIX)
                && !matches!(tag.name, "callout" | "table" | "finding"))
        {
            return None;
        }
        Some(match tag.local() {
            "card" => {
                tag.allowed(&["title", "tone"])?;
                Component::Card {
                    title: tag.get("title")?.into(),
                    tone: tag.tone()?,
                    body: self.body(&tag, depth + 1)?,
                }
            }
            "grid" => {
                tag.allowed(&["columns"])?;
                Component::Grid {
                    columns: tag.get("columns")?.parse().ok()?,
                    items: self.body(&tag, depth + 1)?,
                }
            }
            "stat" if tag.empty => {
                tag.allowed(&["label", "value", "unit", "tone", "hint"])?;
                Component::Stat {
                    label: tag.get("label")?.into(),
                    value: tag.get("value")?.into(),
                    unit: tag.get("unit").map(str::to_owned),
                    tone: tag.tone()?,
                    hint: tag.get("hint").map(str::to_owned),
                }
            }
            "progress" => self.progress(&tag)?,
            "text" => {
                tag.allowed(&["style"])?;
                let kind = match tag.get("style").unwrap_or("body") {
                    "body" => TextKind::Body,
                    "heading" => TextKind::Heading,
                    _ => return None,
                };
                let mut body = self.flow(&tag, None)?;
                let [Component::Text { kind: actual, .. }] = body.as_mut_slice() else {
                    return None;
                };
                *actual = kind;
                body.pop()?
            }
            "badge" | "kbd" | "code" => paragraph(vec![self.span(&tag)?]),
            "list" => {
                tag.allowed(&["kind"])?;
                let kind = match tag.get("kind").unwrap_or("unordered") {
                    "unordered" => ListKind::Unordered,
                    "ordered" => ListKind::Ordered,
                    "checklist" => ListKind::Checklist,
                    _ => return None,
                };
                let items = self.members(&tag, MAX_CHILDREN, |xml, item| {
                    if item.name != "fuzukue:item" {
                        return None;
                    }
                    item.allowed(&["checked"])?;
                    let checked = item.get("checked").map(str::parse).transpose().ok()?;
                    Some(ListItem {
                        checked,
                        body: xml.body(&item, depth + 1)?,
                    })
                })?;
                Component::List { kind, items }
            }
            "steps" => {
                tag.allowed(&[])?;
                let steps = self.members(&tag, MAX_CHILDREN, |xml, step| {
                    if step.name != "fuzukue:step" {
                        return None;
                    }
                    step.allowed(&["title", "status"])?;
                    let status = match step.get("status").unwrap_or("pending") {
                        "pending" => StepStatus::Pending,
                        "current" => StepStatus::Current,
                        "done" => StepStatus::Done,
                        _ => return None,
                    };
                    Some(Step {
                        title: step.get("title")?.into(),
                        status,
                        body: xml.body(&step, depth + 1)?,
                    })
                })?;
                Component::Steps { steps }
            }
            "details" | "accordion" => {
                tag.allowed(&["title", "open"])?;
                Component::Details {
                    title: tag.get("title")?.into(),
                    open: tag.get("open").unwrap_or("false").parse().ok()?,
                    body: self.body(&tag, depth + 1)?,
                }
            }
            "divider" if tag.empty => {
                tag.allowed(&[])?;
                Component::Divider
            }
            "callout" if !tag.empty => {
                tag.allowed(&["severity", "title"])?;
                let severity = match tag.get("severity")? {
                    "info" => Severity::Info,
                    "warn" => Severity::Warn,
                    "error" => Severity::Error,
                    _ => return None,
                };
                let body = if tag.name == "callout" {
                    let text = self.text(MAX_TEXT)?.trim().to_owned();
                    self.close(tag.name)?;
                    (!text.is_empty())
                        .then(|| paragraph(vec![Span::Plain { text }]))
                        .into_iter()
                        .collect()
                } else {
                    self.body(&tag, depth + 1)?
                };
                Component::Callout {
                    severity,
                    title: tag.get("title")?.trim().into(),
                    body,
                }
            }
            "table" => Component::Table {
                rows: self.table(&tag)?,
            },
            "finding" if tag.empty => {
                tag.allowed(&["deck", "id"])?;
                Component::Finding {
                    deck: tag.get("deck")?.into(),
                    id: tag.get("id")?.into(),
                }
            }
            _ => return None,
        })
    }

    fn progress(&mut self, tag: &Tag<'_>) -> Option<Component> {
        tag.allowed(&["value", "max", "percent", "label", "caption", "tone"])?;
        let (value, max) = match (tag.get("percent"), tag.get("value"), tag.get("max")) {
            (Some(percent), None, None) => (percent.parse().ok()?, 100.0),
            (None, Some(value), Some(max)) => (value.parse().ok()?, max.parse().ok()?),
            _ => return None,
        };
        let segments = self.members(tag, MAX_CHILDREN, |_, segment| {
            if segment.name != "fuzukue:segment" || !segment.empty {
                return None;
            }
            segment.allowed(&["value", "tone"])?;
            Some(Segment {
                value: segment.get("value")?.parse().ok()?,
                tone: segment.tone()??,
            })
        })?;
        Some(Component::Progress {
            value,
            max,
            label: tag.get("label").map(str::to_owned),
            caption: tag.get("caption").map(str::to_owned),
            tone: tag.tone()?,
            segments,
        })
    }

    fn span(&mut self, tag: &Tag<'_>) -> Option<Span> {
        if tag.empty {
            return None;
        }
        let text = self.text(MAX_LABEL)?;
        self.close(tag.name)?;
        Some(match tag.name {
            "fuzukue:badge" => {
                tag.allowed(&["tone"])?;
                Span::Badge {
                    text,
                    tone: tag.tone()?,
                }
            }
            "fuzukue:kbd" => {
                tag.allowed(&[])?;
                Span::Kbd { text }
            }
            "fuzukue:code" => {
                tag.allowed(&[])?;
                Span::Code { text }
            }
            _ => return None,
        })
    }

    fn body(&mut self, tag: &Tag<'_>, depth: usize) -> Option<Vec<Component>> {
        self.flow(tag, Some(depth))
    }

    // Collect phrasing content into a single text block between adjacent components.
    fn flow(&mut self, tag: &Tag<'_>, depth: Option<usize>) -> Option<Vec<Component>> {
        let mut body = Vec::new();
        let mut spans = Vec::new();
        if tag.empty {
            return Some(body);
        }
        while !self.rest.starts_with("</") {
            if self.rest.starts_with('<') {
                let child = self.open()?;
                if matches!(child.name, "fuzukue:badge" | "fuzukue:kbd" | "fuzukue:code") {
                    spans.push(self.span(&child)?);
                } else {
                    flush_paragraph(&mut body, &mut spans);
                    body.push(self.component(child, depth?)?);
                }
            } else {
                spans.push(Span::Plain {
                    text: self.text(MAX_TEXT)?,
                });
            }
            if spans.len() > MAX_CHILDREN || body.len() > MAX_CHILDREN {
                return None;
            }
        }
        self.close(tag.name)?;
        flush_paragraph(&mut body, &mut spans);
        Some(body)
    }

    fn table(&mut self, tag: &Tag<'_>) -> Option<Vec<Vec<String>>> {
        tag.allowed(&[])?;
        self.members(tag, MAX_ROWS, |xml, row| {
            if !matches!(row.name, "row" | "fuzukue:row") || row.empty {
                return None;
            }
            row.allowed(&[])?;
            xml.members(&row, MAX_CELLS, |xml, cell| {
                if !matches!(cell.name, "cell" | "fuzukue:cell") || cell.empty {
                    return None;
                }
                cell.allowed(&[])?;
                let text = xml.text(MAX_LABEL)?;
                xml.close(cell.name)?;
                Some(text)
            })
        })
    }

    fn members<T>(
        &mut self,
        tag: &Tag<'_>,
        limit: usize,
        mut parse: impl FnMut(&mut Self, Tag<'a>) -> Option<T>,
    ) -> Option<Vec<T>> {
        let mut members = Vec::new();
        if !tag.empty {
            self.whitespace();
            while !self.rest.starts_with("</") {
                if members.len() == limit {
                    return None;
                }
                let member = self.open()?;
                members.push(parse(self, member)?);
                self.whitespace();
            }
            self.close(tag.name)?;
        }
        Some(members)
    }

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
            .take_while(|byte| byte.is_ascii_lowercase() || matches!(*byte, b':' | b'-'))
            .count();
        let (name, rest) = self.rest.split_at(end);
        self.rest = rest;
        (!name.is_empty()).then_some(name)
    }

    fn open(&mut self) -> Option<Tag<'a>> {
        self.take("<")?;
        let name = self.name()?;
        let mut attrs = Vec::new();
        loop {
            let before_space = self.rest.len();
            self.whitespace();
            if self.take("/>").is_some() {
                return Some(Tag {
                    name,
                    attrs,
                    empty: true,
                });
            }
            if self.take(">").is_some() {
                return Some(Tag {
                    name,
                    attrs,
                    empty: false,
                });
            }
            if self.rest.len() == before_space || attrs.len() == 6 {
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
            attrs.push((key, decode(value, MAX_LABEL)?));
            self.rest = rest;
        }
    }

    fn close(&mut self, name: &str) -> Option<()> {
        self.take("</")?;
        if self.name()? != name {
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
}

fn paragraph(spans: Vec<Span>) -> Component {
    Component::Text {
        kind: TextKind::Body,
        spans,
    }
}

fn flush_paragraph(body: &mut Vec<Component>, spans: &mut Vec<Span>) {
    if let Some(Span::Plain { text }) = spans.first_mut() {
        *text = text.trim_start().to_owned();
    }
    if let Some(Span::Plain { text }) = spans.last_mut() {
        text.truncate(text.trim_end().len());
    }
    spans.retain(|span| !span.text().is_empty());
    if !spans.is_empty() {
        body.push(paragraph(std::mem::take(spans)));
    }
}

fn decode(text: &str, limit: usize) -> Option<String> {
    if text.len() > limit || text.contains('<') || !ui_kit::safe_text(text) {
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
                body: vec![paragraph(vec![Span::Plain {
                    text: "Literal **Markdown** and <b>.".into(),
                }])],
            },
            Component::Table {
                rows: vec![
                    vec!["Item".into(), "Result".into()],
                    vec!["Proof".into(), "Ready".into()],
                ],
            },
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
            Some(Component::Table { .. })
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
            [Component::Table {
                rows: vec![vec!["<script> javascript:example()".into()]]
            }]
        );
    }

    #[test]
    fn bounds_large_attributes_content_and_tables() {
        let huge = "x".repeat(MAX_LABEL + 1);
        let xml = format!(r#"<callout severity="error" title="{huge}">Body</callout>"#);
        assert!(components(&fence(&xml)).is_empty());
        assert!(components(&fence(&xml.repeat(MAX_BLOCK / xml.len() + 1))).is_empty());

        let rows = "<row><cell>x</cell></row>".repeat(MAX_ROWS + 1);
        assert!(components(&fence(&format!("<table>{rows}</table>"))).is_empty());
        let columns = "<cell>x</cell>".repeat(MAX_CELLS + 1);
        assert!(components(&fence(&format!("<table><row>{columns}</row></table>"))).is_empty());
        let cell = "x".repeat(MAX_LABEL + 1);
        assert!(components(&fence(&format!(
            "<table><row><cell>{cell}</cell></row></table>"
        )))
        .is_empty());
    }

    #[test]
    fn namespaced_and_legacy_core_tags_share_the_model() {
        for legacy in [CALLOUT, TABLE, FINDING] {
            let prefixed = legacy
                .replace('<', "<fuzukue:")
                .replace("<fuzukue:/", "</fuzukue:");
            assert_eq!(components(&fence(&prefixed)), components(&fence(legacy)));
        }
        let tags: Vec<_> = capabilities()
            .into_iter()
            .map(|capability| {
                assert_eq!(capability.version, 1);
                capability.tag
            })
            .collect();
        assert_eq!(tags, TAGS);
    }

    #[test]
    fn nested_layout_and_inline_content_round_trip_through_json() {
        let xml = r#"<fuzukue:card xmlns:fuzukue="urn:fuzukue:ui:v1" title="Proofs &amp; notes">
<fuzukue:grid columns="2">
  <fuzukue:stat label="Partial" value="10" unit=" proofs" tone="warn" hint="Reviewed"/>
  <fuzukue:stat label="Open" value="3"/>
</fuzukue:grid>
<fuzukue:progress percent="76.9" caption="10 of 13">
  <fuzukue:segment value="10" tone="success"/>
  <fuzukue:segment value="20" tone="info"/>
</fuzukue:progress>
<fuzukue:divider/>
<fuzukue:text style="heading">Use <fuzukue:code>&lt;x&gt;</fuzukue:code> and <fuzukue:kbd>Enter</fuzukue:kbd> <fuzukue:badge tone="accent">New</fuzukue:badge></fuzukue:text>
<fuzukue:list kind="checklist"><fuzukue:item checked="true">First</fuzukue:item></fuzukue:list>
<fuzukue:steps><fuzukue:step title="Prove" status="current">
  <fuzukue:accordion title="More" open="true"><fuzukue:callout title="Read" severity="info">Check &quot;this&quot;.</fuzukue:callout></fuzukue:accordion>
</fuzukue:step></fuzukue:steps>
</fuzukue:card>"#;
        let source = fence(xml);
        let parsed = components(&source);
        let [tree @ Component::Card { body, .. }] = parsed.as_slice() else {
            panic!("{parsed:?}")
        };
        assert!(matches!(&body[0], Component::Grid { columns: 2, items } if items.len() == 2));
        assert!(
            matches!(&body[1], Component::Progress { value, max, segments, .. }
            if *value == 76.9 && *max == 100.0 && segments.len() == 2)
        );
        assert!(
            matches!(&body[3], Component::Text { kind: TextKind::Heading, spans }
            if spans.iter().any(|span| matches!(span, Span::Code { text } if text == "<x>")))
        );
        assert!(tree.plain_text().contains("Use <x> and Enter New"));
        assert!(tree.plain_text().contains("Check \"this\"."));
        assert_eq!(
            &ui_kit::from_json(&ui_kit::to_json(tree).unwrap()).unwrap(),
            tree
        );
        for (end, _) in source.char_indices() {
            assert!(components(&source[..end]).is_empty(), "prefix {end}");
        }
    }

    #[test]
    fn bounds_nesting_children_and_text() {
        let nested = |count| {
            format!(
                "{}<fuzukue:divider/>{}",
                r#"<fuzukue:card title="Nested">"#.repeat(count),
                "</fuzukue:card>".repeat(count)
            )
        };
        assert_eq!(components(&fence(&nested(MAX_DEPTH - 1))).len(), 1);
        assert!(components(&fence(&nested(MAX_DEPTH))).is_empty());
        let card = |body| format!(r#"<fuzukue:card title="Limits">{body}</fuzukue:card>"#);
        assert!(
            components(&fence(&card("<fuzukue:divider/>".repeat(MAX_CHILDREN + 1)))).is_empty()
        );
        assert!(components(&fence(&card("x".repeat(MAX_TEXT + 1)))).is_empty());
        let split_text = format!("{}<fuzukue:kbd>X</fuzukue:kbd>y", "x".repeat(MAX_TEXT - 1));
        assert!(components(&fence(&card(split_text))).is_empty());
    }

    #[test]
    fn invalid_namespaces_links_and_malformed_nested_values_decline_the_whole_block() {
        for xml in [
            r#"<card title="Only legacy tags are unprefixed"/>"#,
            r#"<fuzukue:card title="Other namespace" xmlns:fuzukue="urn:fuzukue:ui:v2"/>"#,
            r#"<trpg:dice xmlns:trpg="urn:trpg:v1"/>"#,
            r#"<fuzukue:card title="Nested declaration"><fuzukue:divider xmlns:fuzukue="urn:fuzukue:ui:v1"/></fuzukue:card>"#,
            r#"<fuzukue:card title="Unknown"><fuzukue:future/><fuzukue:divider/></fuzukue:card>"#,
            r#"<fuzukue:card title="Unclosed"><fuzukue:grid columns="2"></fuzukue:card>"#,
            r#"<fuzukue:card title="Mismatched"></card>"#,
            r#"<fuzukue:card title="Link" href="https://example.invalid"/>"#,
            r#"<fuzukue:text><fuzukue:kbd xlink:href="javascript:example()">Enter</fuzukue:kbd></fuzukue:text>"#,
            r#"<fuzukue:card title="Active contents"><script>run()</script></fuzukue:card>"#,
            r#"<fuzukue:text>Invalid &external;</fuzukue:text>"#,
            r#"<fuzukue:progress percent="101"/>"#,
            r#"<fuzukue:progress value="NaN" max="1"/>"#,
            r#"<fuzukue:progress value="1" max="0"/>"#,
            r#"<fuzukue:progress percent="30"><fuzukue:segment tone="info" value="31"/></fuzukue:progress>"#,
            r#"<fuzukue:list kind="ordered"><fuzukue:item checked="true">No</fuzukue:item></fuzukue:list>"#,
        ] {
            assert!(components(&fence(xml)).is_empty(), "{xml}");
        }
    }

    #[test]
    fn documented_examples_use_the_supported_grammar() {
        let docs = include_str!("../../../docs/xml-components.md");
        let counts: Vec<_> = docs
            .split("````markdown\n")
            .skip(1)
            .map(|rest| {
                let example = rest.split_once("\n````").unwrap().0;
                components(example).len()
            })
            .collect();
        assert_eq!(counts, [1, 1, 3]);
        let example = docs.split_once("````json\n").unwrap().1;
        let json = example.split_once("\n````").unwrap().0;
        assert!(matches!(
            ui_kit::from_json(json),
            Ok(Component::Card { .. })
        ));
    }
}
