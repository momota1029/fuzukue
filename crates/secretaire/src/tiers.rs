use fuzukue_proto::{
    AgentBackend, AgentOutcome, AgentSelection, AgentSettings, GoalPace, Tier, TierAttempt,
    TierDecision, TierModel, TierRequest, UsageWindow,
};

use crate::{
    agent_usage::RunMetadata,
    usage::pacing::{self, Candidates, Cause, Choice, Outcome},
    Workspace,
};

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
    /// Someone is waiting for the result, so usage pacing does not hold it back.
    pub urgent: bool,
    /// High-risk work keeps its tier and waits instead.
    pub sensitive: bool,
}

/// Where a queued job goes now, given current usage.
pub(crate) struct Routed {
    pub decision: TierDecision,
    pub model: TierModel,
    /// Why usage moved the job to another model or tier.
    pub note: Option<String>,
    /// Jobs allowed to run at once on the backend while its usage is tight.
    pub limit: Option<usize>,
    /// Set when no suitable model can start yet.
    pub hold: Option<(AgentBackend, Option<i64>, Cause)>,
}

/// Whether an assignment must not be moved to a cheaper tier without being asked.
pub(crate) fn sensitive(text: &str, request: TierRequest) -> bool {
    let text = text.to_lowercase();
    request.tier == Some(Tier::Heavy)
        || request.min_tier > Some(Tier::Light)
        || [
            "security", "vulnerab", "architecture", "design judgement", "design review", "cross-cutting",
            "lean proof", "formaliz", "proof", "セキュリティ", "脆弱", "設計判断", "設計の査読", "横断",
            "証明", "形式化",
        ]
        .iter()
        .any(|term| text.contains(term))
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
            urgent: false,
            sensitive: min > Tier::Light,
        }
    }

    /// A worker model fixed on the thread stays when it also applies to a named agent,
    /// which happens once a worker default replaces the agent definitions' models.
    pub fn keep_thread_model(&mut self, app: &Workspace, session: &str, named: bool) {
        self.pinned |= app.hub.read_threads(|store| {
            store.worker_pinned(session) && (!named || store.worker_default_configured)
        });
    }

    pub fn initial(&self, app: &Workspace, pace: Option<GoalPace>) -> (TierDecision, TierModel) {
        let routed = self.plan(app, pace);
        (routed.decision, routed.model)
    }

    pub fn plan(&self, app: &Workspace, pace: Option<GoalPace>) -> Routed {
        let usage = app.worker_admission().usage_pacing();
        // Planning runs outside the job queue and keeps its own model.
        let queued = self.role != crate::planner::ROLE;
        // The threshold of a paced goal, matching its pause progress.
        let threshold = pace.map(|pace| match pace {
            GoalPace::Full => 1.0,
            GoalPace::Normal => 0.90,
            GoalPace::Economy => 0.80,
        });
        let condition = |backend: AgentBackend| app.usage.condition(backend.usage_key(), threshold, usage.pacing_from);
        let settings = app.agent_settings.lock().unwrap();
        let windows = app.usage.current();
        let mut decision = self.decision.clone();
        let mut model = self.model(&settings, &windows, decision.tier, pace);
        // Planning, and queued work with rerouting off, choose a tier from usage alone.
        let rerouted = queued && usage.reroute;
        let nearly_exhausted = (usage.pacing || !queued) && !(rerouted && self.sensitive)
            && windows.iter().any(|window| {
                window.agent == model.selection.backend.usage_key()
                    && (window.rejected || window.used >= 0.90)
            })
            // A paused backend looks for room at the same tier first.
            && !(rerouted && matches!(condition(model.selection.backend), pacing::Condition::Paused { .. }));
        let economy = pace == Some(GoalPace::Economy) && !(rerouted && self.sensitive);
        let mut lowered = false;
        if economy || nearly_exhausted {
            let lower = decision.tier.lower().max(self.min);
            if lower < decision.tier {
                lowered = true;
                decision.tier = lower;
                decision.reason.push_str(harness_text!(
                    "; reduced for pacing or usage",
                    "。ペースまたは使用枠に合わせて一段下げた"
                ));
                model = self.model(&settings, &windows, lower, pace);
            }
        }
        let mut routed = Routed { decision, model, note: None, limit: None, hold: None };
        if !queued {
            return routed;
        }
        // An ordered list may already have skipped a nearly full backend.
        let preferred = self.model(&settings, &[], routed.decision.tier, pace).selection;
        if preferred != routed.model.selection {
            routed.note = Some(switch_note(&preferred, &routed.model.selection, None, None));
        }
        let tier = routed.decision.tier;
        let current = routed.model.selection.clone();
        let can_lower = !self.sensitive && !lowered && tier > self.min && tier > Tier::Light;
        let candidates = Candidates {
            same: self.alternatives(app, &settings, tier, &current),
            lower: if can_lower {
                let lower = tier.lower();
                self.alternatives(app, &settings, lower, &current).into_iter()
                    .map(|model| Choice { tier: lower, model }).collect()
            } else {
                Vec::new()
            },
            current: Choice { tier, model: routed.model.clone() },
            pinned: self.pinned,
            urgent: self.urgent || pace == Some(GoalPace::Full),
        };
        let rules = pacing::Settings { pacing: usage.pacing && pace != Some(GoalPace::Full), ..usage };
        match pacing::decide(candidates, rules, condition) {
            Outcome::Start { choice, changed, limit } => {
                routed.limit = limit;
                if let Some(cause) = changed {
                    let lowered = (choice.tier != tier).then_some(choice.tier);
                    let note = switch_note(&current, &choice.model.selection, Some(cause), lowered);
                    routed.decision.tier = choice.tier;
                    routed.model = choice.model;
                    routed.note = Some(note);
                }
            }
            Outcome::Wait { backend, resume_at, cause } => routed.hold = Some((backend, resume_at, cause)),
        }
        if let Some(note) = &routed.note {
            routed.decision.reason.push_str(harness_text!("; ", "。"));
            routed.decision.reason.push_str(note);
        }
        routed
    }

    /// Configured models for a tier, then the built-in ones when no ordered list is set.
    fn alternatives(
        &self, app: &Workspace, settings: &AgentSettings, tier: Tier, current: &AgentSelection,
    ) -> Vec<TierModel> {
        let listed = settings.tiers.models.get(&tier).filter(|models| !models.is_empty());
        let plain = |selection: &AgentSelection| TierModel {
            selection: selection.clone(),
            cost_weight: listed.into_iter().flatten()
                .find(|model| model.selection == *selection).and_then(|model| model.cost_weight),
        };
        let preferred = settings.tier_models.get(&tier).map(&plain);
        // A role's fallback model is its light-tier choice. Heavier work reaches it only
        // by lowering its tier, and high-risk work never takes it.
        let role = settings.fallbacks.get(&self.role)
            .filter(|_| tier == Tier::Light && !self.sensitive).map(&plain);
        let defaults = listed.is_none().then(|| pacing::default_fallbacks(tier)).into_iter().flatten()
            .filter(|model| app.usage.known(model.selection.backend.usage_key()));
        preferred.into_iter()
            .chain(listed.into_iter().flatten().cloned())
            .chain(role)
            .chain(defaults)
            .filter(|model| {
                model.selection.backend != current.backend || model.selection.model != current.model
            })
            .collect()
    }

    /// The route of the next attempt one tier up. It queues again, so the usage gate
    /// and the model choice apply to it, and pacing never lowers it back.
    pub fn retry(&self, app: &Workspace, tier: Tier, retries: usize, reason: &str) -> Option<Self> {
        let limit = app.agent_settings.lock().unwrap().tiers.max_escalations;
        let next = next_tier(tier, retries, limit)?;
        let mut route = self.clone();
        route.decision = TierDecision { tier: next, reason: reason.into() };
        route.min = next;
        Some(route)
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

/// The line shown in job details and lists when usage changes a job's model.
fn switch_note(from: &AgentSelection, to: &AgentSelection, cause: Option<Cause>, lowered: Option<Tier>) -> String {
    let name = |selection: &AgentSelection| match &selection.model {
        Some(model) => format!("{} {model}", selection.backend.label()),
        None => selection.backend.label().into(),
    };
    let (backend, from, to) = (from.backend.label(), name(from), name(to));
    let why = match cause {
        Some(Cause::Paused) => harness_format!("{backend} usage is paused", "{backend} の使用量による一時停止"),
        Some(Cause::Tight) => harness_format!(
            "{backend} usage is projected to run out before reset",
            "{backend} の使用量がリセット前に尽きる見込み"
        ),
        None => harness_format!("{backend} usage is near its limit", "{backend} の使用量が上限に近い"),
    };
    let mut note = harness_format!(
        "switched from {from} to {to} because {why}",
        "{why}ため、{from} から {to} に切り替えた"
    );
    if let Some(tier) = lowered {
        let tier = tier.key();
        note.push_str(&harness_format!(" (lowered to the {tier} tier)", "（段階を {tier} に下げた）"));
    }
    note
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
    fn risk_keeps_the_tier_and_an_ordered_list_replaces_built_in_fallbacks() {
        let request = TierRequest::default();
        assert!(sensitive("Do a security review of the parser", request));
        assert!(sensitive("承認の設計判断", request));
        assert!(sensitive("Tidy", TierRequest { tier: None, min_tier: Some(Tier::Standard) }));
        assert!(!sensitive("Run the tests and fix small failures", request));

        let project = crate::test_support::TestDir::new();
        let app = Workspace::new(project.path().into(), "127.0.0.1:0".into(),
            project.path().join("state"), std::sync::Arc::new(crate::Hub::new()));
        let reset = crate::usage::now() + 3600;
        for (percent, expected) in [(20, "gpt-6.2-sol"), (96, "claude-sonnet-5-5")] {
            app.usage.update_claude(&serde_json::json!({
                "unifiedWindows": {"five_hour": {"utilization": 0.1, "resetsAt": reset}},
            }));
            app.usage.update_codex(&serde_json::json!({
                "primary": {"usedPercent": percent, "windowDurationMins": 300, "resetsAt": reset},
            }));
            let route = Route::new(&app, "fixture", "worker",
                AgentSelection::from_model(Some("gpt-6.2-sol".into()), None), false,
                classify("Update the notes", None, request, 1));
            let routed = route.plan(&app, None);
            assert_eq!(routed.model.selection.model.as_deref(), Some(expected));
            assert_eq!(routed.note.is_some(), percent == 96);
            assert!(routed.hold.is_none());
        }

        // The configured list names only the paused backend, so the job waits.
        app.agent_settings.lock().unwrap().tiers.models.insert(Tier::Standard, vec![TierModel {
            selection: AgentSelection::from_model(Some("gpt-6.2-sol".into()), None),
            cost_weight: None,
        }]);
        let mut route = Route::new(&app, "fixture", "worker",
            AgentSelection::from_model(Some("gpt-6.2-sol".into()), None), false,
            classify("Security review", Some(Tier::Standard), request, 1));
        route.sensitive = true;
        assert!(matches!(route.plan(&app, None).hold, Some((AgentBackend::Codex, Some(_), Cause::Paused))));
    }

    #[test]
    fn a_thread_model_applied_to_a_named_agent_is_never_replaced() -> anyhow::Result<()> {
        let project = crate::test_support::TestDir::new();
        let app = Workspace::new(project.path().into(), "127.0.0.1:0".into(),
            project.path().join("state"), std::sync::Arc::new(crate::Hub::new()));
        app.hub.load_threads(project.path(), &app.state_dir);
        let worker = fuzukue_proto::DelegateSettings { model: "gpt-6.2-sol".into(), effort: String::new() };
        let session = app.hub.edit_threads(|store| {
            store.worker_default_configured = true;
            store.create_workshop_thread("fixture".into(), Vec::new(), worker)
        })?;
        let reset = crate::usage::now() + 3600;
        app.usage.update_claude(&serde_json::json!({
            "unifiedWindows": {"five_hour": {"utilization": 0.1, "resetsAt": reset}},
        }));
        app.usage.update_codex(&serde_json::json!({
            "primary": {"usedPercent": 96, "windowDurationMins": 300, "resetsAt": reset},
        }));
        let mut route = Route::new(&app, &session, "reviewer",
            AgentSelection::from_model(Some("gpt-6.2-sol".into()), None), false,
            classify("Update the notes", None, TierRequest::default(), 1));
        assert_eq!(route.plan(&app, None).model.selection.backend, AgentBackend::Claude);
        route.keep_thread_model(&app, &session, true);
        let routed = route.plan(&app, None);
        assert_eq!(routed.model.selection.model.as_deref(), Some("gpt-6.2-sol"));
        assert!(matches!(routed.hold, Some((AgentBackend::Codex, _, Cause::Paused))));
        Ok(())
    }

    #[test]
    fn a_role_fallback_serves_heavier_work_only_as_a_lower_tier() {
        let project = crate::test_support::TestDir::new();
        let selection = |model: &str| AgentSelection::from_model(Some(model.into()), None);
        let workspace = |pacing: bool| {
            let app = Workspace::new(project.path().into(), "127.0.0.1:0".into(),
                project.path().join("state"), std::sync::Arc::new(crate::Hub::new()));
            std::fs::write(project.path().join("fuzukue.toml"), format!("[delegate]\nusage_pacing = {pacing}\n")).unwrap();
            app.delegates.set_test_admission(crate::delegate::admission::Scheduler::new(
                crate::delegate::admission::Config::load(project.path(), Default::default()).unwrap()));
            {
                let mut settings = app.agent_settings.lock().unwrap();
                // The ordered list leaves the role fallback as the only other model.
                settings.tiers.models.insert(Tier::Standard,
                    vec![TierModel { selection: selection("gpt-6.2-sol"), cost_weight: None }]);
                settings.fallbacks.insert("worker".into(), selection("claude-haiku-5-5"));
            }
            app.usage.update_codex(&serde_json::json!({
                "primary": {"usedPercent": 96, "windowDurationMins": 300, "resetsAt": crate::usage::now() + 3600},
            }));
            app
        };
        let route = |app: &Workspace, text: &str| {
            let mut route = Route::new(app, "fixture", "worker", selection("gpt-6.2-sol"), false,
                classify(text, Some(Tier::Standard), TierRequest::default(), 1));
            route.sensitive |= sensitive(text, TierRequest::default());
            route.plan(app, None)
        };
        let app = workspace(true);
        let routed = route(&app, "Update the notes");
        assert_eq!(routed.model.selection.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(routed.decision.tier, Tier::Light);
        let lowered = harness_text!("lowered to the light tier", "段階を light に下げた");
        assert!(routed.note.is_some_and(|note| note.contains(lowered)));

        let risky = route(&app, "Check the security notes");
        assert!(risky.hold.is_some());
        assert_eq!(risky.model.selection.model.as_deref(), Some("gpt-6.2-sol"));

        let unpaced = route(&workspace(false), "Update the notes");
        assert!(unpaced.hold.is_some());
        assert_eq!(unpaced.decision.tier, Tier::Standard);
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
