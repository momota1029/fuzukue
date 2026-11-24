use std::collections::BTreeMap;

use fuzukue_proto::{DocReference, DocReferences};
use markdown::{mdast::Node, Constructs, ParseOptions};

use super::{group, link_card_refs_with, tex_to_markdown, tex_to_markdown_with_refs};

/// The printed heading for a theorem-like card, using its PDF reference when available.
pub(super) fn theorem_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "theorem" | "introthm" => "Theorem",
        "lemma" => "Lemma",
        "proposition" => "Proposition",
        "corollary" => "Corollary",
        "definition" | "introdef" => "Definition",
        "remark" => "Remark",
        "example" => "Example",
        "question" => "Question",
        _ => return None,
    })
}

pub fn theorem_heading(
    kind: &str,
    id: &str,
    title: &str,
    refs: Option<&DocReferences>,
) -> Option<String> {
    Some(theorem_heading_with_label(theorem_kind(kind)?, id, title, refs))
}

/// Format a theorem heading using a translated fallback when the document has no label.
pub fn theorem_heading_with_label(
    label: &str,
    id: &str,
    title: &str,
    refs: Option<&DocReferences>,
) -> String {
    let reference = refs.and_then(|refs| {
        refs.labels.get(&format!("card:{id}")).or_else(|| {
            refs.labels
                .values()
                .find(|reference| reference.card.as_deref() == Some(id))
        })
    });
    let mut heading = reference
        .map(|reference| reference.kind.trim())
        .filter(|kind| !kind.is_empty())
        .unwrap_or(label)
        .to_owned();
    if let Some(number) = reference
        .map(|reference| reference.number.trim())
        .filter(|number| !number.is_empty())
    {
        heading.push(' ');
        heading.push_str(number);
    }
    if !title.trim().is_empty() {
        heading.push_str(" · ");
        heading.push_str(title.trim());
    }
    heading
}

pub fn paper_markdown(body: &str, refs: Option<&DocReferences>) -> String {
    let Some(refs) = refs else {
        return super::link_card_refs(&tex_to_markdown(body));
    };
    let cards = card_references(refs);
    let md = tex_to_markdown_with_refs(body, Some(refs));
    let md = link_card_refs_with(&md, |id, label| match label {
        Some(_) => None,
        None => cards.get(id).map(|reference| numbered(reference, false)),
    });
    if cards.is_empty() {
        return md;
    }
    link_bare_cards(md, &cards)
}

pub(super) fn card_references(refs: &DocReferences) -> BTreeMap<&str, &DocReference> {
    let mut cards = BTreeMap::new();
    for (key, reference) in &refs.labels {
        if let Some(id) = reference
            .card
            .as_deref()
            .or_else(|| key.strip_prefix("card:"))
        {
            let entry = cards.entry(id).or_insert(reference);
            if key.starts_with("card:") {
                *entry = reference;
            }
        }
    }
    cards
}

pub(super) fn numbered(reference: &DocReference, equation: bool) -> String {
    let number = escape_text(&reference.number);
    if equation {
        format!("({number})")
    } else if reference.kind.is_empty() {
        number
    } else {
        format!("{} {number}", escape_text(&reference.kind))
    }
}

fn escape_text(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '[' | ']' | '*' | '_' | '<' | '>' | '&' | '$' | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

pub(super) fn reference<'a>(
    command: &str,
    tail: &'a str,
    refs: Option<&DocReferences>,
) -> Option<(String, &'a str)> {
    if command == "autoref" && refs.is_none() {
        return None;
    }
    let tail = if refs.is_some() {
        tail.strip_prefix('*').unwrap_or(tail)
    } else {
        tail
    };
    let (labels, rest) = group(tail.trim_start(), '{')?;
    let labels = labels
        .split(',')
        .map(str::trim)
        .map(|key| {
            let conventional = key.strip_prefix("card:");
            if let Some(reference) = refs.and_then(|refs| refs.labels.get(key)) {
                let label = numbered(reference, command == "eqref");
                return match reference.card.as_deref().or(conventional) {
                    Some(id) => format!("[{label}](fuzukue-card:{id})"),
                    None => label,
                };
            }
            if refs.is_some() {
                let marker = format!("**??{}**", escape_text(key));
                return match conventional {
                    Some(id) => format!("[{marker}](fuzukue-card:{id})"),
                    None => marker,
                };
            }
            match conventional {
                Some(id) => format!("[{id}](fuzukue-card:{id})"),
                None => format!("`{key}`"),
            }
        })
        .collect::<Vec<_>>();
    Some((labels.join(", "), rest))
}

pub(super) fn citation<'a>(
    tail: &'a str,
    refs: Option<&DocReferences>,
) -> Option<(String, &'a str)> {
    let tail = tail.trim_start();
    let (before, tail) = group(tail, '[').map_or((None, tail), |(note, rest)| (Some(note), rest));
    let (keys, rest) = group(tail.trim_start(), '{')?;
    let (after, rest) = match group(rest.trim_start_matches([' ', '\t']), '[') {
        Some((note, rest)) if refs.is_some() => (Some(note), rest),
        _ => (None, rest),
    };
    let labels = keys.split(',').map(str::trim).map(|key| match refs {
        Some(refs) => refs
            .citations
            .get(key)
            .map(|label| escape_text(label))
            .unwrap_or_else(|| format!("**??{}**", escape_text(key))),
        None => key.to_owned(),
    });
    let parts = labels
        .chain(before.into_iter().chain(after).map(str::to_owned))
        .collect::<Vec<_>>();
    Some((format!("[{}]", parts.join(", ")), rest))
}

