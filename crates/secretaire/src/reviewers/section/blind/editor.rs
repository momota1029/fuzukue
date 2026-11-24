//! Explicit editor actions authenticated by the server's device or shared bearer credential.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use fuzukue_proto::{
    ReviewFindingChange, ReviewFindingChangeReference, ReviewFindingStatus as Status,
    ReviewFindingTransitionAction,
};

use crate::remote::ReviewEditor;
use crate::review_findings::{self as ledger, UpdateArgs};
use crate::{App, WorkspaceQuery};

use super::*;
use report::{Criterion, Location};
use workflow::{Append, Decision, DecisionAction, Workflow};

pub(crate) const ROUTE: &str = "/section-review/editor";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Decide {
        session: String,
        finding: String,
        reviewed_version: String,
        input_hash: String,
        choice: Choice,
        reason: String,
        append_to: Option<Append>,
    },
    Fix {
        session: String,
        finding: String,
        recheck_session: String,
        reviewed_version: String,
        commit: String,
        checked: Vec<Checked>,
        expected_revision: String,
        reason: String,
    },
    Defer {
        session: String,
        finding: String,
        expected_revision: String,
        reason: String,
    },
    Stop {
        session: String,
        reason: StopReason,
        detail: String,
    },
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Choice {
    Accept,
    Reject,
    Hold,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checked {
    card: String,
    location: Location,
    criterion: Criterion,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    ApprovalTargetUnmet,
    BudgetStop,
    UserStop,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Stop {
    pub(super) reason: StopReason,
    detail: String,
    editor: String,
    time: i64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Repair {
    pub(super) action: ReviewFindingTransitionAction,
    review: Option<RepairReview>,
    expected_revision: String,
    pub(super) completed: bool,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct RepairReview {
    session: String,
    version: String,
    input_hash: String,
    checked: Vec<Checked>,
}

pub(crate) async fn route(
    State(registry): State<Arc<App>>,
    Query(query): Query<WorkspaceQuery>,
    editor: Option<Extension<ReviewEditor>>,
    Json(action): Json<Action>,
) -> Response {
    let Some(Extension(editor)) = editor else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let app = match registry.route(query.workspace).await {
        Ok(app) => app,
        Err(error) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("{error:#}")})),
            )
                .into_response()
        }
    };
    match apply(app, editor, action).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("{error:#}")})),
        )
            .into_response(),
    }
}

