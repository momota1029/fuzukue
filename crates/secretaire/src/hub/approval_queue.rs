use std::{path::Path, sync::{Arc, Weak}, time::Duration};

use anyhow::{Context, Result};
use fuzukue_proto::{
    Actor, ApprovalDecision, ApprovalOwner, ApprovalSummary, FullApprovalChange, FullApprovalMode,
    FullApprovalSettings, PermissionMode, ServerEvent,
};
use tokio::sync::broadcast;

use super::{
    ApprovalWaiter, Hub, Inner,
    approval_store::{Action, FullAuthorization, Queue, Registration, eligible_for_full},
};

#[cfg(test)]
#[path = "approval_wire_tests.rs"]
mod tests;

// Only producers whose operation is eligible for full mode attach a resumption.
pub(super) struct LegacyFullApproval {
    registration: Registration,
    resume: Box<dyn FnOnce(bool) + Send>,
}

fn approval_job<'a>(item: &'a str, actor: &'a Actor) -> &'a str {
    fuzukue_proto::delegate_job_id(item, actor).unwrap_or("front_desk")
}

fn history_with_approval_receipts(
    session: &str, events: Vec<ServerEvent>, receipts: &[fuzukue_proto::ApprovalReceipt],
) -> Vec<ServerEvent> {
    let mut by_anchor = std::collections::HashMap::<&str, Vec<_>>::new();
    for receipt in receipts {
        let anchor = receipt.history_anchor();
        if let Some(anchor) = anchor {
            by_anchor.entry(anchor).or_default().push(receipt);
        }
        if receipt.state == fuzukue_proto::ApprovalState::Denied {
            if let Some(job) = receipt.job().filter(|job| Some(*job) != anchor) {
                by_anchor.entry(job).or_default().push(receipt);
            }
        }
    }
    if by_anchor.is_empty() {
        return events;
    }
    let mut history = Vec::with_capacity(events.len());
    for event in events {
        let matching = match &event {
            ServerEvent::ToolCall { item, .. } | ServerEvent::TextDelta { item, .. }
            | ServerEvent::UserPrompt { item, .. } => by_anchor.remove(item.as_str()),
            _ => None,
        };
        history.push(event);
        history.extend(matching.into_iter().flatten().map(|receipt| receipt.event(session)));
    }
    history
}

impl Hub {
    pub(crate) fn init_approval_queue(&self, workspace: String, state_dir: &Path) {
        self.approval_queue
            .get_or_init(|| Queue::new(workspace, state_dir));
    }

    fn approval_queue(&self) -> Result<&Queue> {
        self.approval_queue
            .get()
            .context("approval queue is unavailable")
    }

    pub(crate) fn approval_record(&self, id: &str) -> Result<Option<(ApprovalSummary, Action)>> {
        self.approval_queue()?.lookup(id)
    }

    pub(crate) fn register_deferred_approval(&self, mut registration: Registration, binding: String) -> Result<String> {
        Self::prepare_approval_registration(&self.inner.lock().unwrap(), &mut registration);
        self.approval_queue()?.register_deferred(registration, binding)
    }

    pub(crate) fn approval_binding(&self, id: &str) -> Result<Option<String>> {
        self.approval_queue()?.binding(id)
    }

    pub(crate) fn withdraw_pending_approval(&self, id: &str, owner: &ApprovalOwner, reason: &str) -> Result<bool> {
        self.approval_queue()?.withdraw_pending(id, owner, reason)
    }

    pub(crate) fn approval_ids_for_tool(&self, tool: &str) -> Result<Vec<String>> {
        self.approval_queue()?.ids_for_tool(tool)
    }

    pub(crate) fn subscribe_approval_changes(&self) -> Result<broadcast::Receiver<ApprovalSummary>> {
        Ok(self.approval_queue()?.subscribe())
    }

    #[cfg(test)]
    pub(crate) fn register_approval(&self, mut registration: Registration) -> Result<String> {
        Self::prepare_approval_registration(&self.inner.lock().unwrap(), &mut registration);
        self.approval_queue()?.register(registration)
    }

