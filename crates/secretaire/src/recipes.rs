//! Server-owned recipe inventory, caller-scoped requests, and FIFO execution.

mod config;
#[cfg(target_os = "linux")]
mod execution;
mod journal;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::cancellation::Cancellation;
use config::{Arguments, Config, Recipe};
use journal::Journal;

static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();
const RESPONSE_WAIT: Duration = Duration::from_secs(15);

pub(crate) fn configure(
    source: &str,
    state_dir: &Path,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let config = Config::parse(source)?;
    let journal = state_dir.join("recipe-audit.jsonl");
    if config.recipes.is_empty() && !journal.exists() {
        return Ok(());
    }
    let paths = execution_paths();
    let private = [
        state_dir.parent().map(Path::to_owned),
        crate::state::config_dir(),
        config_path,
        paths
            .host_home
            .as_ref()
            .map(|home| home.join(".config/fuzukue")),
        paths
            .host_home
            .as_ref()
            .map(|home| home.join(".local/state/fuzukue")),
    ]
    .into_iter()
    .flatten()
    .flat_map(|path| [path.canonicalize().ok(), Some(path)])
    .flatten()
    .collect();
    let broker = Broker::open(config, &journal, private, paths)?;
    BROKER
        .set(broker)
        .map_err(|_| anyhow::anyhow!("recipe broker already configured"))
}

fn execution_paths() -> Paths {
    Paths {
        bwrap: ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file()),
        host_home: std::env::var_os("HOME")
            .and_then(|home| PathBuf::from(home).canonicalize().ok()),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    List,
    Run,
    Status,
    Cancel,
}

impl Tool {
    pub(crate) const ALL: [Self; 4] = [Self::List, Self::Run, Self::Status, Self::Cancel];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::List => "list_recipes",
            Self::Run => "run_recipe",
            Self::Status => "recipe_status",
            Self::Cancel => "cancel_recipe",
        }
    }

    pub(crate) fn definition(self) -> Value {
        let request_id = json!({"request_id": {"type": "string", "description": "Request ID returned by run_recipe."}});
        let (description, properties, required): (&str, Value, &[&str]) = match self {
            Self::List => (harness_text!("List server-approved recipes, queue state, and recent requests in scope.", "サーバーで許可されたレシピ、待ち行列、参照可能な最近の依頼を表示する。"), json!({}), &[]),
            Self::Run => (harness_text!("Queue a server-approved recipe with declared arguments. If still active on return, poll recipe_status with its request ID.", "許可済みのレシピを宣言済みの引数で実行する。実行中の場合は依頼 ID を recipe_status に渡す。"),
                json!({"name": {"type": "string"}, "args": {"type": "object", "additionalProperties": {"type": "string"}}}), &["name"]),
            Self::Status => (harness_text!("Read a recipe request's state, result, and log path.", "レシピ依頼の状態、結果、ログのパスを表示する。"), request_id, &["request_id"]),
            Self::Cancel => (harness_text!("Cancel a queued or running recipe request.", "待機中または実行中のレシピ依頼を取り消す。"), request_id, &["request_id"]),
        };
        json!({
            "name": self.name(), "description": description,
            "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
        })
    }
}