async fn apply(app: Arc<Workspace>, editor: ReviewEditor, action: Action) -> Result<Value> {
    ensure!(
        !crate::canary(),
        "Editor review actions are unavailable in canary mode"
    );
    let work = app.admit_work()?;
    let save = app.file_save.clone().lock_owned().await;
    // Keep the lock and complete an authorized edit if the client disconnects mid-request.
    tokio::spawn(async move {
        let (_work, _save) = (work, save);
        let session = match &action {
            Action::Decide { session, .. }
            | Action::Fix { session, .. }
            | Action::Defer { session, .. }
            | Action::Stop { session, .. } => session,
        };
        let first = workflow::initial(&app, session, None)?;
        let mut state = Workflow::read(&app, &first)?;
        match action {
            Action::Decide {
                finding,
                reviewed_version,
                input_hash,
                choice,
                reason,
                append_to,
                ..
            } => {
                ensure!(state.stop.is_none(), "This review workflow has ended");
                ensure!(
                    reviewed_version == first.scope.reviewed_version
                        && input_hash == first.review.input_hash,
                    "Approval version or input hash does not match the captured response"
                );
                ensure!(
                    !reason.trim().is_empty(),
                    "An editor decision requires a reason"
                );
                ensure!(
                    choice == Choice::Accept || append_to.is_none(),
                    "Only acceptance can append a source"
                );
                let candidate = workflow::candidate(&first, &finding)?;
                if let Some(old) = state.decisions.get(&finding) {
                    let same = match (&old.action, choice) {
                        (DecisionAction::Accept { append, .. }, Choice::Accept) => {
                            append == &append_to
                        }
                        (DecisionAction::Reject, Choice::Reject)
                        | (DecisionAction::Hold, Choice::Hold) => true,
                        _ => false,
                    };
                    if same && old.reason == reason && old.editor == editor.0 {
                        return Ok(json!({"decision": old}));
                    }
                    if let DecisionAction::Accept { id, registered, .. } = &old.action {
                        let saved = ledger::read_one(&app, &first.deck, id)?;
                        ensure!(
                            !registered
                                && !saved.is_some_and(|(item, _)| item.content.sources.iter().any(
                                    |source| {
                                        source.finding.as_deref() == Some(&finding)
                                            && source.review.as_deref()
                                                == Some(first.review.session.as_str())
                                    }
                                )),
                            "This approved finding is already registered"
                        );
                    }
                }
                ensure!(
                    workflow::recheck(&app, &first)?.is_none(),
                    "The linked recheck has already started"
                );
                let action = match choice {
                    Choice::Reject => DecisionAction::Reject,
                    Choice::Hold => DecisionAction::Hold,
                    Choice::Accept => {
                        workflow::complete_coverage(&first)?;
                        require_current(&app, &first)?;
                        let mut content = workflow::content(&first, candidate)?;
                        let id = if let Some(append) = &append_to {
                            let (existing, revision) =
                                ledger::read_one(&app, &first.deck, &append.id)?
                                    .context("Append target does not exist")?;
                            ensure!(
                                revision == append.expected_revision,
                                "Append target revision changed. Read it again."
                            );
                            let source = content.sources.pop().expect("one captured source");
                            let location = &content.locations[0];
                            ensure!(
                                existing.status == Status::Open
                                    && existing.content.locations.iter().any(|old| {
                                        old.card == location.card
                                            && old.file == location.file
                                            && old.lines == location.lines
                                    }),
                                "Append target must be open and match the reviewed location"
                            );
                            content = existing.content;
                            ensure!(
                                !content.sources.contains(&source),
                                "This source is already present"
                            );
                            content.sources.push(source);
                            append.id.clone()
                        } else {
                            workflow::finding_id(&first, &finding)?
                        };
                        DecisionAction::Accept {
                            id,
                            content,
                            append: append_to,
                            registered: false,
                        }
                    }
                };
                let decision = Decision {
                    editor: editor.0,
                    time: usage::now(),
                    reason,
                    reviewed_version,
                    input_hash,
                    action,
                };
                if let Some(previous) = state.decisions.insert(finding.clone(), decision) {
                    state.previous_decisions.push((finding.clone(), previous));
                }
                state.save(&app, &first)?;
                Ok(json!({"decision": state.decisions[&finding]}))
            }
            Action::Fix {
                finding,
                recheck_session,
                reviewed_version,
                commit,
                checked,
                expected_revision,
                reason,
                ..
            } => {
                ensure!(
                    !reason.trim().is_empty(),
                    "Repair confirmation requires a reason"
                );
                let repair = verify_repair(
                    &app,
                    &first,
                    &finding,
                    &recheck_session,
                    &reviewed_version,
                    &commit,
                    checked,
                )
                .await?;
                let repair = Repair {
                    action: ReviewFindingTransitionAction {
                        status: Status::Fixed,
                        actor: editor.0,
                        reason: Some(reason.clone()),
                        changes: vec![ReviewFindingChange {
                            summary: reason,
                            reference: ReviewFindingChangeReference::Commit { oid: commit },
                            locations: repair.1,
                        }],
                    },
                    review: Some(repair.0),
                    expected_revision,
                    completed: false,
                };
                write_repair(&app, &first, &mut state, finding, repair)
            }
            Action::Defer {
                finding,
                expected_revision,
                reason,
                ..
            } => {
                ensure!(!reason.trim().is_empty(), "Deferring requires a reason");
                let repair = Repair {
                    action: ReviewFindingTransitionAction {
                        status: Status::Deferred,
                        actor: editor.0,
                        reason: Some(reason),
                        changes: Vec::new(),
                    },
                    review: None,
                    expected_revision,
                    completed: false,
                };
                write_repair(&app, &first, &mut state, finding, repair)
            }
            Action::Stop { reason, detail, .. } => {
                ensure!(!detail.trim().is_empty(), "Stopping requires a reason");
                if let Some(stop) = &state.stop {
                    ensure!(
                        stop.reason == reason && stop.detail == detail && stop.editor == editor.0,
                        "This review workflow already has a stop decision"
                    );
                } else {
                    state.stop = Some(Stop {
                        reason,
                        detail,
                        editor: editor.0,
                        time: usage::now(),
                    });
                    state.save(&app, &first)?;
                    let active = app.reviews.blind.lock().unwrap();
                    for session in [&first.review.session, &workflow::recheck_session(&first)] {
                        if let Some(run) = active.get(session) {
                            run.cancel();
                        }
                    }
                }
                workflow::view(
                    &app,
                    &first,
                    workflow::complete_coverage(&first).is_ok()
                        && require_current(&app, &first).is_ok(),
                )
            }
        }
    })
    .await?
}

