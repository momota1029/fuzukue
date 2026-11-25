//! Workspace card graph. The disk cache stores per-file inputs; the in-memory
//! index resolves their edges together so edits also update incoming links.

mod dependencies;
mod index;
mod symbols;
pub(crate) mod mcp;
mod card_tools;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use fuzukue_cards::{Card, DeckConfig, Link};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

pub(crate) use self::dependencies::requires_cycles;
use self::index::Index;
pub(crate) use self::symbols::{card_symbols, review_symbols};

pub(crate) fn prompt() -> String {
    let graph = harness_text!(
        "\n\nStart with fuzukue graph_overview to understand a project or book. Follow dependencies and references with graph_neighbors or graph_path before opening many files. To edit a book, use graph_overview(depth=1), then graph_overview(depth=2, chapter=<chapter key>), then card_read and card_edit. Use card_read to read only the parts and lines you need, and card_edit to replace only the affected section. For book edits, use these tools instead of reading raw .tex files. Delegate cross-chapter work with a narrow list of card IDs. Across decks, use definition to find definitions, references to find uses, and symbols to search by name.",
        "\n\nプロジェクトや本の全体像を掴むときは fuzukue の graph_overview から始める。依存や参照は、多数のファイルを開く前に graph_neighbors や graph_path で辿る。 本の編集では graph_overview(depth=1) → graph_overview(depth=2, chapter=章のkey) → card_read → card_edit の順で使う。生の .tex ファイルを直接読まず、card_read で必要な部位と行だけを読み、card_edit で変更箇所だけを置き換える。章をまたぐ作業は、対象のカード ID を絞って作業役へ渡す。デッキをまたぐ定義は definition、使用箇所は references、名前の検索は symbols を使う。",
    );
    format!("{graph}{}{}", crate::library::prompt(), crate::memos::prompt())
}

const CACHE_VERSION: u32 = 2;

#[derive(Default)]
pub(crate) struct WorkspaceGraph {
    dirty: AtomicBool,
    data: Mutex<Option<Graph>>,
    pending: Mutex<Option<FileSet>>,
}

impl WorkspaceGraph {
    pub(crate) fn invalidate(&self) {
        let mut pending = self.pending.lock().unwrap();
        *pending = None;
        self.dirty.store(true, Ordering::Release);
    }

