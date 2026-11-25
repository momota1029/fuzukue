//! Workspace-wide search over the reference corpus, cards, conversations and drafts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{ensure, Result};
use fuzukue_cards::DeckConfig;
use fuzukue_proto::{LibraryHit, LibraryKind, LibraryLocation, LibrarySort, LIBRARY_PAGE_SIZE};
use serde::Serialize;
use walkdir::WalkDir;

use crate::references::{
    self,
    search::{index_words, score, snippet, Query, Words},
};
use crate::{comments, hub::Hub, memos, threads::LibraryStamp, workspace, Workspace};

const TITLE_WEIGHT: f64 = 3.0;

#[derive(Default)]
pub(crate) struct Index {
    files: Files,
    decks: BTreeMap<String, DeckEntry>,
    threads: BTreeMap<String, (LibraryStamp, Vec<Document>)>,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    size: u64,
}

type FileStamps = BTreeMap<PathBuf, Stamp>;
type FileKey = (PathBuf, PathBuf);
type Files = BTreeMap<FileKey, FileEntry>;

struct FileEntry {
    stamps: FileStamps,
    documents: Vec<Document>,
}

struct DeckEntry {
    stamps: FileStamps,
    config: Option<DeckConfig>,
}

struct Text {
    source: String,
    words: Words,
}

impl From<String> for Text {
    fn from(source: String) -> Self {
        Self {
            words: index_words(&source),
            source,
        }
    }
}

struct Document {
    title: String,
    deck: Option<String>,
    location: LibraryLocation,
    heading: Text,
    body: Text,
    updated: i64,
}

impl Document {
    fn new(
        title: String,
        deck: Option<String>,
        location: LibraryLocation,
        heading: String,
        body: String,
        updated: i64,
    ) -> Self {
        Self {
            title,
            deck,
            location,
            heading: heading.into(),
            body: body.into(),
            updated,
        }
    }
}

#[derive(Default, Serialize)]
pub(crate) struct Results {
    pub hits: Vec<LibraryHit>,
    pub more: bool,
    pub decks: Vec<String>,
}

pub(crate) struct Search {
    pub query: String,
    pub kinds: Vec<LibraryKind>,
    pub deck: Option<String>,
    pub sort: LibrarySort,
    pub offset: usize,
    pub limit: usize,
    pub protocol: u32,
}

pub(crate) fn prompt() -> &'static str {
    harness_text!(
        "\nUse search_library to search deck titles and summaries, references, cards, memos, threads, comment drafts, and external review reports. Use the deck path to locate a deck, key and file with read_reference, and deck#id with card_read.",
        "\nデッキの題と概要・資料・カード・メモ・スレッド・コメントの下書き・外部査読報告は search_library で検索できる。デッキの場所は deck、資料はキーとファイルを read_reference、カードは deck#id を card_read に渡して読む。",
    )
}

pub(crate) async fn search(app: Arc<Workspace>, search: Search) -> Result<Results> {
    if !search.query.trim().is_empty() {
        Query::new(&search.query)?;
    }
    ensure!(
        (1..=LIBRARY_PAGE_SIZE).contains(&search.limit),
        harness_format!(
            "Use a limit between 1 and {LIBRARY_PAGE_SIZE}.",
            "limit は 1〜{LIBRARY_PAGE_SIZE} で指定する"
        )
    );
    let reference_guard = app.reference_lock.clone().lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _reference_guard = reference_guard;
        let mut index = app.library.lock().unwrap();
        index.refresh(&app.project_dir.canonicalize()?, &app.hub);
        index.search(&search)
    })
    .await?
}

fn stamps(paths: impl IntoIterator<Item = PathBuf>) -> FileStamps {
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = fs::symlink_metadata(&path).ok()?;
            meta.is_file().then_some(())?;
            Some((
                path,
                Stamp {
                    modified: meta.modified().ok()?,
                    size: meta.len(),
                },
            ))
        })
        .collect()
}

fn updated(stamps: &FileStamps) -> i64 {
    stamps
        .values()
        .map(|stamp| chrono::DateTime::<chrono::Utc>::from(stamp.modified).timestamp_millis())
        .max()
        .unwrap_or_default()
}

fn relative(project: &Path, path: &Path) -> String {
    let path = path.strip_prefix(project).unwrap_or(path);
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}

