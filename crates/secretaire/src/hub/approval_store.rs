//! The workspace approval queue and its durable decision history.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

use anyhow::{Context, Result, ensure};
use fuzukue_proto::{
    ApprovalDecision, ApprovalDisplayAction, ApprovalGrantScope, ApprovalOwner, ApprovalRisk,
    ApprovalState, ApprovalSummary, ApprovalUnattended, FullApprovalAudit, FullApprovalChange,
    FullApprovalDuration, FullApprovalMode, FullApprovalSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

const FILE: &str = "async-approvals.json";
const FULL_MODE_FILE: &str = "full-approval-mode.json";
const FULL_MODE_PRINCIPAL: &str = "full_mode";
const HOUR_MS: i64 = 60 * 60 * 1000;
const AUDIT_PAGE: usize = 50;

#[derive(Clone, Serialize, Deserialize)]
struct ActiveMode {
    id: String,
    settings: FullApprovalMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FullAuthorization {
    Human,
    Granted,
    AlreadyConsumed,
}

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
    Granted {
        scope: ApprovalGrantScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full_mode_id: Option<String>,
    },
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

    fn full_mode_id(&self) -> Option<&str> {
        self.history.iter().find_map(|entry| match &entry.event {
            AuditEvent::Granted { full_mode_id, .. } => full_mode_id.as_deref(),
            _ => None,
        })
    }

    fn grant_full(&mut self, now: i64, mode: &ActiveMode) {
        self.summary.state = ApprovalState::Granted;
        self.summary.grant_scope = Some(ApprovalGrantScope::Once);
        self.summary.updated_at = now;
        self.audit(now, FULL_MODE_PRINCIPAL, None, AuditEvent::Granted {
            scope: ApprovalGrantScope::Once,
            full_mode_id: Some(mode.id.clone()),
        });
    }

    fn consume(&mut self, now: i64) {
        self.summary.consumed_at = Some(now);
        self.summary.updated_at = now;
        let requester = self.summary.owner.requester.clone();
        self.audit(now, &requester, None, AuditEvent::Consumed);
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
    full: RwLock<Option<Option<ActiveMode>>>,
    mode_changes: tokio::sync::watch::Sender<()>,
    updates: broadcast::Sender<ApprovalSummary>,
}

impl Queue {
    pub(crate) fn new(workspace: String, dir: &Path) -> Self {
        Self {
            workspace,
            dir: dir.into(),
            store: Mutex::new(None),
            full: RwLock::new(None),
            mode_changes: tokio::sync::watch::channel(()).0,
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
        self.write_json(FILE, next)
    }

    fn write_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        // Use a private file in the same directory for atomic replacement.
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(name))?;
        Ok(())
    }

    fn full_mode(&self, now: i64) -> Result<Option<ActiveMode>> {
        if let Some(mode) = &*self.full.read().unwrap() {
            return Ok(mode.clone().filter(|mode| mode.settings.expires_at.is_none_or(|deadline| now < deadline)));
        }
        let mut cached = self.full.write().unwrap();
        let mode = match &*cached {
            Some(mode) => mode.clone(),
            None => {
                let mode = self.load_full_mode()?;
                *cached = Some(mode.clone());
                mode
            }
        };
        Ok(mode.filter(|mode| mode.settings.expires_at.is_none_or(|deadline| now < deadline)))
    }

    pub(crate) fn full_mode_lease(&self) -> Result<Option<(FullApprovalMode, tokio::sync::watch::Receiver<()>)>> {
        let _serial = self.store.lock().unwrap();
        let updates = self.mode_changes.subscribe();
        Ok(self.full_mode(now())?.map(|mode| (mode.settings, updates)))
    }

    fn load_full_mode(&self) -> Result<Option<ActiveMode>> {
        let bytes = match fs::read(self.dir.join(FULL_MODE_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("could not read full approval settings"),
        };
        serde_json::from_slice(&bytes).context("could not decode full approval settings")
    }

    pub(crate) fn set_full_mode(
        &self,
        change: FullApprovalChange,
        principal: &str,
    ) -> Result<Option<FullApprovalMode>> {
        self.set_full_mode_at(change, principal, now)
    }

    fn set_full_mode_at(
        &self,
        change: FullApprovalChange,
        principal: &str,
        clock: impl FnOnce() -> i64,
    ) -> Result<Option<FullApprovalMode>> {
        let _serial = self.store.lock().unwrap();
        let at = clock();
        let active = match change {
            FullApprovalChange::Off => None,
            FullApprovalChange::Full { duration } => {
                let hours = match duration {
                    FullApprovalDuration::OneHour => Some(1),
                    FullApprovalDuration::EightHours => Some(8),
                    FullApprovalDuration::TwentyFourHours => Some(24),
                    FullApprovalDuration::UntilDisabled => None,
                };
                Some(ActiveMode {
                    id: crate::mail::session_key(),
                    settings: FullApprovalMode {
                        duration,
                        enabled_at: at,
                        expires_at: hours.map(|hours| at + hours * HOUR_MS),
                        enabled_by: principal.into(),
                    },
                })
            }
        };
        self.write_json(FULL_MODE_FILE, &active)?;
        *self.full.write().unwrap() = Some(active.clone());
        self.mode_changes.send_replace(());
        Ok(active.map(|mode| mode.settings))
    }

    pub(crate) fn full_settings(&self, before: Option<usize>) -> Result<FullApprovalSettings> {
        self.with_store(now, |store, now| {
            let active = self.full_mode(now)?.map(|mode| mode.settings);
            let mut counts = std::collections::BTreeMap::new();
            let mut audit = Vec::new();
            for record in &store.records {
                for entry in &record.history {
                    if !matches!(entry.event, AuditEvent::Granted { full_mode_id: Some(_), .. }) {
                        continue;
                    }
                    let summary = &record.summary;
                    if entry.at >= now - 24 * HOUR_MS {
                        *counts.entry(summary.action.tool.clone()).or_insert(0) += 1;
                    }
                    audit.push(FullApprovalAudit {
                        approval_id: summary.approval_id.clone(),
                        at: entry.at,
                        granted_by: FULL_MODE_PRINCIPAL.into(),
                        thread: summary.owner.thread.clone(),
                        job: summary.owner.job.clone(),
                        risk: summary.risk,
                        action: ApprovalDisplayAction {
                            // Never expose raw arguments, including display data from legacy producers.
                            args: Value::String("[redacted]".into()),
                            ..summary.action.clone()
                        },
                    });
                }
            }
            audit.sort_by_key(|entry| entry.at);
            let end = before.unwrap_or(audit.len()).min(audit.len());
            let start = end.saturating_sub(AUDIT_PAGE);
            let audit = audit.drain(start..end).rev().collect();
            Ok(FullApprovalSettings {
                active,
                audit,
                next_before: (start > 0).then_some(start),
                last_24_hours_by_tool: counts,
            })
        })
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
        Ok(self.register_inner(request, false)?.0)
    }

    /// Grant and claim the exact operation of a live caller in one durable mutation.
    pub(crate) fn authorize_full(&self, request: Registration) -> Result<FullAuthorization> {
        Ok(self.register_inner(request, true)?.1)
    }

    fn register_inner(&self, request: Registration, live: bool) -> Result<(String, FullAuthorization)> {
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
            let mode = if eligible_for_full(&request) { self.full_mode(now)? } else { None };
            if live && mode.is_none() {
                return Ok((String::new(), FullAuthorization::Human));
            }
            let mut next = store.clone();
            if let Some(record) = next.records.iter_mut().rev().find(|record| {
                let summary = &record.summary;
                let same_grant = mode.as_ref().is_some_and(|mode| {
                    summary.state == ApprovalState::Granted
                        && record.full_mode_id() == Some(&mode.id)
                        && (summary.consumed_at.is_none() || live)
                        && record.deadline() > now
                });
                (summary.state == ApprovalState::Pending || same_grant)
                    && summary.owner == request.owner
                    && summary.risk == request.risk
                    && summary.unattended == request.unattended
                    && record.action == request.action
            }) {
                let id = record.summary.approval_id.clone();
                record.audit(now, &request.owner.requester, None, AuditEvent::Deduplicated);
                let mut changed = None;
                let mut authorization = FullAuthorization::Human;
                if let Some(mode) = &mode {
                    if record.summary.consumed_at.is_some() {
                        authorization = FullAuthorization::AlreadyConsumed;
                    } else {
                        if record.summary.state == ApprovalState::Pending {
                            record.grant_full(now, mode);
                            changed = Some(record.summary.clone());
                        }
                        if live {
                            record.consume(now);
                            authorization = FullAuthorization::Granted;
                            changed = Some(record.summary.clone());
                        }
                    }
                }
                self.commit(store, next, changed)?;
                return Ok((id, authorization));
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
            let mut authorization = FullAuthorization::Human;
            if let Some(mode) = &mode {
                record.grant_full(now, mode);
                if live {
                    record.consume(now);
                    authorization = FullAuthorization::Granted;
                }
            }
            let summary = record.summary.clone();
            next.records.push(record);
            self.commit(store, next, [summary])?;
            Ok((approval_id, authorization))
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
                        AuditEvent::Granted { scope, full_mode_id: None },
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
            if let Some(mode_id) = store.records[index].full_mode_id() {
                if self.full_mode(now)?.is_none_or(|mode| mode.id != mode_id) {
                    return Ok(false);
                }
            }
            let mut next = store.clone();
            let record = &mut next.records[index];
            record.consume(now);
            let summary = record.summary.clone();
            self.commit(store, next, [summary])?;
            Ok(true)
        })
    }
}

// Producers attach the server's own worker or front-desk identity.
fn eligible_for_full(request: &Registration) -> bool {
    request.owner.requester != "front_desk"
        || (request.unattended != ApprovalUnattended::NeverUnattended
            && matches!(request.action.tool.as_str(),
                "folder_read" | "git_integrate_patch" | "github_comment" | "github_label"
                | "github_issue_create" | "github_issue_close"
            ))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
#[path = "approval_store_tests.rs"]
mod tests;