    pub(crate) fn invalidate_paths(&self, paths: &[PathBuf]) {
        let mut pending = self.pending.lock().unwrap();
        if let Some(pending) = pending.as_mut() {
            pending.extend(paths.iter().cloned());
        }
        self.dirty.store(true, Ordering::Release);
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub(crate) fn refresh(&self, project: &Path, state: &Path) {
        self.with_graph(project, state, true, |_| ());
    }

    fn with_graph<T>(&self, project: &Path, state: &Path, refresh_external: bool, run: impl FnOnce(&Graph) -> T) -> T {
        let mut data = self.data.lock().unwrap();
        // Clear before reading files so an event received during the build
        // remains pending for the next refresh. Out-of-workspace inputs are
        // checked here because the workspace watcher cannot see those files.
        let (dirty, paths) = {
            let mut pending = self.pending.lock().unwrap();
            let dirty = self.dirty.swap(false, Ordering::AcqRel);
            (dirty, std::mem::replace(&mut *pending, Some(FileSet::new())))
        };
        match data.as_mut() {
            Some(graph) if dirty || refresh_external && graph.has_unwatched_inputs => {
                graph.refresh(project, state, dirty, paths.as_ref())
            }
            Some(_) => {}
            None => *data = Some(Graph::open(project, state)),
        }
        run(data.as_ref().unwrap())
    }

    pub(crate) fn route_scores(
        &self, project: &Path, state: &Path, texts: &[String], paths: &[String], search: bool,
    ) -> Vec<(fuzukue_proto::DeckRoute, usize)> {
        self.with_graph(project, state, false, |graph| {
            graph.index.route_scores(&graph.cache, project, texts, paths, search)
        })
    }

    pub(crate) fn deck_for_file(
        &self, project: &Path, state: &Path, file: &Path,
    ) -> Option<fuzukue_proto::FileDeck> {
        self.with_graph(project, state, false, |graph| {
            let project = project.canonicalize().ok()?;
            let mut fallback = None;
            for deck in &graph.inputs.decks {
                let Ok(path) = deck.root.strip_prefix(&project) else { continue };
                let Some(path) = path.to_str() else { continue };
                for card in &deck.cards {
                    let cached = &graph.cache.cards[card];
                    if let Some(data) = &cached.data {
                        if cached.stamps.iter().any(|stamp| stamp.path == file) {
                            return Some(fuzukue_proto::FileDeck {
                                path: path.into(), card: Some(data.id.clone()),
                            });
                        }
                    }
                }
                let configured = [
                    deck.cfg.meta.preamble.as_deref(),
                    deck.cfg.macros_file.as_deref(),
                    deck.cfg.meta.bibliography.as_deref(),
                ].into_iter().flatten().any(|source| deck.root.join(source) == file);
                if configured || deck.bib.iter().any(|source| source == file) {
                    fallback = Some(fuzukue_proto::FileDeck { path: path.into(), card: None });
                }
            }
            fallback
        })
    }

    pub(crate) async fn symbol_lookup(
        app: std::sync::Arc<crate::Workspace>,
        lookup: fuzukue_proto::SymbolLookup,
    ) -> fuzukue_proto::SymbolResults {
        tokio::task::spawn_blocking(move || {
            app.graph.with_graph(&app.project_dir, &app.state_dir, true, |graph| {
                graph.symbols(&app.project_dir).lookup(&app.project_dir, lookup)
            })
        }).await.unwrap_or_default()
    }

    /// `card` identifies the current definition's owner. For a removed name,
    /// pass `None` to search uses by spelling within the deck.
    pub(crate) async fn card_references(
        app: std::sync::Arc<crate::Workspace>,
        deck: String,
        card: Option<String>,
        (kind, name): (fuzukue_proto::SymbolKind, String),
        limit: usize,
    ) -> Vec<fuzukue_proto::SymbolReference> {
        tokio::task::spawn_blocking(move || {
            app.graph.with_graph(&app.project_dir, &app.state_dir, true, |graph| {
                graph.symbols(&app.project_dir).card_references(&app.project_dir, &deck, card.as_deref(), &name, kind, limit)
            })
        }).await.unwrap_or_default()
    }

    pub(crate) async fn call(
        app: std::sync::Arc<crate::Workspace>,
        tool: mcp::Tool,
        args: Value,
    ) -> anyhow::Result<Value> {
        tokio::task::spawn_blocking(move || {
            let mut result = app.graph
                .with_graph(&app.project_dir, &app.state_dir, true, |graph| {
                    let result = tool.call(graph, args, &app.project_dir);
                    if tool.writes() && result.is_ok() {
                        app.graph.invalidate();
                    }
                    result
                })?;
            if tool.writes() {
                if let Some(path) = result.as_object_mut().and_then(|result| result.remove("path"))
                    .and_then(|path| path.as_str().map(PathBuf::from))
                {
                    crate::extensions::card_saved(&app, &path);
                }
            }
            Ok(result)
        })
        .await?
    }
}

#[derive(Default)]
struct Graph {
    cache: Cache,
    index: Index,
    has_unwatched_inputs: bool,
    inputs: Inputs,
    symbols: std::cell::OnceCell<symbols::Symbols>,
}

impl Graph {
    fn open(project: &Path, state: &Path) -> Self {
        let cache = fs::File::open(state.join("card-graph.json"))
            .ok()
            .and_then(|file| {
                serde_json::from_reader::<_, Cache>(std::io::BufReader::new(file)).ok()
            })
            .filter(|cache| cache.version == CACHE_VERSION && cache.project == project)
            .unwrap_or_default();
        let mut graph = Self {
            cache,
            ..Self::default()
        };
        graph.refresh(project, state, true, None);
        graph
    }

    fn symbols(&self, project: &Path) -> &symbols::Symbols {
        self.symbols.get_or_init(|| {
            let mut symbols = symbols::Symbols::default();
            symbols.refresh(project, &self.inputs.decks, &self.cache, None, false);
            symbols
        })
    }

