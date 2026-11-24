//! エージェントの使用量（レート制限の枠）を集め、休むかどうかを決める。
//! cards/design/usage-awareness

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fuzukue_proto::{ServerEvent, UsageWindow};
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

pub struct Usage {
    windows: Mutex<BTreeMap<(String, String), UsageWindow>>,
    hub: Arc<Hub>,
    changed: tokio::sync::watch::Sender<()>,
    /// これを超えたら休む（0.0〜1.0）。FUZUKUE_PAUSE_AT で変えられる。
    pub pause_at: f32,
}

impl Usage {
    pub fn new(hub: Arc<Hub>) -> Self {
        let pause_at = fuzukue_proto::compat::env("FUZUKUE_PAUSE_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.95);
        Self { windows: Mutex::new(BTreeMap::new()), hub, changed: tokio::sync::watch::channel(()).0, pause_at }
    }

    pub(crate) fn restore(&self, windows: Vec<UsageWindow>) {
        for window in windows {
            if window.resets_at.is_some_and(|until| until > now()) {
                self.set(window);
            }
        }
        self.publish();
    }

    fn set(&self, w: UsageWindow) {
        self.windows.lock().unwrap().insert((w.agent.clone(), w.name.clone()), w);
    }

    /// Claude の `rate_limit_event.rate_limit_info`
    pub fn update_claude(&self, info: &Value) {
        if let Some(map) = info["unifiedWindows"].as_object() {
            for (key, w) in map {
                self.set(UsageWindow {
                    agent: "claude".into(),
                    name: window_name(key, None),
                    used: w["utilization"].as_f64().unwrap_or(0.0) as f32,
                    resets_at: w["resetsAt"].as_i64(),
                    rejected: w["status"].as_str().is_some_and(|s| s != "allowed" && s != "allowed_warning"),
                });
            }
        }
        self.publish();
    }

    /// Codex の `rateLimits`（`account/rateLimits/updated` / `account/rateLimits/read`）
    pub fn update_codex(&self, rl: &Value) {
        for key in ["primary", "secondary"] {
            let w = &rl[key];
            if w.is_null() {
                continue;
            }
            self.set(UsageWindow {
                agent: "codex".into(),
                name: window_name(key, w["windowDurationMins"].as_i64()),
                used: (w["usedPercent"].as_f64().unwrap_or(0.0) / 100.0) as f32,
                resets_at: w["resetsAt"].as_i64(),
                rejected: false,
            });
        }
        self.publish();
    }

    /// リセット時刻を過ぎた枠は 0 に戻した、今の値。
    pub fn current(&self) -> Vec<UsageWindow> {
        let t = now();
        self.windows
            .lock()
            .unwrap()
            .values()
            .map(|w| {
                let mut w = w.clone();
                if w.resets_at.is_some_and(|r| r <= t) {
                    w.used = 0.0;
                    w.rejected = false;
                    w.resets_at = None;
                }
                w
            })
            .collect()
    }

    pub fn snapshot(&self) -> ServerEvent {
        ServerEvent::Usage { windows: self.current(), pause_at: self.pause_at }
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
        self.current()
            .into_iter()
            .filter(|w| w.agent == agent && (w.rejected || w.used >= self.pause_at))
            .filter_map(|w| w.resets_at.map(|r| (r, format!("{} の {} 枠が {:.0}%", w.agent, w.name, w.used * 100.0))))
            .max_by_key(|(r, _)| *r)
    }

    /// エージェントに渡す、画面には出さない使用量の 1 行。
    pub fn context_line(&self) -> String {
        let t = now();
        let parts: Vec<String> = self
            .current()
            .iter()
            .map(|w| {
                let left = w.resets_at.map(|r| harness_format!(", resets in about {} min", "、リセットまで約{}分", ((r - t).max(0) + 59) / 60)).unwrap_or_default();
                harness_format!("{} {} window {:.0}%{}", "{} {}枠 {:.0}%{}", w.agent, w.name, w.used * 100.0, left)
            })
            .collect();
        if parts.is_empty() {
            harness_text!("[secretaire usage note] No data yet.", "[secretaire の使用量メモ] まだ値がない。").into()
        } else {
            harness_format!(
                "[secretaire usage note, hidden from the user] {}. Above {:.0}%, secretaire pauses work and resumes it after the reset.", "[secretaire の使用量メモ（ユーザーには見えない）] {}。{:.0}% を超えると secretaire が作業を中断し、リセット後に再開させる。",
                parts.join(" / "),
                self.pause_at * 100.0
            )
        }
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
