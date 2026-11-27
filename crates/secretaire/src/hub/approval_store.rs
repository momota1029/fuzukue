//! The workspace approval queue and its durable decision history.

use std::{
    collections::HashSet,
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, ensure};
use fuzukue_proto::{
    ApprovalDecision, ApprovalDisplayAction, ApprovalGrantScope, ApprovalOwner, ApprovalRisk,
    ApprovalState, ApprovalSummary, ApprovalUnattended, FolderGrant, FullApprovalAudit, FullApprovalChange,
    FullApprovalDuration, FullApprovalMode, FullApprovalSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

const FILE: &str = "async-approvals.json";
const FULL_MODE_FILE: &str = "full-approval-mode.json";
const FULL_MODE_PRINCIPAL: &str = "full_mode";
const THREAD_MODE_PRINCIPAL: &str = "thread_mode";
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

#[derive(Clone)]
pub(crate) struct Registration {
    pub owner: ApprovalOwner,
    pub thread_auto: bool,
    pub tool_item: Option<String>,
    pub anchor_item: Option<String>,
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
    /// Digest of the server-side snapshot a deferred request is bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<String>,
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

    fn was_granted(&self) -> bool {
        self.history.iter().any(|entry| matches!(entry.event, AuditEvent::Granted { .. }))
    }

    fn was_granted_by_full_mode(&self) -> bool {
        self.history.iter().any(|entry| matches!(entry.event, AuditEvent::Granted { full_mode_id: Some(_), .. }))
    }

    fn same_operation(&self, owner: &ApprovalOwner, action: &Action) -> bool {
        self.summary.owner == *owner && self.action == *action
    }

    fn grant_full(&mut self, now: i64, mode: &ActiveMode) {
        self.grant_automatic(now, FULL_MODE_PRINCIPAL, &mode.id);
    }

    fn grant_automatic(&mut self, now: i64, principal: &str, mode_id: &str) {
        self.summary.state = ApprovalState::Granted;
        self.summary.automatic = true;
        self.summary.grant_scope = Some(ApprovalGrantScope::Once);
        self.summary.updated_at = now;
        self.audit(now, principal, None, AuditEvent::Granted {
            scope: ApprovalGrantScope::Once,
            full_mode_id: Some(mode_id.into()),
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

#[derive(Clone, Serialize, Deserialize)]
struct ModeState {
    active: Option<ActiveMode>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Store {
    records: Vec<Record>,
    // Absent in queue files written before the mode and grants shared a transaction.
    #[serde(default)]
    full_mode: Option<ModeState>,
    #[serde(default)]
    folder_sweep: Vec<FolderGrant>,
}

impl Store {
    fn active_mode(&self, now: i64) -> Option<&ActiveMode> {
        self.full_mode.as_ref()?.active.as_ref()
            .filter(|mode| mode.settings.expires_at.is_none_or(|deadline| now < deadline))
    }

    fn grant_pending(&mut self, now: i64, mode: &ActiveMode) -> Vec<ApprovalSummary> {
        let mut changed = Vec::new();
        for index in 0..self.records.len() {
            let record = &self.records[index];
            let summary = &record.summary;
            if summary.state != ApprovalState::Pending || summary.expires_at <= now
                || !eligible_for_full(&summary.owner, summary.unattended, &record.action.tool)
            {
                continue;
            }
            let duplicate = self.records.iter().any(|previous| {
                previous.same_operation(&summary.owner, &record.action) && previous.was_granted()
            });
            let record = &mut self.records[index];
            if duplicate {
                record.summary.state = ApprovalState::Withdrawn;
                record.summary.updated_at = now;
                record.audit(now, FULL_MODE_PRINCIPAL, Some("Operation already granted".into()), AuditEvent::Withdrawn);
            } else {
                record.grant_full(now, mode);
            }
            changed.push(record.summary.clone());
        }
        changed
    }
}

pub(crate) struct Queue {
    workspace: String,
    dir: PathBuf,
    // The same lock covers deadline checks, durable changes and update publication.
    store: Mutex<Option<Store>>,
    mode_changes: tokio::sync::watch::Sender<()>,
    updates: broadcast::Sender<ApprovalSummary>,
}

impl Queue {
    pub(crate) fn new(workspace: String, dir: &Path) -> Self {
        Self {
            workspace,
            dir: dir.into(),
            store: Mutex::new(None),
            mode_changes: tokio::sync::watch::channel(()).0,
            updates: broadcast::channel(128).0,
        }
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.dir
    }

    fn load(&self) -> Result<Store> {
        let bytes = match fs::read(self.dir.join(FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Store::default()),
            Err(error) => return Err(error).context("could not read the approval queue"),
        };
        let mut store: Store =
            serde_json::from_slice(&bytes).context("could not decode the approval queue")?;
        for record in &mut store.records {
            record.summary.automatic |= record.was_granted_by_full_mode();
        }
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
        crate::durable::persist(file, self.dir.join(name))?;
        Ok(())
    }

    pub(crate) fn full_mode_lease(&self) -> Result<Option<(FullApprovalMode, tokio::sync::watch::Receiver<()>)>> {
        self.with_store(now, |store, now| {
            Ok(store.active_mode(now).map(|mode| (mode.settings.clone(), self.mode_changes.subscribe())))
        })
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
        legacy: Vec<(String, Registration)>,
        folders: Vec<FolderGrant>,
    ) -> Result<(Option<FullApprovalMode>, HashSet<String>)> {
        self.set_full_mode_at(change, principal, now, legacy, folders)
    }

    fn set_full_mode_at(
        &self,
        change: FullApprovalChange,
        principal: &str,
        clock: impl FnOnce() -> i64,
        legacy: Vec<(String, Registration)>,
        folders: Vec<FolderGrant>,
    ) -> Result<(Option<FullApprovalMode>, HashSet<String>)> {
        self.with_store(clock, |store, at| {
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
            let mut next = store.clone();
            let mut changed = Vec::new();
            let mut granted = HashSet::new();
            if let Some(mode) = &active {
                changed = next.grant_pending(at, mode);
                for (id, mut registration) in legacy {
                    if next.records.iter().any(|previous| {
                        previous.same_operation(&registration.owner, &registration.action) && previous.was_granted()
                    }) {
                        continue;
                    }
                    registration.expires_at = at + HOUR_MS;
                    let mut record = self.new_record(id.clone(), registration, at);
                    record.grant_full(at, mode);
                    record.consume(at);
                    next.records.push(record);
                    granted.insert(id);
                }
                next.folder_sweep.extend(folders.into_iter().filter(|grant| granted.contains(&grant.id)));
            }
            next.full_mode = Some(ModeState { active: active.clone() });
            self.commit(store, next, changed)?;
            self.mode_changes.send_replace(());
            Ok((active.map(|mode| mode.settings), granted))
        })
    }

    pub(crate) fn folder_sweep(&self) -> Result<Vec<FolderGrant>> {
        self.with_store(now, |store, _| Ok(store.folder_sweep.clone()))
    }

    pub(crate) fn stage_thread_folders(&self, folders: Vec<FolderGrant>) -> Result<()> {
        self.with_store(now, |store, at| {
            let (principal, mode_id) = store.active_mode(at).map_or(
                (THREAD_MODE_PRINCIPAL, THREAD_MODE_PRINCIPAL),
                |mode| (FULL_MODE_PRINCIPAL, mode.id.as_str()),
            );
            let mut next = store.clone();
            for mut grant in folders {
                if next.records.iter().any(|record| record.summary.approval_id == grant.id) {
                    continue;
                }
                let registration = super::folder_grants::full_registration(
                    &grant.session, None, &grant.path, &grant.id,
                );
                let mut record = self.new_record(grant.id.clone(), registration, at);
                record.grant_automatic(at, principal, mode_id);
                record.consume(at);
                grant.granted_by = principal.into();
                next.records.push(record);
                next.folder_sweep.push(grant);
            }
            self.commit(store, next, [])
        })
    }

    pub(crate) fn finish_folder_sweep(&self) -> Result<()> {
        self.with_store(now, |store, _| {
            if store.folder_sweep.is_empty() {
                return Ok(());
            }
            let mut next = store.clone();
            next.folder_sweep.clear();
            self.commit(store, next, [])
        })
    }

    pub(crate) fn full_settings(&self, before: Option<usize>) -> Result<FullApprovalSettings> {
        self.with_store(now, |store, now| {
            let active = store.active_mode(now).map(|mode| mode.settings.clone());
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
                        granted_by: entry.principal.clone(),
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
        let now = clock();
        if store.is_none() {
            let mut loaded = self.load()?;
            if loaded.full_mode.is_none() {
                loaded.full_mode = self.load_full_mode()?.map(|mode| ModeState { active: Some(mode) });
            }
            if let Some(mode) = loaded.active_mode(now).cloned() {
                let mut next = loaded.clone();
                let changed = next.grant_pending(now, &mode);
                if !changed.is_empty() {
                    self.commit(&mut loaded, next, changed)?;
                }
            }
            *store = Some(loaded);
        }
        let store = store.as_mut().unwrap();
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

    #[cfg(test)]
    pub(crate) fn register(&self, request: Registration) -> Result<String> {
        Ok(self.register_inner(request, false, true, false, None, None)?.0)
    }

    /// Register a request that runs a saved snapshot after its caller has returned.
    /// Only a person can grant it, whatever mode is active now or enabled later.
    /// The result never names a request that was already decided.
    pub(crate) fn register_deferred(&self, mut request: Registration, binding: String) -> Result<String> {
        request.unattended = ApprovalUnattended::NeverUnattended;
        request.thread_auto = false;
        Ok(self.register_inner(request, false, false, false, None, Some(binding))?.0)
    }

    pub(crate) fn binding(&self, id: &str) -> Result<Option<String>> {
        self.with_store(now, |store, _| Ok(store.records.iter()
            .find(|record| record.summary.approval_id == id)
            .and_then(|record| record.binding.clone())))
    }

    /// Withdraw the owner's request only while no decision has been made.
    pub(crate) fn withdraw_pending(&self, id: &str, owner: &ApprovalOwner, reason: &str) -> Result<bool> {
        self.with_store(now, |store, now| {
            let Some(index) = store.records.iter().position(|record| {
                let summary = &record.summary;
                summary.approval_id == id && summary.owner == *owner && summary.state == ApprovalState::Pending
            }) else { return Ok(false) };
            let mut next = store.clone();
            let record = &mut next.records[index];
            record.summary.state = ApprovalState::Withdrawn;
            record.summary.updated_at = now;
            record.audit(now, &owner.requester, Some(reason.into()), AuditEvent::Withdrawn);
            let summary = record.summary.clone();
            self.commit(store, next, [summary])?;
            Ok(true)
        })
    }

    /// Grant and claim the exact operation of a live caller in one durable mutation.
    #[cfg(test)]
    pub(crate) fn authorize_full(&self, request: Registration) -> Result<FullAuthorization> {
        self.authorize_automatic(request, true, false, None)
    }

    pub(crate) fn authorize_automatic(
        &self, request: Registration, full: bool, thread: bool, legacy_id: Option<&str>,
    ) -> Result<FullAuthorization> {
        Ok(self.register_inner(request, true, full, thread, legacy_id, None)?.1)
    }

    fn register_inner(
        &self, mut request: Registration, live: bool, full: bool, thread: bool, legacy_id: Option<&str>,
        binding: Option<String>,
    ) -> Result<(String, FullAuthorization)> {
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
            let eligible = eligible_for_full(&request.owner, request.unattended, &request.action.tool);
            let mode = store.active_mode(now).filter(|_| eligible && full)
                .map(|mode| (FULL_MODE_PRINCIPAL, mode.id.clone()))
                .or_else(|| (eligible && thread).then(|| (THREAD_MODE_PRINCIPAL, THREAD_MODE_PRINCIPAL.into())));

            // A grant's operation identity outlives its lease and process. A
            // finalized grant stops replay even if its original deadline passed.
            // A deferred request needs a new decision once an earlier one is final.
            let replayed = binding.is_none().then(|| store.records.iter().position(|record| {
                record.same_operation(&request.owner, &request.action)
                    && (record.was_granted_by_full_mode() || mode.is_some() && record.was_granted())
            })).flatten();
            if let Some(index) = replayed {
                let record = &store.records[index];
                let id = record.summary.approval_id.clone();
                if !live {
                    return Ok((id, FullAuthorization::Human));
                }
                if !eligible || record.summary.state != ApprovalState::Granted
                    || record.summary.grant_scope != Some(ApprovalGrantScope::Once)
                    || store.records.iter().any(|other| {
                        other.same_operation(&request.owner, &request.action) && other.summary.consumed_at.is_some()
                    })
                {
                    return Ok((id, FullAuthorization::AlreadyConsumed));
                }
                let mut next = store.clone();
                next.records[index].consume(now);
                let summary = next.records[index].summary.clone();
                self.commit(store, next, [summary])?;
                return Ok((id, FullAuthorization::Granted));
            }
            if live && mode.is_none() {
                return Ok((String::new(), FullAuthorization::Human));
            }
            let mut next = store.clone();
            if let Some(record) = next.records.iter_mut().rev().find(|record| {
                let summary = &record.summary;
                summary.state == ApprovalState::Pending
                    && record.same_operation(&request.owner, &request.action)
                    && summary.risk == request.risk
                    && summary.unattended == request.unattended
                    && record.binding.is_some() == binding.is_some()
            }) {
                let id = record.summary.approval_id.clone();
                record.audit(now, &request.owner.requester, None, AuditEvent::Deduplicated);
                let mut changed = None;
                let mut authorization = FullAuthorization::Human;
                if let Some((principal, mode_id)) = &mode {
                    record.grant_automatic(now, principal, mode_id);
                    if live {
                        record.consume(now);
                        authorization = FullAuthorization::Granted;
                    }
                    changed = Some(record.summary.clone());
                }
                self.commit(store, next, changed)?;
                return Ok((id, authorization));
            }
            if request.action.tool.starts_with("mcp/") && request.tool_item.is_some()
                && store.records.iter().any(|record| record.summary.owner == request.owner
                    && record.summary.tool_item == request.tool_item)
            {
                request.tool_item = None;
            }
            let approval_id = legacy_id.map(str::to_owned).unwrap_or_else(|| loop {
                let id = format!("apr_{:032x}", rand::random::<u128>());
                if next.records.iter().all(|record| record.summary.approval_id != id) {
                    break id;
                }
            });
            let mut record = self.new_record(approval_id.clone(), request, now);
            record.binding = binding;
            let mut authorization = FullAuthorization::Human;
            if let Some((principal, mode_id)) = &mode {
                record.grant_automatic(now, principal, mode_id);
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

    fn new_record(&self, approval_id: String, request: Registration, now: i64) -> Record {
        let requester = request.owner.requester.clone();
        let mut record = Record {
            summary: ApprovalSummary {
                approval_id,
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
                automatic: false,
                tool_item: request.tool_item,
                anchor_item: request.anchor_item,
                action: ApprovalDisplayAction {
                    tool: request.action.tool.clone(),
                    args: request.display_args,
                    scope: request.display_scope,
                },
            },
            action: request.action,
            history: Vec::new(),
            binding: None,
        };
        record.audit(now, &requester, None, AuditEvent::Registered);
        record
    }

    /// Take the snapshot and subscribe at the same point in the mutation order.
    pub(super) fn transcript_receipts(&self, session: &str) -> Result<Vec<fuzukue_proto::ApprovalReceipt>> {
        self.with_store(now, |store, _| Ok(store.records.iter()
            .filter(|record| record.summary.owner.thread == session)
            .filter_map(|record| record.summary.receipt())
            .collect()))
    }

    pub(super) fn bind_mcp_tool(
        &self, thread: &str, job: &str, name: &str, input: &Value, item: &str, previous: Option<&str>,
    ) -> Result<()> {
        let Some(previous) = previous else { return Ok(()) };
        let fingerprint = crate::full_approval::fingerprint(input);
        self.with_store(now, |store, _| {
            if store.records.iter().any(|record| record.summary.owner.thread == thread
                && record.summary.tool_item.as_deref() == Some(item))
            {
                return Ok(());
            }
            let Some(index) = store.records.iter().position(|record| {
                let summary = &record.summary;
                summary.owner.thread == thread && summary.owner.job == job
                    && summary.tool_item.is_none() && summary.anchor_item.as_deref() == Some(previous)
                    && summary.state == ApprovalState::Granted
                    && record.action.tool.starts_with("mcp/")
                    && record.action.args["fingerprint"].as_str() == Some(fingerprint.as_str())
                    && fuzukue_view::chat::approval_matches_tool(&record.action.tool, name)
            }) else { return Ok(()) };
            let mut next = store.clone();
            next.records[index].summary.tool_item = Some(item.into());
            let summary = next.records[index].summary.clone();
            self.commit(store, next, [summary])
        })
    }

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

    /// The current summary of one request and its exact server-side action.
    pub(crate) fn lookup(&self, id: &str) -> Result<Option<(ApprovalSummary, Action)>> {
        self.with_store(now, |store, _| Ok(store.records.iter()
            .find(|record| record.summary.approval_id == id)
            .map(|record| (record.summary.clone(), record.action.clone()))))
    }

    /// IDs of every request registered for a tool, in registration order.
    pub(crate) fn ids_for_tool(&self, tool: &str) -> Result<Vec<String>> {
        self.with_store(now, |store, _| Ok(store.records.iter()
            .filter(|record| record.action.tool == tool)
            .map(|record| record.summary.approval_id.clone())
            .collect()))
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ApprovalSummary> {
        self.updates.subscribe()
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
                ApprovalDecision::Withdraw if previous.unstarted() && !previous.was_granted_by_full_mode() => (
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

    #[cfg(test)]
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
            // Old servers could persist multiple full-mode grants across leases.
            let mut full_grant = false;
            let mut consumed = false;
            for record in store.records.iter().filter(|record| record.same_operation(owner, action)) {
                full_grant |= record.was_granted_by_full_mode();
                consumed |= record.summary.consumed_at.is_some();
            }
            // A deferred request is a new human decision and its record is consumed once.
            if full_grant && consumed && store.records[index].binding.is_none() {
                return Ok(false);
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
pub(super) fn eligible_for_full(owner: &ApprovalOwner, unattended: ApprovalUnattended, tool: &str) -> bool {
    unattended != ApprovalUnattended::NeverUnattended
        && (owner.requester != "front_desk"
            || matches!(tool,
                "exec" | "folder_read" | "git_integrate_patch"
                | "Bash" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
                | "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval"
            ) || tool.starts_with("mcp/") || tool.starts_with(crate::full_approval::NATIVE_TOOL))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
#[path = "approval_store_tests.rs"]
mod tests;
