//! References from the LaTeX outputs for the PDF shown by the paper view.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

use fuzukue_cards::{DeckConfig, Graph};
use fuzukue_proto::{DocReference, DocReferences};
use regex::Regex;

use crate::build::{self, harness};

pub(crate) fn read(
    deck: &Path,
    cfg: &DeckConfig,
    built_pdf: Option<&Path>,
) -> Option<DocReferences> {
    let out = deck.join("build");
    if !out.is_dir() {
        return None;
    }
    let graph = Graph::open_local(deck).ok()?;
    let pdf = match built_pdf {
        Some(path) => path.to_owned(),
        None if build::uses_harness(cfg, &graph) => out
            .join(harness::targets(cfg).ok()?.first()?.0)
            .with_extension("pdf"),
        None => out
            .join(build::deck_target(&cfg.meta))
            .with_extension("pdf"),
    };
    let aux_path = pdf.with_extension("aux");
    let bbl_path = pdf.with_extension("bbl");
    if built_pdf.is_none() {
        let pdf_time = modified(&pdf)?;
        let aux_time = modified(&aux_path)?;
        let newer = |path: &Path| modified(path).is_some_and(|time| time > pdf_time);
        if aux_time > pdf_time
            || newer(&bbl_path)
            || newer(&deck.join(&cfg.structure))
            || cfg
                .meta
                .preamble
                .as_ref()
                .is_some_and(|path| newer(&deck.join(path)))
            || graph.cards.values().any(|card| {
                std::iter::once(&card.path)
                    .chain(&card.tex)
                    .any(|path| newer(&deck.join(path)))
            })
        {
            return None;
        }
    }
    let aux = std::fs::read_to_string(&aux_path).ok()?;
    let bbl = std::fs::read_to_string(bbl_path).unwrap_or_default();
    let preamble = std::fs::read_to_string(pdf.with_extension("tex"))
        .or_else(|_| {
            std::fs::read_to_string(
                deck.join(
                    cfg.meta
                        .preamble
                        .as_deref()
                        .unwrap_or(Path::new("preamble.tex")),
                ),
            )
        })
        .unwrap_or_default();
    let names = cref_names(&preamble);
    let mut labels = parse_aux(&aux, &names);
    for card in graph.cards.values() {
        let conventional = format!("card:{}", card.id);
        for key in std::iter::once(conventional.as_str()).chain(card.label.as_deref()) {
            if let Some(reference) = labels.get_mut(key) {
                reference.card = Some(card.id.clone());
                if reference.kind.is_empty() && !card.kind.is_empty() {
                    reference.kind = kind_name(&card.kind, &names);
                }
            }
        }
    }
    Some(DocReferences {
        labels,
        citations: parse_bbl(&bbl),
    })
}

fn modified(path: &Path) -> Option<SystemTime> {
    path.metadata().ok()?.modified().ok()
}

/// TeX arguments can contain groups, escaped delimiters and optional arguments with grouped brackets.
fn group(s: &str, open: char) -> Option<(&str, &str)> {
    let s = s.trim_start().strip_prefix(open)?;
    let close = if open == '{' { '}' } else { ']' };
    let mut braces = 0usize;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            c if c == close && braces == 0 => return Some((&s[..i], &s[i + 1..])),
            '{' => braces += 1,
            '}' => braces = braces.checked_sub(1)?,
            _ => {}
        }
    }
    None
}

fn decode_tex_bytes(s: &str) -> String {
    let mut bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    while let Some((&first, rest)) = bytes.split_first() {
        if let Some(escaped) = bytes.strip_prefix(b"^^") {
            if let Some(hex) = escaped
                .get(..2)
                .and_then(|hex| std::str::from_utf8(hex).ok())
            {
                if hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    bytes = &escaped[2..];
                    continue;
                }
            }
            if let Some((&c, tail)) = escaped
                .split_first()
                .filter(|(c, _)| c.is_ascii() && **c != b'\n')
            {
                out.push(c ^ 64);
                bytes = tail;
                continue;
            }
        }
        out.push(first);
        bytes = rest;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn cref_names(preamble: &str) -> BTreeMap<String, String> {
    static COMMAND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\(?:crefname|begin)\b").unwrap());
    static COMMENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)(^|[^\\])(?:\\\\)*%[^\r\n]*").unwrap());
    let preamble = COMMENT.replace_all(preamble, "$1");
    let mut names = BTreeMap::new();
    for command in COMMAND.find_iter(&preamble) {
        let Some((kind, tail)) = group(&preamble[command.end()..], '{') else {
            continue;
        };
        if command.as_str() == r"\begin" {
            if kind == "document" {
                break;
            }
        } else if let Some((name, tail)) = group(tail, '{') {
            if group(tail, '{').is_some() {
                names.insert(kind.to_owned(), decode_tex_bytes(name));
            }
        }
    }
    names
}