fn link_bare_cards(md: String, cards: &BTreeMap<&str, &DocReference>) -> String {
    // Source ranges let us keep Markdown formatting and leave code, links and math alone.
    let options = ParseOptions {
        constructs: Constructs {
            math_flow: true,
            math_text: true,
            ..Constructs::gfm()
        },
        ..ParseOptions::default()
    };
    let Ok(tree) = markdown::to_mdast(&md, &options) else {
        return md;
    };
    let mut out = String::with_capacity(md.len());
    let mut copied = 0;
    link_text_nodes(&tree, &md, cards, &mut copied, &mut out);
    out.push_str(&md[copied..]);
    out
}

fn link_text_nodes(
    node: &Node,
    md: &str,
    cards: &BTreeMap<&str, &DocReference>,
    copied: &mut usize,
    out: &mut String,
) {
    match node {
        Node::Link(_) | Node::LinkReference(_) | Node::Image(_) | Node::ImageReference(_) => return,
        Node::Text(text) => {
            let Some(pos) = &text.position else { return };
            let mut offset = pos.start.offset;
            for word in md[offset..pos.end.offset].split_inclusive(|c: char| !id_char(c)) {
                let token = word.trim_end_matches(|c: char| !id_char(c));
                let id = if cards.contains_key(token) {
                    token
                } else {
                    token.trim_end_matches(['.', ':'])
                };
                let end = offset + id.len();
                if let Some(reference) = cards.get(id) {
                    let before = &md[..offset];
                    let after = &md[end..];
                    if !before.ends_with(['\\', '{'])
                        && !before.ends_with("??")
                        && !after.starts_with('}')
                    {
                        out.push_str(&md[*copied..offset]);
                        out.push_str(&format!("[{}](fuzukue-card:{id})", numbered(reference, false)));
                        *copied = end;
                    }
                }
                offset += word.len();
            }
        }
        _ => {
            for child in node.children().into_iter().flatten() {
                link_text_nodes(child, md, cards, copied, out);
            }
        }
    }
}

/// The same identifier characters as the harness's prose references.
fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(kind: &str, number: &str, card: Option<&str>) -> DocReference {
        DocReference {
            kind: kind.into(),
            number: number.into(),
            card: card.map(str::to_owned),
            page: "1".into(),
            title: String::new(),
        }
    }

    #[test]
    fn references_show_numbers_and_keep_card_links() {
        let refs = DocReferences {
            labels: BTreeMap::from([
                ("sec:intro".into(), reference("Section", "1.2", None)),
                (
                    "intro:E".into(),
                    reference("定理", "E", Some("intro-claim")),
                ),
                ("card:sh-thm".into(), reference("Theorem", "5.2", None)),
                ("eq:main".into(), reference("Equation", "7", None)),
            ]),
            citations: BTreeMap::new(),
        };
        assert_eq!(
            paper_markdown(
                r"\Cref{sec:intro, intro:E} / \cref{intro:E} / \ref {sec:intro} / \eqref{eq:main} / \autoref{card:sh-thm} / \ref{missing_id}",
                Some(&refs),
            ),
            r"Section 1.2, [定理 E](fuzukue-card:intro-claim) / [定理 E](fuzukue-card:intro-claim) / Section 1.2 / (7) / [Theorem 5.2](fuzukue-card:sh-thm) / **??missing\_id**",
        );
        assert_eq!(
            paper_markdown(
                "See [[sh-thm]], intro-claim. [[intro-claim|別名]] [sh-thm](fuzukue-card:sh-thm) `sh-thm` $sh-thm$ sh-thm-more.",
                Some(&refs),
            ),
            "See [Theorem 5.2](fuzukue-card:sh-thm), [定理 E](fuzukue-card:intro-claim). [別名](fuzukue-card:intro-claim) [sh-thm](fuzukue-card:sh-thm) `sh-thm` $sh-thm$ sh-thm-more.",
        );
        assert_eq!(
            paper_markdown(r"\Cref{sec:intro} \Cref{card:sh-thm} [[sh-thm]]", None),
            "`sec:intro` [sh-thm](fuzukue-card:sh-thm) [sh-thm](fuzukue-card:sh-thm)",
        );
    }

    #[test]
    fn citations_use_bibliography_labels_and_notes() {
        let refs = DocReferences {
            citations: BTreeMap::from([("a".into(), "13".into()), ("b".into(), "ABC+24".into())]),
            ..DocReferences::default()
        };
        assert_eq!(
            paper_markdown(
                r"\cite{a, b}[p.~5] / \cite[Thm~2]{b} / \cite{unknown}",
                Some(&refs)
            ),
            "[13, ABC+24, p. 5] / [ABC+24, Thm 2] / [**??unknown**]",
        );
        assert_eq!(paper_markdown(r"\cite{a, b}", None), "[a, b]");
    }

    #[test]
    fn theorem_headings_use_pdf_numbers_and_optional_titles() {
        let refs = DocReferences {
            labels: BTreeMap::from([
                ("intro:E".into(), reference("定理", "E", Some("intro"))),
                ("card:lemma".into(), reference("Lemma", "3.2", None)),
                ("other".into(), reference("Lemma", "9", Some("lemma"))),
            ]),
            ..DocReferences::default()
        };
        assert_eq!(
            theorem_heading("introthm", "intro", "集中", Some(&refs)).as_deref(),
            Some("定理 E · 集中")
        );
        assert_eq!(
            theorem_heading("lemma", "lemma", "", Some(&refs)).as_deref(),
            Some("Lemma 3.2")
        );
        assert_eq!(
            theorem_heading("definition", "new", "距離", None).as_deref(),
            Some("Definition · 距離")
        );
        assert_eq!(theorem_heading("explanation", "intro", "", Some(&refs)), None);
    }
}
