use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use fuzukue_cards::{Card, DeckConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{DEADLINE, ReadScope, open_regular_source, paths};
use crate::graph::{requires_cycles, structure_card_order};

const DEFAULT_DECK: &str = "roadmap";
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;
const MAX_CARD_LINKS: usize = 16;
const MAX_SOURCE_BYTES: u64 = 256 * 1024;

pub(super) fn definition(name: &str) -> Value {
    json!({
        "name": name,
        "description": harness_text!(
            "Read a deck's status counts, ready todo cards, dependencies, and remaining order. Defaults to the roadmap deck in the assigned checkout. Limit applies to each list (default 50, range 1–100). Update card status in the same commit as the work.",
            "デッキの状態別件数、着手可能な todo、依存関係、残作業の順序を読む。省略時は作業ツリーの roadmap デッキ。各一覧の上限は既定 50 件、指定範囲は 1〜100 件。status は作業の変更と同じコミットで更新する。"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "deck": {"type": "string", "default": DEFAULT_DECK, "description": harness_text!(
                    "Deck directory relative to the assigned working directory.",
                    "割り当てられた作業ディレクトリからのデッキの相対パス。"
                )},
                "status": {"type": "string", "description": harness_text!("Exact card status.", "カードの状態に完全一致。")},
                "id_prefix": {"type": "string", "description": harness_text!("Case-sensitive card ID prefix.", "大文字小文字を区別するカード ID の接頭辞。")},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT},
                "include_done": {"type": "boolean", "default": false, "description": harness_text!(
                    "Include done cards in the status lists. An explicit done filter also includes them.",
                    "状態別一覧に done を含める。status に done を明示した場合も表示する。"
                )}
            },
            "required": [],
            "additionalProperties": false
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    deck: Option<PathBuf>,
    status: Option<String>,
    id_prefix: Option<String>,
    limit: Option<i64>,
    #[serde(default)]
    include_done: bool,
}

impl Args {
    fn matches(&self, card: &Card) -> bool {
        (self.include_done || card.status != "done" || self.status.as_deref() == Some("done"))
            && self
                .status
                .as_ref()
                .is_none_or(|status| card.status == *status)
            && self
                .id_prefix
                .as_ref()
                .is_none_or(|prefix| card.id.starts_with(prefix))
    }
}

fn is_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn resolve_optional(scope: &ReadScope, path: &Path) -> Result<Option<PathBuf>> {
    match scope.resolve(path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_config_file(scope: &ReadScope, path: &Path) -> Result<Option<String>> {
    let Some(path) = resolve_optional(scope, path)? else {
        return Ok(None);
    };
    let mut source = String::new();
    open_regular_source(&path)?
        .take(MAX_SOURCE_BYTES + 1)
        .read_to_string(&mut source)?;
    ensure!(
        source.len() as u64 <= MAX_SOURCE_BYTES,
        "{}: {}",
        path.display(),
        harness_text!(
            "Deck configuration exceeds 256 KiB.",
            "デッキ設定が 256 KiB を超えている。"
        )
    );
    Ok(Some(source))
}

fn read_card(scope: &ReadScope, root: &Path, path: &Path) -> Result<Option<Card>> {
    scope.audit_read(path)?;
    let mut reader = BufReader::new(open_regular_source(path)?.take(MAX_SOURCE_BYTES + 1));
    let mut source = Vec::new();
    reader.read_until(b'\n', &mut source)?;
    if source != b"---\n" && source != b"---\r\n" {
        return Ok(None);
    }
    loop {
        let start = source.len();
        if reader.read_until(b'\n', &mut source)? == 0 {
            break;
        }
        if matches!(&source[start..], b"---" | b"---\n" | b"---\r\n") {
            ensure!(
                source.len() as u64 <= MAX_SOURCE_BYTES,
                "{}: {}",
                path.display(),
                harness_text!(
                    "Card frontmatter exceeds 256 KiB.",
                    "カードの frontmatter が 256 KiB を超えている。"
                )
            );
            let card = fuzukue_cards::parse_card(root, path, std::str::from_utf8(&source)?)
                .with_context(|| path.display().to_string())?;
            return Ok(Some(card));
        }
    }
    anyhow::bail!(
        "{}: {}",
        path.display(),
        harness_text!(
            "Card frontmatter has no closing delimiter within 256 KiB.",
            "256 KiB 以内にカードの frontmatter を閉じる区切りがない。"
        )
    )
}

fn load_deck(scope: &ReadScope, deck: &Path) -> Result<(PathBuf, Vec<Card>)> {
    ensure!(
        deck.is_relative(),
        "{}",
        harness_text!(
            "Use a deck path relative to the assigned working directory.",
            "デッキは割り当てられた作業ディレクトリからの相対パスで指定する。"
        )
    );
    let root = scope.resolve(deck)?;
    ensure!(
        root.is_dir(),
        "{}",
        harness_text!(
            "Select a deck directory.",
            "デッキのディレクトリを指定する。"
        )
    );
    let mut has_manifest = false;
    let config = DeckConfig::read_with(&root, |name| {
        let source = read_config_file(scope, &root.join(name))?;
        has_manifest |= source.is_some();
        Ok(source)
    })?;
    let structure = read_config_file(scope, &root.join(&config.structure))?;
    ensure!(
        has_manifest || structure.is_some() || root == scope.cwd,
        "{}",
        harness_text!(
            "Deck configuration or structure file not found.",
            "デッキ設定または構成ファイルが見つからない。"
        )
    );
    let priority = structure
        .map(|source| serde_yaml::from_str(&source).map(structure_card_order))
        .transpose()?
        .unwrap_or_default();

    let deadline = Instant::now() + DEADLINE;
    let mut scan = paths::Scan::new(scope, usize::MAX, false, false, deadline);
    let mut files = BTreeSet::new();
    for dir in &config.dirs {
        let Some(dir) = resolve_optional(scope, &root.join(dir))? else {
            continue;
        };
        let file = paths::open(&dir)?;
        ensure!(
            file.metadata()?.is_dir(),
            "{}: {}",
            dir.display(),
            harness_text!(
                "Configured card directory is not a directory.",
                "カードの設定先がディレクトリではない。"
            )
        );
        let complete = scan.visit(&dir, &file, &mut |entry| {
            if entry.kind == "file" && entry.path.extension().is_some_and(|ext| ext == "md") {
                files.insert(entry.path);
            }
            true
        })?;
        ensure!(
            complete,
            "{}",
            harness_text!(
                "Deck scan did not finish within 20,000 entries and 5 seconds.",
                "デッキの走査が 20,000 項目・5 秒以内に完了しなかった。"
            )
        );
    }

    let mut cards = BTreeMap::new();
    for path in files {
        ensure!(
            Instant::now() < deadline,
            "{}",
            harness_text!(
                "Deck has too many card frontmatter files to read within 5 seconds.",
                "カードの frontmatter を 5 秒以内に読み切れなかった。"
            )
        );
        let Some(card) = read_card(scope, &root, &path)? else {
            continue;
        };
        // Match the graph index's first-file precedence for duplicate IDs.
        cards.entry(card.id.clone()).or_insert(card);
    }

    let cards = priority
        .into_iter()
        .filter_map(|id| cards.remove(&id))
        .collect::<Vec<_>>()
        .into_iter()
        .chain(cards.into_values())
        .collect();
    Ok((root, cards))
}

#[derive(Default)]
struct Dependencies<'a> {
    declared: BTreeSet<&'a str>,
    requires: Vec<usize>,
    blocks: Vec<usize>,
    unresolved: Vec<Unresolved<'a>>,
}

#[derive(Serialize)]
struct Unresolved<'a> {
    target: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

fn dependencies<'a>(cards: &'a [Card], deck: &str) -> Vec<Dependencies<'a>> {
    let by_id: BTreeMap<_, _> = cards
        .iter()
        .enumerate()
        .map(|(i, card)| (card.id.as_str(), i))
        .collect();
    let alias = deck.rsplit('/').next().unwrap_or(deck);
    let mut deps: Vec<_> = cards
        .iter()
        .map(|card| Dependencies {
            declared: card
                .links
                .iter()
                .filter(|link| !card.status.is_empty() && link.rel == "requires")
                .map(|link| link.to.as_str())
                .collect(),
            ..Dependencies::default()
        })
        .collect();
    for node in 0..cards.len() {
        let mut resolved = BTreeSet::new();
        let dep = &mut deps[node];
        for &target in &dep.declared {
            let qualified = target.rsplit_once('#').or_else(|| target.split_once(':'));
            let local = qualified
                .filter(|&(namespace, _)| namespace == deck || namespace == alias)
                .and_then(|(_, id)| by_id.get(id));
            // Like the graph, prefer literal IDs over colon-qualified names.
            let target_id = if target.contains('#') {
                local
            } else {
                by_id.get(target).or(local)
            };
            let kind = match target_id {
                Some(&id) if !cards[id].status.is_empty() => {
                    resolved.insert(id);
                    continue;
                }
                Some(_) => "missing_status",
                None if qualified
                    .is_some_and(|(namespace, _)| namespace != deck && namespace != alias) =>
                {
                    "external_requires"
                }
                None => "dangling_requires",
            };
            dep.unresolved.push(Unresolved { target, kind });
        }
        for target in resolved {
            deps[node].requires.push(target);
            deps[target].blocks.push(node);
        }
    }
    deps
}

fn remaining_order(
    cards: &[Card],
    deps: &[Dependencies<'_>],
) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut pending = vec![0; cards.len()];
    let mut available = BTreeSet::new();
    let mut ready = Vec::new();
    for (node, card) in cards.iter().enumerate() {
        if card.status.is_empty() || card.status == "done" {
            continue;
        }
        pending[node] = deps[node].unresolved.len()
            + deps[node]
                .requires
                .iter()
                .filter(|&&target| cards[target].status != "done")
                .count();
        if pending[node] == 0 {
            available.insert(node);
            if card.status == "todo" {
                ready.push(node);
            }
        }
    }
    let mut order = Vec::new();
    while let Some(node) = available.pop_first() {
        order.push(node);
        for &child in &deps[node].blocks {
            if !cards[child].status.is_empty() && cards[child].status != "done" {
                pending[child] -= 1;
                if pending[child] == 0 {
                    available.insert(child);
                }
            }
        }
    }
    let unscheduled = (0..cards.len())
        .filter(|&node| pending[node] != 0)
        .collect();
    (ready, order, unscheduled)
}

struct Window<T> {
    items: Vec<T>,
    total: usize,
}

impl<T: Serialize> Window<T> {
    fn new<I>(
        source: impl IntoIterator<Item = I>,
        limit: usize,
        bytes: usize,
        render: impl Fn(I) -> T,
    ) -> Self {
        let mut source = source.into_iter();
        let mut items = Vec::new();
        let mut used = 0;
        for _ in 0..limit {
            let Some(item) = source.next() else {
                return Self {
                    total: items.len(),
                    items,
                };
            };
            let item = render(item);
            used += serde_json::to_string(&item)
                .expect("plan status values are JSON serializable")
                .len()
                + 1;
            if used > bytes {
                return Self {
                    total: items.len() + 1 + source.count(),
                    items,
                };
            }
            items.push(item);
        }
        Self {
            total: items.len() + source.count(),
            items,
        }
    }

    fn truncated(&self) -> bool {
        self.items.len() < self.total
    }
}

#[derive(Serialize)]
struct CardRow<'a> {
    id: &'a str,
    title: String,
    requires: Vec<&'a str>,
    blocks: Vec<&'a str>,
    issues: Vec<&'a str>,
    unresolved_requires: Vec<&'a Unresolved<'a>>,
    totals: BTreeMap<&'static str, usize>,
    truncated: bool,
}

