use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use fuzukue_proto::{
    ApprovalDecision, ApprovalOwner, ApprovalSummary, FullApprovalChange, FullApprovalMode,
    FullApprovalSettings,
};
use tokio::sync::broadcast;

use super::{
    Hub,
    approval_store::{Action, FullAuthorization, Queue, Registration},
};

#[cfg(test)]
#[path = "approval_wire_tests.rs"]
mod tests;

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
        self.approval_queue()?.set_full_mode(change, principal.as_str())
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
        if let Some(queue) = self.approval_queue.get() {
            queue.authorize_full(request)
        } else {
            Ok(FullAuthorization::Human)
        }
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
                if let Err(error) = hub.approval_queue().and_then(Queue::expire) {
                    tracing::warn!(%error, "could not expire workspace approvals");
                    // Retry storage failures without logging on every tick.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
    }
}
