use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::poll::{Observation, Sample};

fn origin() -> String {
    "origin".into()
}
fn interval() -> u32 {
    60
}
fn stale_minutes() -> u32 {
    25
}
fn true_value() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Wake {
    All,
    #[default]
    Attention,
    Silent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    pub name: String,
    pub repo: PathBuf,
    #[serde(default = "origin")]
    remote: String,
    #[serde(default = "interval")]
    interval: u32,
    #[serde(default)]
    wake: Wake,
    #[serde(default)]
    paused: bool,
    // Static watches can start with a designated thread. The MCP caller supplies its own.
    pub thread: Option<String>,
    kind: Option<Preset>,
    sources: Option<Vec<Source>>,
    attention_patterns: Option<Vec<String>>,
    #[serde(default = "true_value")]
    attention_marked_lines: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Preset {
    Dots,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Watch {
    pub id: String,
    pub name: String,
    pub repo: PathBuf,
    pub remote: String,
    pub interval: u32,
    pub thread: String,
    pub wake: Wake,
    pub paused: bool,
    pub sources: Vec<Source>,
    attention_patterns: Vec<String>,
    attention_marked_lines: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Source {
    BranchHeads {
        pattern: String,
    },
    NewFile {
        branch: String,
        glob: Option<String>,
        regex: Option<String>,
        #[serde(default)]
        order: Order,
    },
    Heartbeat {
        branch: String,
        file: String,
        #[serde(default = "stale_minutes")]
        stale_minutes: u32,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Order {
    #[default]
    Number,
}

const DEFAULT_PATTERNS: &[&str] = &[
    "needs-owner",
    "needs owner",
    "owner decision needed",
    "owner decision required",
    "question for owner",
    "questions for owner",
    "決定待ち",
    "要判断",
    "オーナー判断",
    "ユーザー判断",
    "判断を仰",
];

impl Input {
    pub fn into_watch(self, id: String, base: &Path, thread: &str) -> Result<Watch> {
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 120
                && !self.name.chars().any(char::is_control),
            "監視名を指定する"
        );
        ensure!(self.interval >= 15, "監視間隔は 15 秒以上");
        ensure!(!self.repo.as_os_str().is_empty(), "リポジトリを指定する");
        ensure!(
            valid_remote(&self.remote),
            "remote には登録済みのリモート名を指定する"
        );
        let sources = match (self.kind, self.sources) {
            (Some(Preset::Dots), None) => dots(&self.name)?,
            (None, Some(sources)) if !sources.is_empty() => sources,
            _ => anyhow::bail!("kind = dots か sources のどちらかを指定する"),
        };
        let mut unique = std::collections::HashSet::new();
        for source in &sources {
            source.validate()?;
            ensure!(
                unique.insert(serde_json::to_vec(source)?),
                "同じ監視対象が重複している"
            );
        }
        let attention_patterns = self
            .attention_patterns
            .unwrap_or_else(|| DEFAULT_PATTERNS.iter().map(|s| (*s).into()).collect());
        ensure!(
            attention_patterns
                .iter()
                .all(|pattern| !pattern.trim().is_empty() && !pattern.contains(['\n', '\r'])),
            "判断依頼の目印は空でない一行を指定する"
        );
        let repo = if let Ok(relative) = self.repo.strip_prefix("~") {
            let home = std::env::var_os("HOME").context("HOME が設定されていない")?;
            PathBuf::from(home).join(relative)
        } else {
            base.join(self.repo)
        };
        Ok(Watch {
            id,
            name: self.name,
            repo,
            remote: self.remote,
            interval: self.interval,
            thread: self.thread.unwrap_or_else(|| thread.into()),
            wake: self.wake,
            paused: self.paused,
            sources,
            attention_patterns,
            attention_marked_lines: self.attention_marked_lines,
        })
    }
}

fn valid_remote(remote: &str) -> bool {
    !remote.trim().is_empty() && !remote.starts_with('-') && !remote.chars().any(char::is_control)
}

pub(super) fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.ends_with('.')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch
            .chars()
            .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

fn dots(name: &str) -> Result<Vec<Source>> {
    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
        "dots の監視名には英数字、-、_ を使う"
    );
    let branch = format!("work/{name}");
    let report = if name == "dots-audit" {
        "notes/audit/cycle-*.md"
    } else {
        "notes/handoff/dots-cycle-*.md"
    };
    Ok(vec![
        Source::BranchHeads {
            pattern: format!("refs/heads/{branch}"),
        },
        Source::NewFile {
            branch: branch.clone(),
            glob: Some(report.into()),
            regex: None,
            order: Order::Number,
        },
        Source::Heartbeat {
            branch,
            file: format!("notes/heartbeat/{name}.md"),
            stale_minutes: stale_minutes(),
        },
    ])
}

impl Source {
    fn validate(&self) -> Result<()> {
        if let Self::BranchHeads { pattern } = self {
            ensure!(
                !pattern.is_empty()
                    && !pattern.starts_with('-')
                    && !pattern.chars().any(char::is_control),
                "ブランチのパターンを指定する"
            );
        }
        if let Some(branch) = self.branch() {
            ensure!(valid_branch(branch), "無効なブランチ名: {branch}");
        }
        match self {
            Self::NewFile { glob, regex, .. } => {
                report_matcher(glob.as_deref(), regex.as_deref())?;
            }
            Self::Heartbeat {
                file,
                stale_minutes,
                ..
            } => {
                ensure!(*stale_minutes > 0, "stale_minutes は 1 以上");
                ensure!(
                    !file.is_empty()
                        && !file.contains(['\n', '\r', '\0'])
                        && Path::new(file)
                            .components()
                            .all(|part| matches!(part, Component::Normal(_))),
                    "ハートビートにはリポジトリ相対のファイルパスを指定する"
                );
            }
            Self::BranchHeads { .. } => {}
        }
        Ok(())
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::BranchHeads { .. } => None,
            Self::NewFile { branch, .. } | Self::Heartbeat { branch, .. } => Some(branch),
        }
    }

    pub fn key(&self, watch: &Watch) -> String {
        // Changing the repo, remote or source starts a new baseline for that source.
        let input =
            serde_json::to_vec(&(&watch.repo, &watch.remote, self)).expect("serializable source");
        format!("{:x}", Sha256::digest(input))
    }

    fn label(&self) -> String {
        match self {
            Self::BranchHeads { pattern } => format!("branches {pattern}"),
            Self::NewFile { branch, .. } => format!("reports {branch}"),
            Self::Heartbeat { branch, file, .. } => format!("{branch}:{file}"),
        }
    }
}

pub(super) fn report_matcher(glob: Option<&str>, regex: Option<&str>) -> Result<regex::Regex> {
    let pattern = match (glob, regex) {
        (Some(glob), None) => {
            let (prefix, suffix) = glob.split_once('*').context("glob の * に報告番号が入る")?;
            ensure!(
                !suffix.contains('*') && !glob.contains(['?', '[', ']']),
                "glob には番号を表す * を一つ使う"
            );
            format!(
                "^{}([0-9]+){}$",
                regex::escape(prefix),
                regex::escape(suffix)
            )
        }
        (None, Some(regex)) => regex.to_owned(),
        _ => anyhow::bail!("glob か regex のどちらかを指定する"),
    };
    let regex = regex::Regex::new(&pattern)?;
    ensure!(
        regex.captures_len() == 2,
        "regex には報告番号を読む捕捉グループを一つ使う"
    );
    Ok(regex)
}

pub(super) fn report_number(regex: &regex::Regex, path: &str) -> Option<u64> {
    let number = regex.captures(path)?.get(1)?.as_str();
    if number.bytes().all(|byte| byte.is_ascii_digit()) {
        number.parse().ok()
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(super) struct Heartbeat {
    pub status: String,
    pub cycle: Option<u64>,
    pub updated: Option<i64>,
    pub activity: String,
}

impl Heartbeat {
    pub fn parse(text: &str) -> Result<Self> {
        let fields: BTreeMap<_, _> = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let line = line.strip_prefix("- ").unwrap_or(line);
                let (key, value) = line.split_once(':')?;
                Some((key.trim().to_ascii_lowercase(), value.trim()))
            })
            .collect();
        let status = fields
            .get("status")
            .copied()
            .unwrap_or("")
            .to_ascii_lowercase();
        let updated = ["updated", "heartbeat_utc"]
            .iter()
            .filter_map(|key| fields.get(*key))
            .find_map(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|time| time.timestamp());
        ensure!(
            status == "completed" || updated.is_some(),
            "heartbeat needs an RFC 3339 updated or heartbeat_utc timestamp"
        );
        Ok(Self {
            status,
            updated,
            cycle: fields.get("cycle").and_then(|value| value.parse().ok()),
            activity: fields
                .get("activity")
                .copied()
                .unwrap_or("")
                .chars()
                .take(500)
                .collect(),
        })
    }

    fn stale(&self, now: i64, stale_minutes: u32) -> bool {
        self.status != "completed"
            && self
                .updated
                .is_some_and(|updated| now.saturating_sub(updated) > i64::from(stale_minutes) * 60)
    }

    fn detail(&self) -> String {
        format!(
            "status: {} · cycle: {} · updated: {}\n{}",
            self.status,
            self.cycle
                .map_or_else(|| "—".into(), |cycle| cycle.to_string()),
            self.updated
                .and_then(|time| DateTime::from_timestamp(time, 0))
                .map_or_else(|| "—".into(), |time| time.to_rfc3339()),
            self.activity
        )
    }
}

pub(super) fn attention_line(text: &str, patterns: &[String], marked_only: bool) -> Option<usize> {
    let patterns: Vec<_> = patterns
        .iter()
        .map(|pattern| pattern.to_lowercase())
        .collect();
    let mut fence = None;
    text.lines().enumerate().find_map(|(index, line)| {
        let line = line.trim_start();
        if let Some((marker, length)) = fence {
            if line.starts_with(marker)
                && line.chars().take_while(|c| *c == marker).count() >= length
                && line.trim_start_matches(marker).trim().is_empty()
            {
                fence = None;
            }
            return None;
        }
        if let Some(marker) = line.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let length = line.chars().take_while(|c| *c == marker).count();
            if length >= 3 {
                fence = Some((marker, length));
                return None;
            }
        }
        let heading = line.starts_with('#')
            && line
                .trim_start_matches('#')
                .starts_with(char::is_whitespace);
        let marked = heading
            || ["- ", "* ", "+ ", "**", "__"]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            || line.trim_start_matches(|c: char| c.is_ascii_digit()).len() < line.len()
                && [". ", ") "].iter().any(|prefix| {
                    line.trim_start_matches(|c: char| c.is_ascii_digit())
                        .starts_with(prefix)
                });
        let lower = line.to_lowercase();
        ((!marked_only || marked) && patterns.iter().any(|pattern| lower.contains(pattern)))
            .then_some(index)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    BranchCreated,
    BranchAdvanced,
    Report,
    Stale,
    AliveAgain,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Event {
    pub kind: Kind,
    pub source: String,
    pub summary: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    serial: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attention: Option<String>,
}

impl Event {
    fn new(kind: Kind, source: String, summary: String, detail: String) -> Self {
        Self {
            kind,
            source,
            summary,
            detail,
            path: None,
            serial: 0,
            attention: None,
        }
    }

    pub fn item(&self, watch: &str) -> String {
        format!("watch:{watch}:{}", self.serial)
    }

    pub fn note(&self) -> String {
        let text = self.context();
        let fence = "`".repeat(
            text.split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        format!("**{}**\n\n{fence}\n{text}\n{fence}", self.summary)
    }

    fn context(&self) -> String {
        let path = self.path.as_deref().unwrap_or(&self.source);
        format!("{path}\n\n{}", self.detail)
    }

    fn wakes(&self, policy: Wake) -> bool {
        match policy {
            Wake::All => true,
            Wake::Attention => {
                matches!(self.kind, Kind::Stale | Kind::AliveAgain | Kind::Error)
                    || self.kind == Kind::Report && self.attention.is_some()
            }
            Wake::Silent => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Binding {
    pub thread: String,
    pub previous: String,
    pub creating: bool,
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Runtime {
    pub binding: Option<Binding>,
    pub paused: Option<bool>,
    pub last_poll: Option<i64>,
    pub last_error: Option<String>,
    pub failures: u32,
    pub states: BTreeMap<String, Seen>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub report_checkpoints: BTreeMap<String, String>,
    pub last_event: Option<Event>,
    pub outbox: VecDeque<Event>,
    pub last_wake: Option<i64>,
    pub wake_digest: String,
    pub wake_items: Vec<WakeItem>,
    pub pending_wakes: VecDeque<WakeBatch>,
    serial: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Seen {
    BranchHeads(BTreeMap<String, String>),
    Report(Option<u64>),
    Heartbeat { stale: bool, heartbeat: Heartbeat },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WakeItem {
    pub thread: String,
    pub item: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WakeBatch {
    pub id: WakeId,
    pub digest: String,
    pub items: Vec<WakeItem>,
    pub reply_thread: Option<String>,
    #[serde(default)]
    pub acceptance: Option<WakeAcceptance>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct WakeId(u64);

impl WakeId {
    pub(super) fn reply_item(self) -> String {
        format!("watch:reply:{:016x}", self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) enum WakeAcceptance {
    Reply { text: String, actor: fuzukue_proto::Actor },
    Silent,
}

impl Runtime {
    pub fn thread<'a>(&'a self, watch: &'a Watch) -> &'a str {
        self.binding
            .as_ref()
            .map_or(&watch.thread, |binding| &binding.thread)
    }

    pub fn delay(&self, interval: u32) -> u64 {
        if self.failures == 0 {
            return u64::from(interval);
        }
        u64::from(interval).max((u64::from(interval) << self.failures.min(10)).min(600))
    }

    pub fn apply(&mut self, watch: &Watch, observed: Observation, now: i64) {
        self.last_poll = Some(now);
        let mut errors = Vec::new();
        match observed {
            Observation::MissingRepo => {
                self.paused = Some(true);
                errors.push(
                    harness_text!(
                        "Repository path disappeared. Watch paused.",
                        "リポジトリが見つからないため監視を一時停止した。"
                    )
                    .into(),
                );
            }
            Observation::RepoError(error) => errors.push(error),
            Observation::Sources(samples) => {
                for (source, sample) in watch.sources.iter().zip(samples) {
                    match sample {
                        Ok(sample) => self.apply_sample(watch, source, sample, now),
                        Err(error) => errors.push(format!("{}: {error}", source.label())),
                    }
                }
            }
        }
        if errors.is_empty() {
            self.last_error = None;
            self.failures = 0;
        } else {
            let detail: String = errors.join("\n").chars().take(6000).collect();
            self.last_error = Some(detail.clone());
            self.failures = self.failures.saturating_add(1);
            if self.failures == 3 {
                self.emit(Event::new(
                    Kind::Error,
                    watch.name.clone(),
                    format!(
                        "{}: {}",
                        watch.name,
                        harness_text!("watch error", "監視エラー")
                    ),
                    detail,
                ));
            }
        }
    }

    fn emit(&mut self, mut event: Event) {
        self.serial += 1;
        event.serial = self.serial;
        self.outbox.push_back(event);
    }

    fn apply_sample(&mut self, watch: &Watch, source: &Source, sample: Sample, now: i64) {
        let key = source.key(watch);
        let previous = self.states.remove(&key);
        let next = match (source, sample) {
            (Source::BranchHeads { .. }, Sample::BranchHeads(heads)) => {
                if let Some(Seen::BranchHeads(before)) = previous {
                    for (branch, sha) in &heads {
                        let kind = match before.get(branch) {
                            None => Kind::BranchCreated,
                            Some(old) if old != sha => Kind::BranchAdvanced,
                            _ => continue,
                        };
                        let label = match kind {
                            Kind::BranchCreated => harness_text!("branch created", "ブランチ作成"),
                            _ => harness_text!("branch advanced", "ブランチ更新"),
                        };
                        let detail = before
                            .get(branch)
                            .map_or_else(|| sha.clone(), |old| format!("{old} → {sha}"));
                        self.emit(Event::new(
                            kind,
                            source.label(),
                            format!("{branch}: {label}"),
                            detail,
                        ));
                    }
                }
                Some(Seen::BranchHeads(heads))
            }
            (Source::NewFile { .. }, Sample::Reports { reports, checkpoint }) => {
                let last = match previous {
                    Some(Seen::Report(last)) => last,
                    _ => None,
                };
                let mut latest = last;
                for report in reports {
                    if last.is_some_and(|known| report.number <= known) {
                        continue;
                    }
                    latest = Some(report.number);
                    if previous.is_some() {
                        let mut event = Event::new(
                            Kind::Report,
                            source.label(),
                            format!(
                                "{}: {} {}",
                                watch.name,
                                harness_text!("cycle report", "サイクル報告"),
                                report.number
                            ),
                            excerpt(&report.text, 0, 40, 6000),
                        );
                        event.path = Some(report.path);
                        event.attention = attention_line(
                            &report.text,
                            &watch.attention_patterns,
                            watch.attention_marked_lines,
                        )
                        .map(|line| excerpt(&report.text, line, 40, 5500));
                        self.emit(event);
                    }
                }
                // The scheduler saves this checkpoint and the queued events together.
                if let Some(checkpoint) = checkpoint {
                    self.report_checkpoints.insert(key.clone(), checkpoint);
                }
                Some(Seen::Report(latest))
            }
            (Source::Heartbeat { stale_minutes, .. }, Sample::Heartbeat(heartbeat)) => {
                let (before, last) = match previous {
                    Some(Seen::Heartbeat { stale, heartbeat }) => (Some(stale), Some(heartbeat)),
                    _ => (None, None),
                };
                let Some(heartbeat) = heartbeat.or(last) else {
                    return;
                };
                let stale = heartbeat.stale(now, *stale_minutes);
                if before.is_some_and(|before| before != stale) {
                    let (kind, label) = if stale {
                        (
                            Kind::Stale,
                            harness_text!("heartbeat is stale", "ハートビートが途絶えた"),
                        )
                    } else {
                        (
                            Kind::AliveAgain,
                            harness_text!("heartbeat recovered", "ハートビートが戻った"),
                        )
                    };
                    self.emit(Event::new(
                        kind,
                        source.label(),
                        format!("{}: {label}", watch.name),
                        heartbeat.detail(),
                    ));
                }
                Some(Seen::Heartbeat { stale, heartbeat })
            }
            _ => unreachable!("poll samples must match their sources"),
        };
        if let Some(next) = next {
            self.states.insert(key, next);
        }
    }

    pub fn last_report(&self, watch: &Watch, source: &Source) -> Option<Option<u64>> {
        match self.states.get(&source.key(watch)) {
            Some(Seen::Report(last)) => Some(*last),
            _ => None,
        }
    }

    pub fn record(&mut self, watch: &Watch, event: Event) {
        if event.wakes(watch.wake) {
            let text = match &event.attention {
                Some(attention) => format!(
                    "{}\n{}\n{}",
                    event.summary,
                    event.path.as_deref().unwrap_or(""),
                    attention
                ),
                None => format!("{}\n{}", event.summary, event.context()),
            };
            merge_digest(&mut self.wake_digest, &text);
            self.wake_items.push(WakeItem {
                thread: self.thread(watch).to_owned(),
                item: event.item(&watch.id),
            });
        }
        self.last_event = Some(event);
    }

    pub fn ready_wake(&self, now: i64) -> Option<&str> {
        (!self.wake_digest.is_empty()
            && self
                .last_wake
                .is_none_or(|last| now.saturating_sub(last) >= 600))
        .then_some(&self.wake_digest)
    }

    pub fn seal_wake(&mut self, now: i64) -> bool {
        let Some(digest) = self.take_wake(now) else { return false };
        self.pending_wakes.push_back(WakeBatch {
            id: WakeId(rand::random()),
            digest,
            items: std::mem::take(&mut self.wake_items),
            reply_thread: None,
            acceptance: None,
        });
        true
    }

    pub(super) fn take_wake(&mut self, now: i64) -> Option<String> {
        self.ready_wake(now)?;
        self.last_wake = Some(now);
        Some(std::mem::take(&mut self.wake_digest))
    }
}

fn excerpt(text: &str, start: usize, lines: usize, limit: usize) -> String {
    text.lines()
        .skip(start)
        .take(lines)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(limit)
        .collect()
}

pub(crate) fn merge_digest(current: &mut String, latest: &str) {
    const LIMIT: usize = 6000;
    if current.is_empty() {
        *current = latest.chars().take(LIMIT).collect();
        return;
    }
    // Leave room for both batches when an excerpt is long.
    let latest_limit = LIMIT - current.chars().count().min(LIMIT / 3) - 2;
    let mut merged: String = latest.chars().take(latest_limit).collect();
    let remaining = LIMIT - merged.chars().count() - 2;
    merged.push_str("\n\n");
    merged.extend(current.chars().take(remaining));
    *current = merged;
}
