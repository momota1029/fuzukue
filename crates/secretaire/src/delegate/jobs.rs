//! Workspace-scoped inspection of delegated work.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Result};
use fuzukue_proto::{DelegateFileObservation, DelegateReport, ServerEvent, DELEGATE_JOB};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{activity, admission::WaitReason, Activity, Workers};

const QUIET_MS: i64 = 10 * 60 * 1000;
const RETAINED_COMPLETIONS: usize = 256;
const STATUS_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    Report,
    Summary,
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    #[default]
    Active,
    Queued,
    Running,
    Finished,
    Failed,
    All,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Queued,
    Running,
    /// Started, and waiting to begin a retry.
    Waiting,
    Finished,
    Failed,
}

impl State {
    fn matches(self, filter: Filter) -> bool {
        match filter {
            Filter::Active => self.active(),
            Filter::Queued => self == Self::Queued,
            Filter::Running => matches!(self, Self::Running | Self::Waiting),
            Filter::Finished => self == Self::Finished,
            Filter::Failed => self == Self::Failed,
            Filter::All => true,
        }
    }

    pub(super) fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

#[derive(Serialize)]
struct Line {
    at: i64,
    text: String,
    #[serde(skip)]
    item: String,
    #[serde(skip)]
    prose: bool,
}

pub(super) struct Job {
    pub(super) id: String,
    pub(super) session: String,
    pub(super) title: String,
    pub(super) display: fuzukue_proto::DelegateDisplay,
    pub(super) agent: String,
    pub(super) isolation: Option<super::blind::Isolation>,
    pub(super) prompt: String,
    pub(super) worktree: PathBuf,
    pub(super) declared: Option<Vec<String>>,
    pub(super) effective: DelegateFileObservation,
    pub(super) selection: fuzukue_proto::AgentSelection,
    pub(super) tier: fuzukue_proto::Tier,
    pub(super) queued_at: i64,
    pub(super) started_at: Option<i64>,
    pub(super) wait: Option<WaitReason>,
    /// Why usage moved the job to another model or tier.
    pub(super) routing_note: Option<String>,
    pub(super) cancel_reason: Option<String>,
    pub(super) final_report: Option<DelegateReport>,
    pub(super) start_error: Option<String>,
    pub(super) finish_error: Option<String>,
    pub(super) finished_at: Option<i64>,
    pub(super) activity: Timeline,
}

#[derive(Default)]
pub(super) struct Timeline {
    lines: VecDeque<Line>,
    tools: HashMap<String, (String, Value)>,
    tool_count: usize,
    last_tool: Option<String>,
    tool_guard: Option<Arc<crate::tool_guard::Guard>>,
    had_error: bool,
    changed_files: BTreeSet<String>,
    latest_digest: u64,
    relayed_digest: u64,
}

impl Timeline {
    pub(super) fn tool_activity(&self) -> fuzukue_proto::DelegateToolActivity {
        let mut activity = self.tool_guard.as_ref().map_or_else(
            || fuzukue_proto::DelegateToolActivity { count: 0, last_tool: None },
            |guard| guard.activity(),
        );
        activity.count = activity.count.saturating_add(self.tool_count);
        activity.last_tool = activity.last_tool.or_else(|| self.last_tool.clone());
        activity
    }

    pub(super) fn set_tool_guard(&mut self, guard: Option<Arc<crate::tool_guard::Guard>>) {
        let previous = self.tool_activity();
        self.tool_count = previous.count;
        self.last_tool = previous.last_tool;
        self.tool_guard = guard;
    }

    fn push(&mut self, line: Line) {
        self.lines.push_back(line);
        if self.lines.len() > 5 {
            self.lines.pop_front();
        }
    }
}

impl Job {
    pub(super) fn record_report(&mut self, report: &DelegateReport) {
        // Status views already display only these prefixes. The full report
        // is attached to the thread's tool result.
        let report = DelegateReport {
            text: prefix(&report.text, 4001).into(),
            changes: prefix(&report.changes, 4001).into(),
            elapsed_ms: report.elapsed_ms,
            interrupted: report.interrupted,
            attempts: report.attempts.clone(),
            tool_activity: report.tool_activity.clone(),
            files: report.files.clone(),
        };
        self.final_report = Some(report);
        self.activity.set_tool_guard(None);
        self.activity.tools.clear();
    }