    fn prepare_approval_registration(inner: &Inner, registration: &mut Registration) {
        let owner = &registration.owner;
        if registration.anchor_item.is_none() {
            registration.anchor_item = Self::latest_approval_anchor(inner, &owner.thread, &owner.job).map(str::to_owned);
        }
        let Some(fingerprint) = registration.action.args["fingerprint"].as_str()
            .filter(|_| registration.action.tool.starts_with("mcp/") && registration.tool_item.is_none())
        else { return };
        let latest = inner.log.iter().rev().filter(|envelope| {
            envelope.event.transcript_session() == Some(owner.thread.as_str())
        }).take_while(|envelope| {
            !matches!(&envelope.event, ServerEvent::TurnEnded { .. } | ServerEvent::UserPrompt { .. })
        }).find_map(|envelope| match &envelope.event {
            ServerEvent::ToolCall { item, actor, name, input, .. }
                if approval_job(item, actor) == owner.job
                    && fuzukue_view::chat::approval_matches_tool(&registration.action.tool, name)
                    && crate::full_approval::fingerprint(input) == fingerprint => Some(item.clone()),
            _ => None,
        });
        registration.tool_item = latest;
    }

    fn latest_approval_anchor<'a>(inner: &'a Inner, session: &str, job: &str) -> Option<&'a str> {
        inner.log.iter().rev().find_map(|envelope| {
            if envelope.event.transcript_session() != Some(session) {
                return None;
            }
            match &envelope.event {
                ServerEvent::ToolCall { item, actor, .. } | ServerEvent::TextDelta { item, actor, .. }
                    if approval_job(item, actor) == job || item == job => Some(item.as_str()),
                ServerEvent::UserPrompt { item, .. } if job == "front_desk" => Some(item.as_str()),
                _ => None,
            }
        })
    }

    pub(super) fn bind_approval_tool(&self, inner: &Inner, event: &ServerEvent) {
        let ServerEvent::ToolCall { session, item, actor, name, input } = event else { return };
        if !(name.starts_with("mcp__fuzukue__") || name.starts_with("fuzukue.")) {
            return;
        }
        let Some(queue) = self.approval_queue.get() else { return };
        let job = approval_job(item, actor);
        let previous = Self::latest_approval_anchor(inner, session, job);
        if let Err(error) = queue.bind_mcp_tool(session, job, name, input, item, previous) {
            tracing::warn!(%error, "Could not link an approval to its transcript tool");
        }
    }

    pub(super) fn history_with_approval_receipts(&self, session: &str, events: Vec<ServerEvent>) -> Vec<ServerEvent> {
        let Some(queue) = self.approval_queue.get() else { return events };
        let receipts = match queue.transcript_receipts(session) {
            Ok(receipts) => receipts,
            Err(error) => {
                tracing::warn!(%error, "Could not load approval receipts");
                return events;
            }
        };
        history_with_approval_receipts(session, events, &receipts)
    }

    pub(crate) fn set_full_approval(
        &self,
        change: FullApprovalChange,
        principal: &crate::remote::SettingsPrincipal,
    ) -> Result<Option<FullApprovalMode>> {
        let mut inner = self.inner.lock().unwrap();
        let queue = self.approval_queue()?;
        if matches!(change, FullApprovalChange::Off) {
            return queue.set_full_mode(change, principal.as_str(), Vec::new(), Vec::new()).map(|(mode, _)| mode);
        }
        self.finish_folder_sweep_locked(&mut inner)?;

        let mut legacy: Vec<_> = inner.approvals.iter().filter_map(|(id, pending)| {
            let full = pending.full.as_ref().filter(|full| {
                pending.live() && Self::automatic_modes(&inner, &full.registration).0
            })?;
            Some((id.clone(), full.registration.clone()))
        }).collect();
        let (folders, invalid) = self.plan_folder_sweep(&inner, None);
        legacy.extend(folders.iter().map(|grant| {
            (grant.id.clone(), super::folder_grants::full_registration(&grant.session, None, &grant.path, &grant.id))
        }));
        let legacy_ids: Vec<_> = legacy.iter().map(|(id, _)| id.clone()).collect();
        let (mode, granted) = queue.set_full_mode(change, principal.as_str(), legacy, folders)?;
        for id in legacy_ids {
            if let Some(full) = inner.approvals.get_mut(&id).and_then(|pending| pending.full.take()) {
                let granted = granted.contains(&id);
                let decision = if granted { "accept" } else { "decline" };
                self.publish_locked(&mut inner, ServerEvent::ApprovalResolved {
                    request_id: id, decision: decision.into(), automatic: true,
                }, None, ApprovalWaiter::Backend);
                (full.resume)(granted);
            }
        }
        for id in invalid {
            self.publish_locked(&mut inner, ServerEvent::ApprovalResolved {
                request_id: id, decision: "cancelled".into(), automatic: false,
            }, None, ApprovalWaiter::Backend);
        }
        if let Err(error) = self.finish_folder_sweep_locked(&mut inner) {
            tracing::warn!(%error, "Could not complete committed full-mode folder grants");
        }
        Ok(mode)
    }

    pub(crate) fn restore_full_approval(&self) -> Result<()> {
        // Loading the queue sweeps durable pending requests before clients can access it.
        self.finish_folder_sweep_locked(&mut self.inner.lock().unwrap())
    }

    pub(super) fn finish_folder_sweep_locked(&self, inner: &mut Inner) -> Result<()> {
        let Some(queue) = self.approval_queue.get() else { return Ok(()) };
        let grants = queue.folder_sweep()?;
        if grants.is_empty() {
            return Ok(());
        }
        let saved = self.apply_folder_sweep(inner, &grants)?;
        queue.finish_folder_sweep()?;
        for grant in grants {
            let granted = saved.contains(&grant.id);
            if granted {
                let _ = self.tx.send(crate::snapshot(ServerEvent::FolderGrantsChanged { session: grant.session }));
            }
            if inner.approvals.contains_key(&grant.id) {
                let decision = if granted { "accept" } else { "cancelled" };
                self.publish_locked(inner, ServerEvent::ApprovalResolved {
                    request_id: grant.id, decision: decision.into(), automatic: granted,
                }, None, ApprovalWaiter::Backend);
            }
        }
        Ok(())
    }

    pub(crate) fn publish_sweepable_approval(
        &self, event: ServerEvent, waiter: Weak<()>, mut registration: Registration,
        resume: impl FnOnce(bool) + Send + 'static,
    ) -> Result<FullAuthorization> {
        let mut inner = self.inner.lock().unwrap();
        Self::prepare_approval_registration(&inner, &mut registration);
        let authorization = self.authorize_automatic_locked(&inner, registration.clone(), None)?;
        if authorization == FullAuthorization::Human {
            let ServerEvent::ApprovalRequested { request_id, .. } = &event else {
                unreachable!("only approval requests can have a resumption");
            };
            let id = request_id.clone();
            self.publish_locked(&mut inner, event, None, ApprovalWaiter::Local(waiter));
            if !crate::canary()
                && eligible_for_full(&registration.owner, registration.unattended, &registration.action.tool)
            {
                inner.approvals.get_mut(&id).unwrap().full = Some(LegacyFullApproval {
                    registration, resume: Box::new(resume),
                });
            }
        }
        Ok(authorization)
    }

    pub(super) fn automatic_modes(inner: &Inner, registration: &Registration) -> (bool, bool) {
        if !registration.thread_auto {
            return (true, false);
        }
        if registration.owner.requester == "front_desk"
            && inner.partner_started.get(&registration.owner.thread).is_some_and(|turn| turn.external_approval_input)
        {
            return (false, false);
        }
        let mode = inner.threads.as_ref()
            .and_then(|store| store.role_settings(&registration.owner.thread))
            .map(|(settings, _)| settings.permission_mode);
        (
            !matches!(mode, Some(PermissionMode::Default | PermissionMode::Plan)),
            mode == Some(PermissionMode::BypassPermissions),
        )
    }

    pub(super) fn authorize_automatic_locked(
        &self, inner: &Inner, mut request: Registration, legacy_id: Option<&str>,
    ) -> Result<FullAuthorization> {
        Self::prepare_approval_registration(inner, &mut request);
        let (full, thread) = Self::automatic_modes(inner, &request);
        if let Some(queue) = self.approval_queue.get().filter(|_| !crate::canary()) {
            queue.authorize_automatic(request, full, thread, legacy_id)
        } else {
            Ok(FullAuthorization::Human)
        }
    }

    pub(crate) fn sweep_thread_approvals(&self, session: &str) -> Result<()> {
        if crate::canary() {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        let pending: Vec<_> = inner.approvals.iter().filter_map(|(id, pending)| {
            let full = pending.full.as_ref()?;
            let request = &full.registration;
            (pending.live() && request.owner.thread == session && Self::automatic_modes(&inner, request).1)
                .then(|| (id.clone(), request.clone()))
        }).collect();
        for (id, request) in pending {
            let decision = self.authorize_automatic_locked(&inner, request, Some(&id))?;
            if decision == FullAuthorization::Human {
                continue;
            }
            let full = inner.approvals.get_mut(&id).unwrap().full.take().unwrap();
            let granted = decision == FullAuthorization::Granted;
            self.publish_locked(&mut inner, ServerEvent::ApprovalResolved {
                request_id: id, decision: if granted { "accept" } else { "decline" }.into(), automatic: true,
            }, None, ApprovalWaiter::Backend);
            (full.resume)(granted);
        }
        let (folders, invalid) = self.plan_folder_sweep(&inner, Some(session));
        if !folders.is_empty() {
            self.approval_queue()?.stage_thread_folders(folders)?;
        }
        for request_id in invalid {
            self.publish_locked(&mut inner, ServerEvent::ApprovalResolved {
                request_id, decision: "cancelled".into(), automatic: false,
            }, None, ApprovalWaiter::Backend);
        }
        self.finish_folder_sweep_locked(&mut inner)
    }

    // The producer holds its own waiter lock. Taking this slot prevents a mode
    // transition from claiming the request before the producer finishes its reply.
    pub(crate) fn claim_legacy_approval(&self, id: &str) -> bool {
        self.claim_legacy_approval_with(id, || Ok(())).expect("claim cannot fail").is_some()
    }

    pub(crate) fn claim_legacy_approval_with<T>(&self, id: &str, before_claim: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.resolved_approvals.contains(id) {
            return Ok(None);
        }
        let result = before_claim()?;
        if let Some(pending) = inner.approvals.get_mut(id) {
            pending.full = None;
        }
        Ok(Some(result))
    }

    pub(crate) fn full_approval_lease(&self) -> Result<Option<(FullApprovalMode, tokio::sync::watch::Receiver<()>)>> {
        match self.approval_queue.get() {
            Some(queue) => queue.full_mode_lease(),
            None => Ok(None),
        }
    }

    pub(crate) fn full_approval_settings(&self, before: Option<usize>) -> Result<FullApprovalSettings> {
        self.approval_queue()?.full_settings(before)
    }

    pub(crate) fn authorize_full_approval(&self, request: Registration) -> Result<FullAuthorization> {
        self.authorize_automatic_locked(&self.inner.lock().unwrap(), request, None)
    }

    pub(crate) fn desk_automatic(&self, session: &str, fallback: PermissionMode) -> Result<bool> {
        if crate::canary() {
            return Ok(false);
        }
        let mode = self.role_settings(session).map_or(fallback, |(settings, _)| settings.permission_mode);
        Ok(match mode {
            PermissionMode::Default | PermissionMode::Plan => false,
            PermissionMode::AcceptEdits => self.full_approval_lease()?.is_some(),
            PermissionMode::BypassPermissions => true,
        })
    }

    pub(crate) fn pending_approvals(
        &self,
    ) -> Result<(Vec<ApprovalSummary>, broadcast::Receiver<ApprovalSummary>)> {
        self.approval_queue()?.pending()
    }

    pub(crate) fn decide_approval(
        &self,
        id: &str,
        decision: ApprovalDecision,
        principal: &str,
        reason: Option<String>,
    ) -> Result<ApprovalSummary> {
        self.approval_queue()?
            .decide(id, decision, principal, reason)
    }

    pub(crate) fn withdraw_own_approval(
        &self,
        id: &str,
        owner: &ApprovalOwner,
        reason: Option<String>,
    ) -> Result<ApprovalSummary> {
        self.approval_queue()?.withdraw_owned(id, owner, reason)
    }

    pub(crate) fn withdraw_approvals_for(
        &self,
        thread: &str,
        job: Option<&str>,
        reason: &str,
    ) -> Result<()> {
        if let Some(queue) = self.approval_queue.get() {
            queue.withdraw_for(thread, job, reason)?;
        }
        Ok(())
    }

    // The caller must recheck current tool permissions and preconditions before consumption.
    pub(crate) fn consume_once_approval(
        &self,
        id: &str,
        owner: &ApprovalOwner,
        action: &Action,
    ) -> Result<bool> {
        self.approval_queue()?.consume_once(id, owner, action)
    }

    pub(crate) fn watch_approval_expiry(self: &Arc<Self>) {
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(hub) = hub.upgrade() else { break };
                if let Err(error) = hub.restore_full_approval() {
                    tracing::warn!(%error, "could not update workspace approvals");
                    // Retry storage failures without logging on every tick.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
    }
}