async fn verify_repair(
    app: &Workspace,
    first: &Receipt,
    id: &str,
    recheck_session: &str,
    version: &str,
    oid: &str,
    checked: Vec<Checked>,
) -> Result<(RepairReview, Vec<fuzukue_proto::ReviewFindingLocation>)> {
    let initial = workflow::candidate(first, id)?;
    let recheck = workflow::recheck(app, first)?.context("There is no linked recheck")?;
    let run = recheck.blind.as_ref().context("Expected a blind recheck")?;
    let link = run.recheck.as_ref().context("Expected a linked recheck")?;
    ensure!(
        link.initial_session == first.review.session
            && recheck.review.session == recheck_session
            && recheck.scope.reviewed_version == version
            && link.commit == oid,
        "Repair must match the linked recheck session, version and full commit OID"
    );
    let coverage = workflow::complete_coverage(&recheck)?;
    require_current(app, &recheck).context("Review input changed after recheck")?;
    let packet = run.packet.as_ref().context("Missing recheck packet")?;
    workflow::verify_commit(app, packet, oid).await?;
    require_current(app, &recheck).context("Review input changed after recheck")?;
    let required = link
        .affected
        .by_finding
        .get(id)
        .context("This finding has no recheck scope")?;
    let previous = first
        .blind
        .as_ref()
        .and_then(|run| run.packet.as_ref())
        .context("Missing initial packet")?;
    let old = report::location(previous, &initial.location)?;
    ensure!(
        checked
            .iter()
            .map(|check| &check.card)
            .collect::<BTreeSet<_>>()
            == required.iter().collect(),
        "Repair confirmation must name checked lines for the finding and each affected use"
    );

    let mut locations = Vec::new();
    let mut original_criterion = false;
    for check in &checked {
        let target = coverage
            .targets
            .iter()
            .find(|target| target.card == check.card)
            .context("No coverage for the affected card")?;
        ensure!(
            target.verdict == Verdict::Pass,
            "Recheck has unresolved findings for {}",
            check.card
        );
        let excerpt = report::location(packet, &check.location)?;
        ensure!(
            (check.location.lines[0]..=check.location.lines[1]).all(|line| target
                .checked
                .iter()
                .any(|item| {
                    item.location.excerpt == check.location.excerpt
                        && item.criteria.contains(&check.criterion)
                        && (item.location.lines[0]..=item.location.lines[1]).contains(&line)
                })),
            "Repair lines and criteria must match verified recheck coverage"
        );
        original_criterion |= check.card == initial.card
            && excerpt.part == old.part
            && check.criterion == initial.criterion;
        locations.push(workflow::ledger_location(
            &check.card,
            version,
            excerpt,
            check.location.lines,
        )?);
    }
    ensure!(
        original_criterion
            && previous.fingerprints.get(&initial.card) != packet.fingerprints.get(&initial.card),
        "Repair confirmation must recheck the original finding's criterion against changed input"
    );
    Ok((
        RepairReview {
            session: recheck_session.into(),
            version: version.into(),
            input_hash: recheck.review.input_hash,
            checked,
        },
        locations,
    ))
}

fn write_repair(
    app: &Workspace,
    first: &Receipt,
    state: &mut Workflow,
    finding: String,
    repair: Repair,
) -> Result<Value> {
    let DecisionAction::Accept {
        id,
        content,
        registered: true,
        ..
    } = &state
        .decisions
        .get(&finding)
        .context("No editor approval for this finding")?
        .action
    else {
        bail!("This finding must be approved and registered");
    };
    let (actual, revision) =
        ledger::read_one(app, &first.deck, id)?.context("Registered finding is missing")?;
    let action = serde_json::to_value(&repair.action)?;
    let already_saved = if let Some(existing) = state.repairs.get(&finding) {
        ensure!(
            existing.review == repair.review
                && existing.expected_revision == repair.expected_revision
                && serde_json::to_value(&existing.action)? == action,
            "The finding already has a different repair decision"
        );
        let applied = serde_json::to_value(&actual.transitions.last().unwrap().action)? == action;
        if existing.completed {
            ensure!(applied, "The registered finding has changed in the ledger");
            return ledger::read_result(app, &first.deck, id);
        }
        applied
    } else {
        false
    };
    if !already_saved {
        ensure!(state.stop.is_none(), "This review workflow has ended");
        ensure!(actual.status == Status::Open && revision == repair.expected_revision
            && workflow::same_content(&actual.content, content)
            && content.sources.iter().all(|source| actual.content.sources.contains(source)),
            "The finding changed after approval. Read the current ledger revision and compare the content.");
    }
    let id = id.clone();
    let request = UpdateArgs {
        deck: None,
        id: id.clone(),
        expected_revision: repair.expected_revision.clone(),
        finding: None,
        transition: Some(repair.action.clone()),
    };
    state.repairs.insert(finding.clone(), repair);
    state.save(app, first)?;
    let result = if already_saved {
        ledger::read_result(app, &first.deck, &id)?
    } else {
        ledger::run_locked(app, &first.deck, ledger::Request::Update(request))?
    };
    state.repairs.get_mut(&finding).unwrap().completed = true;
    state.save(app, first)?;
    Ok(result)
}