fn kind_name(kind: &str, names: &BTreeMap<String, String>) -> String {
    if let Some(name) = names.get(kind) {
        return name.clone();
    }
    let kind = match kind {
        "subsection" | "subsubsection" => "section",
        other => other,
    };
    let mut chars = kind.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().to_string() + chars.as_str()
    })
}

fn parse_aux(aux: &str, names: &BTreeMap<String, String>) -> BTreeMap<String, DocReference> {
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^[ \t]*\\newlabel\b").unwrap());
    let mut labels = BTreeMap::new();
    let mut cleveref = BTreeMap::new();
    for command in LABEL.find_iter(aux) {
        let Some((key, tail)) = group(&aux[command.end()..], '{') else {
            continue;
        };
        let Some((fields, _)) = group(tail, '{') else {
            continue;
        };
        let Some((number, tail)) = group(fields, '{') else {
            continue;
        };
        let Some((page, tail)) = group(tail, '{') else {
            continue;
        };
        let key = decode_tex_bytes(key);
        if let Some(key) = key.strip_suffix("@cref") {
            if let Some((kind, number)) = group(number, '[')
                .and_then(|(kind, rest)| Some((kind, group(group(rest, '[')?.1, '[')?.1)))
            {
                cleveref.insert(key.to_owned(), (kind, number.trim()));
            }
            continue;
        }
        let (title, tail) = group(tail, '{').unwrap_or_default();
        let (anchor, _) = group(tail, '{').unwrap_or_default();
        let kind = anchor.split_once('.').map_or("", |(kind, _)| kind);
        labels.insert(
            key,
            DocReference {
                kind: kind_name(kind, names),
                number: decode_tex_bytes(number),
                page: decode_tex_bytes(page),
                title: decode_tex_bytes(title),
                card: None,
            },
        );
    }
    for (key, (kind, number)) in cleveref {
        if let Some(reference) = labels.get_mut(&key) {
            reference.kind = kind_name(kind, names);
            reference.number = decode_tex_bytes(number);
        }
    }
    labels
}

