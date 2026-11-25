//! Workspace-scoped HTTP reads for clients that support lazy deck views.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use fuzukue_cards::{Diagnostic, Graph, Severity};
use fuzukue_proto::{CardContent, CardEditPart, DeckCardPage, DeckDiagnosticPage, CARD_CONTENT_BYTES, DECK_PAGE_BYTES};

use super::{card_content, card_summary, diagnostic_view, edit_source, tex_access, translation};
use crate::App;

#[derive(Default)]
pub(crate) struct Cache(Mutex<CacheState>);

#[derive(Default)]
struct CacheState {
    revision: u64,
    decks: VecDeque<Arc<Deck>>,
}

struct Deck {
    revision: u64,
    graph: Graph,
    body_bytes: usize,
    rules: Vec<PathBuf>,
    diagnostics: Mutex<Option<Vec<Diagnostic>>>,
}

impl Cache {
    pub(super) fn clear(&self) {
        let mut state = self.0.lock().unwrap();
        state.revision += 1;
        state.decks.clear();
    }

    fn get(&self, project: &Path, root: &Path) -> Result<Arc<Deck>> {
        let mut state = self.0.lock().unwrap();
        if let Some(index) = state.decks.iter().position(|deck| deck.graph.root == root) {
            let deck = state.decks.remove(index).unwrap();
            state.decks.push_back(deck.clone());
            return Ok(deck);
        }
        let rules = crate::rules::resolve(project)?.layers(project);
        let graph = Graph::open_for_browsing(root, &rules, project)?;
        // Local bodies are absent. Namespaced cards can still carry large content.
        const BODY_BUDGET: usize = 32 * 1024 * 1024;
        let body_bytes = graph.cards.values().map(|card| {
            card.body.capacity() + [&card.statement, &card.proof, &card.prose].into_iter()
                .flatten().map(|part| part.text.capacity()).sum::<usize>()
        }).sum();
        let deck = Arc::new(Deck { revision: state.revision, graph, body_bytes, rules, diagnostics: Mutex::default() });
        if body_bytes <= BODY_BUDGET {
            while state.decks.len() >= 4
                || state.decks.iter().map(|deck| deck.body_bytes).sum::<usize>() > BODY_BUDGET - body_bytes
            {
                state.decks.pop_front();
            }
            state.decks.push_back(deck.clone());
        }
        Ok(deck)
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct DeckQuery {
    workspace: Option<String>,
    deck: String,
    revision: Option<u64>,
    #[serde(default)]
    offset: usize,
    card: Option<String>,
}

type ReadError = (StatusCode, String);

pub(crate) async fn summaries(State(app): State<Arc<App>>, Query(query): Query<DeckQuery>) -> Result<Response, ReadError> {
    read(app, query, |project, prefix, deck, query| {
        let mut sources = translation::Sources::new(project, &deck.graph);
        let rows = deck.graph.cards.values().skip(query.offset)
            .map(|card| card_summary(project, prefix, &deck.graph, card, &mut sources));
        let (cards, next) = page(rows, query.offset, 128)?;
        json(&DeckCardPage { cards, next, revision: deck.revision }, DECK_PAGE_BYTES)
    }).await
}

pub(crate) async fn content(State(app): State<Arc<App>>, Query(query): Query<DeckQuery>) -> Result<Response, ReadError> {
    read(app, query, |project, _, deck, query| {
        let card = deck.graph.cards.get(query.card.as_deref().unwrap_or_default())
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Card is no longer in this deck.".into()))?;
        let root = &deck.graph.root;
        let tex = tex_access(project, root, &deck.graph);
        let content = if card.external {
            card_content(root, card, tex)
        } else {
            let card = deck.graph.read_browsing_card(card, project).map_err(read_error)?;
            CardContent {
                edit_body: edit_source(root, &card, CardEditPart::Body, tex).ok(),
                edit_proof: edit_source(root, &card, CardEditPart::Proof, tex).ok(),
                body: card.body,
            }
        };
        json(&content, CARD_CONTENT_BYTES)
    }).await
}

#[derive(serde::Deserialize)]
pub(crate) struct ProseQuery {
    workspace: Option<String>,
    deck: String,
    path: String,
    start: u64,
    len: usize,
    sha256: String,
}

pub(crate) async fn prose(State(app): State<Arc<App>>, Query(query): Query<ProseQuery>) -> Result<Response, ReadError> {
    let app = app.route(query.workspace).await.map_err(read_error)?;
    let text = tokio::task::spawn_blocking(move || {
        let deck = crate::safe_join(&app.project_dir, &query.deck)?;
        let span = fuzukue_proto::DocProseSpan {
            path: query.path,
            start: query.start,
            len: query.len,
            sha256: query.sha256,
            newline_before: false,
        };
        read_prose(&deck, &span)
    }).await.map_err(|err| read_error(err.into()))?.map_err(read_error)?;
    Ok(text.into_response())
}

pub(super) fn read_prose(deck: &Path, span: &fuzukue_proto::DocProseSpan) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    use sha2::{Digest, Sha256};

    anyhow::ensure!(span.len <= fuzukue_proto::DOC_PROSE_SPAN_BYTES, "prose range exceeds the size limit");
    let path = crate::safe_join(deck, &span.path)?;
    anyhow::ensure!(!fuzukue_proto::is_thread_save_artifact(&path), "private thread save path");
    anyhow::ensure!(path.is_file() && path.extension().is_some_and(|ext| ext == "tex"), "prose source must be a TeX file");
    let mut file = crate::mcp::workspace_read::open_regular_source(&path)?;
    file.seek(SeekFrom::Start(span.start))?;
    let mut bytes = vec![0; span.len];
    file.read_exact(&mut bytes)?;
    anyhow::ensure!(format!("{:x}", Sha256::digest(&bytes)) == span.sha256, "manuscript changed");
    Ok(String::from_utf8(bytes)?)
}

pub(crate) async fn diagnostics(State(app): State<Arc<App>>, Query(query): Query<DeckQuery>) -> Result<Response, ReadError> {
    read(app, query, |project, prefix, deck, query| {
        let path = query.card.as_ref().map(|id| {
            deck.graph.cards.get(id).map(|card| &card.path)
                .ok_or_else(|| (StatusCode::NOT_FOUND, "Card is no longer in this deck.".into()))
        }).transpose()?;
        let mut cache = deck.diagnostics.lock().unwrap();
        if cache.is_none() {
            *cache = Some(deck.graph.browsing_diagnostics(&deck.rules, project).map_err(read_error)?);
        }
        let matches = || cache.as_ref().unwrap().iter()
            .filter(|diag| path.is_none_or(|path| &diag.path == path));
        let total = matches().count();
        let errors = matches().filter(|diag| diag.severity == Severity::Error).count();
        let rows = matches().skip(query.offset).map(|diag| {
            let view = diagnostic_view(prefix, diag);
            if crate::bounded_json_len(&view, DECK_PAGE_BYTES - 1025).is_some() {
                view
            } else {
                fuzukue_proto::CardDiagnostic {
                    error: view.error,
                    path: view.path,
                    message: "Diagnostic text exceeds the display limit.".into(),
                    style: None,
                }
            }
        });
        let (diagnostics, next) = page(rows, query.offset, 64)?;
        json(&DeckDiagnosticPage { diagnostics, next, total, errors }, DECK_PAGE_BYTES)
    }).await
}

async fn read(
    registry: Arc<App>,
    query: DeckQuery,
    f: impl FnOnce(&Path, &Path, &Deck, &DeckQuery) -> Result<Response, ReadError> + Send + 'static,
) -> Result<Response, ReadError> {
    let app = registry.route(query.workspace.clone()).await.map_err(read_error)?;
    tokio::task::spawn_blocking(move || {
        let project = &app.project_dir;
        let root = crate::safe_join(project, &query.deck).map_err(read_error)?;
        if !root.is_dir() {
            return Err((StatusCode::NOT_FOUND, "Deck directory is missing.".into()));
        }
        let deck = app.deck_view.get(project, &root).map_err(read_error)?;
        if query.revision.is_some_and(|revision| revision != deck.revision) {
            return Err((StatusCode::CONFLICT, "Deck changed while it was being read. Reload the deck.".into()));
        }
        f(project, Path::new(&query.deck), &deck, &query)
    }).await.context("reading deck").map_err(read_error)?
}

fn page<T: serde::Serialize>(rows: impl Iterator<Item = T>, offset: usize, count: usize) -> Result<(Vec<T>, Option<usize>), ReadError> {
    // Leave room for the page envelope and the JSON separators.
    let mut remaining = DECK_PAGE_BYTES - 1024;
    let mut page = Vec::new();
    for row in rows {
        if page.len() == count {
            return Ok((page, Some(offset + count)));
        }
        let oversized = || (StatusCode::PAYLOAD_TOO_LARGE, "A deck entry exceeds the page size limit.".into());
        let size = crate::bounded_json_len(&row, DECK_PAGE_BYTES - 1025).ok_or_else(oversized)? + 1;
        if size > remaining && !page.is_empty() {
            let next = offset + page.len();
            return Ok((page, Some(next)));
        }
        remaining = remaining.saturating_sub(size);
        page.push(row);
    }
    Ok((page, None))
}

fn json(value: &impl serde::Serialize, budget: usize) -> Result<Response, ReadError> {
    let bytes = crate::bounded_json(value, budget)
        .ok_or_else(|| (StatusCode::PAYLOAD_TOO_LARGE, "Deck response exceeds the size limit.".into()))?;
    Ok(([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], bytes).into_response())
}

fn read_error(error: anyhow::Error) -> ReadError {
    (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}"))
}
