//! Human approvals that run after the requesting turn has returned.
//!
//! A producer binds an immutable snapshot of the operation when it registers the
//! request in the workspace approval queue. Only a person can grant it, and the
//! request has no deadline. The front desk receives the approval ID at once and
//! continues. After a decision, this module checks the snapshot against the
//! digest saved in the approval record, runs it at most once and hands the result
//! to the requesting thread exactly once through the durable desk queue.
//!
//! Each snapshot moves through these phases, saved before the next step:
//!
//! - `waiting`: the request is pending in the approval queue.
//! - `running`: execution is about to claim the grant with `consume_once`. After a
//!   restart, an unconsumed grant returns to `waiting`. A consumed grant has an
//!   unknown outcome. It is reported and never run again.
//! - `finished`: the outcome is saved. The desk turn is not yet queued.
//! - `handed_off`: the desk turn is in the held-work journal. The desk delivers it
//!   only in this phase, so a restart never queues it twice.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{ApprovalDecision, ApprovalOwner, ApprovalState, ApprovalSummary};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::hub::approval_store::Action;
use crate::Workspace;

const DIR: &str = "deferred-approvals";
/// Held-work ID prefix of the desk turn that reports one outcome.
pub(crate) const RESULT_PREFIX: &str = "approval-result-";
pub(crate) const FRONT_DESK: &str = "front_desk";
const RECOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const MISMATCH: &str =
    "The saved request does not match the approved record. Nothing was changed. Request it again if it is still needed.";

/// Tools whose approvals use this module.
pub(crate) const TOOLS: [&str; 1] = [crate::mcp::git_write::integrate::NAME];

#[derive(Default)]
pub(crate) struct Runner {
    // Serializes registration, decisions, execution and hand-off across all snapshots.
    lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Completed,
    /// Nothing was written.
    Failed,
    Denied,
    Withdrawn,
    Expired,
    /// Writing began or may have begun. A person has to review what landed.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub status: Status,
    pub at: i64,
    pub result: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum Phase {
    Waiting,
    Running { started_at: i64 },
    Finished { outcome: Outcome },
    HandedOff { outcome: Outcome, silent: bool },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub approval_id: String,
    pub tool: String,
    pub session: String,
    pub created_at: i64,
    /// The exact operation. Dropped after the hand-off.
    pub payload: Value,
    pub phase: Phase,
}

pub(crate) fn owner(session: &str) -> ApprovalOwner {
    ApprovalOwner { thread: session.into(), job: FRONT_DESK.into(), requester: FRONT_DESK.into() }
}

/// Digest of everything execution reads: the operation, its thread and the whole snapshot.
pub(crate) fn binding(tool: &str, session: &str, payload: &Value) -> String {
    crate::full_approval::fingerprint(&json!({"tool": tool, "thread": session, "snapshot": payload}))
}

/// The decision identity that a snapshot describes.
fn snapshot_action(tool: &str, payload: &Value) -> Result<Action> {
    match tool {
        crate::mcp::git_write::integrate::NAME => crate::mcp::git_write::integrate::snapshot_action(payload),
        _ => bail!("Unsupported deferred approval"),
    }
}

/// Check that a saved snapshot is the one its approval record was registered with.
fn verify(app: &Workspace, entry: &Entry, summary: &ApprovalSummary, action: &Action) -> Result<()> {
    ensure!(summary.owner == owner(&entry.session), "The snapshot belongs to another thread");
    ensure!(entry.tool == action.tool, "The snapshot belongs to another operation");
    ensure!(
        app.hub.approval_binding(&entry.approval_id)? == Some(binding(&entry.tool, &entry.session, &entry.payload)),
        "The snapshot digest differs from the approved record"
    );
    ensure!(snapshot_action(&entry.tool, &entry.payload)? == *action, "The snapshot action differs from the approved action");
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn dir(app: &Workspace) -> PathBuf {
    app.state_dir.join(DIR)
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("apr_").is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn path(dir: &Path, id: &str) -> Result<PathBuf> {
    ensure!(valid_id(id), "Unknown approval ID");
    Ok(dir.join(format!("{id}.json")))
}

fn load(dir: &Path, id: &str) -> Result<Option<Entry>> {
    match std::fs::read(path(dir, id)?) {
        Ok(bytes) => {
            let entry: Entry = serde_json::from_slice(&bytes).context("could not decode a deferred approval")?;
            ensure!(entry.approval_id == id, "A deferred approval is saved under another ID");
            Ok(Some(entry))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("could not read a deferred approval"),
    }
}

fn save(dir: &Path, entry: &Entry) -> Result<()> {
    crate::durable::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec(entry)?)?;
    crate::durable::persist(file, path(dir, &entry.approval_id)?)?;
    Ok(())
}

fn saved_ids(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("could not list deferred approvals"),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")).filter(|id| valid_id(id)) {
            ids.push(id.to_owned());
        }
    }
    ids.sort();
    Ok(ids)
}

/// What a producer supplies to register a deferred request.
pub(crate) struct Request {
    pub registration: crate::hub::approval_store::Registration,
    pub payload: Value,
    /// Snapshot fields shown to the user. A pending card is reused only while they match.
    pub reviewed: &'static [&'static str],
}

