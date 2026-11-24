use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_cards::{section_range, split_frontmatter, DeckConfig};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{clip, index, CardData, Graph};

pub(super) fn definition(name: &str, writes: bool) -> Value {
    let mut properties = json!({
        "card": {"type": "string", "description": harness_text!("Card ID. Use <deck>#<id> to disambiguate.", "カード ID。曖昧なら <deck>#<id>。")},
        "part": {"type": "string", "enum": ["statement", "proof", "notes", "all"]}
    });
    let (description, required) = if writes {
        properties["old"] = json!({"type": "string"});
        properties["new"] = json!({"type": "string"});
        (harness_text!("Replace old with new when it occurs exactly once in the selected card part. Return the diff and nearby check results.", "カードの指定部位で old が一度だけ見つかるとき、new に置き換える。差分と周辺の検査結果を返す。"), vec!["card", "part", "old", "new"])
    } else {
        properties["part"]["default"] = json!("statement");
        properties["lines"] = json!({"type": "string", "pattern": "^[1-9][0-9]*-[1-9][0-9]*$", "description": harness_text!("Line range within the part, starting at 1, inclusive.", "部位内の行。1 始まり、両端を含む。")});
        (
            harness_text!("Read only the needed parts of a card’s statement, proof, or notes.", "主張・証明・ノートのうち必要な部分だけを読む。"),
            vec!["card"],
        )
    };
    json!({"name": name, "description": description, "inputSchema": {
        "type": "object", "properties": properties, "required": required, "additionalProperties": false
    }})
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Part {
    #[default]
    Statement,
    Proof,
    Notes,
    All,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Statement => "statement",
            Self::Proof => "proof",
            Self::Notes => "notes",
            Self::All => "all",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Statement => "Statement",
            Self::Proof => "Proof",
            Self::Notes => "Notes",
            Self::All => "",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Read {
    card: String,
    #[serde(default)]
    part: Part,
    lines: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    card: String,
    part: Part,
    old: String,
    new: String,
}

struct Source {
    path: PathBuf,
    text: String,
    selection: Range<usize>,
}

fn folder_tex(md: &Path, part: Part) -> Option<PathBuf> {
    let filename = match part {
        Part::Statement => "statement.tex",
        Part::Proof => "proof.tex",
        Part::Notes | Part::All => return None,
    };
    let path = md.parent()?.join(filename);
    (md.ends_with("card.md") && path.is_file()).then_some(path)
}

fn inside(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize()?;
    ensure!(
        path.starts_with(root) && path.is_file(),
        "カードのソースがデッキの外にある"
    );
    Ok(path)
}

fn source(root: &Path, md: &Path, part: Part) -> Result<Option<Source>> {
    if let Some(path) = folder_tex(md, part) {
        let path = inside(root, &path)?;
        let text = fs::read_to_string(&path)?;
        let selection = 0..text.len();
        return Ok(Some(Source {
            path,
            text,
            selection,
        }));
    }
    let path = inside(root, md)?;
    let text = fs::read_to_string(&path)?;
    let (_, body) = split_frontmatter(&text).context("カードの front matter が見つからない")?;
    let offset = text.len() - body.len();
    let section = section_range(body, part.heading());
    let fallback = match part {
        Part::All => true,
        Part::Statement => {
            !md.ends_with("card.md")
                && section_range(body, "Proof").is_none()
                && section_range(body, "Notes").is_none()
        }
        Part::Notes => {
            md.ends_with("card.md")
                && section_range(body, "Statement").is_none()
                && section_range(body, "Proof").is_none()
        }
        Part::Proof => false,
    };
    let Some(selection) = section.or_else(|| fallback.then_some(0..body.len())) else {
        return Ok(None);
    };
    let selection = offset + selection.start..offset + selection.end;
    Ok(Some(Source {
        path,
        text,
        selection,
    }))
}

fn deck_root(project: &Path, deck: &str) -> Result<PathBuf> {
    Ok(project.join(deck).canonicalize()?)
}

fn lines(text: &str, range: Option<&str>) -> Result<(usize, String)> {
    let Some(range) = range else {
        return Ok((1, text.to_owned()));
    };
    let (first, last) = range.split_once('-').context("lines は a-b で指定する")?;
    let first: usize = first.parse().context("開始行が不正")?;
    let last: usize = last.parse().context("終了行が不正")?;
    ensure!(
        first > 0 && first <= last,
        "lines は 1 始まりで、終了行は開始行以降を指定する"
    );
    let selected = text
        .split_inclusive('\n')
        .skip(first - 1)
        .take(last - first + 1)
        .collect::<String>();
    ensure!(!selected.is_empty(), "開始行が部位の行数を超えている");
    Ok((first, selected))
}

pub(super) fn read(graph: &Graph, args: Read) -> Result<Value> {
    let id = graph.index.card_key(&args.card)?;
    let (deck, key, md) = graph.index.card_source(id);
    let root = deck_root(&graph.cache.project, deck)?;
    let data = if matches!(args.part, Part::All) && md.ends_with("card.md") {
        let mut text = String::new();
        for part in [Part::Statement, Part::Proof] {
            if folder_tex(md, part).is_some() {
                let s = source(&root, md, part)?.context("部位が見つからない")?;
                text.push_str(&format!(
                    "## {}\n{}\n",
                    part.heading(),
                    &s.text[s.selection.clone()]
                ));
            }
        }
        if let Some(s) = source(&root, md, Part::All)? {
            text.push_str(&s.text[s.selection]);
        }
        Some(text)
    } else {
        source(&root, md, args.part)?.map(|s| s.text[s.selection].to_owned())
    };
    let Some(data) = data else {
        return Ok(json!({"card": key, "part": args.part.name(), "present": false, "text": ""}));
    };
    let (first, text) = lines(&data, args.lines.as_deref())?;
    Ok(json!({"card": key, "part": args.part.name(), "first_line": first, "text": text}))
}

fn replacement(text: &str, old: &str) -> Result<Option<usize>> {
    let Some(start) = text.find(old) else {
        return Ok(None);
    };
    let skip = old.chars().next().unwrap().len_utf8();
    ensure!(
        !text[start + skip..].contains(old),
        "old が複数箇所に一致する"
    );
    Ok(Some(start))
}

pub(super) fn edit(graph: &Graph, project: &Path, args: Edit) -> Result<Value> {
    let id = graph.index.card_key(&args.card)?;
    let (deck, key, md) = graph.index.card_source(id);
    let project = project.canonicalize()?;
    let root = deck_root(&project, deck)?;
    ensure!(
        root.starts_with(&project),
        "外部デッキのカードはここで編集できない"
    );
    ensure!(!args.old.is_empty(), "old は空にできない");
    let _write = crate::cards::CARD_WRITE_LOCK.lock().unwrap();
    let before =
        fuzukue_cards::read_card(&root, &inside(&root, md)?)?.context("カードが見つからない")?;
    ensure!(
        key.rsplit_once('#')
            .is_some_and(|(_, expected)| expected == before.id),
        "カード ID が変わったため、見取り図を更新してから再試行する"
    );
    let mut parts = vec![args.part];
    if matches!(args.part, Part::All) && md.ends_with("card.md") {
        parts.extend(
            [Part::Statement, Part::Proof]
                .into_iter()
                .filter(|&part| folder_tex(md, part).is_some()),
        );
    }
    let mut found = None;
    for part in parts {
        let Some(s) = source(&root, md, part)? else {
            continue;
        };
        if let Some(start) = replacement(&s.text[s.selection.clone()], &args.old)? {
            if found.is_some() {
                bail!("old が複数箇所に一致する");
            }
            found = Some((s, start, part));
        }
    }
    let (s, start, part) = found.context("old が指定された部位に見つからない")?;
    let selected = &s.text[s.selection.clone()];
    let line = 1 + selected[..start].bytes().filter(|&b| b == b'\n').count();
    if args.old == args.new {
        return Ok(json!({"card": key, "changed": false}));
    }

    let before = CardData::from(before);
    let permissions = fs::metadata(&s.path)?.permissions();
    let mut draft = s.text.clone();
    let at = s.selection.start + start;
    draft.replace_range(at..at + args.old.len(), &args.new);
    if s.path == md {
        fuzukue_cards::parse_card(&root, md, &draft)?;
    }
    let file = crate::cards::replacement_file(&s.path, permissions, &draft)?;
    ensure!(
        fs::read_to_string(&s.path)? == s.text,
        "編集中にカードが変わったため、読み直してから再試行する"
    );
    file.persist(&s.path)?;

    let diff_line = |prefix: &str, text: &str| {
        let preview = clip(text, 180).replace('\n', &format!("\n{prefix}"));
        format!("{prefix}{preview}")
    };
    let diff = format!(
        "@@ {line} @@\n{}\n{}",
        diff_line("-", &args.old),
        diff_line("+", &args.new)
    );
    let checks = checks(graph, &project, &root, id, &before)
        .unwrap_or_else(|err| json!({"error": clip(&format!("{err:#}"), 300)}));
    Ok(json!({"card": key, "part": part.name(), "changed": true, "diff": diff, "checks": checks,
        "path": s.path.strip_prefix(&project)?}))
}

fn checks(
    graph: &Graph,
    project: &Path,
    root: &Path,
    edited: usize,
    before: &CardData,
) -> Result<Value> {
    let index = &graph.index;
    let (_, _, md) = index.card_source(edited);
    let card = fuzukue_cards::read_card(root, md)?.context("変更したカードを読めない")?;
    let after = CardData::from(card.clone());
    let cfg = DeckConfig::read(root)?;
    let external = |namespace: &str| {
        cfg.external_roots(project, root)
            .find(|(ns, _)| *ns == namespace)
            .and_then(|(_, path)| path.ok())
            .and_then(|path| path.canonicalize().ok())
            .map(|path| index::deck_id(project, &path))
    };
    let affected = index.edit_neighbors(edited, before, &after, external);
    let layers = crate::rules::resolve(project)?.layers(project);
    let mut styles = BTreeMap::<PathBuf, BTreeMap<String, fuzukue_cards::Card>>::new();
    styles
        .entry(root.to_path_buf())
        .or_default()
        .insert(card.id.clone(), card);
    let mut refs = index.edit_refs(edited, &after, (edited, &after), external);
    for &id in affected.iter().filter(|&&id| id != edited) {
        let (deck, _, path) = index.card_source(id);
        let root = deck_root(project, deck)?;
        let path = inside(&root, path)?;
        let Some(card) = fuzukue_cards::read_card(&root, &path)? else {
            continue;
        };
        let cfg = DeckConfig::read(&root)?;
        refs.extend(index.edit_refs(
            id,
            &CardData::from(card.clone()),
            (edited, &after),
            |namespace| {
                cfg.external_roots(project, &root)
                    .find(|(ns, _)| *ns == namespace)
                    .and_then(|(_, path)| path.ok())
                    .and_then(|path| path.canonicalize().ok())
                    .map(|path| index::deck_id(project, &path))
            },
        ));
        styles
            .entry(root)
            .or_default()
            .insert(card.id.clone(), card);
    }
    let mut diagnostics = Vec::new();
    for (root, cards) in styles {
        diagnostics.extend(fuzukue_cards::Graph::lint_style_subset(&root, &layers, cards, Some(project))?);
    }
    let (status, diagnostics): (Vec<_>, Vec<_>) = diagnostics.into_iter()
        .partition(|d| d.code == Some("style/textlint-status"));
    Ok(json!({
        "textlint_status": status.iter().map(|d| d.display_message()).collect::<Vec<_>>(),
        "cards_checked": affected.len(),
        "style_count": diagnostics.len(),
        "reference_count": refs.len(),
        "style": diagnostics.into_iter().take(6).map(|d| json!({"path": d.path, "code": d.code, "textlint_rule": d.textlint_rule,
            "message": clip(&d.message, 140), "line": d.line, "column": d.column, "fix_hint": d.fix_hint})).collect::<Vec<_>>(),
        "references": refs.into_iter().take(6).collect::<Vec<_>>()
    }))
}
