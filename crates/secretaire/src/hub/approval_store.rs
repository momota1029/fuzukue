//! The workspace approval queue and its durable decision history.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, ensure};
use fuzukue_proto::{
    ApprovalDecision, ApprovalDisplayAction, ApprovalGrantScope, ApprovalOwner, ApprovalRisk,
    ApprovalState, ApprovalSummary, ApprovalUnattended,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

const FILE: &str = "async-approvals.json";

/// The producer resolves tool defaults and normalizes the execution scope before registration.
/// JSON object member order and whitespace are immaterial. String contents remain exact.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Action {
    pub tool: String,
    pub args: Value,
    pub scope: Value,
}

pub(crate) struct Registration {
    pub owner: ApprovalOwner,
    pub risk: ApprovalRisk,
    pub unattended: ApprovalUnattended,
    /// Safe to display to clients.
    pub reason: String,
    pub expires_at: i64,
    pub action: Action,
    /// Supply display values separately so raw execution data stays in the server store.
    pub display_args: Value,
    pub display_scope: Value,
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    summary: ApprovalSummary,
    action: Action,
    history: Vec<AuditEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct AuditEntry {
    at: i64,
    principal: String,
    reason: Option<String>,
    #[serde(flatten)]
    event: AuditEvent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum AuditEvent {
    Registered,
    Deduplicated,
    Granted { scope: ApprovalGrantScope },
    Denied,
    Withdrawn,
    Expired,
    Consumed,
}

impl Record {
    fn audit(&mut self, at: i64, principal: &str, reason: Option<String>, event: AuditEvent) {
        self.history.push(AuditEntry {
            at,
            principal: principal.into(),
            reason,
            event,
        });
    }

    fn unstarted(&self) -> bool {
        matches!(
            self.summary.state,
            ApprovalState::Pending | ApprovalState::Granted
        ) && self.summary.consumed_at.is_none()
    }

    fn deadline(&self) -> i64 {
        match self.summary.grant_scope {
            Some(
                ApprovalGrantScope::Scoped { expires_at }
                | ApprovalGrantScope::TimeLimited { expires_at },
            ) => expires_at,
            _ => self.summary.expires_at,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Store {
    records: Vec<Record>,
}

pub(crate) struct Queue {
    workspace: String,
    dir: PathBuf,
    // The same lock covers deadline checks, durable changes and update publication.
    store: Mutex<Option<Store>>,
    updates: broadcast::Sender<ApprovalSummary>,
}

impl Queue {
    pub(crate) fn new(workspace: String, dir: &Path) -> Self {
        Self {
            workspace,
            dir: dir.into(),
            store: Mutex::new(None),
            updates: broadcast::channel(128).0,
        }
    }

    fn load(&self) -> Result<Store> {
        let bytes = match fs::read(self.dir.join(FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Store::default()),
            Err(error) => return Err(error).context("could not read the approval queue"),
        };
        let store: Store =
            serde_json::from_slice(&bytes).context("could not decode the approval queue")?;
        ensure!(
            store
                .records
                .iter()
                .all(|record| record.summary.workspace == self.workspace),
            "approval queue belongs to another workspace"
        );
        Ok(store)
    }

    fn save(&self, next: &Store) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        // A private temporary file in the same directory makes each state and history change atomic.
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(&serde_json::to_vec(next)?)?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(FILE))?;
        Ok(())
    }

    fn commit(
        &self,
        store: &mut Store,
        next: Store,
        changed: impl IntoIterator<Item = ApprovalSummary>,
    ) -> Result<()> {
        self.save(&next)
            .context("could not save the approval queue")?;
        *store = next;
        for summary in changed {
            let _ = self.updates.send(summary);
        }
        Ok(())
    }

    fn with_store<T>(
        &self,
        clock: impl FnOnce() -> i64,
        run: impl FnOnce(&mut Store, i64) -> Result<T>,
    ) -> Result<T> {
        let mut store = self.store.lock().unwrap();
        if store.is_none() {
            *store = Some(self.load()?);
        }
        let store = store.as_mut().unwrap();
        let now = clock();
        self.close_matching(store, now, |record| record.deadline() <= now, None)?;
        run(store, now)
    }

    fn close_matching(
        &self,
        store: &mut Store,
        now: i64,
        matches: impl Fn(&Record) -> bool,
        withdrawal: Option<(&str, &str)>,
    ) -> Result<()> {
        if !store
            .records
            .iter()
            .any(|record| record.unstarted() && matches(record))
        {
            return Ok(());
        }
        let mut next = store.clone();
        let mut changed = Vec::new();
        for record in next
            .records
            .iter_mut()
            .filter(|record| record.unstarted() && matches(record))
        {
            record.summary.updated_at = now;
            if let Some((principal, reason)) = withdrawal {
                record.summary.state = ApprovalState::Withdrawn;
                record.audit(now, principal, Some(reason.into()), AuditEvent::Withdrawn);
            } else {
                record.summary.state = ApprovalState::Expired;
                record.audit(now, "server", None, AuditEvent::Expired);
            }
            changed.push(record.summary.clone());
        }
        self.commit(store, next, changed)
    }

    pub(crate) fn register(&self, request: Registration) -> Result<String> {
        self.with_store(now, |store, now| {
            ensure!(
                request.expires_at > now,
                "approval request deadline has passed"
            );
            ensure!(
                !request.owner.thread.is_empty()
                    && !request.owner.job.is_empty()
                    && !request.owner.requester.is_empty()
                    && !request.action.tool.is_empty(),
                "approval request requires a thread, job, requester and tool"
            );
            let mut next = store.clone();
            if let Some(record) = next.records.iter_mut().find(|record| {
                let summary = &record.summary;
                summary.state == ApprovalState::Pending
                    && summary.owner == request.owner
                    && summary.risk == request.risk
                    && summary.unattended == request.unattended
                    && record.action == request.action
            }) {
                let id = record.summary.approval_id.clone();
                record.audit(
                    now,
                    &request.owner.requester,
                    None,
                    AuditEvent::Deduplicated,
                );
                self.commit(store, next, [])?;
                return Ok(id);
            }
            let approval_id = loop {
                let id = format!("apr_{:032x}", rand::random::<u128>());
                if next
                    .records
                    .iter()
                    .all(|record| record.summary.approval_id != id)
                {
                    break id;
                }
            };
            let summary = ApprovalSummary {
                approval_id: approval_id.clone(),
                workspace: self.workspace.clone(),
                owner: request.owner,
                risk: request.risk,
                unattended: request.unattended,
                reason: request.reason,
                created_at: now,
                expires_at: request.expires_at,
                updated_at: now,
                state: ApprovalState::Pending,
                grant_scope: None,
                consumed_at: None,
                action: ApprovalDisplayAction {
                    tool: request.action.tool.clone(),
                    args: request.display_args,
                    scope: request.display_scope,
                },
            };
            let mut record = Record {
                summary: summary.clone(),
                action: request.action,
                history: Vec::new(),
            };
            record.audit(now, &summary.owner.requester, None, AuditEvent::Registered);
            next.records.push(record);
            self.commit(store, next, [summary])?;
            Ok(approval_id)
        })
    }

    /// Take the snapshot and subscribe at the same point in the mutation order.
    pub(crate) fn pending(
        &self,
    ) -> Result<(Vec<ApprovalSummary>, broadcast::Receiver<ApprovalSummary>)> {
        self.with_store(now, |store, _| {
            let pending = store
                .records
                .iter()
                .filter(|record| record.summary.state == ApprovalState::Pending)
                .map(|record| record.summary.clone())
                .collect();
            Ok((pending, self.updates.subscribe()))
        })
    }

    pub(crate) fn decide(
        &self,
        id: &str,
        decision: ApprovalDecision,
        principal: &str,
        reason: Option<String>,
    ) -> Result<ApprovalSummary> {
        self.decide_as(id, decision, principal, None, reason)
    }

    pub(crate) fn withdraw_owned(
        &self,
        id: &str,
        owner: &ApprovalOwner,
        reason: Option<String>,
    ) -> Result<ApprovalSummary> {
        self.decide_as(
            id,
            ApprovalDecision::Withdraw,
            &owner.requester,
            Some(owner),
            reason,
        )
    }

    fn decide_as(
        &self,
        id: &str,
        decision: ApprovalDecision,
        principal: &str,
        owner: Option<&ApprovalOwner>,
        reason: Option<String>,
    ) -> Result<ApprovalSummary> {
        self.with_store(now, |store, now| {
            let index = store
                .records
                .iter()
                .position(|record| record.summary.approval_id == id)
                .context("approval request does not exist")?;
            let previous = &store.records[index];
            ensure!(
                owner.is_none_or(|owner| previous.summary.owner == *owner),
                "approval request does not exist"
            );
            let (state, scope, event) = match decision {
                ApprovalDecision::Grant { scope }
                    if previous.summary.state == ApprovalState::Pending =>
                {
                    if let ApprovalGrantScope::Scoped { expires_at }
                    | ApprovalGrantScope::TimeLimited { expires_at } = scope
                    {
                        ensure!(expires_at > now, "approval grant deadline has passed");
                    }
                    (
                        ApprovalState::Granted,
                        Some(scope),
                        AuditEvent::Granted { scope },
                    )
                }
                ApprovalDecision::Deny if previous.summary.state == ApprovalState::Pending => {
                    (ApprovalState::Denied, None, AuditEvent::Denied)
                }
                ApprovalDecision::Withdraw if previous.unstarted() => (
                    ApprovalState::Withdrawn,
                    previous.summary.grant_scope,
                    AuditEvent::Withdrawn,
                ),
                _ => anyhow::bail!("approval request has already been decided or consumed"),
            };
            let mut next = store.clone();
            let record = &mut next.records[index];
            record.summary.state = state;
            record.summary.grant_scope = scope;
            record.summary.updated_at = now;
            record.audit(now, principal, reason, event);
            let summary = record.summary.clone();
            self.commit(store, next, [summary.clone()])?;
            Ok(summary)
        })
    }

    pub(crate) fn withdraw_for(&self, thread: &str, job: Option<&str>, reason: &str) -> Result<()> {
        self.with_store(now, |store, now| {
            self.close_matching(
                store,
                now,
                |record| {
                    record.summary.owner.thread == thread
                        && job.is_none_or(|job| record.summary.owner.job == job)
                },
                Some(("server", reason)),
            )
        })
    }

    pub(crate) fn expire(&self) -> Result<()> {
        self.expire_at(now)
    }

    fn expire_at(&self, clock: impl FnOnce() -> i64) -> Result<()> {
        self.with_store(clock, |_, _| Ok(()))
    }

    pub(crate) fn consume_once(
        &self,
        id: &str,
        owner: &ApprovalOwner,
        action: &Action,
    ) -> Result<bool> {
        self.consume_once_at(id, owner, action, now)
    }

    fn consume_once_at(
        &self,
        id: &str,
        owner: &ApprovalOwner,
        action: &Action,
        clock: impl FnOnce() -> i64,
    ) -> Result<bool> {
        self.with_store(clock, |store, now| {
            let Some(index) = store.records.iter().position(|record| {
                let summary = &record.summary;
                summary.approval_id == id
                    && summary.owner == *owner
                    && record.action == *action
                    && summary.state == ApprovalState::Granted
                    && summary.grant_scope == Some(ApprovalGrantScope::Once)
                    && summary.consumed_at.is_none()
            }) else {
                return Ok(false);
            };
            let mut next = store.clone();
            let record = &mut next.records[index];
            record.summary.consumed_at = Some(now);
            record.summary.updated_at = now;
            record.audit(now, &owner.requester, None, AuditEvent::Consumed);
            let summary = record.summary.clone();
            self.commit(store, next, [summary])?;
            Ok(true)
        })
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
#[path = "approval_store_tests.rs"]
mod tests;
