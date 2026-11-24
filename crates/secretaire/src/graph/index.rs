use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use super::{clip, Cache, CardData, Inputs, Item};

type NodeId = usize;

#[derive(Serialize)]
struct Node {
    key: String,
    id: String,
    deck: String,
    #[serde(rename = "type")]
    node_type: &'static str,
    kind: String,
    title: String,
    status: String,
    summary: String,
}

#[derive(Default)]
struct Deck {
    node: NodeId,
    sections: Vec<NodeId>,
    cards: Vec<NodeId>,
    structured: bool,
    search: SearchText,
}

struct Edge {
    from: NodeId,
    to: NodeId,
    rel: String,
    order: Option<usize>,
}

#[derive(Default)]
pub(super) struct Index {
    native_labels: HashSet<String>,
    nodes: Vec<Node>,
    decks: BTreeMap<String, Deck>,
    cards: HashMap<String, Vec<NodeId>>,
    qualified: HashMap<String, NodeId>,
    aliases: HashMap<String, Vec<String>>,
    labels: HashMap<(String, String), NodeId>,
    citations: HashMap<(String, String), NodeId>,
    origins: HashMap<String, NodeId>,
    // Card/section placement in the first top-level chapter containing it.
    chapter: HashMap<NodeId, usize>,
    card_files: HashMap<NodeId, std::path::PathBuf>,
    edges: Vec<Edge>,
    outgoing: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    dangling: Vec<Value>,
}

impl Index {
    pub(super) fn build(project: &Path, cache: &Cache, inputs: &Inputs) -> Self {
        let mut graph = Self::default();
        let mut pending = Vec::new();
        for input in &inputs.decks {
            let deck = deck_id(project, &input.root);
            let structure = cache
                .structures
                .get(&input.structure)
                .and_then(|entry| entry.data.as_ref());
            let meta = &input.cfg.meta;
            if meta.imported_latex {
                graph.native_labels.insert(deck.clone());
            }
            let title = if meta.title.trim().is_empty() {
                crate::cards::deck_display_title(
                    &input.root,
                    Some(&input.cfg),
                    structure.and_then(|s| s.title.as_deref()),
                )
            } else {
                meta.title.trim().to_owned()
            };
            let search = SearchText::new([&deck, &title, &meta.summary, ""]);
            let node = graph.push_node(Node {
                key: format!("deck:{deck}"),
                id: deck.clone(),
                deck: deck.clone(),
                node_type: "deck",
                kind: if input.cfg.harness && meta.kind.is_empty() {
                    "paper".into()
                } else {
                    meta.kind.clone()
                },
                title,
                status: meta.status.clone(),
                summary: super::one_line(&meta.summary, false),
            });
            let mut info = Deck {
                node,
                structured: structure.is_some(),
                search,
                ..Deck::default()
            };
            graph
                .aliases
                .entry(deck.rsplit('/').next().unwrap_or(&deck).to_owned())
                .or_default()
                .push(deck.clone());
            for path in &input.cards {
                let Some(card) = cache.cards[path].data.as_ref() else {
                    continue;
                };
                let key = format!("{deck}#{}", card.id);
                if graph.qualified.contains_key(&key) {
                    continue;
                }
                let node = graph.push_node(Node {
                    key: key.clone(),
                    id: card.id.clone(),
                    deck: deck.clone(),
                    node_type: "card",
                    kind: card.kind.clone(),
                    title: card.title.clone(),
                    status: card.status.clone(),
                    summary: card.summary.clone(),
                });
                graph.qualified.insert(key, node);
                graph.cards.entry(card.id.clone()).or_default().push(node);
                for label in &card.labels {
                    graph
                        .labels
                        .entry((deck.clone(), label.clone()))
                        .or_insert(node);
                }
                graph.card_files.insert(node, path.clone());
                info.cards.push(node);
                pending.push((input, card, node));
            }
            info.cards
                .sort_unstable_by(|&a, &b| graph.nodes[a].id.cmp(&graph.nodes[b].id));
            if let Some(structure) = structure {
                for (order, item) in structure.items.iter().enumerate() {
                    if let Some(section) =
                        graph.place(&deck, node, item, &format!("{}", order + 1), order, order)
                    {
                        info.sections.push(section);
                    }
                }
                if input.cfg.harness && !structure.appendix.is_empty() {
                    let order = structure.items.len();
                    let appendix = Item::Section {
                        title: meta
                            .appendix_title
                            .clone()
                            .unwrap_or_else(|| "Deferred proofs".into()),
                        label: None,
                        items: Vec::new(),
                    };
                    let path = format!("{}", order + 1);
                    let section = graph
                        .place(&deck, node, &appendix, &path, order, order)
                        .unwrap();
                    for (pos, item) in structure.appendix.iter().enumerate() {
                        graph.place(
                            &deck,
                            section,
                            item,
                            &format!("{path}.{}", pos + 1),
                            order,
                            pos,
                        );
                    }
                    info.sections.push(section);
                }
            } else {
                for (order, &card) in info.cards.iter().enumerate() {
                    graph.edge(node, card, "contains", Some(order));
                }
            }
            for bib in &input.bib {
                if let Some(keys) = cache
                    .bibliographies
                    .get(bib)
                    .and_then(|entry| entry.data.as_ref())
                {
                    for key in keys {
                        graph.citation(&deck, key, key, "");
                    }
                }
            }
            graph.decks.insert(deck, info);
        }
        for (id, dir) in &inputs.references {
            let entry = cache.references[dir].data.as_ref();
            let title = entry.and_then(|entry| entry.title.as_deref()).unwrap_or(id);
            let cite = graph.citation("", id, id, title);
            if let Some(key) = entry.and_then(|entry| entry.key.as_ref()) {
                graph.citations.insert((String::new(), key.clone()), cite);
            }
        }

        let mut seen = HashSet::new();
        for (input, card, from) in pending {
            let deck = graph.nodes[from].deck.clone();
            let source = card.from.iter().map(|target| (target.as_str(), "from"));
            for (target, rel) in card
                .links
                .iter()
                .map(|link| (link.to.as_str(), link.rel.as_str()))
                .chain(source)
            {
                let to = if rel == "cite" {
                    graph.lookup_citation(&deck, target)
                } else {
                    let target = if rel == "from" {
                        target
                            .strip_prefix("lab:")
                            .or_else(|| {
                                let name = project.file_name()?.to_str()?;
                                target.strip_prefix(name)?.strip_prefix(':')
                            })
                            .unwrap_or(target)
                    } else {
                        target
                    };
                    let resolved = graph.resolve(&deck, target, rel, |namespace| {
                        input
                            .external
                            .get(namespace)
                            .map(|path| deck_id(project, path))
                    }, None);
                    resolved.or_else(|| {
                        (rel == "from"
                            && external_origin(target)
                            && !input
                                .external
                                .contains_key(target.split(':').next().unwrap_or_default()))
                        .then(|| graph.origin(target))
                    })
                };
                if let Some(to) = to {
                    if seen.insert((from, to, rel)) {
                        graph.edge(from, to, rel, None);
                    }
                } else {
                    graph.missing(from, target, rel);
                }
            }
        }
        graph
    }