    pub(super) fn state(&self) -> State {
        if self.finished_at.is_some() {
            if self.start_error.is_some() || self.finish_error.is_some() {
                State::Failed
            } else {
                State::Finished
            }
        } else if self.started_at.is_some() && self.wait.is_some() {
            State::Waiting
        } else if self.started_at.is_some() {
            State::Running
        } else {
            State::Queued
        }
    }

    pub(super) fn observe(&mut self, event: &ServerEvent, now: i64, artifact_paths: Option<&[String]>) {
        let line = match activity(event, &self.session, &self.id) {
            Some(Activity::Prose { item, text }) if !text.trim().is_empty() => {
                if let Some(last) = self
                    .activity
                    .lines
                    .back_mut()
                    .filter(|last| last.prose && last.item == item)
                {
                    last.text
                        .extend(text.chars().take(120 - last.text.chars().count()));
                    last.at = now;
                    return;
                }
                Some(Line {
                    at: now,
                    text: truncate(text.trim_start(), 120),
                    item: item.into(),
                    prose: true,
                })
            }
            Some(Activity::Tool { item, name, input }) => {
                let new = self.activity.tools.insert(item.into(), (name.into(), input.clone())).is_none();
                if self.activity.tool_guard.is_none() {
                    self.activity.last_tool = Some(truncate(name, 120));
                    self.activity.tool_count += usize::from(new);
                }
                Some(Line {
                    at: now,
                    text: truncate(&format!("Tool: {name}"), 120),
                    item: item.into(),
                    prose: false,
                })
            }
            _ => None,
        };
        if let Some(line) = line {
            self.activity.push(line);
        }
        if let ServerEvent::ToolResult {
            session,
            item,
            ok,
            output,
        } = event
        {
            if *session != self.session {
                return;
            }
            if let Some((name, input)) = self.activity.tools.remove(item) {
                let text = if *ok {
                    format!("Tool completed: {name}")
                } else {
                    format!("Tool failed: {name}")
                };
                let line = Line {
                    at: now,
                    text: truncate(&text, 120),
                    item: item.clone(),
                    prose: false,
                };
                if let Some(last) = self
                    .activity
                    .lines
                    .back_mut()
                    .filter(|last| !last.prose && last.item == *item)
                {
                    *last = line;
                } else {
                    self.activity.push(line);
                }
                self.activity.had_error |= !ok;
                if *ok {
                    let fallback;
                    let paths = match artifact_paths {
                        Some(paths) => paths,
                        None => {
                            fallback = crate::thread_artifacts::tool_paths(&name, Some(&input), Some(output));
                            &fallback
                        }
                    };
                    self.activity.changed_files.extend(paths.iter().filter_map(|path| {
                        crate::thread_artifacts::relative_path(&self.worktree, path)
                    }));
                }
            }
        }
    }

    fn short_id(&self) -> &str {
        let nonce = self.id.strip_prefix(DELEGATE_JOB).unwrap_or(&self.id);
        nonce.get(..8).unwrap_or(nonce)
    }

    fn files(&self) -> Vec<&str> {
        match self
            .final_report
            .as_ref()
            .and_then(|report| report.files.as_ref())
            .and_then(|files| files.changed_files.as_ref())
        {
            Some(files) => files.iter().map(String::as_str).collect(),
            None => self
                .activity
                .changed_files
                .iter()
                .map(String::as_str)
                .collect(),
        }
    }

