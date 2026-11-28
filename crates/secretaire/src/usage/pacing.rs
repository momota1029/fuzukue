//! Whether a queued job starts as routed, moves to another model, or waits for usage to recover.

use fuzukue_proto::{AgentBackend, Tier, TierModel};

use super::{Usage, UsageEntry};

/// A window stays tight at least this long after the last reading that projected exhaustion.
const MIN_TIGHT_SECS: i64 = 10 * 60;
/// Recovery needs the allowance to outlast the reset by this factor.
const RECOVERY_FACTOR: f64 = 1.25;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// Replace the routed model of a job when its backend is paused or projected to run out.
    pub reroute: bool,
    /// Hold, slow down, or lower the tier of work when a window is projected to run out.
    pub pacing: bool,
    /// Projections apply from this used fraction of a window.
    pub pacing_from: f32,
    /// Jobs allowed to run at once on a backend that is projected to run out.
    pub tight_running: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self { reroute: true, pacing: true, pacing_from: 0.7, tight_running: 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Condition {
    Open,
    /// Projected to run out before the reset.
    Tight { resume_at: Option<i64> },
    Paused { resume_at: Option<i64> },
}

impl Condition {
    fn paused(self) -> bool {
        matches!(self, Self::Paused { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Paused,
    Tight,
}

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub tier: Tier,
    pub model: TierModel,
}

pub(crate) struct Candidates {
    pub current: Choice,
    /// Alternatives at the same tier, in order of preference.
    pub same: Vec<TierModel>,
    /// Cheaper models one tier down. Empty when the job must keep its tier.
    pub lower: Vec<Choice>,
    /// The user chose this model, so it is never replaced.
    pub pinned: bool,
    /// Someone is waiting for the result.
    pub urgent: bool,
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Start {
        choice: Choice,
        /// Set when the choice differs from the current one.
        changed: Option<Cause>,
        /// Start only while fewer jobs than this run on the chosen backend.
        limit: Option<usize>,
    },
    Wait { backend: AgentBackend, resume_at: Option<i64>, cause: Cause },
}

pub(crate) fn decide(
    candidates: Candidates,
    settings: Settings,
    condition: impl Fn(AgentBackend) -> Condition,
) -> Outcome {
    let Candidates { current, same, lower, pinned, urgent } = candidates;
    let condition = |backend| match condition(backend) {
        Condition::Tight { .. } if !settings.pacing => Condition::Open,
        condition => condition,
    };
    let backend = current.model.selection.backend;
    let (cause, resume_at) = match condition(backend) {
        Condition::Open => return Outcome::Start { choice: current, changed: None, limit: None },
        Condition::Tight { resume_at } => (Cause::Tight, resume_at),
        Condition::Paused { resume_at } => (Cause::Paused, resume_at),
    };
    let limit = Some(settings.tight_running);
    if !pinned && settings.reroute {
        let start = |choice: Choice, limit| Outcome::Start { choice, changed: Some(cause), limit };
        let at_tier = |model: &TierModel| Choice { tier: current.tier, model: model.clone() };
        if let Some(model) = same.iter().find(|model| condition(model.selection.backend) == Condition::Open) {
            return start(at_tier(model), None);
        }
        // A tight backend still has room for work someone is waiting for.
        if cause == Cause::Paused && urgent {
            if let Some(model) = same.iter().find(|model| !condition(model.selection.backend).paused()) {
                return start(at_tier(model), limit);
            }
        }
        if settings.pacing {
            if let Some(choice) = lower.iter().find(|c| condition(c.model.selection.backend) == Condition::Open) {
                return start(choice.clone(), None);
            }
            if let Some(choice) = lower.iter().find(|c| !condition(c.model.selection.backend).paused()) {
                return start(choice.clone(), limit);
            }
        }
    }
    if cause == Cause::Tight && urgent {
        return Outcome::Start { choice: current, changed: None, limit };
    }
    Outcome::Wait { backend, resume_at, cause }
}

/// Models tried when no ordered list is configured for a tier.
pub(crate) fn default_fallbacks(tier: Tier) -> [TierModel; 2] {
    let (claude, codex, effort) = match tier {
        Tier::Light => ("claude-haiku-5-5", "gpt-6.1-terra", "low"),
        Tier::Standard => ("claude-sonnet-5-5", "gpt-6.2-sol", "medium"),
        Tier::Heavy => ("claude-opus-5-5", "gpt-6.2-sol", "high"),
    };
    let model = |model: &str, effort: Option<&str>| TierModel {
        selection: fuzukue_proto::AgentSelection::from_model(Some(model.into()), effort.map(str::to_owned)),
        cost_weight: None,
    };
    [model(claude, None), model(codex, Some(effort))]
}

impl UsageEntry {
    /// The reset time while this window is projected to run out first. Entering is immediate.
    /// Leaving needs a clear margin or an expired estimate, so work does not flap.
    fn tight(&mut self, now: i64, from: f32) -> Option<i64> {
        let reset = self.window.resets_at.filter(|&reset| reset > now);
        let Some(reset) = reset.filter(|_| self.window.used >= from) else {
            // The floor can differ between callers, so a higher one keeps the latch.
            return None;
        };
        let remaining = (reset - now) as f64;
        let runway = self.recent_pace(now).and_then(|pace| pace.runway_seconds(self.window.used, now));
        let held = self.tight_at.map(|at| now - at);
        match runway {
            Some(runway) if runway < remaining => self.tight_at = Some(now),
            Some(runway) if runway >= remaining * RECOVERY_FACTOR
                && held.is_some_and(|held| held >= MIN_TIGHT_SECS) => self.tight_at = None,
            None if held.is_some_and(|held| held >= fuzukue_proto::UsagePace::MAX_AGE_SECS) => self.tight_at = None,
            _ => {}
        }
        self.tight_at.map(|_| reset)
    }
}

impl Usage {
    /// `goal_threshold` is the pause threshold of a paced goal, which also honors windows
    /// without a reset time. Other work pauses at `pause_at` until a known reset.
    pub(crate) fn condition(&self, agent: &str, goal_threshold: Option<f32>, pacing_from: f32) -> Condition {
        self.condition_at(agent, super::now(), goal_threshold, pacing_from)
    }

    fn condition_at(&self, agent: &str, now: i64, goal_threshold: Option<f32>, pacing_from: f32) -> Condition {
        let threshold = goal_threshold.unwrap_or(self.pause_at);
        let blockers = self.pause_windows(now, threshold, true, |w| w.agent == agent);
        let resume_at = blockers.iter().filter_map(|window| window.resets_at).max();
        if resume_at.is_some() || goal_threshold.is_some() && !blockers.is_empty() {
            return Condition::Paused { resume_at };
        }
        let resume_at = self.windows.lock().unwrap().values_mut()
            .filter(|entry| entry.window.agent == agent)
            // Evaluate every window so that each keeps its own latch current.
            .filter_map(|entry| entry.tight(now, pacing_from))
            .max();
        resume_at.map_or(Condition::Open, |at| Condition::Tight { resume_at: Some(at) })
    }

    /// Whether this backend has reported any usage, which shows that it is in use here.
    pub(crate) fn known(&self, agent: &str) -> bool {
        self.windows.lock().unwrap().keys().any(|(name, _)| name == agent)
    }
}

#[cfg(test)]
mod tests;
