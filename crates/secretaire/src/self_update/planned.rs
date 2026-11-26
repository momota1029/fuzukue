//! In-memory admission checks for operator-requested shutdowns and updates.

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{DrainReason, SelfUpdateBlockerKind as Kind};
use tokio::{sync::RwLockWriteGuard, time::Instant};

use super::{
    SelfUpdateWait, SelfUpdateWaitReason, State, Step, Update, WaitAction, WaitControl, WaitGuard,
    WaitOutcome,
};
use crate::{App, Workspace};

#[cfg(test)]
#[path = "planned_tests.rs"]
mod tests;

/// Identifies a single authenticated front-desk turn in this server process.
#[derive(Clone)]
pub(crate) struct RequestingTurn {
    workspace: String,
    session: String,
    generation: u64,
}

impl RequestingTurn {
    pub(crate) fn capture(workspace: &Workspace, session: &str, proof: &str) -> Option<Self> {
        Some(Self {
            workspace: workspace.id.clone(),
            session: session.into(),
            generation: workspace.hub.update_request_generation(session, proof)?,
        })
    }

    // The backend receives this proof in its turn input, never through the
    // session's MCP connection. A delayed call retains the proof of its origin.
    pub(crate) fn proof_instruction(proof: &str) -> String {
        harness_format!(
            "(secretaire) For self_update start, canary, or promote in this turn, pass turn_proof: {proof}. Use it only as a tool argument.",
            "（secretaire）このターンで self_update の start、canary、promote を呼ぶときは turn_proof に {proof} を渡す。道具の引数にだけ使う。"
        )
    }
}

impl Update {
    // Use the same gate as work admission. Holding the write guard through
    // begin_wait closes admission before any racing work can start.
    pub(super) fn check_planned(
        &self,
        app: &App,
        requester: Option<&RequestingTurn>,
    ) -> Result<RwLockWriteGuard<'_, ()>> {
        let gate = self.gate.try_write().ok();
        planned_activity(app, requester, gate.is_none(), false)?;
        ensure!(
            matches!(*self.wait_control.lock().unwrap(), WaitControl::Closed)
                && !*self.swapping.borrow()
                && !self.trial_pending(),
            "The server is already draining or switching builds"
        );
        gate.context("Planned operation refused: an admitted operation is active")
    }

    pub(super) fn begin_planned_wait(
        &self,
        app: &App,
        reason: DrainReason,
        requester: Option<&RequestingTurn>,
    ) -> Result<i64> {
        let _gate = self.check_planned(app, requester)?;
        self.begin_wait_inner(reason, false, true)?
            .context("planned drain did not start")
    }

    pub(super) async fn wait_for_planned<'a>(
        &'a self,
        app: &App,
        wait: &WaitGuard<'_>,
        requester: Option<&RequestingTurn>,
        deadline: Instant,
    ) -> Result<WaitOutcome<'a>> {
        if wait.reason == DrainReason::Update {
            self.step(Step::Wait, State::Running);
        }
        loop {
            let changed = self.wait_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let gate = self.gate.try_write().ok();
            let action = {
                let mut control = self.wait_control.lock().unwrap();
                let WaitControl::Planned(id, reason, action) = *control else {
                    return Ok(WaitOutcome::Cancelled);
                };
                if (id, reason) != (wait.id, wait.reason) {
                    return Ok(WaitOutcome::Cancelled);
                }
                if action == Some(WaitAction::Force) {
                    *control = WaitControl::Requested(id, reason, WaitAction::Force);
                }
                action
            };
            match action {
                Some(WaitAction::Cancel) => return self.finish_cancelled_wait(wait),
                Some(WaitAction::Force) => {
                    drop(gate);
                    let idle = self.wait_for_idle(app, wait, None).await;
                    if idle.is_err() {
                        app.notify_drain_cancelled(wait.id, wait.reason).await;
                    }
                    return idle;
                }
                None => {}
            }
            let (mut reason, mut waiting) = planned_activity(app, requester, gate.is_none(), true)?;
            let gate =
                gate.context("Planned operation refused: an admitted operation is active")?;
            if !waiting {
                tokio::time::timeout_at(deadline, app.flush_dirty_transcripts())
                    .await
                    .context("Planned operation aborted: timed out saving dirty transcripts")??;
                // The write gate spans the last inventory check and readiness.
                (reason, waiting) = planned_activity(app, requester, false, true)?;
            }
            if !waiting {
                let mut control = self.wait_control.lock().unwrap();
                match *control {
                    WaitControl::Planned(id, reason, None)
                        if (id, reason) == (wait.id, wait.reason) =>
                    {
                        *control = WaitControl::Ready(id, reason);
                        return Ok(WaitOutcome::Ready(gate));
                    }
                    WaitControl::Planned(id, reason, Some(_))
                        if (id, reason) == (wait.id, wait.reason) =>
                    {
                        continue
                    }
                    _ => return Ok(WaitOutcome::Cancelled),
                }
            }

            let now = Instant::now();
            ensure!(
                now < deadline,
                "Server update aborted: the requesting front-desk turn is still active or finishing"
            );
            self.drain_status.send_if_modified(|current| {
                let Some(status) = current
                    .as_mut()
                    .filter(|status| status.started_at == wait.id)
                else {
                    return false;
                };
                if status.blocking == reason && status.jobs_running == 1 {
                    return false;
                }
                status.jobs_running = 1;
                status.blocking = reason.clone();
                true
            });
            if self.status.send_if_modified(|status| {
                if status
                    .wait
                    .as_ref()
                    .is_some_and(|current| current.reason == reason)
                {
                    return false;
                }
                status.wait = Some(SelfUpdateWait {
                    started_at: wait.id,
                    elapsed_secs: super::elapsed_wait(wait.id),
                    reason,
                    forcing: false,
                });
                true
            }) {
                self.save_status()?;
            }
            drop(gate);
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.min(now + std::time::Duration::from_millis(250))) => {}
                _ = &mut changed => {}
            }
        }
    }
}