fn refresh_file(
    previous: &mut Files,
    next: &mut Files,
    key: FileKey,
    stamps: FileStamps,
    load: impl FnOnce() -> Vec<Document>,
) {
    let entry = previous
        .remove(&key)
        .filter(|entry| entry.stamps == stamps)
        .unwrap_or_else(|| FileEntry {
            stamps,
            documents: load(),
        });
    next.insert(key, entry);
}

impl Index {
    fn refresh(&mut self, project: &Path, hub: &Hub) {
        let mut next = BTreeMap::new();
        self.refresh_decks(project, &mut next);
        match references::root(project) {
            Ok(Some(root)) => match references::library_files(&root) {
                Ok(files) => {
                    for file in files {
                        let metadata = stamps(file.metadata_paths(&root));
                        let source = stamps([file.source.clone()]);
                        let date = updated(&source).max(updated(&metadata));
                        let mut title = None;
                        for (part_path, part) in &file.parts {
                            let mut dependencies = stamps([part_path.clone()]);
                            // Replacing the bibliography changes the heading of each part.
                            dependencies.extend(metadata.clone());
                            dependencies.extend(source.clone());
                            let key = (root.clone(), part_path.clone());
                            refresh_file(&mut self.files, &mut next, key, dependencies, || {
                                let Ok(text) = fs::read_to_string(part_path) else {
                                    return Vec::new();
                                };
                                let title = title.get_or_insert_with(|| file.title(&root)).clone();
                                let heading = format!("{title}\n{}\n{}", file.key, file.file);
                                let location = LibraryLocation::Reference {
                                    key: file.key.clone(),
                                    file: file.file.clone(),
                                    path: relative(project, &file.source),
                                    part: part.clone(),
                                };
                                vec![Document::new(title, None, location, heading, text, date)]
                            });
                        }
                    }
                }
                Err(err) => tracing::warn!("library reference corpus: {err:#}"),
            },
            Ok(None) => {}
            Err(err) => tracing::warn!("library reference root: {err:#}"),
        }
        self.files = next;
        let changes = hub.library_threads(|id, stamp| {
            self.threads
                .get(id)
                .is_some_and(|(previous, _)| previous == stamp)
        });
        self.threads = changes
            .into_iter()
            .map(|snapshot| {
                let crate::threads::LibraryThread {
                    id,
                    stamp,
                    messages,
                } = snapshot;
                let entry = match messages {
                    Some(messages) => {
                        let deck = stamp.decks.first().cloned();
                        let title = &stamp.title;
                        let date = stamp.updated;
                        let title_location = LibraryLocation::Thread {
                            thread: id.clone(),
                            message_index: None,
                            item: None,
                        };
                        let mut docs = vec![Document::new(
                            title.clone(),
                            deck.clone(),
                            title_location,
                            title.clone(),
                            String::new(),
                            date,
                        )];
                        docs.extend(messages.into_iter().map(|message| {
                            let location = LibraryLocation::Thread {
                                thread: id.clone(),
                                message_index: Some(message.index),
                                item: Some(message.item),
                            };
                            Document::new(
                                title.clone(),
                                deck.clone(),
                                location,
                                String::new(),
                                message.text,
                                date,
                            )
                        }));
                        (stamp, docs)
                    }
                    None => self.threads.remove(&id).unwrap(),
                };
                (id, entry)
            })
            .collect();
    }