    fn status_header(&self, now: i64) -> Value {
        let state = self.state();
        let elapsed =
            (self.finished_at.unwrap_or(now) - self.started_at.unwrap_or(self.queued_at)).max(0);
        let mut status = json!({
            "id": self.id, "short_id": self.short_id(),
            "title": self.title, "label": self.display.label, "card_id": self.display.card_id,
            "label_source": self.display.label_source, "agent": self.agent, "tier": self.tier,
            "thread": self.session, "session": self.session, "state": state,
            "queued_at": self.queued_at, "started_at": self.started_at, "finished_at": self.finished_at,
            "elapsed_ms": elapsed, "error": state == State::Failed || self.activity.had_error,
            "wait_reason": self.wait.as_ref().map(WaitReason::message), "cancel_reason": self.cancel_reason,
        });
        let tools = self.activity.tool_activity();
        status["tool_count"] = json!(tools.count);
        status["last_tool"] = json!(tools.last_tool);
        status["backend"] = json!(self.selection.backend);
        status["model"] = json!(self.selection.model);
        status["effort"] = json!(self.selection.effort);
        if let Some(note) = &self.routing_note {
            status["routing_note"] = json!(note);
        }
        if let Some(isolation) = self.isolation {
            status["isolation"] = json!(isolation);
        }
        status
    }

    fn summary(&self, now: i64, hide_build_files: bool) -> Value {
        let state = self.state();
        let last = self.activity.lines.back();
        let last_at = last.map(|line| line.at);
        let quiet =
            state == State::Running && now - last_at.or(self.started_at).unwrap_or(now) > QUIET_MS;
        let stalled = state == State::Queued && now - self.queued_at > QUIET_MS;
        let files: Vec<_> = self.files().into_iter().filter(|path| !hide_build_files || !build_output(path)).collect();
        let mut value = self.status_header(now);
        let mut summary = json!({
            "last_activity_at": last_at, "last_activity": last.map(|line| line.text.replace(['\n', '\r'], " ")),
            "touches": {"declared": self.declared, "effective": self.effective.touches, "source": self.effective.source},
            "changed_files": files.iter().take(8).collect::<Vec<_>>(), "changed_file_count": files.len(),
            "wait": self.wait,
            "waiting_for": self.wait.as_ref().and_then(WaitReason::blocking_job),
            "latest_digest_relayed": (self.activity.latest_digest != 0).then(|| self.activity.latest_digest == self.activity.relayed_digest),
            "quiet": quiet, "stalled?": stalled,
        });
        value.as_object_mut().unwrap().append(summary.as_object_mut().unwrap());
        value
    }

    fn detail(&self, now: i64, view: View) -> Value {
        let mut value = match view {
            View::Full => self.summary(now, false),
            View::Report | View::Summary => self.status_header(now),
        };
        value["start_error"] = json!(self.start_error);
        value["finish_error"] = json!(self.finish_error);
        let report = self.final_report.as_ref();
        status_text(&mut value, "change_summary", report.map(|report| report.changes.as_str()), 4000);
        if matches!(view, View::Full | View::Report) {
            status_text(&mut value, "final_report", report.map(|report| report.text.as_str()), 4000);
        }
        if !matches!(view, View::Full) {
            return value;
        }
        status_text(&mut value, "prompt", Some(&self.prompt), 400);
        value["activity"] = json!(self.activity.lines);
        value["attempts"] = json!(report.map(|report| &report.attempts));
        if self.isolation.is_none() {
            value["worktree"] = json!(self.worktree);
            value["write_observation"] = json!(self
                .final_report
                .as_ref()
                .and_then(|report| report.files.as_ref())
                .unwrap_or(&self.effective));
        }
        value
    }
}

fn build_output(path: &str) -> bool {
    path.rsplit(['/', '\\']).skip(1).any(|part| {
        matches!(part, ".gradle" | "build" | "target" | ".kotlin" | "node_modules")
    })
}

fn status_text(value: &mut Value, key: &str, text: Option<&str>, chars: usize) {
    value[key] = json!(text.map(|text| truncate(text, chars)));
    if text.is_some_and(|text| value[key].as_str() != Some(text)) {
        value["truncated"] = true.into();
    }
}

pub(crate) fn limit_status(mut status: Value) -> Value {
    if status.to_string().len() <= STATUS_MAX_BYTES {
        return status;
    }
    status["truncated"] = true.into();
    let fields = [
        "write_observation", "changed_files", "touches", "activity", "attempts", "prompt",
        "last_activity", "worktree", "wait",
    ];
    for field in fields {
        status.as_object_mut().unwrap().remove(field);
        if status.to_string().len() <= STATUS_MAX_BYTES {
            return status;
        }
    }
    // Bound unusually long reports, error strings and durable diagnostics as well.
    let fields = status.as_object_mut().unwrap();
    fields.retain(|key, value| {
        if let Some(text) = value.as_str() {
            *value = truncate(text, if key == "final_report" { 1000 } else { 80 }).into();
        }
        !value.is_array() && !value.is_object()
    });
    status
}

fn prefix(text: &str, chars: usize) -> &str {
    text.char_indices().nth(chars).map_or(text, |(end, _)| &text[..end])
}

fn truncate(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) type Record = Arc<Mutex<Job>>;

#[derive(Default)]
pub(super) struct Inventory {
    records: Mutex<HashMap<String, Record>>,
}

impl Inventory {
    pub(super) fn get(&self, id: &str) -> Option<Record> {
        self.records.lock().unwrap().get(id).cloned()
    }