    fn refresh(&mut self, project: &Path, state: &Path, rebuild: bool, paths: Option<&FileSet>) {
        // Lean and standalone TeX changes have no effect on the card graph.
        let only_symbol_sources = rebuild && !self.has_unwatched_inputs && paths.is_some_and(|paths| {
            !paths.is_empty() && paths.iter().all(|path| !path.is_dir() && match path.extension().and_then(|ext| ext.to_str()) {
                Some("lean") => true,
                Some("tex") => path.parent().is_some_and(|parent| !self.cache.cards.contains_key(&parent.join("card.md"))),
                _ => false,
            })
        });
        if !only_symbol_sources {
            let inputs = self.cache.update(project);
            self.has_unwatched_inputs = inputs.has_unwatched_inputs;
            if rebuild || inputs.changed || self.inputs.deck_stamps != inputs.deck_stamps {
                self.index = Index::build(project, &self.cache, &inputs);
            }
            if inputs.changed {
                if let Err(error) = self.cache.save(state) {
                    tracing::warn!("card graph cache: {error:#}");
                }
            }
            self.inputs = inputs;
        }
        if let Some(symbols) = self.symbols.get_mut() {
            symbols.refresh(project, &self.inputs.decks, &self.cache, paths, self.has_unwatched_inputs);
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    project: PathBuf,
    cards: Files<CardData>,
    structures: Files<Structure>,
    bibliographies: Files<BTreeSet<String>>,
    references: Files<Citation>,
}

type Files<T> = BTreeMap<PathBuf, Cached<T>>;
type FileSet = BTreeSet<PathBuf>;

#[derive(Serialize, Deserialize)]
struct Cached<T> {
    stamps: Vec<Stamp>,
    data: Option<T>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

fn stamp(path: PathBuf) -> Option<Stamp> {
    let meta = fs::metadata(&path).ok()?;
    meta.is_file().then_some(())?;
    Some(Stamp {
        path,
        modified: meta.modified().ok()?,
        len: meta.len(),
    })
}

fn update_file<T>(
    files: &mut Files<T>,
    seen: &mut FileSet,
    path: &Path,
    stamps: Vec<Stamp>,
    changed: &mut bool,
    read: impl FnOnce() -> Option<T>,
) {
    if !seen.insert(path.to_path_buf()) || files.get(path).is_some_and(|old| old.stamps == stamps) {
        return;
    }
    files.insert(
        path.to_path_buf(),
        Cached {
            stamps,
            data: read(),
        },
    );
    *changed = true;
}

fn remove_stale<T>(files: &mut Files<T>, seen: &FileSet, changed: &mut bool) {
    let before = files.len();
    files.retain(|path, _| seen.contains(path));
    *changed |= before != files.len();
}

#[derive(Default)]
struct Inputs {
    decks: Vec<DeckInput>,
    deck_stamps: Vec<(PathBuf, Vec<Stamp>)>,
    references: BTreeMap<String, PathBuf>,
    has_unwatched_inputs: bool,
    changed: bool,
    #[cfg(test)]
    card_reads: usize,
}

struct DeckInput {
    root: PathBuf,
    cfg: DeckConfig,
    external: BTreeMap<String, PathBuf>,
    structure: PathBuf,
    bib: Vec<PathBuf>,
    cards: FileSet,
}

impl Cache {
    fn update(&mut self, project: &Path) -> Inputs {
        let mut inputs = Inputs::default();
        inputs.changed = self.version != CACHE_VERSION;
        self.version = CACHE_VERSION;
        self.project = project.to_path_buf();
        let mut roots: BTreeSet<_> = fuzukue_cards::find_decks(project).into_iter().collect();
        if project.join("cards").is_dir() {
            roots.insert(project.to_path_buf());
        }
        let mut visited = BTreeSet::new();
        let (mut cards, mut structures, mut bibliographies, mut references) = (
            FileSet::new(),
            FileSet::new(),
            FileSet::new(),
            FileSet::new(),
        );
        while let Some(root) = roots.pop_first() {
            if !visited.insert(root.clone()) {
                continue;
            }
            let cfg = if root.starts_with(project) {
                DeckConfig::read_in_workspace(&root, project)
            } else {
                DeckConfig::read(&root)
            };
            let Ok(cfg) = cfg else {
                continue;
            };
            let stamps = ["deck.toml", "cards.toml", "harness.toml"]
                .into_iter()
                .filter_map(|name| stamp(root.join(name)))
                .collect();
            inputs.deck_stamps.push((root.clone(), stamps));
            let external = cfg
                .external_roots(project, &root)
                .filter_map(|(namespace, path)| {
                    let path = path.ok()?.canonicalize().ok()?;
                    inputs.has_unwatched_inputs |= is_unwatched(project, &path);
                    roots.insert(path.clone());
                    Some((namespace.to_owned(), path))
                })
                .collect();
            let structure = root.join(&cfg.structure);
            inputs.has_unwatched_inputs |=
                is_unwatched(project, &root) || is_unwatched(project, &structure);
            if let Some(stamp) = stamp(structure.clone()) {
                update_file(
                    &mut self.structures,
                    &mut structures,
                    &structure,
                    vec![stamp],
                    &mut inputs.changed,
                    || {
                        crate::build::read_structure(&root, &cfg)
                            .ok()
                            .map(Structure::from_yaml)
                    },
                );
            }
            let mut bib: Vec<_> = fs::read_dir(&root)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "bib"))
                .collect();
            if let Some(path) = &cfg.meta.bibliography {
                bib.push(root.join(path));
            }
            bib.sort();
            bib.dedup();
            for path in &bib {
                inputs.has_unwatched_inputs |= is_unwatched(project, path);
                if let Some(stamp) = stamp(path.clone()) {
                    update_file(
                        &mut self.bibliographies,
                        &mut bibliographies,
                        path,
                        vec![stamp],
                        &mut inputs.changed,
                        || {
                            fs::read_to_string(path).ok().map(|text| {
                                BIB_KEY
                                    .captures_iter(&text)
                                    .map(|cap| cap[1].to_owned())
                                    .collect()
                            })
                        },
                    );
                }
            }
            let mut deck = DeckInput {
                root,
                cfg,
                external,
                structure,
                bib,
                cards: FileSet::new(),
            };
            for dir in &deck.cfg.dirs {
                let dir = deck.root.join(dir);
                inputs.has_unwatched_inputs |= is_unwatched(project, &dir);
                for entry in WalkDir::new(dir)
                    .into_iter()
                    .filter_entry(|entry| !crate::cards::is_ignored(&deck.root, entry.path()))
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry.file_type().is_file()
                            && entry.path().extension().is_some_and(|ext| ext == "md")
                    })
                {
                    let path = entry.path();
                    let Some(md) = stamp(path.to_path_buf()) else {
                        continue;
                    };
                    deck.cards.insert(path.to_path_buf());
                    update_file(
                        &mut self.cards,
                        &mut cards,
                        path,
                        card_stamps(md),
                        &mut inputs.changed,
                        || {
                            #[cfg(test)]
                            { inputs.card_reads += 1; }
                            fuzukue_cards::read_card(&deck.root, path)
                                .ok()
                                .flatten()
                                .map(CardData::from)
                        },
                    );
                }
            }
            inputs.decks.push(deck);
        }
        inputs.references = crate::references::graph_entries(project).unwrap_or_default();
        for dir in inputs.references.values() {
            let stamps = ["ref.toml", "reference.md"]
                .into_iter()
                .filter_map(|name| stamp(dir.join(name)))
                .collect();
            update_file(
                &mut self.references,
                &mut references,
                dir,
                stamps,
                &mut inputs.changed,
                || {
                    crate::references::graph_citation(dir)
                        .ok()
                        .map(|(key, title)| Citation { key, title })
                },
            );
        }
        remove_stale(&mut self.cards, &cards, &mut inputs.changed);
        remove_stale(&mut self.structures, &structures, &mut inputs.changed);
        remove_stale(
            &mut self.bibliographies,
            &bibliographies,
            &mut inputs.changed,
        );
        remove_stale(&mut self.references, &references, &mut inputs.changed);
        inputs
    }