fn planned_activity(
    app: &App,
    requester: Option<&RequestingTurn>,
    gate_held: bool,
    wait_for_requester_state: bool,
) -> Result<(SelfUpdateWaitReason, bool)> {
    let workspaces = app
        .try_loaded_workspaces()
        .context("Planned operation refused: workspace registry is busy, activity unknown")?;
    // The gate excludes new turns. Resolve the exemption before collecting
    // activity so completion during the scan cannot turn it into a blocker.
    let active_requester = requester.filter(|requester| {
        workspaces.iter().any(|workspace| {
            workspace.id == requester.workspace
                && workspace
                    .hub
                    .partner_is_only_turn(&requester.session, requester.generation)
        })
    });
    let is_requester = |workspace: &str, session: &str| {
        active_requester.is_some_and(|turn| turn.workspace == workspace && turn.session == session)
    };
    // During cutoff, let Claude release its state lock after turn cleanup.
    // Admission stays closed and readiness still requires an inspected state.
    let checking_requester = |thread: &fuzukue_proto::SelfUpdateBlockingThread| {
        wait_for_requester_state && thread.checking
            && requester.is_some_and(|turn| turn.workspace == thread.workspace && turn.session == thread.id)
    };
    use crate::workspaces::BlockerScan;
    let scan = if wait_for_requester_state { BlockerScan::Cutoff } else { BlockerScan::Preflight };
    let mut reason = App::workspace_blockers(&workspaces, scan);
    let errors = App::workspace_drain_errors(&workspaces, &mut reason);
    let waiting = reason
        .threads
        .iter()
        .any(|thread| checking_requester(thread) || thread.running && is_requester(&thread.workspace, &thread.id));
    let turns = reason
        .threads
        .iter()
        .filter(|thread| thread.running && !is_requester(&thread.workspace, &thread.id))
        .count();
    let workers = reason
        .blockers
        .iter()
        .filter(|blocker| blocker.kind == Kind::Worker)
        .count()
        .max(reason.threads.iter().filter(|thread| thread.worker).count());
    let pending = reason
        .threads
        .iter()
        .filter(|thread| thread.queued || thread.paused)
        .count();
    let approvals = reason
        .blockers
        .iter()
        .filter(|blocker| blocker.kind == Kind::Approval)
        .count();
    let questions = reason
        .blockers
        .iter()
        .filter(|blocker| blocker.kind == Kind::Question)
        .count();
    let mutations = app.mutations.count();
    let jobs: usize = workspaces
        .iter()
        .map(|workspace| App::background_operation_count(workspace))
        .sum();
    let mut commands = 0;
    let mut unknown =
        reason.checking_sessions || reason.threads.iter().any(|thread| thread.checking && !checking_requester(thread));
    for workspace in &workspaces {
        match crate::background_commands::current(workspace) {
            Some(threads) => commands += threads.values().map(Vec::len).sum::<usize>(),
            None => unknown = true,
        }
    }
    let recipes = crate::recipes::active_count();
    let operations = reason
        .blockers
        .iter()
        .filter(|blocker| {
            !matches!(
                blocker.kind,
                Kind::Turn | Kind::Worker | Kind::Approval | Kind::Question
            )
        })
        .count()
        .max(jobs);
    if turns
        + workers
        + pending
        + approvals
        + questions
        + mutations
        + commands
        + recipes
        + operations
        > 0
        || gate_held
        || unknown
        || !errors.is_empty()
    {
        bail!(
            "Planned operation refused: {turns} active turns, {workers} worker threads, {pending} queued work items or paused threads, \
             {approvals} approvals, {questions} questions, {mutations} mutations, {commands} background commands, \
             {recipes} recipes, {operations} other operations; admission held: {gate_held}, activity unknown: {unknown}, save/read errors: {}",
            errors.len(),
        );
    }
    Ok((reason, waiting))
}