    fn push_node(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        self.outgoing.push(Vec::new());
        self.incoming.push(Vec::new());
        id
    }

    fn edge(&mut self, from: NodeId, to: NodeId, rel: &str, order: Option<usize>) {
        let id = self.edges.len();
        self.edges.push(Edge {
            from,
            to,
            rel: rel.to_owned(),
            order,
        });
        self.outgoing[from].push(id);
        self.incoming[to].push(id);
    }

    fn missing(&mut self, from: NodeId, target: &str, rel: &str) {
        let node = &self.nodes[from];
        self.dangling.push(json!({"type": "dangling_reference", "deck": node.deck, "from": node.key, "target": target, "rel": rel}));
    }

    fn place(
        &mut self,
        deck: &str,
        parent: NodeId,
        item: &Item,
        path: &str,
        chapter: usize,
        order: usize,
    ) -> Option<NodeId> {
        let node = match item {
            Item::Card(id) => match self.qualified.get(&format!("{deck}#{id}")) {
                Some(&node) => node,
                None => {
                    self.missing(parent, id, "contains");
                    return None;
                }
            },
            Item::Section {
                title,
                label,
                items,
            } => {
                let key = format!("section:{deck}#{path}");
                let node = self.push_node(Node {
                    key,
                    id: path.to_owned(),
                    deck: deck.to_owned(),
                    node_type: "section",
                    kind: "section".into(),
                    title: title.clone(),
                    status: String::new(),
                    summary: String::new(),
                });
                if let Some(label) = label {
                    self.labels
                        .entry((deck.to_owned(), label.clone()))
                        .or_insert(node);
                }
                for (order, item) in items.iter().enumerate() {
                    self.place(
                        deck,
                        node,
                        item,
                        &format!("{path}.{}", order + 1),
                        chapter,
                        order,
                    );
                }
                node
            }
        };
        self.chapter.entry(node).or_insert(chapter);
        self.edge(parent, node, "contains", Some(order));
        Some(node)
    }