    pub(super) fn insert(&self, job: Job) -> Record {
        let id = job.id.clone();
        let record = Arc::new(Mutex::new(job));
        self.records.lock().unwrap().insert(id, record.clone());
        record
    }

    pub(super) fn prune(&self) {
        let mut records = self.records.lock().unwrap();
        let mut completed = records
            .iter()
            .filter_map(|(id, record)| {
                record
                    .lock()
                    .unwrap()
                    .finished_at
                    .map(|at| (at, id.clone()))
            })
            .collect::<Vec<_>>();
        if completed.len() > RETAINED_COMPLETIONS {
            completed.sort_unstable();
            let excess = completed.len() - RETAINED_COMPLETIONS;
            for (_, id) in completed.into_iter().take(excess) {
                records.remove(&id);
            }
        }
    }

    pub(super) fn digest_ready(&self, id: &str) -> Option<u64> {
        let record = self.records.lock().unwrap().get(id).cloned()?;
        let mut job = record.lock().unwrap();
        job.activity.latest_digest += 1;
        Some(job.activity.latest_digest)
    }

    pub(super) fn digest_relayed(&self, id: &str, revision: u64) {
        if let Some(record) = self.records.lock().unwrap().get(id).cloned() {
            let mut job = record.lock().unwrap();
            job.activity.relayed_digest = job.activity.relayed_digest.max(revision);
        }
    }

    fn resolve(&self, query: &str) -> Result<Record> {
        let query = query.trim();
        ensure!(
            !query.is_empty(),
            "Specify a job ID or title from list_jobs"
        );
        let records = self.records.lock().unwrap();
        if let Some(record) = records.get(query) {
            return Ok(record.clone());
        }
        let prefix = query.strip_prefix(DELEGATE_JOB).unwrap_or(query);
        ensure!(
            !prefix.is_empty(),
            "Specify a job ID or title from list_jobs"
        );
        let title = query.to_lowercase();
        let mut ids = Vec::new();
        let mut titles = Vec::new();
        for record in records.values() {
            let job = record.lock().unwrap();
            let nonce = job.id.strip_prefix(DELEGATE_JOB).unwrap_or(&job.id);
            if nonce.starts_with(prefix) {
                ids.push((job.id.clone(), record.clone()));
            } else if job.title.to_lowercase().contains(&title)
                || job.display.label.as_ref().is_some_and(|label| label.to_lowercase().contains(&title))
            {
                titles.push((job.id.clone(), record.clone()));
            }
        }
        let mut matches = if ids.is_empty() { titles } else { ids };
        match matches.len() {
            0 => bail!("Job {query:?} was not found in this workspace"),
            1 => Ok(matches.pop().unwrap().1),
            _ => {
                matches.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let ids = matches
                    .into_iter()
                    .take(8)
                    .map(|(id, _)| id)
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("Ambiguous job {query:?}. Use a full job ID: {ids}")
            }
        }
    }

    pub(super) fn target(&self, query: &str) -> Result<(String, String)> {
        let record = self.resolve(query)?;
        let job = record.lock().unwrap();
        Ok((job.session.clone(), job.id.clone()))
    }

    pub(super) fn cancel_target(&self, query: &str) -> Result<String> {
        let record = self.resolve(query)?;
        let job = record.lock().unwrap();
        ensure!(job.state().active(), "Job {} has already finished", job.id);
        Ok(job.id.clone())
    }

