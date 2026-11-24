use crate::{scan, Doc, Site};
use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct Target(pub usize, pub usize);

type Lookup = HashMap<(String, String), Vec<Target>>;

pub struct Index {
    pub docs: Vec<Doc>,
    lookup: Lookup,
    explicit: HashSet<(usize, usize)>,
    terms: BTreeMap<String, HashMap<usize, Evidence>>,
}

#[derive(Clone, Copy)]
pub struct Evidence {
    pub site: Site,
    pub defined: bool,
    count: usize,
}

#[derive(Default)]
pub struct Pair {
    pub score: f64,
    pub terms: Vec<(f64, String, [Site; 2])>,
}

fn leaf(name: &str) -> &str {
    name.rsplit('.').next().unwrap()
}

fn lean_alias(name: &str) -> String {
    let leaf = leaf(name);
    let mut name = String::with_capacity(leaf.len());
    let mut last = ' ';
    for ch in leaf.chars() {
        if (last.is_ascii_lowercase() || last.is_ascii_digit()) && ch.is_ascii_uppercase() {
            name.push(' ');
        }
        name.push(ch);
        last = ch;
    }
    scan::normalize(&name)
}

fn rankable(name: &str) -> bool {
    let words = name.split_whitespace().count();
    let cjk = name
        .chars()
        .any(|c| ('ぁ'..='ヿ').contains(&c) || ('一'..='龯').contains(&c));
    (if cjk {
        name.chars().count() >= 3
    } else {
        name.len() >= 8
    }) && (1..=9).contains(&words)
        && !name.contains([':', '：', '#', '=', '/'])
        && !matches!(
            name,
            "definition"
                | "theorem"
                | "proposition"
                | "assumption"
                | "questions"
                | "condition"
                | "conclusion"
                | "following"
                | "statement"
                | "hypothesis"
                | "where"
                | "construction"
                | "structure"
                | "research"
                | "counterexample"
                | "the following"
                | "notation"
                | "property"
                | "important"
                | "observations"
                | "the question"
        )
}

fn insert(lookup: &mut Lookup, kind: &str, key: &str, target: Target) {
    lookup
        .entry((kind.to_owned(), key.to_owned()))
        .or_default()
        .push(target);
}

fn use_key<'a>(kind: &'a str, name: &str) -> (&'a str, String) {
    match kind {
        "concept" => ("term", scan::normalize(name)),
        "lean_prose" | "card_lean" => ("lean", name.to_owned()),
        "tex_ref" => ("label", name.to_owned()),
        "citation" => ("bib_entry", name.to_owned()),
        "card_link" | "wikilink" => ("card", name.to_owned()),
        _ => (kind, name.to_owned()),
    }
}