    fn citation(&mut self, deck: &str, name: &str, id: &str, title: &str) -> NodeId {
        let alias = (deck.to_owned(), name.to_owned());
        if let Some(&node) = self.citations.get(&alias) {
            return node;
        }
        let node = self.push_node(Node {
            key: format!("reference:{deck}#{id}"),
            id: id.to_owned(),
            deck: deck.to_owned(),
            node_type: "reference",
            kind: "reference".into(),
            title: title.to_owned(),
            status: String::new(),
            summary: String::new(),
        });
        self.citations.insert(alias, node);
        node
    }

    fn lookup_citation(&self, deck: &str, key: &str) -> Option<NodeId> {
        self.citations
            .get(&(String::new(), key.to_owned()))
            .or_else(|| self.citations.get(&(deck.to_owned(), key.to_owned())))
            .copied()
    }

    fn origin(&mut self, target: &str) -> NodeId {
        if let Some(&node) = self.origins.get(target) {
            return node;
        }
        let (deck, id) = target.rsplit_once('#').unwrap_or(("", target));
        let node = self.push_node(Node {
            key: format!("origin:{target}"),
            id: id.to_owned(),
            deck: deck.to_owned(),
            node_type: "origin",
            kind: "origin".into(),
            title: target.to_owned(),
            status: String::new(),
            summary: String::new(),
        });
        self.origins.insert(target.to_owned(), node);
        node
    }

    fn resolve(
        &self,
        deck: &str,
        target: &str,
        rel: &str,
        external: impl Fn(&str) -> Option<String>,
        edited: Option<(NodeId, &CardData)>,
    ) -> Option<NodeId> {
        let lookup = |deck: &str, id: &str| {
            if rel == "ref" {
                if let Some((node, card)) = edited.filter(|&(node, _)| self.nodes[node].deck == deck) {
                    if card.labels.contains(id) {
                        return Some(node);
                    }
                }
                if let Some(&node) = self.labels.get(&(deck.to_owned(), id.to_owned())) {
                    if edited.is_none_or(|(edited_node, _)| node != edited_node) {
                        return Some(node);
                    }
                }
                if self.native_labels.contains(deck) {
                    return None;
                }
            }
            self.qualified.get(&format!("{deck}#{id}")).copied()
        };
        if rel == "ref" && self.native_labels.contains(deck) {
            return lookup(deck, target);
        }
        if let Some((namespace, id)) = target.rsplit_once('#') {
            let deck =
                external(namespace).or_else(|| self.deck_key(namespace).ok().map(str::to_owned))?;
            return lookup(&deck, id);
        }
        if let Some(node) = lookup(deck, target) {
            return Some(node);
        }
        if rel == "ref" {
            if let Some(node) = target.strip_prefix("card:").and_then(|id| lookup(deck, id)) {
                return Some(node);
            }
        }
        if let Some((namespace, id)) = target.split_once(':') {
            let deck =
                external(namespace).or_else(|| self.deck_key(namespace).ok().map(str::to_owned))?;
            return lookup(&deck, id);
        }
        self.cards
            .get(target)
            .and_then(|nodes| match nodes.as_slice() {
                &[id] => Some(id),
                _ => None,
            })
    }

