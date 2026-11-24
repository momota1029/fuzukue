mod extract;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuzukue_proto::{
    SymbolHit, SymbolKind, SymbolLocation, SymbolLookup, SymbolReference, SymbolResults,
};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use self::extract::{Extract, Mark};
use super::{stamp, DeckInput, FileSet, Files};

#[derive(Default)]
pub(crate) struct ReviewSymbols {
    pub(crate) labels: BTreeSet<String>,
    pub(crate) references: BTreeSet<String>,
    pub(crate) includes: Vec<String>,
}

pub(crate) fn review_symbols(path: &Path, text: &str) -> ReviewSymbols {
    let Some(source) = extract::source(path, text) else {
        return ReviewSymbols::default();
    };
    ReviewSymbols {
        labels: source.defs.into_iter().filter(|mark| mark.kind == SymbolKind::Label).map(|mark| mark.name).collect(),
        references: source.refs.into_iter().filter(|mark| mark.kind == SymbolKind::Label).map(|mark| mark.name).collect(),
        includes: source.includes,
    }
}

const RESULT_LIMIT: usize = 30;
type Posting = (Arc<Path>, usize);
type Postings = HashMap<(SymbolKind, String), BTreeSet<Posting>>;
type Occurrence<'a> = (&'a Path, &'a Source, &'a Mark);

#[derive(Default)]
pub(super) struct Symbols {
    files: Files<Source>,
    definitions: Postings,
    uses: Postings,
    decks: BTreeMap<PathBuf, String>,
    aliases: BTreeMap<(String, String), String>,
    cards: BTreeMap<PathBuf, (String, String)>,
    preambles: FileSet,
    packages: BTreeMap<PathBuf, String>,
    modules: BTreeMap<String, BTreeSet<PathBuf>>,
    tex_roots: HashMap<PathBuf, BTreeSet<PathBuf>>,
}

struct Source {
    raw: String,
    extracted: Extract,
    digest: [u8; 32],
    lines: Vec<usize>,
    deck: String,
    card: Option<String>,
    is_lean: bool,
    module: Option<String>,
}

impl Source {
    fn location(&self, project: &Path, path: &Path, range: &Range<usize>) -> SymbolLocation {
        let line = self.lines.partition_point(|&start| start <= range.start);
        let end_line = self.lines.partition_point(|&start| start < range.end);
        let start = self.lines[line - 1];
        let end = self.lines.get(line).copied().unwrap_or(self.raw.len());
        let snippet = self.raw[start..end].trim();
        let focus = range.start.saturating_sub(
            start + self.raw[start..end].len() - self.raw[start..end].trim_start().len(),
        );
        let focus = focus.min(snippet.len());
        let lo = snippet[..focus]
            .char_indices()
            .rev()
            .nth(35)
            .map_or(0, |(i, _)| i);
        let hi = snippet[lo..]
            .char_indices()
            .nth(110)
            .map_or(snippet.len(), |(i, _)| lo + i);
        let snippet = format!(
            "{}{}{}",
            if lo > 0 { "…" } else { "" },
            &snippet[lo..hi],
            if hi < snippet.len() { "…" } else { "" }
        );
        SymbolLocation {
            deck: self.deck.clone(),
            file: relative(project, path),
            card: self.card.clone(),
            start: range.start,
            end: range.end,
            line,
            end_line: (end_line > line).then_some(end_line),
            snippet,
        }
    }
}

fn relative(project: &Path, path: &Path) -> String {
    let path = path.strip_prefix(project).unwrap_or(path);
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    }
}

fn key(kind: SymbolKind, name: &str) -> String {
    match kind {
        SymbolKind::Term => extract::term_key(name),
        SymbolKind::Macro if !name.starts_with('\\') => format!("\\{name}"),
        _ => name.to_owned(),
    }
}

pub(crate) fn card_symbols(files: &crate::auto_review::CardFiles) -> Vec<(SymbolKind, String)> {
    let mut names: Vec<_> = files
        .iter()
        .filter_map(|(path, bytes)| extract::source(path, std::str::from_utf8(bytes).ok()?))
        .flat_map(|source| source.defs)
        .filter(|mark| mark.kind != SymbolKind::Card)
        .map(|mark| (mark.kind, mark.qualified.unwrap_or(mark.name)))
        .collect();
    names.sort_unstable_by(|a, b| (a.0 as u8, &a.1).cmp(&(b.0 as u8, &b.1)));
    names.dedup();
    names
}

