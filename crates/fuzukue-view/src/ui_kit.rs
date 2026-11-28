//! Canonical, local UI tree shared by the XML transport and JSON.

use serde::{Deserialize, Serialize};
use std::fmt;

pub(crate) const MAX_DEPTH: usize = 8;
pub(crate) const MAX_CHILDREN: usize = 24;
pub(crate) const MAX_LABEL: usize = 512;
pub(crate) const MAX_TEXT: usize = 2048;
pub(crate) const MAX_ROWS: usize = 24;
pub(crate) const MAX_CELLS: usize = 6;
pub(crate) const MAX_COLUMNS: u16 = 4;
const MAX_COMPONENTS: usize = 256;
const MAX_TOTAL_TEXT: usize = 16 * 1024;
const MAX_JSON: usize = 128 * 1024;

/// JSON Schema for the v1 component tree. Use validation for cross-field and UTF-8 byte limits.
pub const JSON_SCHEMA: &str = include_str!("../../../docs/ui-kit.schema.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Info,
    Success,
    Warn,
    Error,
    Accent,
    Muted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextKind {
    #[default]
    Body,
    Heading,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListKind {
    #[default]
    Unordered,
    Ordered,
    Checklist,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Current,
    Done,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Span {
    Plain {
        text: String,
    },
    Code {
        text: String,
    },
    Kbd {
        text: String,
    },
    Badge {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
    },
}

impl Span {
    pub fn text(&self) -> &str {
        match self {
            Self::Plain { text }
            | Self::Code { text }
            | Self::Kbd { text }
            | Self::Badge { text, .. } => text,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    pub body: Vec<Component>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub title: String,
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub body: Vec<Component>,
}

/// One labeled outcome of a decision.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowBranch {
    pub label: String,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

/// A step or a branching decision in a top-to-bottom flowchart.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowNode {
    Step { label: String, #[serde(skip_serializing_if = "Option::is_none")] tone: Option<Tone> },
    Decision { question: String, branches: Vec<FlowBranch> },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub value: f64,
    pub tone: Tone,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Component {
    Card {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default)]
        body: Vec<Self>,
    },
    Grid {
        columns: u16,
        items: Vec<Self>,
    },
    Stat {
        label: String,
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    Progress {
        value: f64,
        max: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        segments: Vec<Segment>,
    },
    Text {
        #[serde(default)]
        kind: TextKind,
        spans: Vec<Span>,
    },
    List {
        #[serde(default)]
        kind: ListKind,
        items: Vec<ListItem>,
    },
    Steps {
        steps: Vec<Step>,
    },
    Flowchart {
        nodes: Vec<FlowNode>,
    },
    Details {
        title: String,
        #[serde(default)]
        open: bool,
        #[serde(default)]
        body: Vec<Self>,
    },
    Divider,
    Callout {
        severity: Severity,
        title: String,
        #[serde(default)]
        body: Vec<Self>,
    },
    /// The first row contains column headings.
    Table {
        rows: Vec<Vec<String>>,
    },
    Finding {
        deck: String,
        id: String,
    },
}

#[derive(Debug)]
pub enum JsonError {
    TooLarge,
    Syntax(serde_json::Error),
    Invalid(&'static str),
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("UI JSON exceeds 128 KiB"),
            Self::Syntax(error) => error.fmt(f),
            Self::Invalid(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for JsonError {}

/// Decode one JSON tree and enforce the same model constraints used for XML.
pub fn from_json(source: &str) -> Result<Component, JsonError> {
    if source.len() > MAX_JSON {
        return Err(JsonError::TooLarge);
    }
    let component: Component = serde_json::from_str(source).map_err(JsonError::Syntax)?;
    component.validate().map_err(JsonError::Invalid)?;
    Ok(component)
}

/// Encode one validated tree as compact JSON.
pub fn to_json(component: &Component) -> Result<String, JsonError> {
    component.validate().map_err(JsonError::Invalid)?;
    let json = serde_json::to_string(component).map_err(JsonError::Syntax)?;
    if json.len() > MAX_JSON {
        return Err(JsonError::TooLarge);
    }
    Ok(json)
}

impl Component {
    /// Validate a tree supplied through any transport.
    pub fn validate(&self) -> Result<(), &'static str> {
        Validator {
            text_bytes: 0,
            components: 0,
        }
        .component(self, 1)
    }

    /// Human-readable text in document order for selection and fallback.
    pub fn plain_text(&self) -> String {
        let mut lines = Vec::new();
        self.text_lines(&mut lines);
        lines.join("\n")
    }

    fn text_lines(&self, lines: &mut Vec<String>) {
        let mut body_lines = |title: &str, body: &[Self]| {
            lines.push(title.to_owned());
            for child in body {
                child.text_lines(lines);
            }
        };
        match self {
            Self::Card { title, body, .. }
            | Self::Details { title, body, .. }
            | Self::Callout { title, body, .. } => body_lines(title, body),
            Self::Grid { items, .. } => {
                for child in items {
                    child.text_lines(lines);
                }
            }
            Self::Text { spans, .. } => lines.push(spans.iter().map(Span::text).collect()),
            Self::Stat {
                label,
                value,
                unit,
                hint,
                ..
            } => {
                lines.push(format!("{label}: {value}{}", unit.as_deref().unwrap_or("")));
                lines.extend(hint.iter().cloned());
            }
            Self::Progress {
                value,
                max,
                label,
                caption,
                ..
            } => {
                lines.extend(label.iter().cloned());
                lines.push(format!("{value} / {max}"));
                lines.extend(caption.iter().cloned());
            }
            Self::List { items, .. } => {
                for item in items {
                    for child in &item.body {
                        child.text_lines(lines);
                    }
                }
            }
            Self::Steps { steps } => {
                for step in steps {
                    body_lines(&step.title, &step.body);
                }
            }
            Self::Flowchart { nodes } => {
                for node in nodes {
                    match node {
                        FlowNode::Step { label, .. } => lines.push(format!("↓ {label}")),
                        FlowNode::Decision { question, branches } => {
                            lines.push(format!("↓ {question}"));
                            for branch in branches {
                                lines.push(format!("{} → {}", branch.label, branch.result));
                            }
                        }
                    }
                }
            }
            Self::Table { rows } => lines.extend(rows.iter().map(|row| row.join(" · "))),
            Self::Finding { deck, id } => lines.push(format!("{deck} · {id}")),
            Self::Divider => {}
        }
    }
}

struct Validator {
    text_bytes: usize,
    components: usize,
}

impl Validator {
    fn text(&mut self, text: &str, max: usize) -> Result<(), &'static str> {
        self.text_bytes += text.len();
        if text.len() > max || self.text_bytes > MAX_TOTAL_TEXT {
            return Err("UI text exceeds the byte limit");
        }
        if !safe_text(text) {
            return Err("UI text contains unsupported control characters");
        }
        Ok(())
    }

    fn label(&mut self, text: &str) -> Result<(), &'static str> {
        if text.trim().is_empty() || text.contains(['\n', '\r', '\t']) {
            return Err("UI labels must be nonempty single lines");
        }
        self.text(text, MAX_LABEL)
    }

    fn optional_label(&mut self, text: &Option<String>) -> Result<(), &'static str> {
        text.as_deref().map_or(Ok(()), |text| self.label(text))
    }

    fn children(
        &mut self,
        children: &[Component],
        depth: usize,
        nonempty: bool,
    ) -> Result<(), &'static str> {
        count(children, nonempty)?;
        for child in children {
            self.component(child, depth)?;
        }
        Ok(())
    }

    fn component(&mut self, component: &Component, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_DEPTH {
            return Err("UI tree exceeds 8 component levels");
        }
        self.components += 1;
        if self.components > MAX_COMPONENTS {
            return Err("UI tree exceeds 256 components");
        }
        match component {
            Component::Card { title, body, .. }
            | Component::Details { title, body, .. }
            | Component::Callout { title, body, .. } => {
                self.label(title)?;
                self.children(body, depth + 1, false)?;
            }
            Component::Grid { columns, items } => {
                if !(1..=MAX_COLUMNS).contains(columns) {
                    return Err("Grid requires 1 to 4 columns");
                }
                self.children(items, depth + 1, true)?;
            }
            Component::Stat {
                label,
                value,
                unit,
                hint,
                ..
            } => {
                self.label(label)?;
                self.label(value)?;
                self.optional_label(unit)?;
                self.optional_label(hint)?;
            }
            Component::Progress {
                value,
                max,
                label,
                caption,
                segments,
                ..
            } => {
                self.optional_label(label)?;
                self.optional_label(caption)?;
                count(segments, false)?;
                if !max.is_finite() || *max <= 0.0 || !(0.0..=*max).contains(value) {
                    return Err("Progress requires 0 <= value <= max and a positive finite max");
                }
                let mut used = 0.0;
                for segment in segments {
                    if !(0.0..=*value).contains(&segment.value) || segment.value == 0.0 {
                        return Err("Progress segments require positive values");
                    }
                    used += segment.value / max;
                }
                if used > value / max + 1e-12 {
                    return Err("Progress segments exceed the completed value");
                }
            }
            Component::Text { spans, .. } => {
                count(spans, true)?;
                let mut length = 0;
                for span in spans {
                    let text = span.text();
                    length += text.len();
                    match span {
                        Span::Plain { .. } => self.text(text, MAX_TEXT)?,
                        _ => self.label(text)?,
                    }
                }
                if length > MAX_TEXT || spans.iter().all(|span| span.text().trim().is_empty()) {
                    return Err("A text block requires 1 to 2048 bytes of text");
                }
            }
            Component::List { kind, items } => {
                count(items, true)?;
                for item in items {
                    if *kind != ListKind::Checklist && item.checked.is_some() {
                        return Err("Only checklist items may specify checked");
                    }
                    self.children(&item.body, depth + 1, true)?;
                }
            }
            Component::Steps { steps } => {
                count(steps, true)?;
                for step in steps {
                    self.label(&step.title)?;
                    self.children(&step.body, depth + 1, false)?;
                }
            }
            Component::Flowchart { nodes } => {
                count(nodes, true)?;
                for node in nodes {
                    match node {
                        FlowNode::Step { label, .. } => self.label(label)?,
                        FlowNode::Decision { question, branches } => {
                            self.label(question)?;
                            count(branches, true)?;
                            if branches.len() < 2 {
                                return Err("A flowchart decision needs at least two branches");
                            }
                            for branch in branches {
                                self.label(&branch.label)?;
                                self.label(&branch.result)?;
                            }
                        }
                    }
                }
            }
            Component::Table { rows } => {
                let Some(header) = rows.first() else {
                    return Err("Table requires a header row");
                };
                if rows.len() > MAX_ROWS || !(1..=MAX_CELLS).contains(&header.len()) {
                    return Err("Table requires at most 24 rows and 1 to 6 columns");
                }
                for row in rows {
                    if row.len() != header.len() {
                        return Err("Table rows must have the same number of cells");
                    }
                    for cell in row {
                        self.text(cell, MAX_LABEL)?;
                    }
                }
            }
            Component::Finding { deck, id } => {
                self.text(deck, MAX_LABEL)?;
                self.text(id, MAX_LABEL)?;
                if !valid_deck(deck) || !valid_finding_id(id) {
                    return Err("Finding requires a project-relative deck and a lowercase UUID");
                }
            }
            Component::Divider => {}
        }
        Ok(())
    }
}

fn count<T>(items: &[T], nonempty: bool) -> Result<(), &'static str> {
    if items.len() > MAX_CHILDREN || (nonempty && items.is_empty()) {
        return Err("UI collection must contain at most 24 entries and may require at least one");
    }
    Ok(())
}

pub(crate) fn safe_text(text: &str) -> bool {
    !text.chars().any(|ch| {
        (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}'
                         | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}

fn valid_deck(deck: &str) -> bool {
    deck == "."
        || deck.split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && !segment.trim().is_empty()
                && !segment.contains([':', '\\', '%', '?', '#', '<', '>', '\n', '\r', '\t'])
        })
}

fn valid_finding_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trip_preserves_local_text_and_canonical_names() {
        let json = json!({
            "type": "card", "title": "Progress", "tone": "muted", "body": [
                {"type": "stat", "label": "Proofs", "value": "10", "tone": "warn"},
                {"type": "progress", "value": 10, "max": 13},
                {"type": "details", "title": "Notes", "open": true, "body": [
                    {"type": "text", "spans": [
                        {"type":"plain","text":"Literal [link](https://example.invalid) and "},
                        {"type":"code","text":"<script>"},
                        {"type":"badge","text":"New","tone":"accent"},
                        {"type":"kbd","text":"Enter"}
                    ]}
                ]}
            ]
        });
        let tree = from_json(&json.to_string()).unwrap();
        assert!(tree
            .plain_text()
            .contains("Literal [link](https://example.invalid) and <script>"));
        let encoded = to_json(&tree).unwrap();
        assert_eq!(from_json(&encoded).unwrap(), tree);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(JSON_SCHEMA).unwrap()["$ref"],
            "#/$defs/component"
        );
    }

    #[test]
    fn validation_rejects_unknown_fields_and_bad_cross_field_values() {
        for invalid in [
            json!({"type":"future"}),
            json!({"type":"card","title":"External","href":"https://example.invalid"}),
            json!({"type":"text","spans":[{"type":"code","text":"x","src":"file:///x"}]}),
            json!({"type":"stat","label":"Proofs","value":"2","tone":"orange"}),
            json!({"type":"finding","deck":"../other","id":"cb38ac8b-d670-47be-91e3-9c4ecdb61d26"}),
            json!({"type":"progress","value":10,"max":13,"segments":[{"value":12,"tone":"warn"}]}),
            json!({"type":"table","rows":[["a","b"],["c"]]}),
            json!({"type":"list","items":[{"checked":false,"body":[{"type":"divider"}]}]}),
            json!({"type":"text","spans":[{"type":"plain","text":"abc\u{202e}def"}]}),
            json!({"type":"text","spans":[{"type":"plain","text":"   "}]}),
            json!({"type":"text","spans":[{"type":"plain","text":"é".repeat(MAX_TEXT / 2 + 1)}]}),
            json!({"type":"card","title":"x","body":vec![json!({"type":"divider"}); MAX_CHILDREN + 1]}),
        ] {
            assert!(from_json(&invalid.to_string()).is_err(), "{invalid}");
        }
    }

    #[test]
    fn json_bounds_depth_total_text_and_component_count() {
        let mut nested = json!({"type":"divider"});
        for _ in 1..MAX_DEPTH {
            nested = json!({"type":"card", "title":"x", "body":[nested]});
        }
        assert!(from_json(&nested.to_string()).is_ok());
        nested = json!({"type":"card", "title":"x", "body":[nested]});
        assert!(matches!(
            from_json(&nested.to_string()),
            Err(JsonError::Invalid(_))
        ));

        let paragraph =
            json!({"type":"text","spans":[{"type":"plain","text":"x".repeat(MAX_TEXT)}]});
        let body = vec![paragraph; MAX_TOTAL_TEXT / MAX_TEXT];
        let too_much_text = json!({"type":"card","title":"x","body":body});
        assert!(matches!(
            from_json(&too_much_text.to_string()),
            Err(JsonError::Invalid(_))
        ));

        let group =
            json!({"type":"card","title":"x","body":vec![json!({"type":"divider"}); MAX_CHILDREN]});
        let tree = json!({"type":"card","title":"x","body":vec![group; MAX_COMPONENTS / MAX_CHILDREN + 1]});
        assert!(matches!(
            from_json(&tree.to_string()),
            Err(JsonError::Invalid(_))
        ));
        assert!(matches!(
            from_json(&" ".repeat(MAX_JSON + 1)),
            Err(JsonError::TooLarge)
        ));
    }

    #[test]
    fn decimal_progress_round_trips_at_f64_precision() {
        let max = 1.0_f64;
        let value = f64::from_bits(max.to_bits() - 1);
        let tree = Component::Progress {
            value,
            max,
            label: None,
            caption: None,
            tone: None,
            segments: Vec::new(),
        };
        assert_eq!(from_json(&to_json(&tree).unwrap()).unwrap(), tree);
    }
}