fn card_row<'a>(card: &'a Card, dep: &'a Dependencies<'a>, cards: &'a [Card]) -> CardRow<'a> {
    let requires = Window::new(&dep.declared, MAX_CARD_LINKS, 1600, |&target| target);
    let blocks = Window::new(&dep.blocks, MAX_CARD_LINKS, 1600, |&node| {
        cards[node].id.as_str()
    });
    let issues = Window::new(&card.issues, MAX_CARD_LINKS, 1600, String::as_str);
    let unresolved = Window::new(&dep.unresolved, MAX_CARD_LINKS, 1600, |issue| issue);
    let title = crate::graph::clip(&card.title, 240);
    let truncated = requires.truncated()
        || blocks.truncated()
        || issues.truncated()
        || unresolved.truncated()
        || title != card.title;
    CardRow {
        id: &card.id,
        title,
        requires: requires.items,
        blocks: blocks.items,
        issues: issues.items,
        unresolved_requires: unresolved.items,
        totals: BTreeMap::from([
            ("requires", requires.total),
            ("blocks", blocks.total),
            ("issues", issues.total),
            ("unresolved_requires", unresolved.total),
        ]),
        truncated,
    }
}

pub(super) fn status(scope: &ReadScope, args: Args) -> Result<Value> {
    let deck = args
        .deck
        .as_deref()
        .unwrap_or_else(|| Path::new(DEFAULT_DECK));
    let (root, cards) = load_deck(scope, deck)?;
    let deck = root.strip_prefix(&scope.cwd).unwrap_or(deck);
    let deck = if deck.as_os_str().is_empty() {
        ".".into()
    } else {
        deck.to_string_lossy()
    };
    let deps = dependencies(&cards, &deck);
    let (ready, order, unscheduled) = remaining_order(&cards, &deps);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LIMIT as i64)
        .clamp(1, MAX_LIMIT as i64) as usize;

    let mut counts = BTreeMap::new();
    for card in cards.iter().filter(|card| !card.status.is_empty()) {
        *counts.entry(card.status.as_str()).or_insert(0) += 1;
    }
    let counts = Window::new(counts, MAX_LIMIT, 4000, |entry| entry);
    let rows = Window::new(
        cards
            .iter()
            .enumerate()
            .filter(|(_, card)| !card.status.is_empty() && args.matches(card)),
        limit,
        24_000,
        |(node, card)| (card.status.as_str(), card_row(card, &deps[node], &cards)),
    );
    let mut by_status: BTreeMap<&str, Vec<CardRow<'_>>> = BTreeMap::new();
    let rows_truncated = rows.truncated() || rows.items.iter().any(|(_, row)| row.truncated);
    for (status, card) in rows.items {
        by_status.entry(status).or_default().push(card);
    }
    let ids = |nodes: Vec<usize>| {
        Window::new(
            nodes.into_iter().filter(|&node| args.matches(&cards[node])),
            limit,
            6000,
            |node| cards[node].id.as_str(),
        )
    };
    let ready = ids(ready);
    let order = ids(order);
    let unscheduled = ids(unscheduled);

    let cycles = requires_cycles(
        0..cards.len(),
        cards.len(),
        |node| deps[node].requires.iter().copied(),
        |node| deps[node].blocks.iter().copied(),
    );
    let cycles = Window::new(cycles, limit, 6000, |nodes| {
        let members = Window::new(nodes, MAX_CARD_LINKS, 4000, |node| cards[node].id.as_str());
        json!({"cards": members.items, "total": members.total, "truncated": members.truncated()})
    });
    let unresolved = Window::new(
        deps.iter()
            .enumerate()
            .flat_map(|(node, dep)| dep.unresolved.iter().map(move |issue| (node, issue))),
        limit,
        6000,
        |(node, issue)| json!({"from": cards[node].id, "target": issue.target, "type": issue.kind}),
    );

    let truncated = rows_truncated
        || counts.truncated()
        || ready.truncated()
        || order.truncated()
        || unscheduled.truncated()
        || cycles.truncated()
        || unresolved.truncated()
        || cycles.items.iter().any(|cycle| cycle["truncated"] == true);
    let counts_map: BTreeMap<_, _> = counts.items.into_iter().collect();
    Ok(json!({
        "deck": deck, "limit": limit, "counts": counts_map, "by_status": by_status,
        "ready": ready.items, "topological_order": order.items, "unscheduled": unscheduled.items,
        "requires_cycles": cycles.items, "unresolved_requires": unresolved.items,
        "totals": {"statuses": counts.total, "cards": rows.total, "ready": ready.total,
            "topological_order": order.total, "unscheduled": unscheduled.total,
            "requires_cycles": cycles.total, "unresolved_requires": unresolved.total},
        "truncated": truncated
    }))
}