fn posting_keys(mark: &Mark) -> impl Iterator<Item = (SymbolKind, String)> + '_ {
    let name = key(mark.kind, &mark.name);
    let qualified = mark.qualified.as_deref().filter(|full| *full != name);
    std::iter::once(name)
        .chain(qualified.map(str::to_owned))
        .map(|name| (mark.kind, name))
}

fn edit_postings(postings: &mut Postings, path: &Path, marks: &[Mark], add: bool) {
    let path = Arc::<Path>::from(path);
    for (index, mark) in marks.iter().enumerate() {
        for key in posting_keys(mark) {
            let posting = (path.clone(), index);
            if add {
                postings.entry(key).or_default().insert(posting);
            } else if let std::collections::hash_map::Entry::Occupied(mut entry) =
                postings.entry(key)
            {
                entry.get_mut().remove(&posting);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
    }
}

fn eligible(project: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(project).unwrap_or(path);
    !crate::cards::is_ignored(project, path)
        && !rel.starts_with("threads")
        && !rel.components().any(|part| {
            let name = part.as_os_str().to_string_lossy();
            name == "build" || name.starts_with('.') && name.len() > 1
        })
}

fn discover(project: &Path, root: &Path, found: &mut FileSet) {
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| eligible(project, entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if matches!(
            entry.path().extension().and_then(|ext| ext.to_str()),
            Some("tex" | "lean")
        ) || matches!(
            entry.file_name().to_str(),
            Some("lakefile.toml" | "lakefile.lean")
        ) {
            found.insert(entry.into_path());
        }
    }
}

fn inferred_deck(project: &Path, path: &Path, is_lean: bool) -> String {
    let relative_path = path.strip_prefix(project).unwrap_or(path);
    if is_lean {
        let mut components = relative_path.components();
        if components.next().is_some_and(|p| p.as_os_str() == "lean") {
            return relative(
                project,
                &project.join(components.next().map_or_else(
                    || PathBuf::from("lean"),
                    |name| Path::new("lean").join(name),
                )),
            );
        }
    }
    relative(project, path.parent().unwrap_or(path))
}

impl Symbols {
    pub(super) fn refresh(
        &mut self,
        project: &Path,
        decks: &[DeckInput],
        cache: &super::Cache,
        paths: Option<&FileSet>,
        refresh_external: bool,
    ) {
        let decks_map: BTreeMap<_, _> = decks
            .iter()
            .map(|deck| (deck.root.clone(), relative(project, &deck.root)))
            .collect();
        let aliases = decks
            .iter()
            .flat_map(|deck| {
                deck.external.iter().map(|(alias, target)| {
                    (
                        (relative(project, &deck.root), alias.clone()),
                        relative(project, target),
                    )
                })
            })
            .collect();
        let mut cards = BTreeMap::new();
        let mut preambles = FileSet::new();
        for deck in decks {
            let name = &decks_map[&deck.root];
            let preamble = deck
                .cfg
                .meta
                .preamble
                .as_deref()
                .or(deck.cfg.macros_file.as_deref());
            if let Some(path) = preamble.and_then(|path| deck.root.join(path).canonicalize().ok()) {
                preambles.insert(path);
            }
            for path in &deck.cards {
                let Some(entry) = cache.cards.get(path) else {
                    continue;
                };
                let Some(card) = &entry.data else { continue };
                for stamp in &entry.stamps {
                    cards
                        .entry(stamp.path.clone())
                        .or_insert_with(|| (name.clone(), card.id.clone()));
                }
            }
        }
        let mut pending = FileSet::new();
        let full = paths.is_none() || decks_map != self.decks;
        let changed_layout = cards != self.cards || preambles != self.preambles;
        if changed_layout || full {
            for path in self
                .cards
                .keys()
                .chain(cards.keys())
                .chain(self.preambles.iter())
                .chain(preambles.iter())
            {
                if full
                    || self.cards.get(path) != cards.get(path)
                    || self.preambles.contains(path) != preambles.contains(path)
                {
                    pending.insert(path.clone());
                }
            }
        }
        self.decks = decks_map;
        self.aliases = aliases;
        let mut topology_changed = full || changed_layout;
        if full {
            discover(project, project, &mut pending);
            for root in self.decks.keys().filter(|root| !root.starts_with(project)) {
                discover(project, root, &mut pending);
            }
            pending.extend(self.files.keys().cloned());
        } else if let Some(paths) = paths {
            for path in paths.iter().filter(|path| eligible(project, path)) {
                pending.insert(path.clone());
                if path.is_dir() {
                    discover(project, path, &mut pending);
                } else if !path.exists() {
                    pending.extend(
                        self.files
                            .range(path.clone()..)
                            .take_while(|(child, _)| child.starts_with(path))
                            .map(|(p, _)| p.clone()),
                    );
                }
            }
        }
        if refresh_external && !full {
            for root in self.decks.keys().filter(|root| !root.starts_with(project)) {
                discover(project, root, &mut pending);
            }
            pending.extend(
                self.files
                    .keys()
                    .filter(|path| !path.starts_with(project))
                    .cloned(),
            );
        }
        // Package markers and source files share the same discovery and watcher.
        for path in &pending {
            if matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("lakefile.toml" | "lakefile.lean")
            ) {
                let root = path.parent().unwrap();
                if path.is_file() {
                    let name = relative(project, root);
                    topology_changed |= self.packages.insert(root.to_path_buf(), name).is_none();
                } else if !root.join("lakefile.toml").is_file()
                    && !root.join("lakefile.lean").is_file()
                {
                    topology_changed |= self.packages.remove(root).is_some();
                }
            }
        }
        if topology_changed {
            pending.extend(self.files.keys().cloned());
        }
        let mut tex_changed = topology_changed;
        for path in pending {
            let owner = cards.get(&path);
            let extension = path.extension().and_then(|ext| ext.to_str());
            let is_lean = extension == Some("lean")
                && path.file_name().is_none_or(|name| name != "lakefile.lean");
            let configured_deck = path.ancestors().find_map(|p| self.decks.get(p));
            let package = is_lean
                .then(|| path.ancestors().find_map(|p| self.packages.get(p)))
                .flatten();
            let recognized =
                owner.is_some() || preambles.contains(&path) || extension == Some("tex") || is_lean;
            let stamped = recognized.then(|| stamp(path.clone())).flatten();
            let deck = owner
                .map(|(deck, _)| deck)
                .or(package)
                .or(configured_deck)
                .cloned()
                .unwrap_or_else(|| inferred_deck(project, &path, is_lean));
            let card = owner.map(|(_, card)| card.clone());
            let preamble_changed = self.preambles.contains(&path) != preambles.contains(&path);
            let old = self.files.get(&path);
            if !preamble_changed
                && old.is_some_and(|cached| {
                    cached.stamps.first() == stamped.as_ref()
                        && cached
                            .data
                            .as_ref()
                            .is_none_or(|source| source.deck == deck && source.card == card)
                })
            {
                continue;
            }
            let previous = self.files.remove(&path).and_then(|cached| cached.data);
            if let Some(old) = &previous {
                edit_postings(&mut self.definitions, &path, &old.extracted.defs, false);
                edit_postings(&mut self.uses, &path, &old.extracted.refs, false);
                if let Some(module) = &old.module {
                    if let std::collections::btree_map::Entry::Occupied(mut entry) =
                        self.modules.entry(module.clone())
                    {
                        entry.get_mut().remove(&path);
                        if entry.get().is_empty() {
                            entry.remove();
                        }
                    }
                }
            }
            let Some(stamp) = stamped else {
                tex_changed |= extension == Some("tex") && previous.is_some();
                continue;
            };
            let source = fs::read_to_string(&path).ok().and_then(|raw| {
                let mut extracted = extract::source(&path, &raw)?;
                if extension == Some("tex") && card.is_some() {
                    extracted.text = fuzukue_cards::tex_prose_mask(&extracted.text, true);
                }
                if card.is_some() {
                    extracted.text.make_ascii_lowercase();
                } else if !is_lean {
                    extracted.text.clear();
                    extracted.text.shrink_to_fit();
                }
                for mark in &mut extracted.defs {
                    if mark.qualified.is_none() {
                        mark.qualified = Some(format!("{deck}#{}", mark.name));
                    }
                }
                let digest = Sha256::digest(raw.as_bytes()).into();
                let lines = std::iter::once(0)
                    .chain(raw.match_indices('\n').map(|(i, _)| i + 1))
                    .collect();
                let module = is_lean.then(|| {
                    let root = path
                        .ancestors()
                        .find(|p| self.packages.contains_key(*p))
                        .unwrap_or_else(|| path.parent().unwrap());
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .with_extension("")
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(".")
                });
                Some(Source {
                    raw,
                    extracted,
                    digest,
                    lines,
                    deck,
                    card,
                    is_lean,
                    module,
                })
            });
            if extension == Some("tex") {
                let structure = |source: &Source| {
                    (
                        source.extracted.tex_entry,
                        source.extracted.includes.clone(),
                    )
                };
                tex_changed |= previous.as_ref().map(structure) != source.as_ref().map(structure);
            }
            if let Some(source) = &source {
                if let Some(module) = &source.module {
                    self.modules
                        .entry(module.clone())
                        .or_default()
                        .insert(path.clone());
                }
                edit_postings(&mut self.definitions, &path, &source.extracted.defs, true);
                edit_postings(&mut self.uses, &path, &source.extracted.refs, true);
            }
            self.files.insert(
                path,
                super::Cached {
                    stamps: vec![stamp],
                    data: source,
                },
            );
        }
        self.cards = cards;
        self.preambles = preambles;
        // TeX document membership can change when an input is edited or removed.
        if tex_changed {
            self.rebuild_tex_roots();
        }
    }

    fn rebuild_tex_roots(&mut self) {
        self.tex_roots.clear();
        for (file, entry) in &self.files {
            if !entry
                .data
                .as_ref()
                .is_some_and(|source| source.extracted.tex_entry)
            {
                continue;
            }
            let root = file.parent().unwrap();
            let mut pending = vec![file.clone()];
            let mut visited = FileSet::new();
            while let Some(path) = pending.pop() {
                if !visited.insert(path.clone()) {
                    continue;
                }
                let Some(source) = self.files.get(&path).and_then(|entry| entry.data.as_ref())
                else {
                    continue;
                };
                self.tex_roots
                    .entry(path.clone())
                    .or_default()
                    .insert(file.clone());
                for name in &source.extracted.includes {
                    for parent in [root, path.parent().unwrap()] {
                        let mut target = parent.join(name);
                        if target.extension().is_none() {
                            target.set_extension("tex");
                        }
                        if let Ok(path) = target.canonicalize() {
                            if self.files.contains_key(&path) {
                                pending.push(path);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    fn source(&self, path: &Path) -> Option<&Source> {
        self.files.get(path)?.data.as_ref()
    }

    fn exact(&self, name: &str, kind: Option<SymbolKind>) -> Vec<Occurrence<'_>> {
        const KINDS: [SymbolKind; 5] = [
            SymbolKind::Card,
            SymbolKind::Term,
            SymbolKind::Label,
            SymbolKind::Macro,
            SymbolKind::Lean,
        ];
        kind.as_slice()
            .iter()
            .chain(KINDS.iter().filter(|_| kind.is_none()))
            .filter_map(|kind| self.definitions.get(&(*kind, key(*kind, name))))
            .flatten()
            .filter_map(|(path, index)| {
                self.source(path)
                    .map(|source| (path.as_ref(), source, &source.extracted.defs[*index]))
            })
            .collect()
    }

    fn resolved(
        &self,
        deck: &str,
        file: Option<&Path>,
        name: &str,
        kind: SymbolKind,
    ) -> Vec<Occurrence<'_>> {
        let mut name = name;
        let mut requested = None;
        if kind == SymbolKind::Card {
            if let Some((alias, id)) = name.rsplit_once('#').or_else(|| name.split_once(':')) {
                let namespace = alias.split_once(':').map_or(alias, |(_, path)| path);
                requested = self
                    .aliases
                    .get(&(deck.to_owned(), namespace.to_owned()))
                    .map(String::as_str)
                    .or_else(|| {
                        self.decks
                            .values()
                            .find(|&key| key == namespace)
                            .map(String::as_str)
                    });
                if requested.is_none() {
                    return Vec::new();
                }
                name = id;
            }
        }
        let mut hits = self.exact(name, Some(kind));
        if kind == SymbolKind::Label && hits.is_empty() {
            if let Some(id) = name.strip_prefix("card:") {
                return self.resolved(deck, file, id, SymbolKind::Card);
            }
        }
        if let Some(target) = requested {
            hits.retain(|(_, source, _)| source.deck == target);
        } else if matches!(kind, SymbolKind::Label | SymbolKind::Macro)
            && file.and_then(|p| self.tex_roots.get(p)).is_some()
        {
            let roots = &self.tex_roots[file.unwrap()];
            hits.retain(|(path, source, _)| {
                self.tex_roots
                    .get(*path)
                    .is_some_and(|set| !set.is_disjoint(roots))
                    || source.deck == deck
                        && (source.card.is_some() || self.preambles.contains(*path))
            });
        } else if matches!(
            kind,
            SymbolKind::Card | SymbolKind::Label | SymbolKind::Macro
        ) && hits.iter().any(|(_, source, _)| source.deck == deck)
        {
            hits.retain(|(_, source, _)| source.deck == deck);
        }
        hits
    }

    fn id(source: &Source, mark: &Mark) -> String {
        use std::fmt::Write;
        let mut id = format!("{}:{}:", mark.origin, mark.range.start);
        for byte in &source.digest[..16] {
            write!(id, "{byte:02x}").unwrap();
        }
        id
    }

    fn group(&self, project: &Path, hits: Vec<Occurrence<'_>>) -> Vec<SymbolHit> {
        let mut groups: BTreeMap<String, SymbolHit> = BTreeMap::new();
        for (path, source, mark) in hits {
            let id = Self::id(source, mark);
            let mut location = source.location(project, path, &mark.range);
            if let Some(hit) = groups.get_mut(&id) {
                location.snippet.clear();
                hit.alternates.push(location);
            } else {
                groups.insert(
                    id.clone(),
                    SymbolHit {
                        id,
                        kind: mark.kind,
                        name: mark.name.clone(),
                        qualified: mark.qualified.clone().unwrap_or_else(|| mark.name.clone()),
                        origin: mark.origin.into(),
                        location,
                        alternates: Vec::new(),
                    },
                );
            }
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.location.file.cmp(&b.location.file))
                .then(a.location.start.cmp(&b.location.start))
        });
        groups
    }

    pub(super) fn definition(
        &self,
        project: &Path,
        name: &str,
        kind: Option<SymbolKind>,
    ) -> SymbolResults {
        limited(self.group(project, self.exact(name, kind)))
    }

    pub(super) fn search(
        &self,
        project: &Path,
        query: &str,
        kind: Option<SymbolKind>,
        deck: Option<&str>,
    ) -> SymbolResults {
        let query = query.to_ascii_lowercase();
        let mut hits = Vec::new();
        for (path, entry) in &self.files {
            let Some(source) = &entry.data else { continue };
            if deck.is_some_and(|deck| deck != source.deck) {
                continue;
            }
            hits.extend(
                source
                    .extracted
                    .defs
                    .iter()
                    .filter(|mark| kind.is_none_or(|kind| kind == mark.kind))
                    .filter(|mark| {
                        mark.qualified
                            .as_ref()
                            .unwrap_or(&mark.name)
                            .to_ascii_lowercase()
                            .contains(&query)
                    })
                    .map(|mark| (path.as_path(), source, mark)),
            );
        }
        limited(self.group(project, hits))
    }

    pub(super) fn references(
        &self,
        project: &Path,
        name: Option<&str>,
        id: Option<&str>,
        limit: usize,
    ) -> SymbolResults {
        let targets = self.reference_targets(name, id);
        self.find_references(project, &targets, None, None, limit)
    }

    pub(super) fn card_references(
        &self,
        project: &Path,
        deck: &str,
        card: Option<&str>,
        name: &str,
        kind: SymbolKind,
        limit: usize,
    ) -> Vec<SymbolReference> {
        let deck = wire_deck(deck);
        let Some(card) = card else {
            return self.find_references(project, &[], Some((kind, name)), Some(deck), limit).references;
        };
        let ids: BTreeSet<_> = self.resolved(deck, None, name, kind).into_iter()
            .filter(|(_, source, _)| source.deck == deck && source.card.as_deref() == Some(card))
            .map(|(_, source, mark)| Self::id(source, mark))
            .collect();
        ids.into_iter().flat_map(|id| {
            let targets = self.reference_targets(None, Some(&id));
            self.find_references(project, &targets, None, Some(deck), limit).references
        }).collect()
    }

    fn reference_targets(&self, name: Option<&str>, id: Option<&str>) -> Vec<Occurrence<'_>> {
        if let Some(id) = id {
            let origin_start = id.split_once(':').map(|(origin, tail)| {
                (
                    origin,
                    tail.split(':')
                        .next()
                        .and_then(|start| start.parse::<usize>().ok()),
                )
            });
            self.files
                .iter()
                .filter_map(|(path, entry)| Some((path.as_path(), entry.data.as_ref()?)))
                .flat_map(|(path, source)| {
                    source.extracted.defs.iter().filter_map(move |mark| {
                        (origin_start.is_some_and(|(origin, start)| {
                            mark.origin == origin && Some(mark.range.start) == start
                        }) && Self::id(source, mark) == id)
                            .then_some((path, source, mark))
                    })
                })
                .collect()
        } else {
            self.exact(name.unwrap_or_default(), None)
        }
    }

    fn find_references(
        &self,
        project: &Path,
        targets: &[Occurrence<'_>],
        removed: Option<(SymbolKind, &str)>,
        deck: Option<&str>,
        limit: usize,
    ) -> SymbolResults {
        let target_ids: BTreeSet<_> = targets
            .iter()
            .map(|(_, source, mark)| Self::id(source, mark))
            .collect();
        let targets_match = |hits: Vec<Occurrence<'_>>, unique: bool| {
            let mut ids = hits.iter().map(|(_, source, mark)| Self::id(source, mark));
            if unique {
                let Some(first) = ids.next() else {
                    return false;
                };
                target_ids.contains(&first) && ids.all(|id| id == first)
            } else {
                ids.any(|id| target_ids.contains(&id))
            }
        };
        let mut names = BTreeSet::new();
        let mut term_names = BTreeSet::new();
        let mut lean_names = BTreeSet::new();
        for (_, _, mark) in targets {
            names.insert(mark.name.as_str());
            if let Some(full) = &mark.qualified {
                names.insert(full);
            }
            if mark.kind == SymbolKind::Term {
                term_names.insert(key(mark.kind, &mark.name));
            }
            if mark.kind == SymbolKind::Lean {
                lean_names.insert(mark.name.as_str());
            }
        }
        let mut kinds: Vec<_> = targets.iter().map(|(_, _, mark)| mark.kind).collect();
        // The edited card no longer supplies this definition. Look for its
        // spelling in typed references and in the indexed card prose.
        if let Some((kind, name)) = removed {
            kinds.push(kind);
            names.insert(name);
            if kind == SymbolKind::Lean {
                names.insert(extract::short_name(name));
            }
            if kind == SymbolKind::Term {
                term_names.insert(key(kind, name));
            }
        }
        let mut groups: BTreeMap<([u8; 32], usize, usize), SymbolReference> = BTreeMap::new();
        let mut add = |path: &Path, source: &Source, range: Range<usize>| {
            // The consistency reviewer can only draft comments on cards in its own deck.
            if deck.is_some_and(|deck| source.deck != deck || source.card.is_none()) {
                return;
            }
            let mut location = source.location(project, path, &range);
            let group = (source.digest, range.start, range.end);
            if let Some(existing) = groups.get_mut(&group) {
                if existing.location.file != location.file
                    && !existing
                        .alternates
                        .iter()
                        .any(|alt| alt.file == location.file)
                {
                    location.snippet.clear();
                    existing.alternates.push(location);
                }
            } else {
                groups.insert(
                    group,
                    SymbolReference {
                        location,
                        alternates: Vec::new(),
                    },
                );
            }
        };
        // Explicit references can use a deck alias or a conventional card: label.
        for ((kind, spelling), postings) in &self.uses {
            if !kinds.contains(kind)
                && !(*kind == SymbolKind::Label && kinds.contains(&SymbolKind::Card))
            {
                continue;
            }
            let tail = if removed.is_none() && (*kind == SymbolKind::Card || spelling.starts_with("card:")) {
                spelling.rsplit([':', '#']).next().unwrap_or(spelling)
            } else {
                spelling
            };
            if !names.contains(spelling.as_str()) && !names.contains(tail) {
                continue;
            }
            for (path, index) in postings {
                let Some(source) = self.source(path) else {
                    continue;
                };
                let mark = &source.extracted.refs[*index];
                if removed.is_some() || targets_match(
                    self.resolved(&source.deck, Some(path), &mark.name, mark.kind),
                    false,
                ) {
                    add(path, source, mark.range.clone());
                }
            }
        }
        for (path, entry) in &self.files {
            let Some(source) = &entry.data else { continue };
            let text = &source.extracted.text;
            if source.is_lean {
                if !lean_names.iter().any(|name| text.contains(name)) {
                    continue;
                }
                let visible = self.lean_imports(path);
                for scope in &source.extracted.lean {
                    if !lean_names
                        .iter()
                        .any(|name| text[scope.range.clone()].contains(name))
                    {
                        continue;
                    }
                    for mark in extract::lean_uses(text, scope) {
                        if lean_names.contains(extract::short_name(&mark.name))
                            && targets_match(self.resolve_lean(&visible, &mark.name, scope), true)
                        {
                            add(path, source, mark.range);
                        }
                    }
                }
            } else if source.card.is_some() {
                for term in &term_names {
                    for range in extract::terms(text, term) {
                        // A wiki target can spell the old term and still refer
                        // to the card ID, which did not change with the term.
                        if removed.is_some() && source.extracted.refs.iter().any(|mark| {
                            mark.kind == SymbolKind::Card
                                && mark.range.start <= range.start && range.end <= mark.range.end
                        }) {
                            continue;
                        }
                        add(path, source, range);
                    }
                }
            }
        }
        let mut references: Vec<_> = groups.into_values().collect();
        references.sort_by(|a, b| {
            a.location
                .deck
                .cmp(&b.location.deck)
                .then(a.location.file.cmp(&b.location.file))
                .then(a.location.start.cmp(&b.location.start))
        });
        let more = references.len() > limit;
        references.truncate(limit);
        SymbolResults {
            references,
            more,
            ..SymbolResults::default()
        }
    }

    fn lean_imports<'a>(&'a self, path: &'a Path) -> BTreeSet<&'a Path> {
        let mut visible = BTreeSet::new();
        let mut pending = vec![path];
        while let Some(path) = pending.pop() {
            if !visible.insert(path) {
                continue;
            }
            let Some(source) = self.source(path) else {
                continue;
            };
            for import in &source.extracted.imports {
                let Some(paths) = self.modules.get(import) else {
                    continue;
                };
                let mut same_package = paths
                    .iter()
                    .filter(|p| self.source(p).is_some_and(|src| src.deck == source.deck));
                if let Some(local) = same_package.next() {
                    if same_package.next().is_none() {
                        pending.push(local.as_path());
                    }
                } else if paths.len() == 1 {
                    pending.push(paths.first().unwrap().as_path());
                }
            }
        }
        visible
    }

    fn resolve_lean(
        &self,
        visible: &BTreeSet<&Path>,
        name: &str,
        scope: &extract::LeanScope,
    ) -> Vec<Occurrence<'_>> {
        let qualified = |name: &str| {
            let mut hits = self.exact(name, Some(SymbolKind::Lean));
            hits.retain(|(path, src, mark)| {
                src.is_lean && visible.contains(path) && mark.qualified.as_deref() == Some(name)
            });
            hits
        };
        if let Some(name) = name.strip_prefix("_root_.") {
            return qualified(name);
        }
        let mut namespace = scope.namespace.as_str();
        while !namespace.is_empty() {
            let hits = qualified(&format!("{namespace}.{name}"));
            if !hits.is_empty() {
                return hits;
            }
            namespace = namespace.rsplit_once('.').map_or("", |(parent, _)| parent);
        }
        let hits = qualified(name);
        if !hits.is_empty() {
            return hits;
        }
        scope
            .opens
            .iter()
            .flat_map(|open| qualified(&format!("{open}.{name}")))
            .collect()
    }

    fn lean_dependencies(&self, project: &Path, path: &str, line: usize) -> SymbolResults {
        let Ok(path) = crate::safe_join(project, path) else { return SymbolResults::default() };
        let Some(src) = self.source(&path).filter(|src| src.is_lean) else { return SymbolResults::default() };
        let Some(def) = src.extracted.defs.iter().find(|mark| {
            src.lines.partition_point(|&start| start <= mark.range.start) == line
        }) else { return SymbolResults::default() };
        let Some(scope) = src.extracted.lean.iter().find(|scope| scope.range.contains(&def.range.start))
            else { return SymbolResults::default() };
        let visible = self.lean_imports(&path);
        let mut seen = BTreeSet::new();
        let definitions = limited(self.group(project, extract::lean_uses(&src.extracted.text, scope)
            .flat_map(|mark| self.resolve_lean(&visible, &mark.name, scope))
            .filter(|(path, _, mark)| seen.insert((*path, mark.range.start)))
            .collect()));
        let own = self.group(project, vec![(&path, src, def)]);
        let mut result = own.first().map(|hit| self.references(project, None, Some(&hit.id), RESULT_LIMIT))
            .unwrap_or_default();
        result.definitions = definitions.definitions;
        result.more |= definitions.more;
        result
    }

    pub(super) fn lookup(&self, project: &Path, lookup: SymbolLookup) -> SymbolResults {
        match lookup {
            SymbolLookup::LeanDependencies { path, line } => self.lean_dependencies(project, &path, line),
            SymbolLookup::References { id } => {
                self.references(project, None, Some(&id), RESULT_LIMIT)
            }
            SymbolLookup::Name { deck, name, kind } => {
                limited(self.group(project, self.resolved(wire_deck(&deck), None, &name, kind)))
            }
            SymbolLookup::At {
                deck,
                card,
                text,
                start,
                end,
            } => {
                let deck = wire_deck(&deck);
                let Some(clicked) = text.get(start..end).filter(|s| !s.is_empty()) else {
                    return SymbolResults::default();
                };
                let mut candidates = Vec::new();
                let parsed = extract::tex(&text);
                let file = self.cards.iter().find_map(|(path, (owner, id))| {
                    (owner == deck
                        && id == &card
                        && self
                            .source(path)
                            .is_some_and(|source| source.raw.contains(&text)))
                    .then_some(path.as_path())
                });
                let overlaps = |range: &Range<usize>| start < range.end && range.start < end;
                for mark in extract::wikis(&text)
                    .chain(parsed.refs.into_iter())
                    .filter(|mark| overlaps(&mark.range))
                {
                    candidates.extend(self.resolved(deck, file, &mark.name, mark.kind));
                }
                if candidates.is_empty() {
                    let folded = extract::term_key(&text);
                    for ((kind, spelling), _) in &self.definitions {
                        if *kind == SymbolKind::Term
                            && extract::terms(&folded, spelling).any(|range| overlaps(&range))
                        {
                            candidates.extend(self.resolved(deck, file, spelling, *kind));
                        }
                    }
                    if candidates.is_empty() {
                        candidates.extend(self.resolved(deck, file, clicked, SymbolKind::Lean));
                    }
                }
                candidates.sort_by(|a, b| a.0.cmp(b.0).then(a.2.range.start.cmp(&b.2.range.start)));
                candidates
                    .dedup_by(|a, b| a.0 == b.0 && a.2.range == b.2.range && a.2.kind == b.2.kind);
                limited(self.group(project, candidates))
            }
        }
    }
}

fn wire_deck(deck: &str) -> &str {
    if deck.is_empty() {
        "."
    } else {
        deck
    }
}

fn limited(mut definitions: Vec<SymbolHit>) -> SymbolResults {
    let more = definitions.len() > RESULT_LIMIT;
    definitions.truncate(RESULT_LIMIT);
    SymbolResults {
        definitions,
        more,
        ..SymbolResults::default()
    }
}
