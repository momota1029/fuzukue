use serde::{Deserialize, Serialize};

/// Maximum display width of a job label, in half-width columns.
pub const JOB_LABEL_COLUMNS: usize = 40;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DelegateDisplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    /// Full display-name request when the visible label had to be shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateNameChange {
    pub at_ms: i64,
    pub source: String,
    pub previous_label: Option<String>,
    pub label: String,
    pub label_source: Option<String>,
}

impl DelegateDisplay {
    pub fn from_title(title: Option<&str>, label: Option<&str>) -> Self {
        let title = title.unwrap_or_default().trim();
        let (card_id, rest) = split_card_id(title);
        let source = label.unwrap_or(rest);
        let normalized = source.split_whitespace().collect::<Vec<_>>().join(" ");
        let (label, shortened) = compact_label(&normalized);
        Self {
            label: (!label.is_empty()).then_some(label),
            card_id: card_id.map(str::to_owned),
            label_source: shortened.then(|| source.to_owned()),
        }
    }

    pub fn apply_rename(&mut self, change: &DelegateNameChange) {
        self.label = Some(change.label.clone());
        self.label_source.clone_from(&change.label_source);
    }
}

// Reserve one column for the ellipsis. Count wide Unicode conservatively on
// both clients, with explicit support for half-width kana and Latin text.
fn compact_label(text: &str) -> (String, bool) {
    let width = |ch: char| match ch {
        '\u{0000}'..='\u{024f}' | '\u{ff61}'..='\u{ffdc}' | '\u{ffe8}'..='\u{ffee}' | '…' => 1,
        '\u{0300}'..='\u{036f}' | '\u{fe00}'..='\u{fe0f}' => 0,
        _ => 2,
    };
    let mut columns = 0;
    let mut cutoff = text.len();
    for (index, ch) in text.char_indices() {
        columns += width(ch);
        if columns >= JOB_LABEL_COLUMNS {
            cutoff = cutoff.min(index);
        }
        if columns > JOB_LABEL_COLUMNS {
            return (format!("{}…", text[..cutoff].trim_end()), true);
        }
    }
    (text.to_owned(), false)
}

fn split_card_id(title: &str) -> (Option<&str>, &str) {
    let end = title.find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-').unwrap_or(title.len());
    let candidate = &title[..end];
    let Some((prefix, number)) = candidate.rsplit_once('-') else { return (None, title) };
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_uppercase())
        || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit())
        || title[end..].starts_with(|ch: char| ch.is_alphabetic() && ch.is_ascii())
    {
        return (None, title);
    }
    (Some(candidate), title[end..].trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ':' | '：' | '–' | '—')
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_width_labels_preserve_the_full_text_only_when_shortened() {
        let exact = format!("{}{}", "字".repeat(10), "a".repeat(20));
        let display = DelegateDisplay::from_title(Some(&format!("R-81  {exact}")), None);
        assert_eq!(display.card_id.as_deref(), Some("R-81"));
        assert_eq!(display.label.as_deref(), Some(exact.as_str()));
        assert_eq!(display.label_source, None);
        let long = format!("{exact}more");
        let display = DelegateDisplay::from_title(Some("R-81 Full title"), Some(&long));
        assert_eq!(display.card_id.as_deref(), Some("R-81"));
        assert_eq!(display.label.as_deref(), Some(format!("{}…", &exact[..exact.len() - 1]).as_str()));
        assert_eq!(display.label_source.as_deref(), Some(long.as_str()));
        assert_eq!(compact_label(&"ｶ".repeat(40)).1, false);
        assert_eq!(DelegateDisplay::from_title(Some("R-81"), None).card_id.as_deref(), Some("R-81"));
        assert_eq!(DelegateDisplay::from_title(Some("COVID-19-related work"), None).card_id, None);
    }
}