    fn deck_key<'a>(&'a self, deck: &str) -> Result<&'a str> {
        let deck = if deck.is_empty() { "." } else { deck };
        if let Some((key, _)) = self.decks.get_key_value(deck) {
            return Ok(key);
        }
        match self.aliases.get(deck).map(Vec::as_slice) {
            Some([key]) => Ok(key),
            Some(keys) => bail!("デッキ {deck} は曖昧: {}", keys.join(", ")),
            None => bail!("デッキ {deck} が見つからない"),
        }
    }

    pub(super) fn card_key(&self, key: &str) -> Result<NodeId> {
        if let Some((deck, id)) = key.rsplit_once('#') {
            let deck = self.deck_key(deck)?;
            if let Some(&node) = self.qualified.get(&format!("{deck}#{id}")) {
                return Ok(node);
            }
        } else if let Some(nodes) = self.cards.get(key) {
            if let [node] = nodes.as_slice() {
                return Ok(*node);
            }
            let keys: Vec<_> = nodes
                .iter()
                .map(|&node| self.nodes[node].key.as_str())
                .collect();
            bail!(
                "カード {key} は曖昧。<deck>#<id> を使う: {}",
                keys.join(", ")
            );
        }
        bail!("カード {key} が見つからない")
    }

    pub(super) fn card_source(&self, node: NodeId) -> (&str, &str, &Path) {
        (&self.nodes[node].deck, &self.nodes[node].key, &self.card_files[&node])
    }

    pub(super) fn edit_neighbors(&self, edited: NodeId, before: &CardData, after: &CardData, external: impl Fn(&str) -> Option<String>) -> BTreeSet<NodeId> {
        let mut affected: BTreeSet<_> = std::iter::once(edited).chain(
            self.outgoing[edited].iter().chain(&self.incoming[edited])
                .map(|&edge| &self.edges[edge])
                .filter(|edge| edge.rel != "contains")
                .flat_map(|edge| [edge.from, edge.to])
                .filter(|&id| self.card_files.contains_key(&id))
        ).collect();
        let deck = &self.nodes[edited].deck;
        for link in after.links.iter().filter(|link| link.rel != "cite") {
            if let Some(to) = self.resolve(deck, &link.to, &link.rel, &external, Some((edited, after))) {
                if self.card_files.contains_key(&to) {
                    affected.insert(to);
                }
            }
        }
        for label in after.labels.difference(&before.labels) {
            for issue in self.dangling.iter().filter(|issue| issue["deck"] == *deck && issue["target"] == *label && issue["rel"] == "ref") {
                if let Some(id) = issue["from"].as_str().and_then(|key| self.qualified.get(key)) {
                    affected.insert(*id);
                }
            }
        }
        affected
    }

    pub(super) fn edit_refs(&self, from: NodeId, card: &CardData, edited: (NodeId, &CardData),
        external: impl Fn(&str) -> Option<String>,
    ) -> Vec<Value> {
        let deck = &self.nodes[from].deck;
        card.links.iter().filter(|link| matches!(link.rel.as_str(), "ref" | "cite"))
            .filter(|link| {
                if link.rel == "cite" {
                    self.lookup_citation(deck, &link.to).is_none()
                } else {
                    self.resolve(deck, &link.to, &link.rel, &external, Some(edited)).is_none()
                }
            })
            .map(|link| json!({"card": self.nodes[from].key, "rel": link.rel, "target": link.to}))
            .collect()
    }

    fn edge_json(&self, id: usize) -> Value {
        let edge = &self.edges[id];
        let mut data = json!({"from": self.nodes[edge.from].key, "to": self.nodes[edge.to].key, "rel": edge.rel});
        if let Some(order) = edge.order {
            data["order"] = json!(order);
        }
        data
    }

    pub(super) fn overview(&self, deck: Option<&str>, depth: usize) -> Result<Value> {
        let selected = deck.map(|deck| self.deck_key(deck)).transpose()?;
        let mut result = Vec::new();
        let mut omitted_decks = 0;
        // A shared budget bounds the entire response, even across many decks.
        let mut budget = if depth == 1 { 5_500 } else { 14_000 };
        for (key, deck) in &self.decks {
            if selected.is_some_and(|selected| selected != key) {
                continue;
            }
            let node = &self.nodes[deck.node];
            let mut counts = BTreeMap::new();
            for &card in &deck.cards {
                *counts.entry(&self.nodes[card].kind).or_insert(0) += 1;
            }
            let data = json!({ "deck": key, "kind": node.kind, "title": clip(&node.title, 100), "status": node.status,
                "summary": node.summary, "card_count": deck.cards.len(), "counts": counts,
                "section_count": deck.sections.len() });
            let cost = data.to_string().len() + 140;
            if cost > budget {
                omitted_decks += 1;
                continue;
            }
            budget -= cost;
            result.push((deck, data));
        }
        // Reserve the deck headers first, so a big draft cannot hide other decks.
        let deck_count = result.len();
        for (index, (deck, data)) in result.iter_mut().enumerate() {
            let share = budget / (deck_count - index);
            let mut remaining = share;
            let mut shown = HashSet::new();
            let mut sections = Vec::new();
            for &section in &deck.sections {
                if let Some(data) =
                    self.overview_section(section, depth, &mut remaining, &mut shown)
                {
                    sections.push(data);
                }
            }
            data["omitted_sections"] = json!(deck.sections.len() - sections.len());
            data["sections"] = json!(sections);
            if depth == 2 {
                let mut cards = Vec::new();
                for &card in &deck.cards {
                    if !self.chapter.contains_key(&card) {
                        if let Some(data) = self.overview_card(card, &mut remaining, &mut shown) {
                            cards.push(data);
                        }
                    }
                }
                data["cards"] = json!(cards);
            }
            data["omitted_cards"] = json!(deck.cards.len() - shown.len());
            budget -= share - remaining;
        }
        Ok(
            json!({"depth": depth, "decks": result.into_iter().map(|(_, data)| data).collect::<Vec<_>>(),
            "omitted_decks": omitted_decks,
            "zoom": "章の key を graph_overview(depth: 2, chapter: key) に渡す。カードは graph_search や graph_neighbors でも探せる。"}),
        )
    }

    fn overview_section(
        &self,
        node: NodeId,
        depth: usize,
        budget: &mut usize,
        shown: &mut HashSet<NodeId>,
    ) -> Option<Value> {
        let section = &self.nodes[node];
        let edges = &self.outgoing[node];
        let mut data = json!({"key": section.key, "title": clip(&section.title, 100), "item_count": edges.len()});
        let cost = data.to_string().len() + 60;
        *budget = budget.checked_sub(cost)?;
        if depth == 1 {
            return Some(data);
        }
        let mut items = Vec::new();
        for &edge in edges {
            let to = self.edges[edge].to;
            let data = if self.nodes[to].node_type == "section" {
                self.overview_section(to, depth, budget, shown)
            } else if depth == 2 {
                self.overview_card(to, budget, shown)
            } else {
                None
            };
            items.extend(data);
        }
        data["omitted_items"] = json!(edges.len() - items.len());
        data["items"] = json!(items);
        Some(data)
    }

    fn overview_card(
        &self,
        card: NodeId,
        budget: &mut usize,
        shown: &mut HashSet<NodeId>,
    ) -> Option<Value> {
        let node = &self.nodes[card];
        let data = json!({"id": node.id, "kind": node.kind, "title": clip(&node.title, 72), "summary": node.summary, "status": node.status});
        *budget = budget.checked_sub(data.to_string().len() + 1)?;
        shown.insert(card);
        Some(data)
    }

    pub(super) fn chapter_overview(&self, key: &str, deck: Option<&str>, offset: usize) -> Result<Value> {
        let Some((deck_key, _)) = key.strip_prefix("section:").and_then(|key| key.rsplit_once('#')) else {
            bail!("chapter は見取り図の章の key を指定する");
        };
        let canonical = self.deck_key(deck_key)?;
        if let Some(deck) = deck {
            anyhow::ensure!(self.deck_key(deck)? == canonical, "deck と chapter が一致しない");
        }
        let info = &self.decks[canonical];
        let chapter = info.sections.iter().copied().find(|&id| self.nodes[id].key == key)
            .ok_or_else(|| anyhow::anyhow!("章が見つからない ({key})"))?;
        let mut stack = vec![chapter];
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        while let Some(node) = stack.pop() {
            if !seen.insert(node) {
                continue;
            }
            if self.nodes[node].node_type == "card" {
                ordered.push(node);
            } else {
                stack.extend(self.outgoing[node].iter().rev().map(|&edge| &self.edges[edge])
                    .filter(|edge| edge.rel == "contains").map(|edge| edge.to));
            }
        }
        anyhow::ensure!(offset <= ordered.len(), "offset が章のカード数を超えている");
        let mut budget = 13_000;
        let mut shown = HashSet::new();
        let mut cards = Vec::new();
        for &card in &ordered[offset..] {
            match self.overview_card(card, &mut budget, &mut shown) {
                Some(data) => cards.push(data),
                None => break,
            }
        }
        let next = offset + cards.len();
        Ok(json!({"depth": 2, "deck": canonical, "chapter": key,
            "title": clip(&self.nodes[chapter].title, 100), "cards": cards,
            "card_count": ordered.len(), "next_offset": (next < ordered.len()).then_some(next),
        }))
    }

    pub(super) fn neighbors(
        &self,
        card: &str,
        depth: usize,
        rels: Option<&[String]>,
        direction: super::mcp::Direction,
    ) -> Result<Value> {
        let root = self.card_key(card)?;
        let mut nodes = vec![root];
        let mut found = HashSet::from([root]);
        let mut edges = BTreeSet::new();
        let mut queue = VecDeque::from([(root, 0)]);
        while let Some((node, distance)) = queue.pop_front() {
            if distance == depth {
                continue;
            }
            let selects = |id: &usize| rels.is_none_or(|rels| rels.contains(&self.edges[*id].rel));
            let steps = self.outgoing[node]
                .iter()
                .filter(|&&id| direction.out() && selects(&id))
                .map(|&id| (id, self.edges[id].to))
                .chain(
                    self.incoming[node]
                        .iter()
                        .filter(|&&id| direction.input() && selects(&id))
                        .map(|&id| (id, self.edges[id].from)),
                );
            for (edge, next) in steps {
                edges.insert(edge);
                if found.insert(next) {
                    nodes.push(next);
                    queue.push_back((next, distance + 1));
                }
            }
        }
        Ok(
            json!({"card": self.nodes[root].key, "nodes": nodes.iter().map(|&id| &self.nodes[id]).collect::<Vec<_>>(),
            "edges": edges.into_iter().map(|id| self.edge_json(id)).collect::<Vec<_>>() }),
        )
    }

    pub(super) fn path(&self, from: &str, to: &str, max_len: usize) -> Result<Value> {
        let (from, to) = (self.card_key(from)?, self.card_key(to)?);
        let mut parents = HashMap::<NodeId, Vec<usize>>::new();
        let mut distances = HashMap::from([(from, 0)]);
        let mut queue = VecDeque::from([from]);
        while let Some(node) = queue.pop_front() {
            let depth = distances[&node];
            if depth >= max_len
                || distances
                    .get(&to)
                    .is_some_and(|&distance| depth >= distance)
            {
                break;
            }
            for &id in &self.outgoing[node] {
                let edge = &self.edges[id];
                if edge.rel == "contains" || self.nodes[edge.to].node_type != "card" {
                    continue;
                }
                let distance = distances.entry(edge.to).or_insert_with(|| {
                    queue.push_back(edge.to);
                    depth + 1
                });
                if *distance == depth + 1 {
                    parents.entry(edge.to).or_default().push(id);
                }
            }
        }
        const LIMIT: usize = 8;
        let mut paths = Vec::new();
        let mut truncated = false;
        if let Some(&distance) = distances.get(&to) {
            let mut stack = vec![(to, 0)];
            let mut edges = Vec::new();
            while let Some((node, next)) = stack.last_mut() {
                if *node == from {
                    if paths.len() == LIMIT {
                        truncated = true;
                        break;
                    }
                    paths.push(json!({"length": distance,
                        "nodes": stack.iter().rev().map(|&(node, _)| &self.nodes[node].key).collect::<Vec<_>>(),
                        "edges": edges.iter().rev().map(|&id| self.edge_json(id)).collect::<Vec<_>>()
                    }));
                } else if let Some(&edge) = parents[node].get(*next) {
                    *next += 1;
                    edges.push(edge);
                    stack.push((self.edges[edge].from, 0));
                    continue;
                }
                stack.pop();
                edges.pop();
            }
        }
        Ok(
            json!({"from": self.nodes[from].key, "to": self.nodes[to].key, "paths": paths, "truncated": truncated}),
        )
    }

    pub(super) fn search(
        &self,
        cache: &Cache,
        query: &str,
        kinds: Option<&[String]>,
        decks: Option<&[String]>,
        limit: usize,
    ) -> Result<Value> {
        let query = query.trim().to_lowercase();
        let tokens = tokens(&query, false);
        let decks = decks
            .map(|decks| {
                decks
                    .iter()
                    .map(|deck| self.deck_key(deck))
                    .collect::<Result<HashSet<_>>>()
            })
            .transpose()?;
        let mut matches = Vec::new();
        if !tokens.is_empty() {
            for deck in self.decks.values() {
                for &id in &deck.cards {
                    let node = &self.nodes[id];
                    if kinds.is_some_and(|kinds| !kinds.contains(&node.kind))
                        || decks
                            .as_ref()
                            .is_some_and(|decks| !decks.contains(node.deck.as_str()))
                    {
                        continue;
                    }
                    let indexed = &cache.cards[&self.card_files[&id]]
                        .data
                        .as_ref()
                        .unwrap()
                        .search;
                    let mut score = indexed.score(&query, &tokens);
                    if query == node.id.to_lowercase() || query == node.key.to_lowercase() {
                        score += 100;
                    }
                    if score > 0 {
                        matches.push((node, score));
                    }
                }
            }
        }
        matches.sort_unstable_by(|(a, score_a), (b, score_b)| {
            score_b.cmp(score_a).then(a.key.cmp(&b.key))
        });
        Ok(
            json!({"total": matches.len(), "matches": matches.into_iter().take(limit)
            .map(|(node, score)| json!({"card": node, "score": score})).collect::<Vec<_>>() }),
        )
    }

    pub(super) fn route_scores(
        &self,
        cache: &Cache,
        project: &Path,
        texts: &[String],
        paths: &[String],
        search: bool,
    ) -> Vec<(fuzukue_proto::DeckRoute, usize)> {
        let queries: Vec<_> = texts
            .iter()
            .map(|text| {
                let text = text.trim().to_lowercase();
                let terms = tokens(&text, false);
                (text, terms)
            })
            .collect();
        let mut scores = Vec::new();
        for (deck, info) in &self.decks {
            let node = &self.nodes[info.node];
            if std::path::Path::new(deck).is_absolute()
                || node.kind == crate::memos::DECK
                || (!search && !matches!(node.kind.as_str(), "research" | "paper" | "book"))
            {
                continue;
            }
            let [key, title, summary, _] = &info.search.fields;
            let mut score = paths
                .iter()
                .any(|path| {
                    (deck != "."
                        && path
                            .strip_prefix(deck)
                            .is_some_and(|tail| tail.starts_with('/')))
                        || info.cards.iter().any(|id| {
                            self.card_files[id]
                                .strip_prefix(project)
                                .is_ok_and(|card_path| card_path == std::path::Path::new(path))
                        })
                })
                .then_some(95)
                .unwrap_or_default();
            for (text, terms) in &queries {
                if terms.is_empty() {
                    continue;
                }
                let mut hit = if search {
                    info.search.score(text, terms)
                } else {
                    info.search.route_overlap(terms)
                };
                if !search {
                    if deck != "." && mentions(text, key) {
                        hit = hit.max(100);
                    }
                    if strong_title(title) && mentions(text, title) {
                        hit = hit.max(85);
                    }
                    if summary.chars().count() >= 18 && mentions(text, summary) {
                        hit = hit.max(75);
                    }
                }
                for id in &info.cards {
                    let indexed = &cache.cards[&self.card_files[id]]
                        .data
                        .as_ref()
                        .unwrap()
                        .search;
                    let mut card_score = if search {
                        indexed.score(text, terms)
                    } else {
                        indexed.route_overlap(terms)
                    };
                    if !search {
                        let [card_id, title, summary, _] = &indexed.fields;
                        if deck != "." && mentions(text, &format!("{key}#{card_id}")) {
                            card_score = 130;
                        } else if text.contains(&format!("[[{card_id}]]"))
                            || text.contains(&format!("[[{card_id}|"))
                            || text.contains(&format!("`{card_id}`"))
                            || (card_id.len() >= 5
                                && card_id.contains(['-', '_'])
                                && mentions(text, card_id))
                        {
                            card_score = 100;
                        } else if (strong_title(title) && mentions(text, title))
                            || (summary.chars().count() >= 18 && mentions(text, summary))
                        {
                            card_score = 75;
                        }
                    }
                    hit = hit.max(card_score);
                }
                score = score.max(hit);
            }
            if score > 0 {
                scores.push((
                    fuzukue_proto::DeckRoute {
                        deck: deck.clone(),
                        title: node.title.clone(),
                        kind: node.kind.clone(),
                    },
                    score,
                ));
            }
        }
        scores.sort_unstable_by(|(a, sa), (b, sb)| sb.cmp(sa).then(a.deck.cmp(&b.deck)));
        scores
    }

    pub(super) fn issues(&self, deck: Option<&str>) -> Result<Value> {
        let deck = deck.map(|deck| self.deck_key(deck)).transpose()?;
        let includes = |node: NodeId| deck.is_none_or(|deck| self.nodes[node].deck == deck);
        let mut issues: Vec<_> = self
            .dangling
            .iter()
            .filter(|issue| deck.is_none_or(|deck| issue["deck"] == deck))
            .cloned()
            .collect();
        for data in self.decks.values().filter(|data| data.structured) {
            for &node in data
                .cards
                .iter()
                .filter(|&&node| includes(node) && !self.chapter.contains_key(&node))
            {
                let card = &self.nodes[node];
                issues
                    .push(json!({"type": "unreachable_card", "deck": card.deck, "card": card.key}));
            }
        }
        for (id, edge) in self.edges.iter().enumerate() {
            if edge.rel == "contains" || !includes(edge.from) {
                continue;
            }
            let (from, to) = (&self.nodes[edge.from], &self.nodes[edge.to]);
            let from_proved = matches!(from.status.as_str(), "proved" | "formalized" | "verified");
            let unproved = matches!(
                to.kind.as_str(),
                "theorem" | "lemma" | "proposition" | "corollary" | "claim"
            ) && !matches!(to.status.as_str(), "proved" | "formalized" | "verified");
            if edge.rel == "requires" && from_proved && unproved {
                issues.push(json!({"type": "status_inconsistency", "deck": from.deck, "edge": self.edge_json(id),
                    "from_status": from.status, "to_status": to.status}));
            }
            if from.deck == to.deck
                && edge.rel != "from"
                && self
                    .chapter
                    .get(&edge.from)
                    .zip(self.chapter.get(&edge.to))
                    .is_some_and(|(a, b)| a < b)
            {
                issues.push(json!({"type": "forward_chapter_reference", "deck": from.deck, "edge": self.edge_json(id),
                    "from_chapter": self.chapter[&edge.from] + 1, "to_chapter": self.chapter[&edge.to] + 1}));
            }
        }
        for component in self.requires_cycles() {
            if component.iter().any(|&node| includes(node)) {
                issues.push(json!({"type": "requires_cycle", "cards": component.iter().map(|&node| &self.nodes[node].key).collect::<Vec<_>>()}));
            }
        }
        Ok(json!({"total": issues.len(), "issues": issues}))
    }

    fn requires_cycles(&self) -> Vec<Vec<NodeId>> {
        super::requires_cycles(
            self.qualified.values().copied(),
            self.nodes.len(),
            |node| self.outgoing[node].iter().map(|&id| &self.edges[id])
                .filter(|edge| edge.rel == "requires").map(|edge| edge.to),
            |node| self.incoming[node].iter().map(|&id| &self.edges[id])
                .filter(|edge| edge.rel == "requires").map(|edge| edge.from),
        )
    }
}

