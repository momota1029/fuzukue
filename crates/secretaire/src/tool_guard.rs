//! Bound tool traffic for a single worker attempt or front-desk turn.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::DelegateToolActivity;
use serde::Deserialize;
use tokio::time::Instant;

use crate::cancellation::Cancellation;

const WINDOW: Duration = Duration::from_secs(60);
const CONTINUED_CALLS: usize = 8;
static LIMITS: OnceLock<Limits> = OnceLock::new();

#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Limits {
    pub(crate) same_tool_per_minute: usize,
    pub(crate) total_calls: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { same_tool_per_minute: 300, total_calls: 2000 }
    }
}

impl Limits {
    pub(crate) fn configured() -> Self {
        LIMITS.get().copied().unwrap_or_default()
    }
}

pub(crate) fn configure(server_config: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        tool_guard: Limits,
    }
    let limits = toml::from_str::<Config>(server_config).context("server.toml [tool_guard]")?.tool_guard;
    ensure!(limits.same_tool_per_minute > 0 && limits.total_calls > 0,
        "[tool_guard] tool limits must be positive");
    let _ = LIMITS.set(limits);
    Ok(())
}

pub(crate) fn desk_notice(session: &str) -> fuzukue_proto::ServerEvent {
    notice(session, "tool-guard:desk", fuzukue_proto::LocalizedMessage::new("server-tool-guard-desk", []))
}

pub(crate) fn worker_notice(session: &str, job: &str, label: &str) -> fuzukue_proto::ServerEvent {
    notice(session, &format!("tool-guard:{job}"), fuzukue_proto::LocalizedMessage::new(
        "server-tool-guard-worker", [("label", label.to_owned())],
    ))
}

fn notice(session: &str, id: &str, localized: fuzukue_proto::LocalizedMessage) -> fuzukue_proto::ServerEvent {
    fuzukue_proto::ServerEvent::LocalNotice {
        session: session.into(), item: format!("{id}:{}", crate::mail::session_key()),
        text: crate::l10n::render(fuzukue_i18n::Locale::JAPANESE, &localized),
        localized: Some(localized),
    }
}

#[derive(Debug)]
pub(crate) struct WorkerStopped {
    pub(crate) reason: String,
    pub(crate) partial: String,
}

impl std::fmt::Display for WorkerStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Worker stopped after continuing to call tools past the limit ({}).", self.reason)?;
        if !self.partial.trim().is_empty() {
            write!(f, "\n\nPartial worker output\n\n{}", self.partial)?;
        }
        Ok(())
    }
}

impl std::error::Error for WorkerStopped {}

pub(crate) struct Guard {
    limits: Limits,
    worker: bool,
    state: Mutex<State>,
    stop: Cancellation,
}

#[derive(Default)]
struct State {
    count: usize,
    last_tool: Option<String>,
    recent: HashMap<String, VecDeque<Instant>>,
    native_items: HashSet<String>,
    trip: Option<String>,
    continued: usize,
}

pub(crate) struct Refusal {
    pub(crate) reason: String,
    pub(crate) first: bool,
}

impl Refusal {
    pub(crate) fn message(&self) -> String {
        format!("Tool call limit exceeded for this run ({}). Stop calling tools and report what you have so far.", self.reason)
    }
}

impl Guard {
    pub(crate) fn new(limits: Limits, worker: bool) -> Self {
        Self { limits, worker, state: Mutex::default(), stop: Cancellation::default() }
    }

    pub(crate) fn activity(&self) -> DelegateToolActivity {
        let state = self.state.lock().unwrap();
        DelegateToolActivity { count: state.count, last_tool: state.last_tool.clone() }
    }

    pub(crate) async fn stopped(&self) -> String {
        self.stop.cancelled().await;
        self.stop_reason().expect("tool stop has a reason")
    }

    pub(crate) fn stop_reason(&self) -> Option<String> {
        self.stop.check().err()?;
        self.state.lock().unwrap().trip.clone()
    }

    pub(crate) fn record(&self, name: &str, native_item: Option<&str>) -> Option<Refusal> {
        let mut state = self.state.lock().unwrap();
        if state.continued >= CONTINUED_CALLS {
            return state.trip.as_ref().map(|reason| Refusal { reason: reason.clone(), first: false });
        }
        // Backend start/completion and permission events can refer to the same native call.
        if let Some(item) = native_item {
            if !state.native_items.insert(item.into()) {
                return state.trip.as_ref().map(|reason| Refusal { reason: reason.clone(), first: false });
            }
        }
        state.count = state.count.saturating_add(1);
        state.last_tool = Some(name.chars().take(120).collect());
        if let Some(reason) = state.trip.clone() {
            state.continued = state.continued.saturating_add(1);
            if state.continued == CONTINUED_CALLS {
                if self.worker {
                    self.stop.cancel();
                }
                state.native_items.clear();
            }
            return Some(Refusal { reason, first: false });
        }
        let reason = if state.count > self.limits.total_calls {
            format!("more than {} tool calls in one run", self.limits.total_calls)
        } else {
            let now = Instant::now();
            let recent = state.recent.entry(name.into()).or_default();
            while recent.front().is_some_and(|at| now.duration_since(*at) >= WINDOW) {
                recent.pop_front();
            }
            recent.push_back(now);
            if recent.len() <= self.limits.same_tool_per_minute {
                return None;
            }
            format!("more than {} calls to {} in 60 seconds",
                self.limits.same_tool_per_minute, state.last_tool.as_deref().unwrap_or("?"))
        };
        state.recent.clear();
        state.trip = Some(reason.clone());
        Some(Refusal { reason, first: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn normal_heavy_work_and_calls_outside_the_sliding_window_are_allowed() {
        let guard = Guard::new(Limits::default(), true);
        for _ in 0..300 {
            assert!(guard.record("jq", None).is_none());
        }
        tokio::time::advance(Duration::from_secs(59)).await;
        for _ in 0..300 {
            assert!(guard.record("fs_read", None).is_none());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(guard.record("jq", None).is_none());
        assert_eq!(guard.activity().count, 601);
        let refusal = guard.record("fs_read", None).unwrap();
        assert!(refusal.first);
        assert!(refusal.message().contains("more than 300 calls to fs_read in 60 seconds"));
        assert!(guard.stop_reason().is_none());
    }

    #[test]
    fn total_counts_distinct_native_items_and_unmatched_mcp_calls() {
        let guard = Guard::new(Limits { same_tool_per_minute: 300, total_calls: 3 }, true);
        assert!(guard.record("exec", Some("command")).is_none());
        assert!(guard.record("exec", Some("command")).is_none());
        assert!(guard.record("jq", None).is_none());
        assert!(guard.record("fs_read", None).is_none());
        let refusal = guard.record("apply_patch", Some("patch")).unwrap();
        assert!(refusal.message().contains("more than 3 tool calls in one run"));
        for _ in 0..10 {
            assert!(guard.record("apply_patch", Some("patch")).is_some());
        }
        assert_eq!(guard.activity().count, 4);
        assert!(guard.stop_reason().is_none());
    }
}