pub(crate) fn codex_tool_policy(allow: Option<&[String]>) -> Value {
    // The server config is the approval boundary for these tools. Keep the
    // existing Codex approval policy for every other MCP tool.
    Value::Object(
        Tool::ALL
            .into_iter()
            .filter(|tool| {
                !crate::canary()
                    && allow.is_none_or(|names| names.iter().any(|name| name == tool.name()))
            })
            .map(|tool| (tool.name().into(), json!({"approval_mode": "approve"})))
            .collect(),
    )
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Caller {
    workspace: String,
    thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    job: Option<String>,
}

impl Caller {
    pub(crate) fn new(workspace: &str, thread: &str, job: Option<&str>) -> Self {
        Self {
            workspace: workspace.into(),
            thread: thread.into(),
            job: job.map(str::to_owned),
        }
    }

    fn owns(&self, submitted_by: &Self) -> bool {
        self.workspace == submitted_by.workspace
            && self.thread == submitted_by.thread
            && (self.job.is_none() || self.job == submitted_by.job)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    name: String,
    #[serde(default)]
    args: Arguments,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestId {
    request_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

pub(crate) async fn call(
    tool: Tool,
    caller: Caller,
    project: &Path,
    worker: Option<crate::capture::WorkerStorage>,
    args: Value,
) -> Result<Value> {
    ensure!(args.is_object(), "recipe tool arguments must be an object");
    let broker = BROKER.get();
    match tool {
        Tool::List => {
            let _: NoArgs = serde_json::from_value(args)?;
            Ok(broker.map_or_else(
                || json!({"recipes": [], "requests": []}),
                |broker| broker.list(&caller),
            ))
        }
        Tool::Run => {
            let args: RunArgs = serde_json::from_value(args)?;
            ensure!(config::valid_name(&args.name), "invalid recipe name");
            let broker = broker.context("unknown recipe (there are no configured recipes)")?;
            let root = project.canonicalize()?;
            if let Some(worker) = &worker {
                ensure!(
                    worker.cwd == root,
                    "recipes require a worker in the calling workspace"
                );
            }
            let mut request = broker.submit(caller, args, root)?;
            if !request.deduplicated {
                request.wait(RESPONSE_WAIT).await;
            }
            Ok(request.snapshot())
        }
        Tool::Status | Tool::Cancel => {
            let args: RequestId = serde_json::from_value(args)?;
            let broker = broker.context("recipe request not found in this caller's scope")?;
            if matches!(tool, Tool::Cancel) {
                broker.cancel(&caller, &args.request_id)?;
            }
            Ok(broker.find(&caller, &args.request_id)?.snapshot())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Queued,
    Running,
    Exited,
    Skipped,
    Refused,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl State {
    fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    request_id: String,
    caller: Caller,
    recipe: String,
    args: Arguments,
    queue: String,
    queued_at: i64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    state: State,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: Option<u64>,
    reason: Option<String>,
    log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    log_tail: String,
}

impl Record {
    fn finish(&mut self, state: State, reason: Option<String>) {
        let now = Utc::now().timestamp_millis();
        self.ended_at = Some(now);
        self.duration_ms = Some(
            self.started_at
                .map_or(0, |start| now.saturating_sub(start).max(0) as u64),
        );
        self.state = state;
        self.reason = reason;
    }

    fn note(&mut self, message: &str) {
        let reason = self.reason.get_or_insert_with(String::new);
        if !reason.is_empty() {
            reason.push_str(". ");
        }
        reason.push_str(message);
    }
}

struct Request {
    record: watch::Receiver<Record>,
    deduplicated: bool,
}

impl Request {
    async fn wait(&mut self, limit: Duration) {
        let _ = tokio::time::timeout(limit, self.record.wait_for(|record| !record.state.active()))
            .await;
    }

    fn snapshot(&self) -> Value {
        let mut result = json!(*self.record.borrow());
        result["deduplicated"] = json!(self.deduplicated);
        result
    }
}

struct Entry {
    record: watch::Sender<Record>,
    cancel: Arc<Cancellation>,
}

struct Pending {
    id: String,
    recipe: Recipe,
    argv: Vec<String>,
    root: PathBuf,
}

struct Queue {
    waiting: VecDeque<Pending>,
    running: bool,
}

struct Inventory {
    journal: Journal,
    entries: HashMap<String, Entry>,
    queues: BTreeMap<String, Queue>,
    audit_failed: bool,
}

impl Inventory {
    fn append(&mut self, record: &Record) -> Result<()> {
        ensure!(!self.audit_failed, "recipe audit journal is unavailable");
        if let Err(error) = self.journal.append(record) {
            self.audit_failed = true;
            tracing::error!(%error, "could not append recipe audit record");
            return Err(error).context("could not append recipe audit record");
        }
        Ok(())
    }

    fn update(&mut self, mut record: Record) -> Result<()> {
        let saved = self.append(&record);
        if saved.is_err() {
            if record.state.active() {
                record.finish(
                    State::Refused,
                    Some("recipe audit journal is unavailable".into()),
                );
            } else {
                record.note("could not save the final recipe audit record");
            }
        }
        self.entries[&record.request_id].record.send_replace(record);
        saved
    }
}

struct Paths {
    bwrap: Option<PathBuf>,
    host_home: Option<PathBuf>,
}

struct Broker {
    config: Config,
    state: Mutex<Inventory>,
    private: Vec<PathBuf>,
    runtime: PathBuf,
    paths: Paths,
}

impl Broker {
    fn open(config: Config, path: &Path, private: Vec<PathBuf>, paths: Paths) -> Result<Arc<Self>> {
        let (mut journal, records) = Journal::open(path)?;
        let latest: HashMap<_, _> = records
            .into_iter()
            .map(|record| (record.request_id.clone(), record))
            .collect();
        let mut entries = HashMap::new();
        for (id, mut record) in latest {
            if record.state.active() {
                record.finish(
                    State::Interrupted,
                    Some("server restarted before the recipe completed".into()),
                );
                journal.append(&record)?;
            }
            entries.insert(
                id,
                Entry {
                    record: watch::channel(record).0,
                    cancel: Arc::default(),
                },
            );
        }
        Ok(Arc::new(Self {
            config,
            private,
            runtime: path.parent().unwrap().to_owned(),
            paths,
            state: Mutex::new(Inventory {
                journal,
                entries,
                queues: BTreeMap::new(),
                audit_failed: false,
            }),
        }))
    }

    fn list(&self, caller: &Caller) -> Value {
        let state = self.state.lock().unwrap();
        let recipes: Vec<_> = self
            .config
            .recipes
            .iter()
            .map(|(name, recipe)| {
                let queue = state.queues.get(&recipe.queue);
                json!({"name": name, "args": recipe.args, "queue": recipe.queue,
                "running": queue.is_some_and(|queue| queue.running),
                "queued": queue.map_or(0, |queue| queue.waiting.len())})
            })
            .collect();
        let mut records: Vec<_> = state
            .entries
            .values()
            .map(|entry| entry.record.borrow())
            .filter(|record| caller.owns(&record.caller))
            .collect();
        records.sort_unstable_by_key(|record| (record.state.active(), record.queued_at));
        let requests: Vec<_> = records.into_iter().rev().take(20).map(|record| json!({
            "request_id": record.request_id, "name": record.recipe, "state": record.state, "caller": record.caller,
        })).collect();
        json!({"recipes": recipes, "requests": requests})
    }

    fn find(&self, caller: &Caller, id: &str) -> Result<Request> {
        let state = self.state.lock().unwrap();
        let entry = state
            .entries
            .get(id)
            .filter(|entry| caller.owns(&entry.record.borrow().caller))
            .context("recipe request not found in this caller's scope")?;
        Ok(Request {
            record: entry.record.subscribe(),
            deduplicated: false,
        })
    }

    fn submit(self: &Arc<Self>, caller: Caller, args: RunArgs, root: PathBuf) -> Result<Request> {
        ensure!(config::valid_name(&args.name), "invalid recipe name");
        let recipe = self
            .config
            .recipes
            .get(&args.name)
            .with_context(|| format!("unknown recipe: {}", args.name))?;
        let argv = recipe.argv(&args.args)?;
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.values().find(|entry| {
            let record = entry.record.borrow();
            record.state.active()
                && record.caller == caller
                && record.recipe == args.name
                && record.args == args.args
        }) {
            return Ok(Request {
                record: entry.record.subscribe(),
                deduplicated: true,
            });
        }

        let id = crate::mail::session_key();
        let queue = recipe.queue.clone();
        let record = Record {
            request_id: id.clone(),
            caller,
            recipe: args.name,
            args: args.args,
            queue: queue.clone(),
            state: State::Queued,
            queued_at: Utc::now().timestamp_millis(),
            started_at: None,
            ended_at: None,
            exit_code: None,
            signal: None,
            duration_ms: None,
            reason: None,
            log_path: None,
            log_tail: String::new(),
        };
        state.append(&record)?;
        let (sender, receiver) = watch::channel(record);
        state.entries.insert(
            id.clone(),
            Entry {
                record: sender,
                cancel: Arc::default(),
            },
        );
        let pending = Pending {
            id,
            recipe: recipe.clone(),
            argv,
            root,
        };
        match state.queues.entry(queue.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().waiting.push_back(pending)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Queue {
                    waiting: VecDeque::from([pending]),
                    running: false,
                });
                tokio::spawn(Arc::clone(self).drain(queue));
            }
        }
        Ok(Request {
            record: receiver,
            deduplicated: false,
        })
    }

    fn cancel(&self, caller: &Caller, id: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .entries
            .get(id)
            .filter(|entry| caller.owns(&entry.record.borrow().caller))
            .context("recipe request not found in this caller's scope")?;
        let mut record = entry.record.borrow().clone();
        match record.state {
            State::Running => entry.cancel.cancel(),
            State::Queued => {
                state
                    .queues
                    .get_mut(&record.queue)
                    .unwrap()
                    .waiting
                    .retain(|request| request.id != id);
                record.finish(State::Cancelled, Some("cancelled before execution".into()));
                state.update(record)?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn drain(self: Arc<Self>, queue: String) {
        while let Some((pending, record, cancel)) = self.next(&queue) {
            self.run(pending, record, cancel).await;
        }
    }

    fn next(&self, name: &str) -> Option<(Pending, Record, Arc<Cancellation>)> {
        let mut state = self.state.lock().unwrap();
        let queue = state.queues.get_mut(name)?;
        let Some(pending) = queue.waiting.pop_front() else {
            state.queues.remove(name);
            return None;
        };
        queue.running = true;
        let entry = &state.entries[&pending.id];
        let record = entry.record.borrow().clone();
        let cancel = entry.cancel.clone();
        Some((pending, record, cancel))
    }

    async fn run(&self, pending: Pending, mut record: Record, cancel: Arc<Cancellation>) {
        // Claim the request before creating output so a concurrent queued cancellation
        // either removes it from the queue or reaches the process cancellation token.
        {
            let mut state = self.state.lock().unwrap();
            if state.entries[&record.request_id].record.borrow().state != State::Queued {
                return;
            }
            record.state = State::Running;
            record.started_at = Some(Utc::now().timestamp_millis());
            if state.update(record.clone()).is_err() {
                return;
            }
        }

        let start = std::time::Instant::now();
        #[cfg(target_os = "linux")]
        let result = execution::run(self, &pending, &mut record, &cancel).await;
        #[cfg(not(target_os = "linux"))]
        let result: Result<()> = {
            let _ = (
                pending,
                cancel,
                &self.private,
                &self.runtime,
                &self.paths,
                &self.config.recipe_runner,
            );
            anyhow::bail!("recipe execution requires Linux")
        };
        if let Err(error) = result {
            record.finish(State::Refused, Some(format!("{error:#}")));
        }
        record.duration_ms = Some(start.elapsed().as_millis() as u64);
        let _ = self.state.lock().unwrap().update(record);
    }
}
