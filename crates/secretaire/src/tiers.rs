use fuzukue_proto::{
    AgentOutcome, AgentSelection, AgentSettings, GoalPace, Tier, TierAttempt, TierDecision,
    TierModel, TierRequest, UsageWindow,
};

use crate::{agent_usage::RunMetadata, Workspace};

#[derive(Clone, Debug)]
pub(crate) enum Retry {
    Tests(String),
    Stalled(String),
    Confidence(String),
    Rejected(String),
}

impl std::fmt::Display for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (label, reason) = match self {
            Self::Tests(reason) => (harness_text!("Tests failed", "テストが失敗した"), reason),
            Self::Stalled(reason) => (harness_text!("Work stalled", "作業が停滞した"), reason),
            Self::Confidence(reason) => (
                harness_text!("More reasoning needed", "さらに検討が必要"),
                reason,
            ),
            Self::Rejected(reason) => (
                harness_text!("Verification rejected the attempt", "検証で棄却された"),
                reason,
            ),
        };
        write!(f, "{label}: {reason}")
    }
}

impl std::error::Error for Retry {}

#[derive(Clone)]
pub(crate) struct Route {
    pub decision: TierDecision,
    min: Tier,
    role: String,
    primary: AgentSelection,
    pinned: bool,
    configured_default: bool,
}

pub(crate) fn classify(
    text: &str,
    hint: Option<Tier>,
    request: TierRequest,
    steps: usize,
) -> (TierDecision, Tier) {
    classify_with_reason_language(text, hint, request, steps, crate::harness::japanese())
}

pub(crate) fn classify_in_english(
    text: &str,
    hint: Option<Tier>,
    request: TierRequest,
    steps: usize,
) -> (TierDecision, Tier) {
    classify_with_reason_language(text, hint, request, steps, false)
}

fn classify_with_reason_language(
    text: &str,
    hint: Option<Tier>,
    request: TierRequest,
    steps: usize,
    japanese: bool,
) -> (TierDecision, Tier) {
    let text = text.to_lowercase();
    let has = |terms: &[&str]| terms.iter().any(|term| text.contains(term));
    let reason_text = |en, ja| if japanese { ja } else { en };
    let high_risk = has(&[
        "irreversible",
        "destructive",
        "force push",
        "force-push",
        "hard reset",
        "rm -rf",
        "delete permanently",
        "drop table",
        "不可逆",
        "破壊的",
        "完全に削除",
    ]);
    let large_count = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|words| {
            matches!(
                words[1].trim_matches(|c: char| !c.is_alphanumeric()),
                "files" | "steps" | "ファイル" | "手順"
            ) && words[0].parse::<usize>().is_ok_and(|count| count >= 4)
        });
    let min = request.min_tier.unwrap_or(Tier::Light);
    let min = if high_risk {
        min.max(Tier::Standard)
    } else {
        min
    };
    let (tier, reason) = if let Some(tier) = request.tier {
        (
            tier,
            reason_text("Requested tier", "依頼で指定された段階"),
        )
    } else if has(&[
        "lean proof",
        "formaliz",
        "proof",
        "architecture",
        "design judgement",
        "cross-cutting",
        "証明",
        "形式化",
        "設計判断",
        "横断",
    ]) {
        (
            Tier::Heavy,
            reason_text("Proof or cross-cutting design", "証明または横断的な設計"),
        )
    } else if steps >= 4
        || large_count
        || text.chars().count() >= 1000
        || has(&[
            "ambiguous",
            "investigate tradeoffs",
            "要件が曖昧",
            "多数のファイル",
        ])
    {
        (
            Tier::Heavy,
            reason_text("Large or ambiguous assignment", "規模が大きいか要件が曖昧"),
        )
    } else if let Some(tier) = hint {
        (
            tier,
            reason_text("Definition's tier hint", "定義の段階指定"),
        )
    } else if steps <= 1
        && has(&[
            "format",
            "look up",
            "lookup",
            "typo",
            "mechanical",
            "整形",
            "誤字",
            "検索",
            "機械的",
        ])
    {
        (
            Tier::Light,
            reason_text("Mechanical edit or lookup", "機械的な編集または検索"),
        )
    } else {
        (Tier::Standard, reason_text("Normal scope", "通常の規模"))
    };
    let reason = if tier < min {
        reason_text("Required minimum tier", "リスクまたは指定による最低段階")
    } else {
        reason
    };
    (
        TierDecision {
            tier: tier.max(min),
            reason: reason.into(),
        },
        min,
    )
}