    pub(super) fn cancel(&self, workers: &Workers, query: &str, reason: &str) -> Result<Value> {
        let reason = reason.trim();
        ensure!(!reason.is_empty(), "Specify why the job should stop");
        let record = self.resolve(query)?;
        let mut job = record.lock().unwrap();
        ensure!(job.state().active(), "Job {} has already finished", job.id);
        ensure!(
            job.cancel_reason.is_none(),
            "Cancellation already requested for {}",
            job.id
        );
        ensure!(
            workers.interrupt_job(&job.session, &job.id),
            "Job {} has already finished",
            job.id
        );
        job.cancel_reason = Some(reason.into());
        Ok(json!({"id": job.id, "cancel_requested": true, "reason": reason}))
    }

    pub(super) fn list(
        &self,
        filter: Filter,
        thread: Option<&str>,
        limit: usize,
        now: i64,
    ) -> (Value, String) {
        let records = self.records.lock().unwrap();
        let mut rows = records
            .values()
            .filter_map(|record| {
                let job = record.lock().unwrap();
                (job.state().matches(filter) && thread.is_none_or(|thread| thread == job.session))
                    .then(|| (job.queued_at, job.id.clone(), record.clone()))
            })
            .collect::<Vec<_>>();
        rows.sort_unstable_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
        let total = rows.len();
        let mut table = Vec::new();
        let mut jobs = Vec::new();
        for (_, _, record) in rows.into_iter().take(limit) {
            let job = record.lock().unwrap();
            let mut row = job.summary(now, true);
            if let Some(message) = job.wait.as_ref().and_then(WaitReason::localized) {
                let locale = if crate::harness::japanese() { "ja" } else { "en" };
                row["wait_reason"] = crate::l10n::render(fuzukue_i18n::Locale::supported(locale).unwrap(), &message).into();
            }
            let note = if row["stalled?"] == true {
                "stalled?"
            } else if row["quiet"] == true {
                "quiet"
            } else {
                ""
            };
            let activity = row["wait_reason"]
                .as_str()
                .or_else(|| row["last_activity"].as_str())
                .unwrap_or("—");
            let mut note = if note.is_empty() {
                String::new()
            } else {
                format!(" · {note}")
            };
            if let Some(routing) = &job.routing_note {
                note.push_str(&format!(" · {routing}"));
            }
            table.push(format!(
                "{} | {}{note} | {} | {activity} | {}",
                job.short_id(),
                job.state().label(),
                job.display.label.as_deref().unwrap_or(&job.title),
                row["tool_count"],
            ));
            jobs.push(row);
        }
        let lines = jobs
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let table = table.join("\n");
        (
            json!({"jobs": jobs, "total": total}),
            format!(
                "{lines}\n\n{} of {total} jobs\nID | STATE | LABEL | WAIT OR ACTIVITY | TOOL CALLS\n{table}",
                jobs.len()
            ),
        )
    }

