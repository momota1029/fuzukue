//! Persist and run bounded, direct API section reviews.

pub(crate) mod editor;
mod report;
mod sources;
mod workflow;

pub(crate) use workflow::register;

use anyhow::Error;
use fuzukue_proto::AgentCaller;
use serde_json::{json, Value};

use crate::delegate::blind::Isolation;
use crate::isolated_model::{self, Connection};

use super::*;
use report::{Coverage, Verdict};
use sources::{sha256, Allowed, Packet, MAX_CARDS};

pub(crate) fn properties() -> Value {
    json!({
        "isolation": {"type": "string", "enum": ["blind"]},
        "request_key": {"type": "string", "minLength": 1},
        "backend": {"type": "string", "enum": ["codex", "claude"]},
        "model": {"type": "string", "minLength": 1},
        "recheck": {"type": "object", "additionalProperties": false, "required": ["initial_session", "commit"],
            "properties": {
                "initial_session": {"type": "string"},
                "commit": {"type": "string", "description": "Full Git commit OID for the new version"},
                "affected_uses": {"type": "array", "items": {"type": "string"}}
            }
        },
        "allow": {"type": "array", "minItems": 1, "items": {
            "type": "object", "additionalProperties": false, "required": ["source", "lines"],
            "properties": {
                "lines": {"type": "array", "minItems": 2, "maxItems": 2, "items": {"type": "integer", "minimum": 1}},
                "source": {"type": "object", "oneOf": [
                    {"type": "object", "additionalProperties": false, "required": ["type", "card", "part"],
                        "properties": {"type": {"const": "card"}, "card": {"type": "string"},
                            "part": {"enum": ["statement", "proof"]}}},
                    {"type": "object", "additionalProperties": false, "required": ["type", "key"],
                        "properties": {"type": {"const": "material"}, "key": {"type": "string"}}}
                ]}
            }
        }}
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    deck: Option<String>,
    kind: String,
    section: Option<String>,
    cards: Option<Vec<String>>,
    isolation: Isolation,
    request_key: String,
    backend: Option<AgentBackend>,
    model: Option<String>,
    allow: Option<Vec<Allowed>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recheck: Option<RecheckRequest>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecheckRequest {
    initial_session: String,
    commit: String,
    #[serde(default)]
    affected_uses: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Recheck {
    initial_session: String,
    commit: String,
    affected: workflow::Affected,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Pending,
    Reviewed,
    Failed,
}

#[derive(Serialize, Deserialize)]
struct Failure {
    stage: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
struct Sent {
    attempted_at: i64,
    message_bytes: usize,
    input_hash: String,
    excerpts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Run {
    request_id: String,
    round: u8,
    front_desk: AgentBackend,
    origin: String,
    request_hash: String,
    request: Request,
    stage: Stage,
    packet: Option<Packet>,
    sent: Option<Sent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_run_id: Option<String>,
    resolved_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_usage: Option<Value>,
    provider_response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    malformed_response_bytes: Option<Vec<u8>>,
    response: Option<String>,
    coverage: Option<Coverage>,
    failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recheck: Option<Recheck>,
}

impl Run {
    fn fail(&mut self, stage: &str, error: Error) {
        self.stage = Stage::Failed;
        self.failure = Some(Failure {
            stage: stage.into(),
            reason: format!("{error:#}"),
        });
    }

    fn is_complete(&self) -> bool {
        self.coverage
            .as_ref()
            .is_some_and(|record| matches!(record.verdict, Verdict::Pass | Verdict::Findings))
    }
}

struct Selection<'a> {
    deck: &'a str,
    section: &'a str,
    cards: &'a [String],
    backend: AgentBackend,
    model: &'a str,
    allow: &'a [Allowed],
}

impl Request {
    fn selection(&self) -> Result<Selection<'_>> {
        ensure!(
            self.kind == "math",
            "Blind section review requires kind math"
        );
        let deck = self
            .deck
            .as_deref()
            .context("Blind section review requires an explicit deck")?;
        ensure!(
            !deck.is_empty() && Path::new(deck).is_relative(),
            "Use a project-relative deck for blind review"
        );
        Ok(Selection {
            deck,
            section: self
                .section
                .as_deref()
                .filter(|key| !key.trim().is_empty())
                .context("Blind section review requires an explicit section")?,
            cards: self
                .cards
                .as_deref()
                .filter(|cards| (1..=MAX_CARDS).contains(&cards.len()))
                .with_context(|| {
                    format!("Blind section review requires 1 to {MAX_CARDS} ordered targets")
                })?,
            backend: self
                .backend
                .context("Blind section review requires an explicit backend")?,
            model: self
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .context("Blind section review requires an explicit provider model")?,
            allow: self
                .allow
                .as_deref()
                .context("Blind section review requires an explicit excerpt allow-list")?,
        })
    }
}

pub(crate) async fn request(
    app: &Arc<Workspace>,
    origin: &str,
    desk: AgentBackend,
    request: Request,
) -> Result<Value> {
    let work = app.admit_work()?;
    ensure!(
        !request.request_key.trim().is_empty(),
        "Provide a nonempty request_key"
    );
    let request_hash = input_hash(&request)?;
    let save = app.file_save.clone().lock_owned().await;
    let initial = request
        .recheck
        .as_ref()
        .map(|link| workflow::initial(app, &link.initial_session, Some(origin)))
        .transpose()?;
    let (request_id, session, round) = if let Some(first) = &initial {
        (
            first.blind.as_ref().unwrap().request_id.clone(),
            workflow::recheck_session(first),
            2,
        )
    } else {
        let request_id = input_hash(&(origin, &request.request_key))?;
        let digest = request_id.strip_prefix("sha256:").expect("SHA-256 prefix");
        let session = format!("review-blind-{digest}");
        (request_id, session, 1)
    };
    if let Some(existing) = Receipt::read(&app.state_dir, &session)? {
        let run = existing
            .blind
            .as_ref()
            .context("Blind receipt is missing its run")?;
        ensure!(
            run.origin == origin && run.request_hash == request_hash,
            "This review round is already bound to a different input"
        );
        drop(save);
        return view(app, origin, existing).await;
    }

    let recheck = match (&initial, &request.recheck) {
        (Some(first), Some(link)) => Some(workflow::prepare_recheck(app, first, &request, link)?),
        _ => None,
    };
    let mut receipt = Receipt {
        deck: request.deck.clone().unwrap_or_default(),
        scope: Scope {
            section: request.section.clone(),
            cards: request.cards.clone().unwrap_or_default(),
            reviewed_version: String::new(),
        },
        review: Dispatch {
            kind: Kind::Math,
            session,
            input_hash: String::new(),
        },
        findings: Vec::new(),
        capture: None,
        blind: Some(Run {
            request_id,
            round,
            front_desk: desk,
            origin: origin.into(),
            request_hash,
            request,
            stage: Stage::Pending,
            packet: None,
            sent: None,
            model_run_id: None,
            resolved_model: None,
            provider_usage: None,
            provider_response: None,
            malformed_response_bytes: None,
            response: None,
            coverage: None,
            failure: None,
            recheck,
        }),
    };
    let run = receipt.blind.as_mut().expect("new blind receipt");
    let launch = match run.request.selection() {
        Err(error) => Err(("selection", error)),
        Ok(selection) => prepare(app, &mut receipt.deck, &mut receipt.scope, selection).await,
    };
    let (client, model, packet) = match launch {
        Ok(launch) => launch,
        Err((stage, error)) => {
            run.fail(stage, error);
            persist_receipt(&app.state_dir, &receipt, false)?;
            drop(save);
            return view(app, origin, receipt).await;
        }
    };
    if let Some(recheck) = &run.recheck {
        let first = initial.as_ref().expect("linked initial receipt");
        if let Err(error) =
            workflow::verify_new_packet(app, first, &receipt.scope, &packet, recheck).await
        {
            run.fail("version", error);
            run.packet = Some(packet);
            persist_receipt(&app.state_dir, &receipt, false)?;
            drop(save);
            return view(app, origin, receipt).await;
        }
    }
    receipt.review.input_hash = sha256(packet.message.as_bytes());
    run.packet = Some(packet);
    let cancel = Arc::new(Cancellation::default());
    let session = receipt.review.session.clone();
    {
        let mut active = app.reviews.blind.lock().unwrap();
        if app.reviews.stopping.load(Ordering::Relaxed) {
            run.fail("interrupted", anyhow::anyhow!("Review queue is stopping"));
            persist_receipt(&app.state_dir, &receipt, false)?;
        } else {
            persist_receipt(&app.state_dir, &receipt, false)?;
            active.insert(session.clone(), cancel.clone());
        }
    }
    drop(save);
    if receipt
        .blind
        .as_ref()
        .is_some_and(|run| run.stage == Stage::Failed)
    {
        return view(app, origin, receipt).await;
    }
    let accepted = json!({
        "session": session, "request_id": receipt.blind.as_ref().map(|run| &run.request_id),
        "stage": "pending", "round": round, "scope": receipt.scope, "input_hash": receipt.review.input_hash,
    });
    let app = app.clone();
    tokio::spawn(async move {
        let _work = work;
        let result = complete(&app, &mut receipt, client, &model, &cancel).await;
        if let Err((stage, error)) = result {
            receipt
                .blind
                .as_mut()
                .expect("blind receipt")
                .fail(stage, error);
        }
        let mut active = app.reviews.blind.lock().unwrap();
        if let Err(error) = persist_receipt(&app.state_dir, &receipt, true) {
            tracing::error!("could not save blind section review {session}: {error:#}");
        }
        active.remove(&session);
    });
    Ok(accepted)
}

type Launch = (Connection, String, Packet);
type RunError = (&'static str, Error);

async fn prepare(
    app: &Arc<Workspace>,
    deck: &mut String,
    scope: &mut Scope,
    selection: Selection<'_>,
) -> Result<Launch, RunError> {
    app.check_review_policy(Kind::Math, Some(AgentCaller::FrontDesk))
        .await
        .map_err(|e| ("permission", e))?;
    let client = Connection::open(app, selection.backend).map_err(|e| ("credential", e))?;
    let project = app
        .project_dir
        .canonicalize()
        .map_err(|e| ("scope", e.into()))?;
    let (_, resolved) =
        resolve_deck(&project, Path::new(selection.deck)).map_err(|e| ("scope", e))?;
    let (section, cards, allow) = (
        selection.section.to_owned(),
        selection.cards.to_vec(),
        selection.allow.to_vec(),
    );
    let (captured, packet) = tokio::task::spawn_blocking({
        let resolved = resolved.clone();
        move || sources::capture(&project, &resolved, &section, &cards, &allow)
    })
    .await
    .map_err(|e| ("scope", e.into()))?
    .map_err(|e| ("scope", e))?;
    *deck = resolved;
    *scope = captured;
    Ok((client, selection.model.to_owned(), packet))
}

async fn complete(
    app: &Workspace,
    receipt: &mut Receipt,
    client: Connection,
    model: &str,
    cancel: &Arc<Cancellation>,
) -> Result<(), RunError> {
    cancel.check().map_err(|e| ("interrupted", e))?;
    let id = receipt.review.session.clone();
    let packet = receipt
        .blind
        .as_ref()
        .and_then(|run| run.packet.as_ref())
        .expect("prepared blind packet");
    let input = packet.model_input(&receipt.scope);
    let sent = Sent {
        attempted_at: usage::now(),
        message_bytes: packet.message.len(),
        input_hash: receipt.review.input_hash.clone(),
        excerpts: packet
            .excerpts
            .iter()
            .map(|excerpt| excerpt.label.clone())
            .collect(),
    };
    let result = isolated_model::run(app, id.clone(), client, model, input, cancel, || {
        let run = receipt.blind.as_mut().expect("blind receipt");
        run.model_run_id = Some(id);
        run.sent = Some(sent);
        persist_receipt(&app.state_dir, receipt, true)
    })
    .await
    .map_err(|error| ("storage", error))?;
    let run = receipt.blind.as_mut().expect("blind receipt");
    run.model_run_id = Some(result.id);
    if let Some(attempted_at) = result.attempted_at {
        run.sent.as_mut().expect("sent marker").attempted_at = attempted_at;
    } else {
        run.sent = None;
    }
    let outcome = result.outcome.expect("finished blind run");
    run.resolved_model = outcome.resolved_model;
    run.provider_response = outcome.provider_response;
    run.provider_usage = outcome.usage;
    run.malformed_response_bytes = outcome.malformed_response_bytes;
    run.response = outcome.text;
    cancel.check().map_err(|e| ("interrupted", e))?;
    let truncated = outcome.output_limited;
    let failure = outcome
        .failure
        .map(|failure| (failure.stage.label(), anyhow::anyhow!("{}", failure.reason)));
    if !truncated {
        if let Some(failure) = failure {
            return Err(failure);
        }
    }
    let packet = run.packet.as_ref().expect("prepared blind packet");
    let mut coverage = report::validate(&receipt.scope, packet, run.response.as_deref().unwrap())
        .map_err(|error| {
        (
            "response",
            match &failure {
                Some((_, limit)) => error.context(limit.to_string()),
                None => error,
            },
        )
    })?;
    if truncated && coverage.verdict != Verdict::Unchecked {
        coverage.verdict = Verdict::Partial;
    }
    run.coverage = Some(coverage);
    if let Some(failure) = failure {
        return Err(failure);
    }
    run.stage = Stage::Reviewed;
    Ok(())
}

pub(crate) async fn read(app: &Arc<Workspace>, origin: &str, session: &str) -> Result<Value> {
    let receipt =
        Receipt::read(&app.state_dir, session)?.context("Section review receipt was not found")?;
    view(app, origin, receipt).await
}

fn current(app: &Workspace, receipt: &Receipt) -> Result<(Scope, Packet)> {
    let run = receipt.blind.as_ref().context("Expected a blind receipt")?;
    sources::capture(
        &app.project_dir.canonicalize()?,
        &receipt.deck,
        receipt.scope.section.as_deref().unwrap_or_default(),
        &receipt.scope.cards,
        run.request.allow.as_deref().unwrap_or_default(),
    )
}

fn require_current(app: &Workspace, receipt: &Receipt) -> Result<()> {
    let (scope, packet) = current(app, receipt).context("Cannot verify current review input")?;
    let captured = receipt
        .blind
        .as_ref()
        .and_then(|run| run.packet.as_ref())
        .context("Review has no captured input")?;
    ensure!(
        scope.reviewed_version == receipt.scope.reviewed_version
            && packet.fingerprints == captured.fingerprints
            && (receipt.blind.as_ref().is_some_and(|run| run.round == 1)
                || packet.target_parts == captured.target_parts),
        "Review input changed after capture"
    );
    Ok(())
}

fn settle_interrupted(app: &Workspace, mut receipt: Receipt) -> Result<Receipt> {
    if receipt
        .blind
        .as_ref()
        .is_some_and(|run| run.stage == Stage::Pending)
    {
        let active = app.reviews.blind.lock().unwrap();
        if !active.contains_key(&receipt.review.session) {
            // Final persistence and active job removal share this lock.
            receipt = Receipt::read(&app.state_dir, &receipt.review.session)?
                .context("Receipt disappeared")?;
            let run = receipt.blind.as_mut().expect("blind receipt");
            if run.stage == Stage::Pending {
                run.fail(
                    "interrupted",
                    anyhow::anyhow!("Server stopped before the review was recorded"),
                );
                persist_receipt(&app.state_dir, &receipt, true)?;
            }
        }
    }
    Ok(receipt)
}

async fn view(app: &Arc<Workspace>, origin: &str, receipt: Receipt) -> Result<Value> {
    let Some(run) = &receipt.blind else {
        return Ok(json!({
            "format": "legacy", "deck": receipt.deck, "scope": receipt.scope,
            "review": receipt.review, "findings": receipt.findings, "coverage": "unrecorded",
        }));
    };
    ensure!(
        run.origin == origin,
        "This blind review belongs to another front desk thread"
    );
    let save = app.file_save.clone().lock_owned().await;
    let app = app.clone();
    tokio::task::spawn_blocking(move || {
        let _save = save;
        let receipt = settle_interrupted(&app, receipt)?;
        let run = receipt.blind.as_ref().expect("blind receipt");
        let mut result = json!({
            "format": "blind", "receipt": receipt, "session": receipt.review.session,
            "request_id": run.request_id, "stage": run.stage, "complete": false,
        });
        if let (Some(_), Some(packet)) = (&run.coverage, &run.packet) {
            let now = current(&app, &receipt);
            let fresh: BTreeMap<_, _> = packet
                .fingerprints
                .iter()
                .map(|(card, fingerprint)| {
                    (
                        card,
                        now.as_ref().is_ok_and(|(_, current)| {
                            current.fingerprints.get(card) == Some(fingerprint)
                        }),
                    )
                })
                .collect();
            let stale = fresh.values().any(|current| !current)
                || now.as_ref().is_ok_and(|(scope, current)| {
                    scope.reviewed_version != receipt.scope.reviewed_version
                        || run.round == 2 && current.target_parts != packet.target_parts
                });
            let reason = now
                .err()
                .map(|e| format!("{e:#}"))
                .or_else(|| stale.then(|| "Review input changed after capture".into()));
            result["freshness"] = json!({"stale": stale, "targets": fresh, "reason": reason});
            result["complete"] = json!(run.stage == Stage::Reviewed && run.is_complete() && !stale);
        }
        result["workflow"] = workflow::view(&app, &receipt, result["complete"] == true)?;
        Ok(result)
    })
    .await?
}