pub(super) fn deck_id(project: &Path, root: &Path) -> String {
    let path = root.strip_prefix(project).unwrap_or(root);
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}

fn external_origin(target: &str) -> bool {
    target.contains('#') && (target.contains("://") || target.starts_with("git@"))
}

const SEARCH_WEIGHTS: [usize; 4] = [8, 5, 3, 1];

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct SearchText {
    fields: [String; 4],
    terms: BTreeMap<String, usize>,
}

impl SearchText {
    fn score(&self, query: &str, tokens: &HashSet<String>) -> usize {
        let terms = tokens
            .iter()
            .filter_map(|token| self.terms.get(token))
            .sum::<usize>();
        terms
            + self
                .fields
                .iter()
                .zip(SEARCH_WEIGHTS)
                .filter(|(text, _)| text.contains(query))
                .map(|(_, weight)| weight * 2)
                .sum::<usize>()
    }

    fn route_overlap(&self, terms: &HashSet<String>) -> usize {
        // Require several distinct terms before suggesting a deck on overlap alone.
        let matched = terms
            .iter()
            .filter(|term| term.chars().count() >= 2)
            .filter_map(|term| self.terms.get(term));
        if matched.clone().take(3).count() < 3 {
            return 0;
        }
        matched.sum::<usize>().min(29)
    }

