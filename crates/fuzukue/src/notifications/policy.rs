use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    time::{Duration, Instant},
};

use fuzukue_proto::{NotificationKind, SelfUpdateStatus, SelfUpdateStepKind, SelfUpdateStepState};
use serde::{Deserialize, Serialize};

const COOLDOWN: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Thread {
    pub workspace: String,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Destination {
    Thread(Thread),
    Update,
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action<'a> {
    Select(&'a str),
    Switch(&'a str),
    Update,
    Test,
}

impl Destination {
    pub fn action(&self, workspace: Option<&str>) -> Action<'_> {
        match self {
            Self::Thread(thread) if workspace == Some(thread.workspace.as_str()) => {
                Action::Select(&thread.session)
            }
            Self::Thread(thread) => Action::Switch(&thread.workspace),
            Self::Update => Action::Update,
            Self::Test => Action::Test,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Thread(NotificationKind),
    UpdateFinished,
    UpdateFailed,
    Test,
}

impl Kind {
    pub fn urgent(self) -> bool {
        matches!(self, Self::Thread(kind) if kind.needs_user())
    }

    pub fn message(self) -> &'static str {
        use NotificationKind::*;
        match self {
            Self::Thread(TurnFinished) => "notifications-turn-finished",
            Self::Thread(TurnFailed) => "notifications-turn-failed",
            Self::Thread(Approval) => "notifications-approval",
            Self::Thread(Question) => "notifications-question",
            Self::Thread(GoalBlocked) => "notifications-goal",
            Self::Thread(WorkerFinished) => "notifications-worker-finished",
            Self::Thread(WorkerFailed) => "notifications-worker-failed",
            Self::Thread(WorkerStalled) => "notifications-worker-stalled",
            Self::UpdateFinished => "notifications-update-finished",
            Self::UpdateFailed => "notifications-update-failed",
            Self::Test => "notifications-test-body",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Settings {
    pub enabled: bool,
    pub only_when_away: bool,
    pub sound: bool,
    pub banner: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    threads: BTreeMap<String, BTreeMap<String, ThreadSettings>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: !running_under_wsl(),
            only_when_away: true,
            sound: true,
            banner: true,
            threads: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct ThreadSettings {
    pub muted: bool,
    pub workers: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Outputs {
    pub banner: bool,
    pub sound: bool,
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::from_table(&crate::settings_file::read(path)?)
    }

    fn from_table(document: &toml::Table) -> anyhow::Result<Self> {
        Ok(document
            .get("notifications")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()?
            .unwrap_or_default())
    }

    pub fn save(path: &Path, change: impl FnOnce(&mut Self)) -> anyhow::Result<Self> {
        let mut saved = None;
        crate::settings_file::update(path, |document| {
            let mut settings = Self::from_table(document)?;
            change(&mut settings);
            document.insert("notifications".into(), toml::Value::try_from(&settings)?);
            saved = Some(settings);
            Ok(true)
        })?;
        Ok(saved.expect("settings update ran"))
    }

    pub fn thread(&self, thread: &Thread) -> ThreadSettings {
        self.threads
            .get(&thread.workspace)
            .and_then(|threads| threads.get(&thread.session))
            .copied()
            .unwrap_or_default()
    }

    pub fn edit_thread(&mut self, thread: &Thread, change: impl FnOnce(&mut ThreadSettings)) {
        let threads = self.threads.entry(thread.workspace.clone()).or_default();
        let entry = threads.entry(thread.session.clone()).or_default();
        change(entry);
        if *entry == ThreadSettings::default() {
            threads.remove(&thread.session);
        }
        if threads.is_empty() {
            self.threads.remove(&thread.workspace);
        }
    }

    fn outputs(&self, target: &Destination, kind: Kind, looking: bool) -> Option<Outputs> {
        if !self.banner && !self.sound {
            return None;
        }
        let outputs = Outputs {
            banner: self.banner,
            sound: self.sound,
        };
        if kind == Kind::Test {
            return Some(outputs);
        }
        if !self.enabled || (self.only_when_away && looking) {
            return None;
        }
        if let Destination::Thread(thread) = target {
            let settings = self.thread(thread);
            if settings.muted
                || matches!(kind, Kind::Thread(kind) if kind.is_worker() && !settings.workers)
            {
                return None;
            }
        }
        Some(outputs)
    }
}

pub(super) fn running_under_wsl() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("WSL_INTEROP").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft")))
}

#[derive(Default)]
pub(super) struct Policy {
    recent: HashMap<Destination, (Instant, bool)>,
    update_active: bool,
}

impl Policy {
    pub fn decide(
        &mut self,
        settings: &Settings,
        target: &Destination,
        kind: Kind,
        looking: bool,
        now: Instant,
    ) -> Option<Outputs> {
        let outputs = settings.outputs(target, kind, looking)?;
        if kind != Kind::Test {
            self.recent
                .retain(|_, (at, _)| now.duration_since(*at) < COOLDOWN);
            if self
                .recent
                .get(target)
                .is_some_and(|(_, urgent)| *urgent || !kind.urgent())
            {
                return None;
            }
            self.recent.insert(target.clone(), (now, kind.urgent()));
        }
        Some(outputs)
    }

    pub fn update(&mut self, status: &SelfUpdateStatus) -> Option<Kind> {
        let was_active = std::mem::replace(&mut self.update_active, status.active);
        if !was_active || status.active || status.cancelled {
            return None;
        }
        if status.error.is_some() {
            Some(Kind::UpdateFailed)
        } else if status.steps.iter().any(|step| {
            step.kind == SelfUpdateStepKind::Swap && step.state == SelfUpdateStepState::Passed
        }) {
            Some(Kind::UpdateFinished)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NotificationKind::*;

    fn thread(workspace: &str, session: &str) -> Thread {
        Thread {
            workspace: workspace.into(),
            session: session.into(),
        }
    }

    #[test]
    fn focus_mute_and_worker_opt_in_apply_before_throttling() {
        let thread = thread("lab", "a");
        let target = Destination::Thread(thread.clone());
        let mut settings = Settings {
            enabled: true,
            ..Settings::default()
        };
        let mut policy = Policy::default();
        let now = Instant::now();
        assert_eq!(
            policy.decide(&settings, &target, Kind::Thread(TurnFinished), true, now),
            None
        );
        assert_eq!(
            policy.decide(&settings, &target, Kind::Thread(WorkerFailed), false, now),
            None
        );
        settings.edit_thread(&thread, |thread| thread.workers = true);
        assert!(policy
            .decide(&settings, &target, Kind::Thread(WorkerFailed), false, now)
            .is_some());
        settings.edit_thread(&thread, |thread| thread.muted = true);
        assert_eq!(
            policy.decide(&settings, &target, Kind::Thread(Approval), false, now),
            None
        );
        settings.edit_thread(&thread, |thread| thread.muted = false);
        settings.only_when_away = false;
        assert!(policy
            .decide(&settings, &target, Kind::Thread(Approval), true, now)
            .is_some());
    }

    #[test]
    fn a_needs_user_event_can_follow_completion_in_the_same_burst() {
        let settings = Settings {
            enabled: true,
            ..Settings::default()
        };
        let a = Destination::Thread(thread("lab", "a"));
        let b = Destination::Thread(thread("other", "a"));
        let mut policy = Policy::default();
        let now = Instant::now();
        assert!(policy
            .decide(&settings, &a, Kind::Thread(TurnFinished), false, now)
            .is_some());
        assert!(policy
            .decide(&settings, &a, Kind::Thread(Question), false, now)
            .is_some());
        assert_eq!(
            policy.decide(&settings, &a, Kind::Thread(Approval), false, now),
            None
        );
        assert_eq!(
            policy.decide(&settings, &a, Kind::Thread(TurnFinished), false, now),
            None
        );
        assert!(policy
            .decide(&settings, &b, Kind::Thread(TurnFinished), false, now)
            .is_some());
        assert!(policy
            .decide(
                &settings,
                &a,
                Kind::Thread(TurnFinished),
                false,
                now + COOLDOWN
            )
            .is_some());
    }

    #[test]
    fn test_uses_selected_outputs_without_the_master_switch_focus_or_cooldown() {
        let settings = Settings {
            enabled: false,
            banner: false,
            sound: true,
            ..Settings::default()
        };
        let mut policy = Policy::default();
        for _ in 0..2 {
            assert_eq!(
                policy.decide(
                    &settings,
                    &Destination::Test,
                    Kind::Test,
                    true,
                    Instant::now()
                ),
                Some(Outputs {
                    banner: false,
                    sound: true
                })
            );
        }
        assert_eq!(
            policy.decide(
                &Settings {
                    sound: false,
                    ..settings
                },
                &Destination::Test,
                Kind::Test,
                true,
                Instant::now()
            ),
            None
        );
    }

    #[test]
    fn settings_round_trip_with_a_workspace_scoped_thread_and_other_settings() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.toml");
        std::fs::write(&path, "locale = 'ja'\n[other]\nkey = 1\n").unwrap();
        let target = thread("lab", "a");
        let saved = Settings::save(&path, |settings| {
            settings.only_when_away = false;
            settings.edit_thread(&target, |thread| thread.muted = true);
        })
        .unwrap();
        assert_eq!(Settings::load(&path).unwrap(), saved);
        assert_eq!(
            saved.thread(&thread("another", "a")),
            ThreadSettings::default()
        );
        let document = crate::settings_file::read(&path).unwrap();
        assert_eq!(document["locale"].as_str(), Some("ja"));
        assert_eq!(document["other"]["key"].as_integer(), Some(1));
    }

    #[test]
    fn update_notifies_only_a_terminal_edge_after_an_observed_run() {
        let mut policy = Policy::default();
        let mut status = SelfUpdateStatus {
            error: Some("private".into()),
            ..Default::default()
        };
        assert_eq!(policy.update(&status), None);
        status.active = true;
        assert_eq!(policy.update(&status), None);
        status.active = false;
        assert_eq!(policy.update(&status), Some(Kind::UpdateFailed));
        assert_eq!(policy.update(&status), None);
        status.active = true;
        status.error = None;
        assert_eq!(policy.update(&status), None);
        status.active = false;
        status.steps.push(fuzukue_proto::SelfUpdateStep {
            kind: SelfUpdateStepKind::Swap,
            state: SelfUpdateStepState::Passed,
            log: String::new(),
        });
        assert_eq!(policy.update(&status), Some(Kind::UpdateFinished));
    }

    #[test]
    fn click_routes_to_the_thread_in_its_own_workspace() {
        let destination = Destination::Thread(thread("lab", "a"));
        assert_eq!(destination.action(Some("lab")), Action::Select("a"));
        assert_eq!(destination.action(Some("another")), Action::Switch("lab"));
        assert_eq!(Destination::Update.action(None), Action::Update);
        assert_eq!(Destination::Test.action(None), Action::Test);
    }
}