    fn refresh_decks(&mut self, project: &Path, next: &mut Files) {
        let mut roots: BTreeSet<_> = fuzukue_cards::find_decks(project).into_iter().collect();
        if project.join("cards").is_dir() {
            roots.insert(project.to_owned());
        }
        roots.extend(
            fs::read_dir(project.join("research"))
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let dir = entry.path();
                    (entry.file_type().ok()?.is_dir() && dir.join("plan.md").is_file())
                        .then_some(dir)
                }),
        );
        let mut decks = BTreeMap::new();
        for root in roots {
            let deck = relative(project, &root);
            let config =
                stamps(["deck.toml", "cards.toml", "harness.toml"].map(|name| root.join(name)));
            let entry = self
                .decks
                .remove(&deck)
                .filter(|entry| entry.stamps == config)
                .unwrap_or_else(|| DeckEntry {
                    stamps: config,
                    config: DeckConfig::read(&root).ok(),
                });
            if let Some(cfg) = &entry.config {
                let structure = root.join(&cfg.structure)
                    .canonicalize().ok()
                    .filter(|path| path.starts_with(project) && path.is_file());
                let mut dependencies = entry.stamps.clone();
                dependencies.extend(stamps(structure.iter().cloned()));
                let date = updated(&dependencies);
                refresh_file(&mut self.files, next, (root.clone(), root.join("deck.toml")), dependencies, || {
                    let manuscript_title = structure.and_then(|_| workspace::structure_title(&root, cfg));
                    let config_title = cfg.meta.title.trim();
                    let summary = cfg.meta.summary.trim();
                    if manuscript_title.is_none() && config_title.is_empty() && summary.is_empty() {
                        return Vec::new();
                    }
                    let title = manuscript_title.as_deref()
                        .or_else(|| (!config_title.is_empty()).then_some(config_title))
                        .unwrap_or(&deck);
                    let heading = if config_title.is_empty() || config_title == title {
                        title.to_owned()
                    } else {
                        format!("{title}\n{config_title}")
                    };
                    vec![Document::new(
                        title.into(), Some(deck.clone()), LibraryLocation::Deck, heading, summary.into(), date,
                    )]
                });
            }
            for dir in entry.config.iter().flat_map(|cfg| &cfg.dirs) {
                let Ok(dir) = root.join(dir).canonicalize() else {
                    continue;
                };
                if !dir.starts_with(project) {
                    continue;
                }
                for item in WalkDir::new(dir).into_iter()
                    .filter_entry(|entry| !entry.path().strip_prefix(project).is_ok_and(fuzukue_proto::is_inbox_upload_path))
                    .filter_map(Result::ok)
                {
                    let path = item.path();
                    if !item.file_type().is_file() || path.extension().is_none_or(|ext| ext != "md")
                    {
                        continue;
                    }
                    let key = (root.clone(), path.to_owned());
                    if next.contains_key(&key) {
                        continue;
                    }
                    let mut dependencies = vec![path.to_owned()];
                    if path.ends_with("card.md") {
                        dependencies.extend(
                            fs::read_dir(path.parent().unwrap())
                                .into_iter()
                                .flatten()
                                .filter_map(Result::ok)
                                .map(|entry| entry.path())
                                .filter(|p| p.extension().is_some_and(|ext| ext == "tex")),
                        );
                    }
                    // Card parsing follows attached TeX paths. Keep those reads inside the workspace.
                    let Some(paths) = dependencies
                        .into_iter()
                        .map(|path| {
                            let kind = fs::symlink_metadata(&path).ok()?.file_type();
                            if kind.is_symlink() {
                                let target = path.canonicalize().ok()?;
                                (target.starts_with(project) && target.is_file()).then_some(target)
                            } else {
                                kind.is_file().then_some(path)
                            }
                        })
                        .collect::<Option<Vec<_>>>()
                    else {
                        continue;
                    };
                    let dependencies = stamps(paths);
                    let date = updated(&dependencies);
                    refresh_file(&mut self.files, next, key, dependencies, || {
                        let Ok(Some(card)) = fuzukue_cards::read_card(&root, path) else {
                            return Vec::new();
                        };
                        let title = if card.title.is_empty() {
                            card.id.clone()
                        } else {
                            card.title
                        };
                        let heading = format!("{title}\n{}\n{}", card.id, card.defines.join("\n"));
                        let location = if deck == memos::DECK {
                            LibraryLocation::Memo {
                                card: card.id,
                                path: relative(project, path),
                            }
                        } else {
                            LibraryLocation::Card { card: card.id }
                        };
                        vec![Document::new(
                            title,
                            Some(deck.clone()),
                            location,
                            heading,
                            card.body,
                            date,
                        )]
                    });
                }
            }
            let path = fuzukue_proto::compat::workspace_data(&root, "comments.json");
            let dependencies = stamps([path.clone()]);
            if !dependencies.is_empty() && path.canonicalize().is_ok_and(|p| p.starts_with(project))
            {
                let date = updated(&dependencies);
                refresh_file(
                    &mut self.files,
                    next,
                    (root.clone(), path),
                    dependencies,
                    || {
                        comments::load(&root)
                            .into_iter()
                            .enumerate()
                            .map(|(index, comment)| {
                                let body = format!(
                                    "{}\n{}\n{}",
                                    comment.text,
                                    comment.quote.unwrap_or_default(),
                                    comment.source.map(|source| source.text).unwrap_or_default()
                                );
                                let updated = if comment.created > 0 {
                                    comment.created.saturating_mul(1000)
                                } else {
                                    date
                                };
                                let title = comment.card;
                                let location = LibraryLocation::Comment {
                                    index,
                                    card: title.clone(),
                                };
                                Document::new(
                                    title.clone(),
                                    Some(deck.clone()),
                                    location,
                                    title,
                                    body,
                                    updated,
                                )
                            })
                            .collect()
                    },
                );
            }
            if deck.starts_with("papers/") {
                self.refresh_review_reports(project, &root, &deck, next);
            }
            decks.insert(deck, entry);
        }
        self.decks = decks;
    }

    fn recent_memos(&self, search: &Search, results: &mut Results) {
        let mut memos: Vec<_> = self.files.values().flat_map(|entry| {
            entry.documents.iter()
                .filter(|doc| doc.location.kind() == LibraryKind::Memo)
                .map(|doc| (doc, entry.stamps.values().map(|stamp| stamp.modified).max()))
        }).collect();
        memos.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
        results.more = memos.len().saturating_sub(search.offset) > search.limit;
        results.hits = memos.into_iter().skip(search.offset).take(search.limit)
            .map(|(doc, _)| LibraryHit {
                title: doc.title.clone(),
                deck: doc.deck.clone(),
                location: doc.location.clone(),
                snippet: snippet(&doc.body.source, 0),
                highlights: Vec::new(),
                score: 0.0,
                updated: doc.updated,
            })
            .collect();
    }

    fn refresh_review_reports(&mut self, project: &Path, root: &Path, deck: &str, next: &mut Files) {
        use crate::review_reports::Store;

        let Ok(store) = Store::open(project, deck) else { return };
        let Ok(entries) = store.entries() else { return };
        for (id, path) in entries {
            let Ok(reports) = Store::report_paths(&path) else { continue };
            let points_path = path.join("points.toml");
            let dependencies = stamps(reports.values().cloned().chain([
                path.join("submission.toml"), points_path.clone(),
            ]));
            let date = updated(&dependencies);
            refresh_file(&mut self.files, next, (root.to_owned(), points_path), dependencies, || {
                let points = Store::points(&path).unwrap_or_default();
                let mut documents = Vec::new();
                for (report, path) in reports {
                    let Ok(text) = fs::read_to_string(path) else { continue };
                    let mut entries: Vec<_> = points.iter().filter(|point| point.report == report).collect();
                    entries.sort_by_key(|point| point.lines);
                    let lines: Vec<_> = text.lines().collect();
                    let location = |point| LibraryLocation::ReviewReport { submission: id.clone(), report, point };
                    if entries.is_empty() {
                        let title = format!("{id} / R{report}");
                        documents.push(Document::new(title.clone(), Some(deck.into()), location(None), title, text, date));
                        continue;
                    }
                    for (index, point) in entries.iter().enumerate() {
                        // Include intervening report text so any hit can open a nearby point.
                        let line = |point: &fuzukue_proto::ReviewPoint| point.lines[0].saturating_sub(1).min(lines.len());
                        let start = if index == 0 { 0 } else { line(point) };
                        let end = entries.get(index + 1).map_or(lines.len(), |next| line(next));
                        let text = lines[start..end].join("\n");
                        let title = format!("{} · {}", point.id, point.summary);
                        let body = format!("{text}\n{}\n{}\n{}", point.quote, point.response, point.fixed_locations.join("\n"));
                        let heading = format!("{title}\n{id}\n{}", point.cards.join("\n"));
                        documents.push(Document::new(title, Some(deck.into()), location(Some(point.id.clone())), heading, body, date));
                    }
                }
                documents
            });
        }
    }

    fn search(&self, search: &Search) -> Result<Results> {
        let mut results = Results {
            hits: Vec::new(),
            more: false,
            decks: self.decks.keys().cloned().collect(),
        };
        if search.query.trim().is_empty() {
            if (search.kinds.is_empty() || search.kinds.contains(&LibraryKind::Memo))
                && search.deck.as_deref().is_none_or(|deck| deck == memos::DECK)
            {
                self.recent_memos(search, &mut results);
            }
            return Ok(results);
        }
        let query = Query::new(&search.query)?;
        let files = self
            .files
            .values()
            .flat_map(|entry| &entry.documents)
            .map(|doc| (doc, None));
        let threads = self
            .threads
            .values()
            .flat_map(|(stamp, docs)| docs.iter().map(|doc| (doc, Some(stamp.decks.as_slice()))));
        let mut documents = 0;
        let mut df = vec![0; query.len()];
        let mut matches = Vec::new();
        for (doc, attached) in files.chain(threads) {
            let kind = doc.location.kind();
            if (kind == LibraryKind::ReviewReport && search.protocol < fuzukue_proto::REVIEW_REPORTS_PROTOCOL)
                || (kind == LibraryKind::Deck && search.protocol < fuzukue_proto::LIBRARY_DECKS_PROTOCOL)
                || (!search.kinds.is_empty() && !search.kinds.contains(&kind))
            {
                continue;
            }
            let deck = if let Some(filter) = &search.deck {
                if doc.deck.as_ref() != Some(filter)
                    && !attached.is_some_and(|decks| decks.contains(filter))
                {
                    continue;
                }
                Some(filter)
            } else {
                doc.deck.as_ref()
            };
            documents += 1;
            let body = query.matches(&doc.body.source, &doc.body.words);
            let heading = query.matches(&doc.heading.source, &doc.heading.words);
            if body.is_none() && heading.is_none() {
                continue;
            }
            for (i, n) in df.iter_mut().enumerate() {
                *n += usize::from(
                    body.as_ref().is_some_and(|(freq, _)| freq[i] > 0)
                        || heading.as_ref().is_some_and(|(freq, _)| freq[i] > 0),
                );
            }
            matches.push((doc, deck, body, heading));
        }
        let mut ranked: Vec<_> = matches
            .into_iter()
            .map(|(doc, deck, body, heading)| {
                let field_score = |matched: &Option<(Vec<usize>, usize)>| {
                    matched
                        .as_ref()
                        .map_or(0.0, |(freq, _)| score(freq, &df, documents))
                };
                let rank = field_score(&body) + TITLE_WEIGHT * field_score(&heading);
                let (text, first) = if let Some((_, first)) = body {
                    (&doc.body.source, first)
                } else {
                    (&doc.heading.source, heading.unwrap().1)
                };
                (doc, deck, rank, text, first)
            })
            .collect();
        ranked.sort_by(|a, b| {
            let relevance = || b.2.total_cmp(&a.2);
            let newest = || b.0.updated.cmp(&a.0.updated);
            match search.sort {
                LibrarySort::Relevance => relevance().then_with(newest),
                LibrarySort::Newest => newest().then_with(relevance),
            }
        });
        results.more = ranked.len().saturating_sub(search.offset) > search.limit;
        results.hits = ranked
            .into_iter()
            .skip(search.offset)
            .take(search.limit)
            .map(|(doc, deck, score, text, first)| {
                let (snippet, highlights) = query.highlighted_snippet(text, first);
                LibraryHit {
                    title: doc.title.clone(),
                    deck: deck.cloned(),
                    location: doc.location.clone(),
                    snippet,
                    highlights,
                    score,
                    updated: doc.updated,
                }
            })
            .collect();
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use fuzukue_proto::{Actor, CommentView, LibraryReferencePart, ServerEvent};

    fn search(index: &Index, query: &str, kinds: Vec<LibraryKind>, deck: Option<&str>) -> Results {
        index
            .search(&Search {
                query: query.into(),
                kinds,
                deck: deck.map(str::to_owned),
                sort: LibrarySort::Relevance,
                offset: 0,
                limit: LIBRARY_PAGE_SIZE,
                protocol: fuzukue_proto::PROTOCOL_VERSION,
            })
            .unwrap()
    }

    #[test]
    fn indexes_both_deck_titles_and_summary_and_refreshes_structure_edits() -> Result<()> {
        let project = TestDir::new();
        let root = project.path().canonicalize()?;
        let hub = Hub::new();
        project.write("papers/draft/deck.toml", "kind = 'paper'\ntitle = 'Alternate caption'\nsummary = 'Geodesic bounds'\nstructure = 'order.yaml'");
        project.write("papers/draft/order.yaml", "title: Sphere concentration estimates\nsections: []");
        let mut index = Index::default();
        index.refresh(&root, &hub);

        for query in ["Sphere concentration estimates", "Alternate caption", "Geodesic bounds"] {
            let result = search(&index, query, vec![], None);
            assert_eq!(result.hits.len(), 1, "{query}");
            let hit = &result.hits[0];
            assert_eq!(hit.title, "Sphere concentration estimates");
            assert_eq!(hit.deck.as_deref(), Some("papers/draft"));
            assert!(matches!(hit.location, LibraryLocation::Deck));
        }

        project.write("papers/draft/order.yaml", "title: Revised manuscript\nsections: []");
        index.refresh(&root, &hub);
        assert!(search(&index, "Sphere concentration estimates", vec![], None).hits.is_empty());
        assert_eq!(search(&index, "Revised manuscript", vec![LibraryKind::Deck], None).hits.len(), 1);
        assert!(search(&index, "Revised manuscript", vec![LibraryKind::Card], None).hits.is_empty());
        Ok(())
    }

    #[test]
    fn indexes_inbox_memos_separately_from_cards_and_respects_empty_query_filters() {
        let project = TestDir::new();
        let hub = Hub::new();
        project.write("memos/deck.toml", "kind = 'memos'\ndirs = ['.']");
        project.write("memos/todo/card.md", "---\nid: todo\nkind: memo\n---\nHemlock");
        project.write("paper/deck.toml", "kind = 'paper'");
        project.write("paper/cards/filed/card.md", "---\nid: filed\nkind: memo\n---\nHemlock");
        let mut index = Index::default();
        index.refresh(&project.path().canonicalize().unwrap(), &hub);

        let recent = search(&index, "", vec![], None);
        assert_eq!(recent.hits.len(), 1);
        assert!(matches!(&recent.hits[0].location, LibraryLocation::Memo { card, path }
            if card == "todo" && path == "memos/todo/card.md"));
        assert_eq!(search(&index, "", vec![LibraryKind::Memo, LibraryKind::Card], None).hits.len(), 1);
        assert!(search(&index, "", vec![LibraryKind::Card], None).hits.is_empty());
        assert!(search(&index, "", vec![LibraryKind::Memo], Some("paper")).hits.is_empty());
        let cards = search(&index, "hemlock", vec![LibraryKind::Card], None);
        assert_eq!(cards.hits.len(), 1);
        assert!(matches!(&cards.hits[0].location, LibraryLocation::Card { card } if card == "filed"));
    }

    #[test]
    fn indexes_four_kinds_and_refreshes_changed_sources() -> Result<()> {
        let project = TestDir::new();
        let state = TestDir::new();
        let deck = project.path().join("paper");
        let tex = "paper/cards/lemma/statement.tex";
        let reference = "references/source/notes.txt";
        project.write("paper/deck.toml", "kind = 'paper'");
        project.write(
            "paper/cards/lemma/card.md",
            "---\nid: lemma\ntitle: Cardinality\ndefines: [injection]\n---\ncard body",
        );
        project.write(tex, "Kernels of 写像理論.");
        project.write(
            reference,
            &format!("{}Kernel and 写像理論.", "empty\n".repeat(60)),
        );
        project.write("references/source/ref.toml", "title = 'Source title'");
        references::refresh(project.path())?;
        comments::save(
            &deck,
            &[CommentView {
                card: "lemma".into(),
                text: "Explain the Kernel in 写像理論.".into(),
                quote: None,
                proof: None,
                source: None,
                created: 0,
            }],
        )?;
        let hub = Hub::new();
        hub.load_threads(project.path(), state.path());
        let thread = hub.edit_threads(|store| {
            store.create_titled(Some("paper"), Some("Kernel discussion".into()))
        })?;
        hub.publish(ServerEvent::TextDelta {
            session: thread.clone(),
            actor: Actor::User,
            text: "About 写像理論.".into(),
            item: "question".into(),
        });
        let reply = |text: &str| ServerEvent::TextDelta {
            session: thread.clone(),
            actor: Actor::Claude,
            text: text.into(),
            item: "answer".into(),
        };
        hub.publish(reply("Ker"));
        hub.publish(ServerEvent::ToolCall {
            session: thread.clone(),
            actor: Actor::Claude,
            item: "tool".into(),
            name: "tool".into(),
            input: serde_json::json!({"payload": "confidentialpayload"}),
        });
        hub.publish(ServerEvent::ToolResult {
            session: thread.clone(),
            item: "tool".into(),
            ok: true,
            output: "confidentialpayload".into(),
        });

        let mut index = Index::default();
        index.refresh(project.path(), &hub);
        let japanese = search(&index, "写像", vec![], None);
        assert_eq!(japanese.hits.len(), 4);
        for hit in japanese.hits {
            let [start, end] = hit.highlights[0];
            assert_eq!(&hit.snippet[start..end], "写像");
        }
        let references = search(&index, "KERNEL", vec![LibraryKind::Reference], None);
        assert!(
            matches!(&references.hits[0].location, LibraryLocation::Reference {
            key, file, part: LibraryReferencePart::Lines { lines: [61, 61] }, ..
        } if key == "source" && file == "notes.txt")
        );
        assert_eq!(references.hits[0].title, "Source title");
        assert!(search(&index, "kernel", vec![LibraryKind::Card], None)
            .hits
            .is_empty());
        assert_eq!(
            search(&index, "INJECTION", vec![], Some("paper"))
                .hits
                .len(),
            1
        );
        assert_eq!(
            search(&index, "kernel", vec![], Some("paper")).hits.len(),
            2
        );
        assert!(search(&index, "confidentialpayload", vec![], None)
            .hits
            .is_empty());

        hub.publish(reply("nel of this map."));
        hub.edit_threads(|store| store.rename(&thread, "A renamed thread"))?;
        project.write(tex, "Revised image lemma.");
        project.write(reference, "New reference word.");
        comments::save(&deck, &[])?;
        index.refresh(project.path(), &hub);
        let hits = search(&index, "kernel", vec![], None).hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "A renamed thread");
        assert!(matches!(&hits[0].location, LibraryLocation::Thread {
            thread: id, message_index: Some(1), item: Some(item),
        } if id == &thread && item == "answer"));
        assert_eq!(search(&index, "image", vec![], None).hits.len(), 1);

        references::refresh(project.path())?;
        index.refresh(project.path(), &hub);
        assert_eq!(
            search(&index, "reference", vec![LibraryKind::Reference], None)
                .hits
                .len(),
            1
        );
        fs::remove_file(project.path().join(tex))?;
        index.refresh(project.path(), &hub);
        assert!(search(&index, "image", vec![], None).hits.is_empty());
        Ok(())
    }

    #[test]
    fn newest_orders_drafts_by_their_own_dates() -> Result<()> {
        let project = TestDir::new();
        project.write("paper/deck.toml", "kind = 'paper'");
        let items = [1_700_000_000, 1_750_000_000].map(|created| CommentView {
            card: "lemma".into(),
            text: "A comment about topology.".into(),
            quote: None,
            proof: None,
            source: None,
            created,
        });
        comments::save(&project.path().join("paper"), &items)?;
        let mut index = Index::default();
        index.refresh(project.path(), &Hub::new());
        let results = index.search(&Search {
            query: "topology".into(),
            kinds: vec![LibraryKind::Comment],
            deck: None,
            sort: LibrarySort::Newest,
            offset: 0,
            limit: LIBRARY_PAGE_SIZE,
            protocol: fuzukue_proto::PROTOCOL_VERSION,
        })?;
        assert!(matches!(
            results.hits[0].location,
            LibraryLocation::Comment { index: 1, .. }
        ));
        assert_eq!(results.hits[0].updated, 1_750_000_000_000);
        Ok(())
    }

    #[test]
    fn highlights_unicode_without_promoting_latin_substrings() -> Result<()> {
        let text = format!("{}写像理論 KÉRNEL kernels", "前".repeat(110));
        let query = Query::new("写像 kérnel")?;
        let (_, first) = query.matches(&text, &index_words(&text)).unwrap();
        let (snippet, ranges) = query.highlighted_snippet(&text, first);
        assert!(snippet.starts_with('…'));
        let terms: Vec<_> = ranges.into_iter().map(|[a, b]| &snippet[a..b]).collect();
        assert_eq!(terms, ["写像", "KÉRNEL"]);
        Ok(())
    }
}
