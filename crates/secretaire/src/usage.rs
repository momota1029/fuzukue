//! エージェントの使用量（レート制限の枠）を集め、休むかどうかを決める。
//! cards/design/usage-awareness

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fuzukue_proto::{ServerEvent, UsagePace, UsageWindow};
use fuzukue_view::usage::outlook;
use serde_json::Value;

use crate::hub::Hub;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn window_name(key: &str, minutes: Option<i64>) -> String {
    match (key, minutes) {
        ("five_hour", _) | (_, Some(300)) => "5時間".into(),
        ("seven_day", _) | (_, Some(10080)) => "7日".into(),
        (_, Some(m)) if m % 1440 == 0 => format!("{}日", m / 1440),
        (_, Some(m)) if m % 60 == 0 => format!("{}時間", m / 60),
        (k, _) => k.into(),
    }
}

const HISTORY_SEC: i64 = 30 * 60;
const MIN_SAMPLE_SPAN_SEC: i64 = 5 * 60;
const MIN_CONSUMPTION: f64 = 0.01;
const SOON_RESET_SECS: i64 = 30 * 60;
const RUNWAY_MARGIN_SECS: i64 = 5 * 60;

struct UsageEntry {
    window: UsageWindow,
    samples: VecDeque<(i64, f32)>,
    threshold_checks: Vec<ThresholdCheck>,
}

struct ThresholdCheck {
    threshold: f32,
    paused: bool,
}

impl UsageEntry {
    fn record(&mut self, mut window: UsageWindow, at: i64, measured: bool) {
        window.recent_pace = None;
        let same_reset = self.window.resets_at == window.resets_at;
        if !same_reset {
            self.threshold_checks.clear();
        }
        self.window = window;
        self.record_sample(at, same_reset, measured);
        // Latch the first failing reading even if several updates reach a watcher together.
        if !self.threshold_checks.is_empty() && !self.has_runway(at) {
            for check in &mut self.threshold_checks {
                check.paused |= self.window.rejected || self.window.used >= check.threshold;
            }
        }
    }

    fn record_sample(&mut self, at: i64, same_reset: bool, measured: bool) {
        if !measured || self.window.resets_at.is_none_or(|reset| reset <= at)
            || self.window.rejected || !self.window.used.is_finite() || self.window.used < 0.0
        {
            self.samples.clear();
            return;
        }
        let used = self.window.used.min(1.0);

        // A reset, a break in observations, or a falling counter starts a new series.
        if !same_reset || self.samples.back().is_some_and(|&(last_at, last_used)| {
            !(0..=UsagePace::MAX_AGE_SECS).contains(&(at - last_at)) || used < last_used
        }) {
            self.samples.clear();
        }
        while self.samples.front().is_some_and(|&(first_at, _)| at - first_at > HISTORY_SEC) {
            self.samples.pop_front();
        }
        // Keep the latest reading in each minute so bursts cannot crowd out older readings.
        if self.samples.back().is_some_and(|&(last_at, _)| last_at / 60 == at / 60) {
            self.samples.pop_back();
        }
        self.samples.push_back((at, used));
    }

    fn recent_pace(&self, now: i64) -> Option<UsagePace> {
        let &(first_at, first_used) = self.samples.front()?;
        let &(_, last_used) = self.samples.back()?;
        let consumed = f64::from(last_used) - f64::from(first_used);
        if consumed < MIN_CONSUMPTION {
            return None;
        }

        // Require observed consumption in at least two intervals, rather than a lone jump.
        let mut increases = self.samples.iter().zip(self.samples.iter().skip(1))
            .filter(|(before, after)| after.1 > before.1)
            .rev();
        let observed_at = increases.next()?.1.0;
        let span = observed_at - first_at;
        if increases.next().is_none() || span < MIN_SAMPLE_SPAN_SEC
            || !(0..=UsagePace::MAX_AGE_SECS).contains(&now.saturating_sub(observed_at))
        {
            return None;
        }
        Some(UsagePace {
            observed_at,
            used_per_hour: consumed * 3600.0 / span as f64,
        })
    }