/// Where a request stands after registration.
pub(crate) enum Registered {
    /// The card waits for the user. `payload` is the snapshot bound to it.
    Pending { id: String, payload: Value, reused: bool, replaced: Option<String> },
    /// The user has decided an identical request whose result is still on its way.
    Decided { id: String, payload: Value },
}

/// Register the request and bind its snapshot. A pending request for the same
/// operation and the same reviewed fields is reused with the snapshot saved
/// with it. A pending card that shows outdated fields is withdrawn and replaced.
pub(crate) async fn register(app: &Arc<Workspace>, session: &str, request: Request) -> Result<Registered> {
    let tool = request.registration.action.tool.clone();
    ensure!(TOOLS.contains(&tool.as_str()), "This approval cannot wait without a deadline");
    ensure!(request.registration.owner == owner(session), "Deferred approvals belong to the front desk");
    let _guard = app.deferred.lock.lock().await;
    let dir = dir(app);
    let mut replaced = None;
    for entry in open_entries(app, &dir, session, &request.registration.action)? {
        let Some((summary, action)) = app.hub.approval_record(&entry.approval_id)? else { continue };
        let id = entry.approval_id.clone();
        if summary.state != ApprovalState::Pending {
            return Ok(Registered::Decided { id, payload: entry.payload });
        }
        let current = verify(app, &entry, &summary, &action).is_ok()
            && request.reviewed.iter().all(|field| entry.payload.get(field) == request.payload.get(field));
        if current {
            return Ok(Registered::Pending { id, payload: entry.payload, reused: true, replaced });
        }
        if !supersede(app, &dir, session, &entry)? {
            return Ok(Registered::Decided { id, payload: entry.payload });
        }
        replaced = Some(id);
    }
    let digest = binding(&tool, session, &request.payload);
    let mut id = app.hub.register_deferred_approval(request.registration.clone(), digest.clone())?;
    if app.hub.approval_binding(&id)?.as_ref() != Some(&digest) {
        // A pending record whose snapshot was never saved cannot run. Start over.
        app.hub.withdraw_pending_approval(&id, &owner(session), "The saved snapshot is missing")?;
        id = app.hub.register_deferred_approval(request.registration, digest.clone())?;
    }
    // Never bind a snapshot to a record that was already used.
    ensure!(
        app.hub.approval_binding(&id)? == Some(digest)
            && app.hub.approval_record(&id)?.is_some_and(|(summary, _)| summary.consumed_at.is_none()),
        "The approval request could not be registered. Nothing was changed."
    );
    let entry = Entry {
        approval_id: id.clone(), tool, session: session.into(), created_at: now(),
        payload: request.payload, phase: Phase::Waiting,
    };
    if let Err(error) = save(&dir, &entry) {
        let _ = app.hub.withdraw_pending_approval(&id, &owner(session), "The snapshot could not be saved");
        return Err(error);
    }
    Ok(Registered::Pending { id, payload: entry.payload, reused: false, replaced })
}

/// Saved requests of this thread for the operation whose result is not delivered yet.
fn open_entries(app: &Workspace, dir: &Path, session: &str, action: &Action) -> Result<Vec<Entry>> {
    let mut open = Vec::new();
    for id in saved_ids(dir)? {
        let Some((summary, approved)) = app.hub.approval_record(&id)? else { continue };
        if approved != *action || summary.owner != owner(session) {
            continue;
        }
        let Some(entry) = load(dir, &id)? else { continue };
        if !matches!(entry.phase, Phase::HandedOff { .. }) {
            open.push(entry);
        }
    }
    Ok(open)
}

/// Close an outdated card without a result message. False when it was decided first.
fn supersede(app: &Workspace, dir: &Path, session: &str, entry: &Entry) -> Result<bool> {
    let reason = "Replaced by a request that shows the current findings";
    if !app.hub.withdraw_pending_approval(&entry.approval_id, &owner(session), reason)? {
        return Ok(false);
    }
    let outcome = Outcome { status: Status::Withdrawn, at: now(), result: json!({}) };
    let closed = Entry { phase: Phase::HandedOff { outcome, silent: true }, payload: Value::Null, ..entry.clone() };
    save(dir, &closed)?;
    Ok(true)
}

