//! `course_plan`, `course_next` and `course_commit`: lecture notes built from an imported book.
//! The logic lives in `fuzukue_cards::course`; this module resolves decks and serializes writes.

use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Result};
use fuzukue_cards::course::{self, Decision};
use fuzukue_cards::Severity;
use fuzukue_proto::ServerEvent;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{rules, workspace, Workspace};

pub(crate) mod reviews;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanArgs {
    deck: Option<String>,
    /// Book deck, project-relative. Required the first time.
    book: Option<String>,
    #[serde(default)]
    sections: Vec<Decision>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NextArgs {
    deck: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitArgs {
    deck: Option<String>,
    section: String,
    cards: Vec<String>,
}

pub(crate) enum Request {
    Plan(PlanArgs),
    Next(NextArgs),
    Commit(CommitArgs),
}

impl Request {
    fn deck(&self) -> Option<&str> {
        match self {
            Self::Plan(a) => a.deck.as_deref(),
            Self::Next(a) => a.deck.as_deref(),
            Self::Commit(a) => a.deck.as_deref(),
        }
    }
}

pub(crate) fn plan_schema() -> Value {
    json!({"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["key"], "properties": {
        "key": {"type": "string"},
        "status": {"type": "string", "enum": ["todo", "adopted", "skipped", "done"]},
        "exercises": {"type": "object", "additionalProperties": {"type": "string", "enum": ["adopt", "skip"]}},
    }}})
}

pub(crate) async fn run(app: Arc<Workspace>, request: Request) -> Result<Value> {
    let deck = request.deck().map(str::to_owned).unwrap_or_else(|| app.current_deck());
    // Every operation reads `course.toml` and may rewrite it, so one at a time.
    let guard = app.file_save.clone().lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        ensure!(
            Path::new(&deck).is_relative() && !deck.is_empty(),
            "deck には講義デッキのプロジェクト相対パスを指定する"
        );
        let project = app.project_dir.canonicalize()?;
        let (lecture, deck_key) = workspace::resolve_deck(&project, Path::new(&deck))?;
        let recovered = reviews::reconcile(&app, &deck_key, &lecture)?;
        let result = match request {
            Request::Plan(args) => {
                if let Some(book) = &args.book {
                    ensure!(
                        Path::new(book).components().all(|c| matches!(c, std::path::Component::Normal(_))),
                        "book はプロジェクト相対のパスで指定する"
                    );
                    workspace::resolve_deck(&project, Path::new(book))?;
                }
                let mut report = course::plan_with_review_check(
                    &project, &lecture, args.book.as_deref(), &args.sections,
                    reviews::freshness(&app, &deck_key, &project, &lecture),
                )?;
                report.changed |= recovered;
                serde_json::to_value(report)?
            }
            Request::Next(_) => serde_json::to_value(course::next(&project, &lecture)?)?,
            Request::Commit(args) => {
                let checked = lecture.clone();
                let project = project.clone();
                serde_json::to_value(course::commit(&lecture, &args.section, &args.cards, move |ids| {
                    style_check(&project, &checked, ids)
                })?)?
            }
        };
        let path = format!("{}/{}", deck.trim_end_matches('/'), course::FILE);
        app.hub.publish_snapshot(ServerEvent::FileChanged { path });
        Ok(result)
    })
    .await?
}

/// The cards in `ids` must have no errors and no style findings.
fn style_check(project: &Path, lecture: &Path, ids: &[String]) -> Result<()> {
    let graph = rules::open_graph(project, lecture)?;
    let mut problems = Vec::new();
    for id in ids {
        let Some(card) = graph.cards.get(id) else { continue };
        let dir = card.path.parent().filter(|_| card.path.ends_with("card.md"));
        for d in &graph.diagnostics {
            let own = d.path == card.path || dir.is_some_and(|dir| d.path.starts_with(dir));
            if own && (d.severity == Severity::Error || d.code.is_some_and(|c| c.starts_with("style/"))) {
                problems.push(format!("{id}: {}", d.display_message()));
            }
        }
    }
    ensure!(problems.is_empty(), "文体検査かカード検査が通らない:\n- {}", problems.join("\n- "));
    Ok(())
}