    fn save(&self, state: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        fs::create_dir_all(state)?;
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        let mut writer = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, self)?;
        writer.flush()?;
        drop(writer);
        crate::durable::persist(file, state.join("card-graph.json"))?;
        Ok(())
    }
}

// Canonicalize the nearest existing ancestor to also detect new files under a
// configured path outside the workspace.
fn is_unwatched(project: &Path, path: &Path) -> bool {
    path.ancestors()
        .find_map(|part| part.canonicalize().ok())
        .is_some_and(|actual| {
            !actual.starts_with(project) || crate::cards::is_ignored(project, &actual)
        })
}

fn card_stamps(md: Stamp) -> Vec<Stamp> {
    let mut sources = Vec::new();
    if md.path.ends_with("card.md") {
        if let Some(dir) = md.path.parent() {
            sources.extend(
                fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "tex")),
            );
        }
    }
    sources.sort();
    let mut stamps = vec![md];
    stamps.extend(sources.into_iter().filter_map(stamp));
    stamps
}

#[derive(Serialize, Deserialize)]
struct Citation {
    key: Option<String>,
    title: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Structure {
    title: Option<String>,
    items: Vec<Item>,
    appendix: Vec<Item>,
}

#[derive(Serialize, Deserialize)]
enum Item {
    Card(String),
    Section {
        title: String,
        label: Option<String>,
        items: Vec<Item>,
    },
}

impl Structure {
    fn from_yaml(value: serde_yaml::Value) -> Self {
        Self {
            title: value["title"].as_str().map(str::to_owned),
            items: sections(&value),
            appendix: items(&value["appendix"]),
        }
    }
}

pub(crate) fn structure_card_order(value: serde_yaml::Value) -> Vec<String> {
    let Structure { items, appendix, .. } = Structure::from_yaml(value);
    let mut stack: Vec<_> = items.into_iter().chain(appendix).rev().collect();
    let mut ids = Vec::new();
    while let Some(item) = stack.pop() {
        match item {
            Item::Card(id) => ids.push(id),
            Item::Section { items, .. } => stack.extend(items.into_iter().rev()),
        }
    }
    ids
}

fn sections(value: &serde_yaml::Value) -> Vec<Item> {
    ["chapters", "sections", "subsections"]
        .into_iter()
        .flat_map(|key| value[key].as_sequence().into_iter().flatten())
        .map(|section| section_item(section, section["title"].as_str().unwrap_or_default()))
        .collect()
}

fn section_item(value: &serde_yaml::Value, title: &str) -> Item {
    let mut children = items(&value["items"]);
    children.extend(sections(value));
    Item::Section {
        title: title.to_owned(),
        label: value["label"].as_str().map(str::to_owned),
        items: children,
    }
}

fn items(value: &serde_yaml::Value) -> Vec<Item> {
    let mut out = Vec::new();
    let mut section = None;
    let mut subsection = None;
    for item in value.as_sequence().into_iter().flatten() {
        if let Some(heading) = item.get("heading") {
            let title = heading["title"].as_str().unwrap_or_default();
            close_subsection(&mut out, &mut section, &mut subsection);
            if heading["level"] == "section" {
                out.extend(section.replace(section_item(heading, title)));
            } else {
                subsection = Some(section_item(heading, title));
            }
        } else if let Some(title) = item["subsection"].as_str() {
            close_subsection(&mut out, &mut section, &mut subsection);
            subsection = Some(section_item(item, title));
        } else if let Some(id) = crate::build::harness::item_card(item) {
            let card = Item::Card(id.to_owned());
            if let Some(Item::Section { items, .. }) = subsection.as_mut().or(section.as_mut()) {
                items.push(card);
            } else {
                out.push(card);
            }
        }
    }
    close_subsection(&mut out, &mut section, &mut subsection);
    out.extend(section);
    out
}

fn close_subsection(out: &mut Vec<Item>, section: &mut Option<Item>, subsection: &mut Option<Item>) {
    if let Some(subsection) = subsection.take() {
        if let Some(Item::Section { items, .. }) = section {
            items.push(subsection);
        } else {
            out.push(subsection);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CardData {
    id: String,
    kind: String,
    title: String,
    status: String,
    summary: String,
    search: index::SearchText,
    labels: BTreeSet<String>,
    links: Vec<Link>,
    from: Option<String>,
}

static WIKI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]|\s]+)(?:\|[^\]]*)?\]\]").unwrap());
static TEX_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\\([Cc]ref|(?:auto|eq|page)?ref|[A-Za-z]*[Cc]ite[A-Za-z]*|label)\*?\s*(?:\[[^\]\n]*\]\s*){0,2}\{([^{}\n]+)\}"
).unwrap()
});
static MARKDOWN_CITE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\[|;)\s*-?@([\w:.+/-]+)").unwrap());
static BIB_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*@[a-z]+\s*[({]\s*([^\s,{}()]+)\s*,").unwrap());
static TEX_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:^|[^\\\n])(?:\\\\)*%[^\n]*").unwrap());

impl From<Card> for CardData {
    fn from(card: Card) -> Self {
        let statement = card
            .statement
            .as_ref()
            .or(card.prose.as_ref())
            .map_or(card.body.as_str(), |part| part.text.as_str());
        let head = if card.tex.iter().any(|path| path.ends_with("statement.tex")) {
            fuzukue_cards::latex_book::statement_head(statement)
        } else {
            statement
        };
        let summary = one_line(
            card.summary
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(head),
            card.summary.as_deref().is_none_or(|s| s.trim().is_empty()),
        );
        let content = fuzukue_cards::strip_code(&card.body);
        let text = TEX_COMMENT.replace_all(&content, |cap: &regex::Captures<'_>| {
            cap[0].split_once('%').unwrap().0.to_owned()
        });
        let mut links = card.links;
        let mut labels: BTreeSet<_> = card.label.into_iter().collect();
        for cap in WIKI.captures_iter(&text) {
            if !cap[1].starts_with("file:") {
                links.push(Link {
                    to: cap[1].to_owned(),
                    rel: "mentions".into(),
                });
            }
        }
        for cap in TEX_LINK.captures_iter(&text) {
            let command = &cap[1].to_ascii_lowercase();
            if command == "label" {
                labels.insert(cap[2].trim().to_owned());
                continue;
            }
            let rel = if command.contains("cite") {
                "cite"
            } else {
                "ref"
            };
            links.extend(
                cap[2]
                    .split(',')
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(|key| Link {
                        to: key.into(),
                        rel: rel.into(),
                    }),
            );
        }
        for cap in MARKDOWN_CITE.captures_iter(&text) {
            links.push(Link {
                to: cap[1].to_owned(),
                rel: "cite".into(),
            });
        }
        links.sort_unstable_by(|a, b| (&a.rel, &a.to).cmp(&(&b.rel, &b.to)));
        links.dedup();
        let search = index::SearchText::new([&card.id, &card.title, &summary, statement]);
        Self {
            id: card.id,
            kind: card.kind,
            title: card.title,
            status: card.status,
            summary,
            search,
            labels,
            links,
            from: card.from,
        }
    }
}

fn one_line(text: &str, first_sentence: bool) -> String {
    let clean = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with(['#', '%']))
        .collect::<Vec<_>>()
        .join(" ");
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if first_sentence {
        let end = clean.char_indices().find(|&(pos, c)| {
            matches!(c, '。' | '！' | '？')
                || matches!(c, '.' | '!' | '?') && clean[pos + c.len_utf8()..].starts_with(' ')
        });
        if let Some((pos, c)) = end {
            return clip(&clean[..pos + c.len_utf8()], 180);
        }
    }
    clip(&clean, 180)
}

pub(crate) fn clip(text: &str, limit: usize) -> String {
    text.char_indices()
        .nth(limit)
        .map_or_else(|| text.to_owned(), |(end, _)| format!("{}…", &text[..end]))
}
