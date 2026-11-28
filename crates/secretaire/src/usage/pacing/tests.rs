use std::sync::Arc;

use fuzukue_proto::{AgentSelection, UsageWindow};

use super::*;

const AT: i64 = 100_000;
const RESET: i64 = AT + 4 * 3600;
const PAUSED: Condition = Condition::Paused { resume_at: Some(RESET) };
const TIGHT: Condition = Condition::Tight { resume_at: Some(RESET) };

fn usage() -> Usage {
    let mut usage = Usage::new(Arc::new(crate::hub::Hub::new()));
    usage.pause_at = 0.95;
    usage
}

fn readings(usage: &Usage, samples: &[(i64, f32)]) {
    for &(at, used) in samples {
        usage.record_windows(at, [(UsageWindow {
            agent: "codex".into(), name: "5h".into(), used, resets_at: Some(RESET),
            rejected: false, recent_pace: None,
        }, true)]);
    }
}

fn state(usage: &Usage, at: i64) -> Condition {
    usage.condition_at("codex", at, None, 0.7)
}

// 0.72 to 0.80 in 20 minutes leaves under an hour, with almost four hours to the reset.
const FAST: [(i64, f32); 3] = [(AT - 1200, 0.72), (AT - 600, 0.76), (AT, 0.80)];

#[test]
fn a_projection_before_reset_is_tight_until_the_estimate_expires() {
    let usage = usage();
    readings(&usage, &FAST);
    assert_eq!(usage.condition_at("codex", AT, None, 0.9), Condition::Open);
    assert_eq!(state(&usage, AT), TIGHT);
    // The pace expires 15 minutes after the last increase and so does the hold.
    assert_eq!(state(&usage, AT + 900), TIGHT);
    assert_eq!(state(&usage, AT + 1801), Condition::Open);
}

#[test]
fn leaving_the_tight_state_needs_time_and_a_margin() {
    let usage = usage();
    readings(&usage, &FAST);
    assert_eq!(state(&usage, AT), TIGHT);
    // A much slower series would outlast the reset, but the tight reading is recent.
    readings(&usage, &[(AT + 60, 0.78), (AT + 240, 0.781), (AT + 480, 0.782)]);
    assert!(usage.windows.lock().unwrap().values().next().unwrap().recent_pace(AT + 480).is_none());
    assert_eq!(state(&usage, AT + 480), TIGHT);
    readings(&usage, &[(AT + 600, 0.786), (AT + 960, 0.791)]);
    assert_eq!(state(&usage, AT + 960), Condition::Open);
}

#[test]
fn the_pause_threshold_takes_priority_and_unknown_backends_are_open() {
    let usage = usage();
    readings(&usage, &[(AT, 0.96)]);
    assert_eq!(state(&usage, AT), PAUSED);
    assert_eq!(usage.condition_at("claude", AT, None, 0.7), Condition::Open);
    assert!(usage.known("codex"));
    assert!(!usage.known("claude"));
    assert_eq!(state(&usage, RESET), Condition::Open);
}

fn model(name: &str) -> TierModel {
    TierModel { selection: AgentSelection::from_model(Some(name.into()), None), cost_weight: None }
}

/// `lowerable` is false for high-risk work, which has no lower candidate.
fn candidates(pinned: bool, urgent: bool, lowerable: bool) -> Candidates {
    Candidates {
        current: Choice { tier: Tier::Standard, model: model("gpt-6.2-sol") },
        same: vec![model("claude-sonnet-5-5")],
        lower: lowerable.then(|| Choice { tier: Tier::Light, model: model("gpt-6.1-terra") }).into_iter().collect(),
        pinned,
        urgent,
    }
}

fn run(candidates: Candidates, settings: Settings, codex: Condition, claude: Condition) -> String {
    let outcome = decide(candidates, settings, |backend| match backend {
        AgentBackend::Codex => codex,
        AgentBackend::Claude => claude,
    });
    match outcome {
        Outcome::Start { choice, limit, .. } => format!(
            "{} {}{}", choice.tier.key(), choice.model.selection.model.unwrap(),
            limit.map(|limit| format!(" limit {limit}")).unwrap_or_default(),
        ),
        Outcome::Wait { cause, .. } => format!("wait {cause:?}"),
    }
}

#[test]
fn paused_or_tight_work_moves_to_a_backend_with_room() {
    let on = Settings::default();
    for urgent in [false, true] {
        for codex in [PAUSED, TIGHT] {
            assert_eq!(run(candidates(false, urgent, true), on, codex, Condition::Open), "standard claude-sonnet-5-5");
        }
    }
    assert_eq!(run(candidates(false, true, false), on, PAUSED, TIGHT), "standard claude-sonnet-5-5 limit 1");
    assert_eq!(run(candidates(false, false, false), on, PAUSED, TIGHT), "wait Paused");
    assert_eq!(run(candidates(false, true, true), on, PAUSED, PAUSED), "wait Paused");
}

#[test]
fn tight_work_is_lowered_slowed_or_held_by_risk_and_urgency() {
    let on = Settings::default();
    assert_eq!(run(candidates(false, false, true), on, TIGHT, PAUSED), "light gpt-6.1-terra limit 1");
    assert_eq!(run(candidates(false, false, false), on, TIGHT, PAUSED), "wait Tight");
    assert_eq!(run(candidates(false, true, false), on, TIGHT, PAUSED), "standard gpt-6.2-sol limit 1");
}

#[test]
fn an_explicit_model_is_never_replaced() {
    let on = Settings::default();
    assert_eq!(run(candidates(true, true, true), on, PAUSED, Condition::Open), "wait Paused");
    assert_eq!(run(candidates(true, false, true), on, TIGHT, Condition::Open), "wait Tight");
    assert_eq!(run(candidates(true, true, true), on, TIGHT, Condition::Open), "standard gpt-6.2-sol limit 1");
    assert_eq!(run(candidates(true, false, true), on, Condition::Open, PAUSED), "standard gpt-6.2-sol");
}

#[test]
fn configuration_turns_rerouting_and_pacing_off() {
    let fixed = Settings { reroute: false, ..Settings::default() };
    assert_eq!(run(candidates(false, true, true), fixed, PAUSED, Condition::Open), "wait Paused");
    assert_eq!(run(candidates(false, false, true), fixed, TIGHT, Condition::Open), "wait Tight");
    let unpaced = Settings { pacing: false, ..Settings::default() };
    assert_eq!(run(candidates(false, false, true), unpaced, TIGHT, Condition::Open), "standard gpt-6.2-sol");
    assert_eq!(run(candidates(false, false, true), unpaced, PAUSED, Condition::Open), "standard claude-sonnet-5-5");
    assert_eq!(run(candidates(false, false, true), unpaced, PAUSED, PAUSED), "wait Paused");
}