    fn has_runway(&self, now: i64) -> bool {
        if self.window.rejected || self.window.used >= 1.0 {
            return false;
        }
        self.window.resets_at.is_some_and(|reset| {
            let seconds = reset.saturating_sub(now);
            (1..=SOON_RESET_SECS).contains(&seconds) && self.recent_pace(now)
                .and_then(|pace| pace.runway_seconds(self.window.used, now))
                .is_some_and(|runway| runway >= (seconds + RUNWAY_MARGIN_SECS) as f64)
        })
    }

    fn should_pause(&mut self, now: i64, threshold: f32, allow_projection: bool) -> bool {
        if self.window.resets_at.is_some_and(|reset| reset <= now) {
            return false;
        }
        if !(self.window.rejected || self.window.used >= threshold) {
            return false;
        }
        let check = self.threshold_checks.iter()
            .position(|check| check.threshold.to_bits() == threshold.to_bits());
        if check.is_some_and(|index| self.threshold_checks[index].paused) {
            return true;
        }
        let paused = !allow_projection || !self.has_runway(now);
        // A pause stays in effect for this reset, even if a later projection improves.
        if self.window.resets_at.is_some() {
            if let Some(index) = check {
                self.threshold_checks[index].paused = paused;
            } else {
                self.threshold_checks.push(ThresholdCheck { threshold, paused });
            }
        }
        paused
    }
}

pub struct Usage {
    windows: Mutex<BTreeMap<(String, String), UsageEntry>>,
    hub: Arc<Hub>,
    changed: tokio::sync::watch::Sender<()>,
    /// 休むかを判定する閾値（0.0〜1.0）。FUZUKUE_PAUSE_AT で変えられる。
    pub pause_at: f32,
}

