use std::{path::Path, sync::{Arc, Weak}, time::Duration};

use anyhow::{Context, Result};
use fuzukue_proto::{
    ApprovalDecision, ApprovalOwner, ApprovalSummary, FullApprovalChange, FullApprovalMode,
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

    // The MCP producers adopt this API in the next implementation step.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn register_approval(&self, registration: Registration) -> Result<String> {
        self.approval_queue()?.register(registration)
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
        &self, event: ServerEvent, waiter: Weak<()>, registration: Registration,
        resume: impl FnOnce(bool) + Send + 'static,
    ) -> Result<FullAuthorization> {
        let mut inner = self.inner.lock().unwrap();
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
        &self, inner: &Inner, request: Registration, legacy_id: Option<&str>,
    ) -> Result<FullAuthorization> {
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
        if !request.thread_auto {
            return match self.approval_queue.get().filter(|_| !crate::canary()) {
                Some(queue) => queue.authorize_full(request),
                None => Ok(FullAuthorization::Human),
            };
        }
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

    #[cfg_attr(not(test), allow(dead_code))]
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
    #[cfg_attr(not(test), allow(dead_code))]
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