impl Route {
    pub fn new(
        app: &Workspace,
        session: &str,
        role: &str,
        primary: AgentSelection,
        locked: bool,
        (decision, min): (TierDecision, Tier),
    ) -> Self {
        let selection = if locked {
            None
        } else {
            app.hub.agent_override(session, role).or_else(|| {
                app.agent_settings
                    .lock()
                    .unwrap()
                    .overrides
                    .get(role)
                    .cloned()
            })
        };
        let pinned = locked || selection.is_some();
        let primary = selection.unwrap_or(primary);
        Self {
            decision,
            min,
            role: role.into(),
            primary,
            pinned,
            configured_default: role != crate::planner::ROLE && app.hub.read_threads(|store| store.worker_default_configured),
        }
    }

    pub fn initial(&self, app: &Workspace, pace: Option<GoalPace>) -> (TierDecision, TierModel) {
        let settings = app.agent_settings.lock().unwrap();
        let windows = app.usage.current();
        let mut decision = self.decision.clone();
        let initial = self.model(
            &settings,
            &windows,
            decision.tier,
            pace,
        );
        let nearly_exhausted = windows.iter().any(|window| {
            window.agent == initial.selection.backend.usage_key()
                && (window.rejected || window.used >= 0.90)
        });
        if pace == Some(GoalPace::Economy) || nearly_exhausted {
            let lower = decision.tier.lower().max(self.min);
            if lower < decision.tier {
                decision.tier = lower;
                decision.reason.push_str(harness_text!(
                    "; reduced for pacing or usage",
                    "。ペースまたは使用枠に合わせて一段下げた"
                ));
                let model = self.model(&settings, &windows, lower, pace);
                return (decision, model);
            }
        }
        (decision, initial)
    }

    pub fn retry(&self, app: &Workspace, tier: Tier, retries: usize) -> Option<(Tier, TierModel)> {
        let settings = app.agent_settings.lock().unwrap();
        let next = next_tier(tier, retries, settings.tiers.max_escalations)?;
        Some((
            next,
            self.model(
                &settings,
                &app.usage.current(),
                next,
                None,
            ),
        ))
    }

    fn model(
        &self,
        settings: &AgentSettings,
        windows: &[UsageWindow],
        tier: Tier,
        pace: Option<GoalPace>,
    ) -> TierModel {
        let configured = settings.tiers.models.get(&tier).into_iter().flatten();
        if self.pinned {
            return TierModel {
                selection: self.primary.clone(),
                cost_weight: configured
                    .filter(|model| model.selection == self.primary)
                    .find_map(|model| model.cost_weight),
            };
        }
        let blocked = |model: &TierModel| {
            windows.iter().any(|window| {
                window.agent == model.selection.backend.usage_key()
                    && (window.rejected
                        || window.used
                            >= match pace {
                                Some(GoalPace::Economy) => 0.80,
                                Some(GoalPace::Normal) => 0.90,
                                _ => 0.95,
                            })
            })
        };
        let preferred = (self.role != crate::planner::ROLE).then(|| settings.tier_models.get(&tier)).flatten()
            .map(|selection| TierModel {
                selection: selection.clone(),
                cost_weight: configured.clone().find(|entry| entry.selection == *selection).and_then(|entry| entry.cost_weight),
            });
        let configured = preferred.iter().chain(configured);
        let mut configured = configured.peekable();
        if let Some(first) = configured.peek().copied() {
            return configured
                .find(|model| !blocked(model))
                .unwrap_or(first)
                .clone();
        }
        let mut selection = self.primary.clone();
        if tier == Tier::Light {
            if let Some(fallback) = settings.fallbacks.get(&self.role) {
                selection.clone_from(fallback);
            } else if !self.configured_default {
                selection.effort = Some("low".into());
            }
        } else if tier == Tier::Heavy && !self.configured_default {
            selection.effort = Some("high".into());
        }
        TierModel {
            selection,
            cost_weight: None,
        }
    }
}