/// Withdraw a pending request owned by the front desk of this thread.
pub(crate) async fn withdraw(app: &Workspace, session: &str, id: &str, reason: Option<String>) -> Result<Value> {
    ensure!(valid_id(id), "Unknown approval ID");
    let _guard = app.deferred.lock.lock().await;
    let dir = dir(app);
    let mut entry = load(&dir, id)?.filter(|entry| entry.session == session).context("Unknown approval ID")?;
    let reason = reason.map(|reason| reason.chars().take(500).collect::<String>())
        .unwrap_or_else(|| "Withdrawn by the front desk".into());
    let summary = app.hub.withdraw_own_approval(id, &owner(session), Some(reason))
        .map_err(|_| anyhow::anyhow!("The approval has already been decided. Wait for its result message."))?;
    ensure!(summary.state == ApprovalState::Withdrawn, "The approval has already been decided");
    let outcome = Outcome { status: Status::Withdrawn, at: now(), result: json!({}) };
    entry.phase = Phase::HandedOff { outcome, silent: true };
    entry.payload = Value::Null;
    save(&dir, &entry)?;
    Ok(json!({"approval_id": id, "status": "withdrawn"}))
}

/// Bring every snapshot forward. Safe to call at any time and after restarts.
pub(crate) async fn recover(app: &Arc<Workspace>) -> Result<()> {
    let dir = dir(app);
    let saved: BTreeSet<_> = saved_ids(&dir)?.into_iter().collect();
    {
        // Requests registered by a process that stopped before saving the snapshot cannot run.
        let _guard = app.deferred.lock.lock().await;
        for tool in TOOLS {
            for id in app.hub.approval_ids_for_tool(tool)? {
                if saved.contains(&id) || load(&dir, &id)?.is_some() {
                    continue;
                }
                let Some((summary, _)) = app.hub.approval_record(&id)? else { continue };
                // Live approvals of the same tool have no digest and are left alone.
                if app.hub.approval_binding(&id)?.is_none() {
                    continue;
                }
                if summary.state == ApprovalState::Pending
                    || summary.state == ApprovalState::Granted && summary.consumed_at.is_none()
                {
                    let _ = app.hub.decide_approval(&id, ApprovalDecision::Withdraw, "server",
                        Some("The request was interrupted before it was saved. Request it again.".into()));
                }
            }
        }
    }
    for id in saved {
        if let Err(error) = process(app, &id).await {
            tracing::warn!(%error, %id, "could not advance a deferred approval");
        }
    }
    Ok(())
}

/// Advance one snapshot after its approval changed.
pub(crate) async fn process(app: &Arc<Workspace>, id: &str) -> Result<()> {
    let _guard = app.deferred.lock.lock().await;
    let dir = dir(app);
    let Some(mut entry) = load(&dir, id)? else { return Ok(()) };
    loop {
        match entry.phase.clone() {
            Phase::HandedOff { .. } => return Ok(()),
            Phase::Finished { outcome } => return hand_off(app, &dir, entry, outcome),
            Phase::Running { .. } => {
                let consumed = app.hub.approval_record(id)?
                    .is_none_or(|(summary, _)| summary.consumed_at.is_some());
                entry.phase = if consumed {
                    finished(Status::Unknown, json!({"needs_review": true, "message": "The server stopped while the approved operation was running. It was not retried. Check what landed before requesting it again."}))
                } else {
                    Phase::Waiting
                };
                save(&dir, &entry)?;
            }
            Phase::Waiting => {
                let Some((summary, action)) = app.hub.approval_record(id)? else {
                    entry.phase = finished(Status::Failed, json!({"message": "The approval record is missing. Nothing was run."}));
                    save(&dir, &entry)?;
                    continue;
                };
                entry.phase = match summary.state {
                    ApprovalState::Pending => return Ok(()),
                    ApprovalState::Granted if summary.consumed_at.is_none() => {
                        if let Err(error) = verify(app, &entry, &summary, &action) {
                            tracing::warn!(%error, %id, "refused a deferred approval");
                            app.hub.decide_approval(id, ApprovalDecision::Withdraw, "server", Some(error.to_string()))?;
                            // The result goes to the thread that owns the approval.
                            entry.session = summary.owner.thread.clone();
                            entry.tool = action.tool.clone();
                            finished(Status::Failed, json!({"message": MISMATCH}))
                        } else {
                            let Some(outcome) = run(app, &dir, &mut entry, &summary.owner, &action).await? else {
                                // Admission is closed for an update or shutdown. Recovery retries after it.
                                return Ok(());
                            };
                            Phase::Finished { outcome }
                        }
                    }
                    ApprovalState::Granted => finished(Status::Unknown, json!({"needs_review": true, "message": "The grant was already used. It was not run again."})),
                    ApprovalState::Denied => finished(Status::Denied, json!({"message": "The user denied the request. Nothing was run."})),
                    ApprovalState::Withdrawn => finished(Status::Withdrawn, json!({"message": "The request was withdrawn. Nothing was run."})),
                    ApprovalState::Expired => finished(Status::Expired, json!({"message": "The request expired. Nothing was run."})),
                };
                save(&dir, &entry)?;
            }
        }
    }
}