fn parse_bbl(bbl: &str) -> BTreeMap<String, String> {
    static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^[ \t]*\\bibitem\b").unwrap());
    let mut number = 0;
    ITEM.find_iter(bbl)
        .filter_map(|command| {
            let tail = &bbl[command.end()..];
            let (label, tail) =
                group(tail, '[').map_or((None, tail), |(label, rest)| (Some(label), rest));
            let (key, _) = group(tail, '{')?;
            let label = match label {
                // Standard LaTeX increments the bibliography counter only for unlabelled items.
                None => {
                    number += 1;
                    number.to_string()
                }
                // alpha.bst braces its suffix and uses \etalchar{+} for truncated author lists.
                Some(label) => decode_tex_bytes(label)
                    .replace(r"\etalchar", "")
                    .replace(['{', '}'], ""),
            };
            Some((decode_tex_bytes(key), label))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUX: &str = include_str!("../tests/data/paper-refs/full.aux");
    const BBL: &str = include_str!("../tests/data/paper-refs/full.bbl");

    #[test]
    fn real_aux_and_bbl() {
        let names = cref_names(r"\crefname{proposition}{命題}{命題}");
        let labels = parse_aux(AUX, &names);
        let section = &labels["sec:intro-results"];
        assert_eq!(
            (
                &*section.kind,
                &*section.number,
                &*section.page,
                &*section.title
            ),
            ("Section", "1.2", "3", "主結果")
        );
        let arzela = &labels["card:pre-arzela-ascoli"];
        assert_eq!(
            (&*arzela.kind, &*arzela.number, &*arzela.title),
            ("命題", "2.7", "Arzelà–Ascoli の定理")
        );
        assert!(labels["intro:E"]
            .title
            .contains(r"\Cref {card:sh-thm-limit}"));
        assert_eq!(labels["intro:E"].number, "E");
        assert_eq!(labels["card:sh-thm-limit"].number, "7.8");
        assert_eq!(labels.len(), 5);
        assert_eq!(parse_bbl(BBL)["DiaconisFreedman1987"], "13");
    }

    #[test]
    fn plain_aux_and_alpha_bibitems() {
        let labels = parse_aux(
            r"
\newlabel{plain}{{A{.}1}{iv}}
\newlabel{reordered@cref}{{[theorem][2][5]5.2}{[1][17][]17}}
\newlabel{reordered}{{2}{17}{With \{ and {groups}}{section.3}{}}
",
            &BTreeMap::new(),
        );
        assert_eq!(
            (&*labels["plain"].number, &*labels["plain"].page),
            ("A{.}1", "iv")
        );
        let reference = &labels["reordered"];
        assert_eq!(
            (&*reference.kind, &*reference.number, &*reference.title),
            ("Theorem", "5.2", r"With \{ and {groups}")
        );

        let cites = parse_bbl(
            r"
\bibitem[Ald81]{Aldous1981}
\bibitem[ABC{\etalchar{+}}24a]
  {many}
\bibitem{numbered}
\bibitem{also-numbered}
",
        );
        assert_eq!(cites["Aldous1981"], "Ald81");
        assert_eq!(cites["many"], "ABC+24a");
        assert_eq!(cites["numbered"], "1");
        assert_eq!(cites["also-numbered"], "2");
    }

    #[test]
    fn japanese_names_from_preamble_only() {
        let names = cref_names(
            r"
% \crefname{lemma}{ignored}{ignored}
\crefname{theorem}
  {定理}{定理}\crefname{proposition}{命題}{命題}
\crefname{equation}{}{}
\begin{document}
\crefname{lemma}{also ignored}{also ignored}
",
        );
        assert_eq!(names["theorem"], "定理");
        assert_eq!(names["proposition"], "命題");
        assert_eq!(names["equation"], "");
        assert!(!names.contains_key("lemma"));
    }

    #[test]
    fn document_uses_fresh_build_and_custom_card_label() -> anyhow::Result<()> {
        use std::fs::{self, File, FileTimes};
        use std::time::Duration;

        let temp = tempfile::tempdir()?;
        let deck = temp.path();
        fs::create_dir(deck.join("cards"))?;
        fs::write(deck.join("harness.toml"), "[build]\ntex = ['full.tex']\n")?;
        let card = deck.join("cards/intro-claim.md");
        fs::write(
            &card,
            "---\nid: intro-claim\nkind: introthm\nlabel: intro:E\n---\n",
        )?;
        let references = |built_pdf| match crate::cards::document_for_deck(deck, built_pdf) {
            fuzukue_proto::ServerEvent::Document { references, .. } => references,
            _ => unreachable!(),
        };
        assert!(references(None).is_none());

        let build = deck.join("build");
        fs::create_dir(&build)?;
        fs::write(build.join("full.aux"), AUX)?;
        fs::write(build.join("full.bbl"), BBL)?;
        fs::write(
            build.join("full.tex"),
            r"\crefname{introthm}{定理}{定理}\begin{document}",
        )?;
        let pdf = build.join("full.pdf");
        File::create(&pdf)?;
        let startup = references(None).unwrap();
        assert_eq!(
            startup.labels["intro:E"].card.as_deref(),
            Some("intro-claim")
        );
        assert_eq!(startup.labels["intro:E"].kind, "定理");
        assert_eq!(startup.citations["DiaconisFreedman1987"], "13");

        let mtime = modified(&pdf).unwrap() + Duration::from_secs(5);
        File::open(card)?.set_times(FileTimes::new().set_modified(mtime))?;
        assert!(references(None).is_none());
        assert!(references(Some(&pdf)).is_some());
        Ok(())
    }
}