    pub(super) fn new(fields: [&str; 4]) -> Self {
        let fields = fields.map(str::to_lowercase);
        let mut terms = BTreeMap::new();
        for (text, weight) in fields.iter().zip(SEARCH_WEIGHTS) {
            for token in tokens(text, true) {
                *terms.entry(token).or_default() += weight;
            }
        }
        Self { fields, terms }
    }
}

fn strong_title(title: &str) -> bool {
    if title.is_ascii() {
        title.split_whitespace().count() >= 2 || title.len() >= 9
    } else {
        title.chars().count() >= 4
    }
}

fn mentions(text: &str, needle: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-');
    text.match_indices(needle).any(|(start, _)| {
        !text[..start].chars().next_back().is_some_and(word)
            && !text[start + needle.len()..]
                .chars()
                .next()
                .is_some_and(word)
    })
}

fn tokens(text: &str, all_single_chars: bool) -> HashSet<String> {
    let mut result = HashSet::new();
    let is_japanese = |c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ff66}'..='\u{ff9f}' | '\u{20000}'..='\u{323af}' | '々');
    let mut previous = None;
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if is_japanese(c) {
            if !word.is_empty() {
                result.insert(std::mem::take(&mut word));
            }
            if all_single_chars
                || previous.is_none() && !chars.peek().copied().is_some_and(is_japanese)
            {
                result.insert(c.to_string());
            }
            if let Some(p) = previous {
                result.insert([p, c].into_iter().collect());
            }
            previous = Some(c);
        } else {
            previous = None;
            if c.is_alphanumeric() {
                word.push(c);
            } else if !word.is_empty() {
                result.insert(std::mem::take(&mut word));
            }
        }
    }
    if !word.is_empty() {
        result.insert(word);
    }
    result
}