    pub(super) fn status(&self, query: &str, now: i64, view: View) -> Result<Value> {
        Ok(self.resolve(query)?.lock().unwrap().detail(now, view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::{Actor, AgentSelection, DelegateWriteSetSource, Tier};

    fn job(id: &str, title: &str, session: &str, now: i64) -> Job {
        Job {
            id: format!("{DELEGATE_JOB}{id}"),
            title: title.into(),
            display: fuzukue_proto::DelegateDisplay::from_title(Some(title), None),
            session: session.into(),
            agent: "worker".into(),
            isolation: None,
            prompt: "p".repeat(401),
            worktree: "/workspace".into(),
            declared: None,
            effective: DelegateFileObservation {
                source: DelegateWriteSetSource::Prompt,
                touches: Some(vec!["**/chat.rs".into()]),
                changed_files: None,
            },
            selection: AgentSelection::summary_default(),
            tier: Tier::Standard,
            queued_at: now,
            started_at: None,
            finished_at: None,
            wait: None,
            routing_note: None,
            cancel_reason: None,
            final_report: None,
            start_error: None,
            finish_error: None,
            activity: Timeline::default(),
        }
    }

    #[test]
    fn inventory_reports_states_filters_and_observed_activity() -> Result<()> {
        let inventory = Inventory::default();
        let now = 1_500_000;
        let mut queued = job("00000000a", "Inspect chat", "main", now - QUIET_MS - 1);
        queued.wait = Some(WaitReason::Overlap {
            job: "delegate:00000000b".into(),
            title: "Edit chat".into(),
            path: Some("**/chat.rs".into()),
        });
        inventory.insert(queued);
        let mut running = job("00000000b", "Edit chat", "main", now - 2 * QUIET_MS);
        running.started_at = Some(now - QUIET_MS - 1);
        let session = running.session.clone();
        let item = format!("{}:tool", running.id);
        let text = "作".repeat(121);
        running.observe(
            &ServerEvent::ToolCall {
                session: session.clone(),
                item: item.clone(),
                actor: Actor::Claude,
                name: "Edit".into(),
                input: json!({}),
            },
            now - QUIET_MS - 10, None,
        );
        running.observe(
            &ServerEvent::ToolResult {
                session: session.clone(),
                item,
                ok: false,
                output: "failed".into(),
            },
            now - QUIET_MS - 9, None,
        );
        running.observe(
            &ServerEvent::TextDelta {
                session,
                item: format!("{}:text", running.id),
                actor: Actor::Claude,
                text,
            },
            now - QUIET_MS - 1, None,
        );
        inventory.insert(running);

        let mut finished = job("111111111", "Other work", "other", now - 300);
        finished.started_at = Some(now - 200);
        finished.finished_at = Some(now - 100);
        finished.final_report = Some(DelegateReport {
            elapsed_ms: 100,
            text: "f".repeat(4001),
            interrupted: false,
            changes: String::new(),
            attempts: vec![],
            tool_activity: None,
            files: Some(DelegateFileObservation {
                changed_files: Some((0..10).map(|i| format!("file-{i}")).collect()),
                ..finished.effective.clone()
            }),
        });
        inventory.insert(finished);
        let mut failed = job("fffffffff", "Failed startup", "other", now - 50);
        failed.finished_at = Some(now - 25);
        failed.start_error = Some("spawn failed".into());
        inventory.insert(failed);

        let (rows, table) = inventory.list(Filter::Active, None, 30, now);
        assert_eq!(rows["total"], 2);
        assert!(table.contains("stalled?") && table.contains("quiet"));
        assert!(table.contains("WAIT OR ACTIVITY | TOOL CALLS"));
        let queued = &inventory.status("inspect CHAT", now, View::Full)?;
        assert_eq!(queued["state"], "queued");
        assert_eq!(queued["waiting_for"], "delegate:00000000b");
        assert_eq!(queued["stalled?"], true);
        assert_eq!(
            inventory.status("delegate:00000000a", now - 1, View::Full)?["stalled?"],
            false
        );
        assert_eq!(
            queued["touches"],
            json!({"declared": null, "effective": ["**/chat.rs"], "source": "prompt"})
        );
        assert_eq!(queued["prompt"].as_str().unwrap().len(), 400);
        let running = &inventory.status("00000000b", now, View::Full)?;
        assert_eq!(running["state"], "running");
        assert_eq!(running["quiet"], true);
        assert_eq!(running["last_activity_at"], now - QUIET_MS - 1);
        assert_eq!(
            running["last_activity"].as_str().unwrap().chars().count(),
            120
        );
        assert_eq!(running["tool_count"], 1);
        assert_eq!(running["error"], true);
        assert_eq!(running["activity"].as_array().unwrap().len(), 2);
        let finished = &inventory.status("11111111", now, View::Full)?;
        assert_eq!(finished["state"], "finished");
        assert_eq!(finished["changed_files"].as_array().unwrap().len(), 8);
        assert_eq!(finished["changed_file_count"], 10);
        assert_eq!(finished["elapsed_ms"], 100);
        assert_eq!(finished["final_report"].as_str().unwrap().len(), 4000);
        assert_eq!(finished["truncated"], true);
        let failed = &inventory.status("failed startup", now, View::Full)?;
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["start_error"], "spawn failed");
        assert_eq!(failed["error"], true);
        assert!(inventory
            .cancel_target("11111111")
            .unwrap_err()
            .to_string()
            .contains("already finished"));
        for (filter, thread, limit, total, shown) in [
            (Filter::Queued, None, 30, 1, 1),
            (Filter::Running, Some("main"), 30, 1, 1),
            (Filter::Finished, Some("main"), 30, 0, 0),
            (Filter::Finished, Some("other"), 30, 1, 1),
            (Filter::Failed, None, 30, 1, 1),
            (Filter::All, None, 2, 4, 2),
        ] {
            let (result, _) = inventory.list(filter, thread, limit, now);
            assert_eq!(result["total"], total);
            assert_eq!(result["jobs"].as_array().unwrap().len(), shown);
        }
        for query in ["00000000", " CHAT "] {
            let error = inventory.status(query, now, View::Full).unwrap_err().to_string();
            assert!(
                error.contains("Ambiguous job") && error.contains("delegate:00000000a"),
                "{error}"
            );
        }
        assert!(inventory
            .status("missing", now, View::Full)
            .unwrap_err()
            .to_string()
            .contains("not found"));

        let id = "delegate:00000000b";
        let first = inventory.digest_ready(id).unwrap();
        let second = inventory.digest_ready(id).unwrap();
        inventory.digest_relayed(id, first);
        assert_eq!(inventory.status(id, now, View::Full)?["latest_digest_relayed"], false);
        inventory.digest_relayed(id, second);
        assert_eq!(inventory.status(id, now, View::Full)?["latest_digest_relayed"], true);
        Ok(())
    }

    #[test]
    fn compact_status_and_list_keep_relevant_fields_within_limits() -> Result<()> {
        let now = 1_000;
        let mut job = job("01234567a", "Test", "thread", now - 100);
        job.started_at = Some(now - 80);
        job.finished_at = Some(now);
        let mut changed_files = [".gradle/cache", "app/build/result", "target/artifact", ".kotlin/log", "app\\node_modules\\dep"]
            .map(str::to_owned).to_vec();
        changed_files.extend((1..=8).map(|index| format!("src/file{index}.rs")));
        changed_files.push("src/build".into());
        changed_files.push("src/build.rs".into());
        job.final_report = Some(DelegateReport {
            elapsed_ms: 80,
            text: "Finished all edits.".into(),
            changes: "Edited source files.".into(),
            interrupted: false,
            attempts: vec![],
            tool_activity: None,
            files: Some(DelegateFileObservation {
                changed_files: Some(changed_files.clone()),
                ..job.effective.clone()
            }),
        });
        let inventory = Inventory::default();
        let record = inventory.insert(job);
        let (list, _) = inventory.list(Filter::All, None, 30, now);
        assert_eq!(list["jobs"][0]["changed_file_count"], 10);
        assert_eq!(list["jobs"][0]["changed_files"], json!(changed_files[5..13]));
        let full = inventory.status("01234567", now, View::Full)?;
        assert_eq!(full["changed_file_count"], changed_files.len());
        assert!(full["write_observation"]["changed_files"].is_array());
        for view in [View::Report, View::Summary] {
            let compact = limit_status(inventory.status("01234567", now, view)?);
            assert_eq!(compact["state"], "finished");
            assert_eq!(compact["elapsed_ms"], 80);
            assert_eq!(compact["finished_at"], now);
            assert_eq!(compact["change_summary"], "Edited source files.");
            assert_eq!(compact["final_report"].as_str(), matches!(view, View::Report).then_some("Finished all edits."));
            for field in ["prompt", "activity", "changed_files", "write_observation", "touches"] {
                assert!(compact.get(field).is_none(), "{field}");
            }
        }
        let mut job = record.lock().unwrap();
        let report = job.final_report.as_mut().unwrap();
        report.text = "報告\u{0000}".repeat(10_000);
        report.changes = report.text.clone();
        report.files.as_mut().unwrap().changed_files = Some(vec!["long/path".repeat(10_000)]);
        for view in [View::Full, View::Report, View::Summary] {
            let status = limit_status(job.detail(now, view));
            assert_eq!(status["truncated"], true);
            assert!(status.to_string().len() <= STATUS_MAX_BYTES);
            assert_eq!(status["state"], "finished");
            assert!(status["change_summary"].as_str().unwrap().starts_with("報告"));
        }
        Ok(())
    }
}