fn unique<T: Copy>(values: impl Iterator<Item = T>) -> Option<T> {
    let mut values = values;
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

fn order(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

fn clean_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn file_target(
    source_id: usize,
    source_path: &str,
    source_project: &str,
    kind: &str,
    name: &str,
    paths: &HashMap<String, usize>,
    modules: &HashMap<String, Vec<usize>>,
) -> Option<usize> {
    if kind == "lean_import" {
        let matches = modules.get(name)?;
        return unique(
            matches
                .iter()
                .copied()
                .filter(|&id| id != source_id)
                .filter(|&id| {
                    paths.get(&format!(
                        "{}/{}.lean",
                        source_project,
                        name.replace('.', "/")
                    )) == Some(&id)
                }),
        )
        .or_else(|| unique(matches.iter().copied().filter(|&id| id != source_id)));
    }
    if name.contains("://") || name.starts_with("mailto:") {
        return None;
    }
    let name = name
        .strip_prefix("file:")
        .or_else(|| name.strip_prefix("run:"))
        .unwrap_or(name);
    let (path, _) = name.split_once('#').unwrap_or((name, ""));
    if path.is_empty() {
        return None;
    }
    let path = path.replace("%20", " ");
    let parent = Path::new(source_path).parent().unwrap();
    let suffixes: &[&str] = match kind {
        "tex_include" => &[".tex", ""],
        "wikilink" => &["", ".md"],
        _ => &[""],
    };
    for base in [parent, Path::new(source_project), Path::new("")] {
        for suffix in suffixes {
            if let Some(path) = clean_path(&base.join(format!("{path}{suffix}"))) {
                if let Some(&id) = paths.get(&path) {
                    return Some(id);
                }
            }
        }
    }
    None
}

impl Index {
    pub fn build(mut docs: Vec<Doc>, bodies: &[String]) -> Result<Self> {
        let mut lookup = Lookup::new();
        let mut aliases = BTreeMap::new();
        let mut paths = HashMap::new();
        let mut modules: HashMap<String, Vec<usize>> = HashMap::new();
        for doc in &mut docs {
            doc.symbols
                .sort_by_key(|s| (s.site.line, s.site.column, s.kind));
            doc.symbols
                .dedup_by(|a, b| a.site == b.site && a.kind == b.kind && a.name == b.name);
            paths.insert(doc.path.clone(), doc.id);
            if doc.ext == "lean" {
                let relative = doc
                    .path
                    .strip_prefix(&format!("{}/", doc.project))
                    .unwrap_or(&doc.path);
                let module = relative.trim_end_matches(".lean").replace('/', ".");
                modules.entry(module).or_default().push(doc.id);
            }
            for (i, symbol) in doc.symbols.iter().enumerate() {
                let target = Target(doc.id, i);
                match symbol.kind {
                    "concept" => {
                        let name = scan::normalize(&symbol.name);
                        insert(&mut lookup, "term", &name, target);
                        if !name.is_empty() {
                            aliases.insert(symbol.name.to_lowercase(), name.clone());
                            aliases.insert(name.clone(), name);
                        }
                    }
                    kind if kind.starts_with("lean_") => {
                        insert(&mut lookup, "lean", &symbol.name, target);
                        let short = leaf(&symbol.name);
                        if short != symbol.name {
                            insert(&mut lookup, "lean", short, target);
                        }
                        let alias = lean_alias(&symbol.name);
                        if alias.split_whitespace().count() >= 2 && rankable(&alias) {
                            aliases.insert(alias.clone(), alias);
                        }
                    }
                    _ => insert(&mut lookup, symbol.kind, &symbol.name, target),
                }
            }
        }
        let patterns: Vec<_> = aliases.keys().cloned().collect();
        let names: HashSet<_> = aliases
            .values()
            .filter(|name| rankable(name))
            .cloned()
            .collect();
        let matcher = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::Standard)
            .build(&patterns)?;
        let macros: HashSet<_> = lookup
            .keys()
            .filter(|(k, _)| k == "macro")
            .map(|(_, n)| n.as_str())
            .collect();
        let lean_names: HashSet<_> = lookup
            .keys()
            .filter(|(k, _)| k == "lean")
            .map(|(_, n)| n.as_str())
            .collect();
        for (doc, body) in docs.iter_mut().zip(bodies) {
            let declarations: HashSet<_> = doc
                .symbols
                .iter()
                .filter(|s| matches!(s.kind, "macro" | "concept") || s.kind.starts_with("lean_"))
                .map(|s| (s.kind == "macro", s.site.line, s.site.column))
                .collect();
            if doc.ext == "lean" {
                for token in scan::LEAN_IDENT.find_iter(body) {
                    let name = token.as_str();
                    if lean_names.contains(name)
                        && !declared_here(doc, &declarations, token.start(), false)
                    {
                        doc.refer("lean", name, token.start());
                    }
                }
            } else if doc.ext != "bib" {
                for caps in scan::MACRO.captures_iter(body) {
                    let m = caps.get(1).unwrap();
                    if macros.contains(m.as_str())
                        && !declared_here(doc, &declarations, m.start(), true)
                    {
                        doc.refer("macro", m.as_str(), m.start());
                    }
                }
                for caps in scan::CODE_SPAN.captures_iter(body) {
                    let m = caps.get(1).unwrap();
                    if lean_names.contains(m.as_str()) {
                        doc.refer("lean_prose", m.as_str(), m.start());
                    }
                }
                // Inline code may name a Lean identifier, but does not declare links.
                if doc.ext == "md" && body.contains('`') {
                    let spans: Vec<_> =
                        scan::CODE_SPAN.find_iter(body).map(|m| m.range()).collect();
                    doc.uses.retain(|u| {
                        !matches!(u.kind, "wikilink" | "file_link") || {
                            let pos = doc.lines[u.site.line - 1] + u.site.column - 1;
                            let n = spans.partition_point(|span| span.end <= pos);
                            !spans.get(n).is_some_and(|span| span.contains(&pos))
                        }
                    });
                }
                for m in matcher.find_overlapping_iter(body) {
                    let pattern = &patterns[m.pattern().as_usize()];
                    if word_boundary(body, m.start(), m.end(), pattern)
                        && !declared_here(doc, &declarations, m.start(), false)
                    {
                        doc.refer("concept", &aliases[pattern], m.start());
                    }
                }
            }
            doc.uses.sort_unstable_by(|a, b| {
                (a.site.line, a.site.column, a.kind, &a.name).cmp(&(
                    b.site.line,
                    b.site.column,
                    b.kind,
                    &b.name,
                ))
            });
            doc.uses
                .dedup_by(|a, b| a.site == b.site && a.kind == b.kind && a.name == b.name);
        }
        let mut explicit = HashSet::new();
        let projects: Vec<_> = docs.iter().map(|d| d.project.clone()).collect();
        let mut terms = BTreeMap::new();
        for doc in &mut docs {
            for s in &doc.symbols {
                let name = match s.kind {
                    "concept" => scan::normalize(&s.name),
                    k if k.starts_with("lean_") => lean_alias(&s.name),
                    _ => continue,
                };
                if names.contains(&name) {
                    record(&mut terms, name, doc.id, s.site, true);
                }
            }
            for u in &mut doc.uses {
                let (kind, mut key) = use_key(u.kind, &u.name);
                let qualifier = if kind == "card" {
                    if let Some((deck, id)) =
                        u.name.rsplit_once('#').or_else(|| u.name.split_once(':'))
                    {
                        key = id.to_owned();
                        Some(deck)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(values) = lookup.get(&(kind.to_owned(), key)) {
                    let scoped = |target: &&Target| {
                        let project = &projects[target.0];
                        qualifier.map_or(project == &doc.project, |deck| {
                            project == deck || project.ends_with(&format!("/{deck}"))
                        })
                    };
                    u.target = if qualifier.is_some() {
                        unique(values.iter().filter(scoped).copied())
                    } else {
                        let local: Vec<_> =
                            values.iter().copied().filter(|t| t.0 == doc.id).collect();
                        let project: Vec<_> = values.iter().filter(scoped).copied().collect();
                        match (local.as_slice(), project.as_slice()) {
                            ([target], _) | ([], [target]) => Some(*target),
                            ([], []) if u.kind != "lean" || u.name.contains('.') => {
                                unique(values.iter().copied())
                            }
                            _ => None,
                        }
                    };
                }
                if matches!(
                    u.kind,
                    "wikilink"
                        | "card_link"
                        | "tex_ref"
                        | "tex_include"
                        | "file_link"
                        | "lean_import"
                        | "card_lean"
                        | "lean_prose"
                ) {
                    let target = u.target.map(|t| t.0).or_else(|| {
                        if matches!(u.kind, "tex_ref" | "card_lean" | "lean_prose") {
                            return None;
                        }
                        file_target(
                            doc.id,
                            &doc.path,
                            &doc.project,
                            u.kind,
                            &u.name,
                            &paths,
                            &modules,
                        )
                    });
                    if let Some(to) = target {
                        if to != doc.id {
                            explicit.insert(order(doc.id, to));
                        }
                        if u.target.is_none() {
                            u.target = Some(Target(to, usize::MAX));
                        }
                    }
                }
                let name = match u.kind {
                    "concept" => Some(u.name.clone()),
                    "lean" | "lean_prose" | "card_lean" => Some(lean_alias(&u.name)),
                    _ => None,
                };
                if let Some(name) = name.filter(|name| names.contains(name)) {
                    record(&mut terms, name, doc.id, u.site, false);
                }
            }
        }
        Ok(Self {
            docs,
            lookup,
            terms,
            explicit,
        })
    }

    fn weight(&self, frequency: usize) -> f64 {
        (1.0 + self.docs.len() as f64 / frequency as f64).ln()
    }

    pub fn linked(&self, a: usize, b: usize) -> bool {
        self.explicit.contains(&order(a, b))
    }

    pub fn results(
        &self,
        left: &str,
        right: &str,
        excluded: &[&str],
        include_linked: bool,
        include_duplicates: bool,
    ) -> Vec<((usize, usize), Pair)> {
        let mut pairs: HashMap<_, Pair> = HashMap::new();
        let left_match: Vec<_> = self.docs.iter().map(|d| d.path.contains(left)).collect();
        let right_match: Vec<_> = self.docs.iter().map(|d| d.path.contains(right)).collect();
        let excluded: Vec<_> = self
            .docs
            .iter()
            .map(|d| excluded.iter().any(|needle| d.path.contains(needle)))
            .collect();
        let max_frequency = (self.docs.len() / 70).clamp(24, 140);
        for (term, postings) in &self.terms {
            if !(2..=max_frequency).contains(&postings.len()) {
                continue;
            }
            let weight = self.weight(postings.len());
            for (&a, &first) in postings {
                for (&b, &second) in postings {
                    if a >= b
                        || excluded[a]
                        || excluded[b]
                        || (!include_duplicates
                            && self.docs[a].fingerprint == self.docs[b].fingerprint)
                        || self.docs[a].folder == self.docs[b].folder
                        || (!include_linked && self.linked(a, b))
                        || !(left_match[a] && right_match[b] || left_match[b] && right_match[a])
                    {
                        continue;
                    }
                    let weight = weight
                        * (if first.defined || second.defined {
                            1.0
                        } else {
                            0.72
                        })
                        * (1.0 + (first.count.min(second.count).min(5) as f64).ln() * 0.17);
                    let pair = pairs.entry((a, b)).or_default();
                    pair.terms
                        .push((weight, term.clone(), [first.site, second.site]));
                }
            }
        }
        let mut pairs: Vec<_> = pairs.into_iter().collect();
        for (_, p) in &mut pairs {
            p.terms
                .sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            // Overlapping phrase aliases and long texts otherwise accumulate
            // many nearly redundant votes.
            let mut independent: Vec<&str> = Vec::new();
            for (weight, term, _) in &p.terms {
                if independent
                    .iter()
                    .any(|existing| existing.contains(term) || term.contains(existing))
                {
                    continue;
                }
                p.score += weight / (1.0 + 0.42 * independent.len() as f64);
                independent.push(term);
                if independent.len() == 5 {
                    break;
                }
            }
        }
        pairs.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));
        pairs
    }

    pub fn hubs(&self, filter: &str) -> Vec<(f64, usize, usize, Vec<(usize, String, Site)>)> {
        let mut mentions: HashMap<&str, Vec<(usize, Evidence)>> = HashMap::new();
        for (term, postings) in &self.terms {
            if !(2..=(self.docs.len() / 90).clamp(16, 120)).contains(&postings.len()) {
                continue;
            }
            mentions.insert(
                term,
                postings.iter().map(|(&id, &site)| (id, site)).collect(),
            );
        }
        let mut hubs = vec![];
        for doc in &self.docs {
            for (i, q) in doc.questions.iter().enumerate() {
                if !filter.is_empty()
                    && !doc.path.contains(filter)
                    && !q.title.to_lowercase().contains(filter)
                {
                    continue;
                }
                let names = doc
                    .uses
                    .iter()
                    .filter(|u| u.kind == "concept" && (q.line..=q.end).contains(&u.site.line))
                    .map(|u| u.name.as_str())
                    .chain(
                        doc.symbols
                            .iter()
                            .filter(|s| {
                                s.kind == "concept" && (q.line..=q.end).contains(&s.site.line)
                            })
                            .map(|s| s.name.as_str()),
                    )
                    .map(scan::normalize)
                    .collect::<BTreeSet<_>>();
                let mut related: HashMap<_, (f64, f64, &str, Site)> = HashMap::new();
                for name in &names {
                    let Some(postings) = mentions.get(name.as_str()) else {
                        continue;
                    };
                    let w = self.weight(postings.len());
                    for &(id, evidence) in postings {
                        if doc.folder == self.docs[id].folder {
                            continue;
                        }
                        let entry =
                            related
                                .entry(id)
                                .or_insert((0., w, name.as_str(), evidence.site));
                        if w > entry.1 {
                            entry.1 = w;
                            entry.2 = name.as_str();
                            entry.3 = evidence.site;
                        }
                        entry.0 += w;
                    }
                }
                let mut related: Vec<_> = related.into_iter().collect();
                related.sort_by(|(a, x), (b, y)| y.0.total_cmp(&x.0).then(a.cmp(b)));
                let mut projects = HashSet::new();
                related.retain(|(id, _)| projects.insert(&self.docs[*id].project));
                if related.is_empty() {
                    continue;
                }
                let score: f64 = related.iter().take(3).map(|(_, e)| e.0).sum();
                hubs.push((
                    score,
                    doc.id,
                    i,
                    related
                        .into_iter()
                        .take(3)
                        .map(|(id, (_, _, term, site))| (id, term.to_owned(), site))
                        .collect(),
                ));
            }
        }
        hubs.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.1, a.2).cmp(&(b.1, b.2))));
        hubs
    }

    pub fn find(&self, name: &str, limit: usize) {
        let normalized = scan::normalize(name);
        println!("Definitions");
        let mut hits = Vec::new();
        for ((_, key), targets) in &self.lookup {
            if key == name || key == &normalized {
                hits.extend(targets.iter().copied());
            }
        }
        hits.sort_unstable();
        hits.dedup();
        for Target(d, s) in hits.iter().copied().take(limit) {
            let doc = &self.docs[d];
            let symbol = &doc.symbols[s];
            println!(
                "{}:{} {} {} [{}]",
                doc.path, symbol.site.line, symbol.kind, symbol.name, symbol.origin
            );
        }
        println!("References");
        for (d, u) in self
            .docs
            .iter()
            .flat_map(|d| d.uses.iter().map(move |u| (d, u)))
            .filter(|(_, u)| {
                u.name == name
                    || u.name == normalized
                    || u.target.is_some_and(|t| hits.binary_search(&t).is_ok())
            })
            .take(limit)
        {
            println!(
                "{}:{} {} {}{}",
                d.path,
                u.site.line,
                u.kind,
                u.name,
                if u.target.is_none() {
                    " [unresolved]"
                } else {
                    ""
                }
            );
        }
    }
}

fn declared_here(
    doc: &Doc,
    declarations: &HashSet<(bool, usize, usize)>,
    offset: usize,
    is_macro: bool,
) -> bool {
    let i = doc.lines.partition_point(|&line| line <= offset) - 1;
    declarations.contains(&(is_macro, i + 1, offset - doc.lines[i] + 1))
}

fn word_boundary(body: &str, start: usize, end: usize, pattern: &str) -> bool {
    (!pattern.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !body[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric()))
        && (!pattern.ends_with(|c: char| c.is_ascii_alphanumeric())
            || !body[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric()))
}

fn record(
    terms: &mut BTreeMap<String, HashMap<usize, Evidence>>,
    name: String,
    id: usize,
    site: Site,
    defined: bool,
) {
    let record = terms
        .entry(name)
        .or_default()
        .entry(id)
        .or_insert(Evidence {
            site,
            defined,
            count: 0,
        });
    record.count += 1;
    if defined && !record.defined {
        record.site = site;
        record.defined = true;
    }
}