fn finished(status: Status, result: Value) -> Phase {
    Phase::Finished { outcome: Outcome { status, at: now(), result } }
}

async fn run(
    app: &Arc<Workspace>, dir: &Path, entry: &mut Entry, owner: &ApprovalOwner, action: &Action,
) -> Result<Option<Outcome>> {
    use crate::mcp::git_write::integrate::{self, Execution};
    // Hold the mutation admission for the execution only. Waiting for the human holds nothing.
    let session = entry.session.clone();
    let mutation = match crate::mutations::Mutation::new(app, &session, "Approved operation".into()) {
        Ok(mutation) => mutation,
        Err(_) => return Ok(None),
    };
    entry.phase = Phase::Running { started_at: now() };
    save(dir, entry)?;
    if !app.hub.consume_once_approval(&entry.approval_id, owner, action)? {
        return Ok(Some(Outcome { status: Status::Failed, at: now(),
            result: json!({"message": "The grant could not be claimed. Nothing was run."}) }));
    }
    #[cfg(test)]
    tests::crash_point("after_consume")?;
    let result = match entry.tool.as_str() {
        integrate::NAME => integrate::execute_snapshot(app, &mutation, &entry.payload).await,
        _ => Err(anyhow::anyhow!("Unsupported deferred approval")),
    };
    drop(mutation);
    let (status, result) = match result {
        Ok(Execution::Completed(result)) => (Status::Completed, result),
        Ok(Execution::NeedsReview(result)) => (Status::Unknown, result),
        Err(error) => (Status::Failed, json!({"message": format!("{error:#}")})),
    };
    Ok(Some(Outcome { status, at: now(), result }))
}

fn hand_off(app: &Arc<Workspace>, dir: &Path, mut entry: Entry, outcome: Outcome) -> Result<()> {
    let id = format!("{RESULT_PREFIX}{}", entry.approval_id);
    let text = report(&entry, &outcome);
    crate::delegate::enqueue_approval_result(app, &entry.session, &id, &entry.approval_id, text)?;
    #[cfg(test)]
    tests::crash_point("after_enqueue")?;
    entry.phase = Phase::HandedOff { outcome, silent: false };
    entry.payload = Value::Null;
    save(dir, &entry)?;
    crate::delegate::wake_durable_desk(app, &entry.session, id);
    Ok(())
}

fn report(entry: &Entry, outcome: &Outcome) -> String {
    let status = match outcome.status {
        Status::Completed => "completed",
        Status::Failed => "failed without changes",
        Status::Denied => "denied by the user",
        Status::Withdrawn => "withdrawn",
        Status::Expired => "expired",
        Status::Unknown => "needs review (outcome unknown; changes may have landed; it was not retried and must not be repeated before checking)",
    };
    let data = json!({
        "approval_id": entry.approval_id,
        "tool": entry.tool,
        "status": outcome.status,
        "result": outcome.result,
    });
    format!("Approval {} for {}: {status}.\n{}", entry.approval_id, entry.tool, data)
}

/// The desk delivers a result only after the hand-off has been saved.
pub(crate) fn ready_for_delivery(app: &Workspace, approval_id: &str) -> bool {
    match load(&dir(app), approval_id) {
        Ok(Some(entry)) => matches!(entry.phase, Phase::HandedOff { silent: false, .. }),
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Run decisions as they arrive and recover after restarts and lost updates.
pub(crate) fn watch(app: &Arc<Workspace>) {
    let weak = Arc::downgrade(app);
    let Ok(mut changes) = app.hub.subscribe_approval_changes() else { return };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RECOVERY_INTERVAL);
        loop {
            let id = tokio::select! {
                change = changes.recv() => match change {
                    Ok(summary) if TOOLS.contains(&summary.action.tool.as_str())
                        && summary.state != ApprovalState::Pending => Some(summary.approval_id),
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => None,
            };
            let Some(app) = weak.upgrade() else { break };
            let result = match id {
                Some(id) => process(&app, &id).await,
                None => recover(&app).await,
            };
            if let Err(error) = result {
                tracing::warn!(%error, "could not advance deferred approvals");
            }
        }
    });
}

#[cfg(test)]
#[path = "deferred_approvals_tests.rs"]
pub(crate) mod tests;