fn next_tier(tier: Tier, retries: usize, limit: u8) -> Option<Tier> {
    (retries < usize::from(limit))
        .then(|| tier.higher())
        .flatten()
}

pub(crate) fn attempt<T>(
    tier: Tier,
    mut model: TierModel,
    result: &anyhow::Result<T>,
    metadata: RunMetadata,
    elapsed: std::time::Duration,
    escalation_reason: Option<String>,
) -> TierAttempt {
    model.selection.model = metadata.model.or(model.selection.model);
    model.selection.effort = metadata.effort.or(model.selection.effort);
    TierAttempt {
        tier,
        selection: model.selection,
        duration_ms: elapsed.as_millis() as u64,
        outcome: outcome(result),
        tokens: metadata.tokens,
        cost_usd: metadata.cost_usd,
        weighted_tokens: model
            .cost_weight
            .filter(|weight| weight.is_finite() && *weight >= 0.0)
            .zip(metadata.tokens)
            .map(|(weight, tokens)| weight * tokens as f64),
        escalation_reason,
    }
}

pub(crate) fn outcome<T>(result: &anyhow::Result<T>) -> AgentOutcome {
    match result {
        Ok(_) => AgentOutcome::Ok,
        Err(error) if matches!(error.downcast_ref::<Retry>(), Some(Retry::Stalled(_))) => {
            AgentOutcome::Stalled
        }
        Err(error) if error.is::<crate::cancellation::Cancelled>() => AgentOutcome::Cancelled,
        Err(_) => AgentOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_scope_hints_explicit_requests_and_risk() {
        for (text, hint, request, steps, expected) in [
            (
                "Format one paragraph",
                None,
                TierRequest::default(),
                1,
                Tier::Light,
            ),
            (
                "Prove a Lean proof",
                None,
                TierRequest::default(),
                1,
                Tier::Heavy,
            ),
            (
                "Check the citation",
                Some(Tier::Light),
                TierRequest::default(),
                1,
                Tier::Light,
            ),
            (
                "Check and update the reference",
                None,
                TierRequest::default(),
                4,
                Tier::Heavy,
            ),
            (
                "Permanently destructive cleanup",
                None,
                TierRequest {
                    tier: Some(Tier::Light),
                    min_tier: None,
                },
                1,
                Tier::Standard,
            ),
            (
                "Fix a typo",
                None,
                TierRequest {
                    tier: Some(Tier::Heavy),
                    min_tier: None,
                },
                1,
                Tier::Heavy,
            ),
        ] {
            let (decision, _) = classify(text, hint, request, steps);
            assert_eq!(decision.tier, expected, "{text}");
            assert!(!decision.reason.is_empty());
        }
    }

    #[test]
    fn retry_caps_are_monotone() {
        assert_eq!(next_tier(Tier::Light, 0, 0), None);
        assert_eq!(next_tier(Tier::Light, 0, 2), Some(Tier::Standard));
        assert_eq!(next_tier(Tier::Standard, 1, 2), Some(Tier::Heavy));
        assert_eq!(next_tier(Tier::Standard, 1, 1), None);
        assert_eq!(next_tier(Tier::Heavy, 2, 99), None);
    }
}
