use std::path::Path as FsPath;

use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

use super::{card_tools, Graph};

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Overview,
    Neighbors,
    Path,
    Search,
    Issues,
    CardRead,
    CardEdit,
    Definition,
    References,
    Symbols,
}

impl Tool {
    pub(crate) const ALL: [Self; 10] = [
        Self::Overview,
        Self::Neighbors,
        Self::Path,
        Self::Search,
        Self::Issues,
        Self::CardRead,
        Self::CardEdit,
        Self::Definition,
        Self::References,
        Self::Symbols,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Overview => "graph_overview",
            Self::Neighbors => "graph_neighbors",
            Self::Path => "graph_path",
            Self::Search => "graph_search",
            Self::Issues => "graph_issues",
            Self::CardRead => "card_read",
            Self::CardEdit => "card_edit",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Symbols => "symbols",
        }
    }

    pub(crate) fn definition(self) -> Value {
        let deck = json!({"type": "string", "description": harness_text!("Workspace-relative deck path. Defaults to all decks.", "デッキのワークスペース相対パス。省略時は全体。")});
        let card = json!({"type": "string", "description": harness_text!("Card ID. Use <deck>#<id> to disambiguate.", "カード ID。曖昧なら <deck>#<id>。")});
        let strings = json!({"type": "array", "items": {"type": "string"}});
        if matches!(self, Self::CardRead | Self::CardEdit) {
            return card_tools::definition(self.name(), self.writes());
        }
        let kind = json!({"type": "string", "enum": ["card", "term", "label", "macro", "lean"]});
        let name = json!({"type": "string"});
        let (description, properties, required): (_, _, &[&str]) = match self {
            Self::Overview => (harness_text!("Map decks and chapters. depth=1 stops at chapters. With depth=2 and a chapter key, list cards and summaries. Use next_offset to continue.", "デッキと章の見取り図。depth=1 は章まで。depth=2 で chapter に章の key を渡すとカードと要約が並ぶ。next_offset で続きを読む。"),
                json!({"deck": deck, "depth": {"type": "integer", "minimum": 1, "maximum": 2, "default": 2},
                    "chapter": {"type": "string"}, "offset": {"type": "integer", "minimum": 0}}), &[]),
            Self::Neighbors => (harness_text!("Show nearby cards, typed edges, and summaries. Filter by edge type and direction.", "カードの周囲を型付きの辺と要約で見る。辺の種類と向きを指定できる。"),
                json!({"card": card, "depth": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1},
                    "edge_types": strings, "direction": {"type": "string", "enum": ["in", "out", "both"], "default": "both"}}), &["card"]),
            Self::Path => (harness_text!("Find the shortest directed path between cards from and to.", "from から to へ、カード間の有向リンクを辿る最短経路を返す。"),
                json!({"from": card, "to": card, "max_len": {"type": "integer", "minimum": 0, "default": 12}}), &["from", "to"]),
            Self::Search => (harness_text!("Search card IDs, titles, summaries, and statements. Japanese text is matched by character pairs.", "ID、題、要約、主張の語句でカードを検索する。日本語は文字の組で照合する。"),
                json!({"query": {"type": "string"}, "kinds": strings, "decks": strings,
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 15}}), &["query"]),
            Self::Issues => (harness_text!("Check unresolved references, requires cycles, cards outside the structure, inconsistent proof states, and references to later chapters.", "未解決の参照、requires の循環、構成から孤立したカード、証明状態の不整合、後の章への参照を調べる。"),
                json!({"deck": deck}), &[]),
            Self::Definition => (harness_text!("Find candidate symbol definitions with locations and excerpts.", "記号の定義候補を位置と短い抜粋付きで返す。"), json!({"name": name, "kind": kind}), &["name"]),
            Self::References => (harness_text!("Find uses of a symbol. Specify either name or a definition id.", "記号を使用している箇所を返す。name または定義の id を一つ指定する。"), json!({
                "name": name, "id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
            }), &[]),
            Self::Symbols => (harness_text!("Find symbols across decks by partial match.", "デッキをまたいで記号を部分一致で検索する。"), json!({"query": name, "kind": kind, "deck": deck}), &["query"]),
            Self::CardRead | Self::CardEdit => unreachable!(),
        };
        json!({"name": self.name(), "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required, "additionalProperties": false
        }})
    }

    pub(crate) fn writes(self) -> bool {
        matches!(self, Self::CardEdit)
    }

    pub(super) fn call(self, graph: &Graph, args: Value, project: &FsPath) -> Result<Value> {
        let index = &graph.index;
        match self {
            Self::Overview => {
                let args: Overview = parse(args)?;
                let depth = bounded(args.depth, 2, 2, "depth")?;
                if let Some(chapter) = args.chapter {
                    ensure!(depth == 2, "chapter は depth=2 で使う");
                    index.chapter_overview(&chapter, args.deck.as_deref(), args.offset.unwrap_or(0))
                } else {
                    ensure!(args.offset.is_none(), "offset には chapter も指定する");
                    index.overview(args.deck.as_deref(), depth)
                }
            }
            Self::Neighbors => {
                let args: Neighbors = parse(args)?;
                index.neighbors(
                    &args.card,
                    bounded(args.depth, 1, 3, "depth")?,
                    args.edge_types.as_deref(),
                    args.direction,
                )
            }
            Self::Path => {
                let args: Path = parse(args)?;
                index.path(&args.from, &args.to, args.max_len.unwrap_or(12))
            }
            Self::Search => {
                let args: Search = parse(args)?;
                index.search(
                    &graph.cache,
                    &args.query,
                    args.kinds.as_deref(),
                    args.decks.as_deref(),
                    bounded(args.limit, 15, 100, "limit")?,
                )
            }
            Self::Issues => index.issues(parse::<Deck>(args)?.deck.as_deref()),
            Self::CardRead => card_tools::read(graph, parse(args)?),
            Self::CardEdit => card_tools::edit(graph, project, parse(args)?),
            Self::Definition => {
                let args: Definition = parse(args)?;
                let result = graph.symbols(project).definition(project, &args.name, args.kind);
                Ok(json!({"definitions": result.definitions, "more": result.more}))
            }
            Self::References => {
                let args: References = parse(args)?;
                ensure!(args.name.is_some() != args.id.is_some(), "name または id を一つ指定する");
                let limit = bounded(args.limit, 20, 100, "limit")?;
                let result = graph.symbols(project).references(project, args.name.as_deref(), args.id.as_deref(), limit);
                Ok(json!({"references": result.references, "more": result.more}))
            }
            Self::Symbols => {
                let args: Symbols = parse(args)?;
                let result = graph.symbols(project).search(project, &args.query, args.kind, args.deck.as_deref());
                Ok(json!({"symbols": result.definitions, "more": result.more}))
            }
        }
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).context("MCP の引数が不正")
}

fn bounded(value: Option<usize>, default: usize, max: usize, name: &str) -> Result<usize> {
    let value = value.unwrap_or(default);
    ensure!((1..=max).contains(&value), "{name} は 1〜{max} で指定する");
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Overview {
    deck: Option<String>,
    depth: Option<usize>,
    chapter: Option<String>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Neighbors {
    card: String,
    depth: Option<usize>,
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    direction: Direction,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Direction {
    In,
    Out,
    #[default]
    Both,
}

impl Direction {
    pub(super) fn input(self) -> bool {
        matches!(self, Self::In | Self::Both)
    }

    pub(super) fn out(self) -> bool {
        matches!(self, Self::Out | Self::Both)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Path {
    from: String,
    to: String,
    max_len: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    kinds: Option<Vec<String>>,
    decks: Option<Vec<String>>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deck {
    deck: Option<String>,
}


#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    name: String,
    kind: Option<fuzukue_proto::SymbolKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct References {
    name: Option<String>,
    id: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Symbols {
    query: String,
    kind: Option<fuzukue_proto::SymbolKind>,
    deck: Option<String>,
}