impl Usage {
    pub fn new(hub: Arc<Hub>) -> Self {
        let pause_at = fuzukue_proto::compat::env("FUZUKUE_PAUSE_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.95);
        Self { windows: Mutex::new(BTreeMap::new()), hub, changed: tokio::sync::watch::channel(()).0, pause_at }
    }

    pub(crate) fn restore(&self, windows: Vec<UsageWindow>) {
        let t = now();
        self.record_windows(t, windows.into_iter()
            .filter(|window| window.resets_at.is_some_and(|until| until > t))
            .map(|window| (window, false)));
        self.publish();
    }

    fn record_windows(&self, at: i64, readings: impl IntoIterator<Item = (UsageWindow, bool)>) {
        let mut windows = self.windows.lock().unwrap();
        for (window, measured) in readings {
            windows.entry((window.agent.clone(), window.name.clone()))
                .or_insert_with(|| UsageEntry {
                    window: window.clone(), samples: VecDeque::new(), threshold_checks: Vec::new(),
                })
                .record(window, at, measured);
        }
    }

    /// Claude の `rate_limit_event.rate_limit_info`
    pub fn update_claude(&self, info: &Value) {
        if let Some(map) = info["unifiedWindows"].as_object() {
            self.record_windows(now(), map.iter().map(|(key, w)| {
                let used = w["utilization"].as_f64();
                let window = UsageWindow {
                    agent: "claude".into(),
                    name: window_name(key, None),
                    used: used.unwrap_or(0.0) as f32,
                    resets_at: w["resetsAt"].as_i64(),
                    rejected: w["status"].as_str().is_some_and(|s| s != "allowed" && s != "allowed_warning"),
                    recent_pace: None,
                };
                (window, used.is_some())
            }));
        }
        self.publish();
    }

    /// Codex の `rateLimits`（`account/rateLimits/updated` / `account/rateLimits/read`）
    pub fn update_codex(&self, rl: &Value) {
        self.record_windows(now(), ["primary", "secondary"].into_iter().filter_map(|key| {
            let w = &rl[key];
            if w.is_null() {
                return None;
            }
            let used = w["usedPercent"].as_f64();
            let window = UsageWindow {
                agent: "codex".into(),
                name: window_name(key, w["windowDurationMins"].as_i64()),
                used: (used.unwrap_or(0.0) / 100.0) as f32,
                resets_at: w["resetsAt"].as_i64(),
                rejected: false,
                recent_pace: None,
            };
            Some((window, used.is_some()))
        }));
        self.publish();
    }

    /// リセット時刻を過ぎた枠は 0 に戻した、今の値。
    pub fn current(&self) -> Vec<UsageWindow> {
        self.windows_at(now(), false)
    }

    fn windows_at(&self, t: i64, display: bool) -> Vec<UsageWindow> {
        self.windows
            .lock()
            .unwrap()
            .values()
            .map(|entry| {
                let mut w = entry.window.clone();
                if w.resets_at.is_some_and(|r| r <= t) {
                    w.used = 0.0;
                    w.rejected = false;
                    w.resets_at = None;
                } else if display {
                    w.recent_pace = entry.recent_pace(t);
                }
                w
            })
            .collect()
    }

    pub fn snapshot(&self) -> ServerEvent {
        ServerEvent::Usage { windows: self.windows_at(now(), true), pause_at: self.pause_at }
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    fn publish(&self) {
        self.changed.send_replace(());
        self.hub.publish_snapshot(self.snapshot());
    }

    /// その agent が休むべきなら、再開してよい時刻と理由。閾値を超えた枠のうち、
    /// 一番遅くリセットされるものまで待つ。
    pub fn blocked(&self, agent: &str) -> Option<(i64, String)> {
        self.blocked_at(agent, now(), true)
    }

    pub(crate) fn provider_limit(&self, agent: &str) -> Option<(i64, String)> {
        self.blocked_at(agent, now(), false)
    }

    fn blocked_at(&self, agent: &str, t: i64, allow_projection: bool) -> Option<(i64, String)> {
        self.pause_windows(t, self.pause_at, allow_projection, |w| w.agent == agent)
        .into_iter()
        .filter_map(|w| w.resets_at.map(|r| (r, format!("{} の {} 枠が {:.0}%", w.agent, w.name, w.used * 100.0))))
        .max_by_key(|(r, _)| *r)
    }

    pub(crate) fn pause_windows(
        &self, t: i64, threshold: f32, allow_projection: bool,
        matches: impl Fn(&UsageWindow) -> bool,
    ) -> Vec<UsageWindow> {
        let mut windows = self.windows.lock().unwrap();
        // A window without a reset cannot qualify. Retain any known pause deadlines
        // when that window is also over the threshold.
        let allow_projection = allow_projection && !windows.values().any(|entry| {
            let window = &entry.window;
            matches(window) && window.resets_at.is_none()
                && (window.rejected || window.used >= threshold)
        });
        windows.values_mut()
            .filter_map(|entry| {
                (matches(&entry.window) && entry.should_pause(t, threshold, allow_projection))
                    .then(|| entry.window.clone())
            })
            .collect()
    }

    /// エージェントに渡す、画面には出さない使用量の 1 行。
    pub fn context_line(&self) -> String {
        self.context_line_at(now())
    }

    fn context_line_at(&self, t: i64) -> String {
        let parts: Vec<String> = self
            .windows_at(t, true)
            .iter()
            .map(|w| {
                let mut line = harness_format!("{} {} window {:.0}%", "{} {}枠 {:.0}%", w.agent, w.name, w.used * 100.0);
                if let Some(look) = outlook(w, t) {
                    if let Some(minutes) = look.reset_minutes {
                        line.push_str(&harness_format!(", resets in about {minutes} min", "、リセットまで約{minutes}分"));
                    } else {
                        line.push_str(harness_text!(", resets in <1 min", "、リセットまで1分未満"));
                    }
                    if let Some(rate) = look.percent_per_hour {
                        line.push_str(&harness_format!(", {rate}%/h available until reset", "、リセットまで毎時{rate}%使える"));
                    }
                    if let Some(estimate) = look.estimate {
                        let minutes = estimate.minutes;
                        line.push_str(&if estimate.reaches_reset {
                            harness_format!(", about {minutes} min left at recent pace (enough until reset)", "、最近のペースなら残り約{minutes}分（リセットまで持つ）")
                        } else {
                            harness_format!(", about {minutes} min left at recent pace (runs out before reset)", "、最近のペースなら残り約{minutes}分（リセット前に使い切る）")
                        });
                    }
                }
                line
            })
            .collect();
        if parts.is_empty() {
            harness_text!("[secretaire usage note] No data yet.", "[secretaire の使用量メモ] まだ値がない。").into()
        } else {
            harness_format!(
                "[secretaire usage note, hidden from the user] {}. From {:.0}%, work pauses until reset. A window can stay open if it resets within {} min and recent use leaves at least {} min to spare.",
                "[secretaire の使用量メモ（ユーザーには見えない）] {}。{:.0}% に達すると、リセットまで{}分以内で、最近のペースで残量に{}分以上の余裕がある場合を除き、リセットまで休む。",
                parts.join(" / "),
                self.pause_at * 100.0,
                SOON_RESET_SECS / 60,
                RUNWAY_MARGIN_SECS / 60
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: i64 = 100_000;

    fn usage() -> Usage {
        let mut usage = Usage::new(Arc::new(Hub::new()));
        usage.pause_at = 0.95;
        usage
    }

    fn window(name: &str, used: f32, reset: Option<i64>) -> UsageWindow {
        UsageWindow {
            agent: "claude".into(), name: name.into(), used, resets_at: reset,
            rejected: false, recent_pace: None,
        }
    }

    fn readings(usage: &Usage, name: &str, reset: i64, samples: &[(i64, f32)]) {
        for &(at, used) in samples {
            usage.record_windows(at, [(window(name, used, Some(reset)), true)]);
        }
    }

    fn steady(usage: &Usage, reset: i64) {
        readings(usage, "5h", reset, &[(AT - 1200, 0.941), (AT - 600, 0.949), (AT, 0.96)]);
    }

    fn blocked(usage: &Usage, at: i64) -> Option<i64> {
        usage.blocked_at("claude", at, true).map(|(until, _)| until)
    }

    #[test]
    fn exception_requires_a_reset_within_thirty_minutes_and_five_minutes_to_spare() {
        let enough = usage();
        steady(&enough, AT + SOON_RESET_SECS);
        assert_eq!(blocked(&enough, AT), None);

        let too_far = usage();
        steady(&too_far, AT + SOON_RESET_SECS + 1);
        assert_eq!(blocked(&too_far, AT), Some(AT + SOON_RESET_SECS + 1));

        // These readings imply exactly 30 minutes of runway.
        for (reset_in, expected) in [(25 * 60, None), (25 * 60 + 1, Some(AT + 25 * 60 + 1))] {
            let usage = usage();
            readings(&usage, "5h", AT + reset_in, &[
                (AT - 900, 0.953125), (AT - 450, 0.9609375), (AT, 0.96875),
            ]);
            assert_eq!(blocked(&usage, AT), expected);
        }
    }

    #[test]
    fn sparse_stale_or_reset_crossing_history_cannot_grant_an_exception() {
        let reset = AT + 1800;
        let sparse = usage();
        readings(&sparse, "5h", reset, &[(AT - 1800, 0.943), (AT - 300, 0.949), (AT, 0.96)]);
        assert_eq!(blocked(&sparse, AT), Some(reset));

        let stale = usage();
        readings(&stale, "5h", reset, &[
            (AT - 600, 0.94), (AT - 300, 0.949), (AT, 0.96),
            (AT + 300, 0.96), (AT + 800, 0.96),
        ]);
        // A repeated reading is fresh quota data, but does not make an old increase recent.
        assert!(stale.windows_at(AT + 800, true)[0].recent_pace.is_some());
        assert!(stale.windows_at(AT + UsagePace::MAX_AGE_SECS + 1, true)[0].recent_pace.is_none());
        assert_eq!(blocked(&stale, AT + UsagePace::MAX_AGE_SECS + 1), Some(reset));

        let rollover = usage();
        readings(&rollover, "5h", AT, &[(AT - 1200, 0.943), (AT - 600, 0.949)]);
        readings(&rollover, "5h", reset, &[(AT, 0.96)]);
        assert_eq!(blocked(&rollover, AT), Some(reset));

        let unknown = usage();
        unknown.record_windows(AT, [(window("7d", 0.96, None), true)]);
        assert_eq!(unknown.pause_windows(AT, 0.95, true, |_| true).len(), 1);
        steady(&unknown, reset);
        assert_eq!(blocked(&unknown, AT), Some(reset));
    }

    #[test]
    fn every_over_threshold_window_must_qualify_and_provider_limits_take_priority() {
        let shared = usage();
        let reset = AT + 1800;
        steady(&shared, reset);
        readings(&shared, "7d", AT + 900, &[(AT, 0.96)]);
        assert_eq!(blocked(&shared, AT), Some(AT + 900));

        let mut rejected = window("5h", 0.96, Some(reset));
        rejected.rejected = true;
        shared.record_windows(AT + 60, [(rejected, true)]);
        assert_eq!(blocked(&shared, AT + 60), Some(reset));

        let exhausted = usage();
        steady(&exhausted, reset);
        readings(&exhausted, "5h", reset, &[(AT + 60, 1.0)]);
        assert_eq!(blocked(&exhausted, AT + 60), Some(reset));
    }

    #[test]
    fn provider_error_overrides_a_safe_projection() {
        let usage = usage();
        let reset = AT + 1800;
        steady(&usage, reset);
        assert_eq!(blocked(&usage, AT), None);
        assert_eq!(usage.blocked_at("claude", AT, false).map(|(until, _)| until), Some(reset));
        assert_eq!(blocked(&usage, AT), Some(reset));
    }

    #[test]
    fn note_displays_budget_and_only_supported_runway() {
        let usage = usage();
        let reset = AT + 900;
        readings(&usage, "5h", reset, &[(AT, 0.89)]);
        let note = usage.context_line_at(AT);
        assert!(note.contains(harness_text!("44%/h available", "毎時44%使える")));
        assert!(!note.contains(harness_text!("left at recent pace", "最近のペースなら")));

        let steady_usage = self::usage();
        steady(&steady_usage, reset);
        assert!(steady_usage.context_line_at(AT)
            .contains(harness_text!("enough until reset", "リセットまで持つ")));
    }

    #[test]
    fn worsening_pace_pauses_and_improving_readings_do_not_restart_before_reset() {
        let usage = usage();
        let reset = AT + 1800;
        steady(&usage, reset);
        assert_eq!(blocked(&usage, AT), None);

        // The watcher can see both readings at once. The first failure must still pause.
        readings(&usage, "5h", reset, &[
            (AT + 60, 0.99), (AT + 120, 0.96), (AT + 420, 0.961), (AT + 720, 0.971),
        ]);
        assert!(usage.windows.lock().unwrap().values().next().unwrap().has_runway(AT + 720));
        assert_eq!(blocked(&usage, AT + 720), Some(reset));

        // Honor a provider correction below the threshold as before.
        readings(&usage, "5h", reset, &[(AT + 780, 0.5)]);
        assert_eq!(blocked(&usage, AT + 780), None);
        assert_eq!(blocked(&usage, reset), None);
        readings(&usage, "5h", reset + 18_000, &[(reset + 60, 0.30)]);
        assert_eq!(blocked(&usage, reset + 60), None);
    }
}

/// エージェントのシステムプロンプトに足す、使用量の扱い方。
pub fn policy() -> &'static str {
    harness_text!(
        "secretaire may prepend a usage note to the user’s message. Use it only to plan work. Mention usage only when it changes the plan, such as splitting work, pausing at a checkpoint, or sending work to Codex. secretaire pauses work near a limit and asks you to resume after the window resets. Save progress at checkpoints during long tasks and continue from where you stopped when asked to resume.",
        "\
ユーザーのメッセージの前に、secretaire が「使用量メモ」を付けることがある。これは作業の計画にだけ使う。\
使用量を話題にするのは、計画を変えるとき（作業を分ける、区切りで止める、Codex に回す）だけにする。\
使用量の枠が上限に近づくと、secretaire が作業を自動で中断し、枠がリセットされたら「続きから再開して」と伝える。\
長い作業は区切りのよいところで成果を保存しながら進め、再開の指示が来たら中断したところから続ける。",
    )
}
