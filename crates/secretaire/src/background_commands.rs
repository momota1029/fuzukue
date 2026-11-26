//! Ephemeral command activity derived from the front desk's tool streams.

use std::collections::{BTreeMap, HashMap, HashSet};

use fuzukue_proto::{BackgroundCommand, BackgroundCommandState, ServerEvent};
use serde_json::Value;

use crate::Workspace;

type Threads = BTreeMap<String, Vec<BackgroundCommand>>;

pub(crate) fn current(app: &Workspace) -> Option<Threads> {
    let mut threads = Threads::new();
    {
        let sessions = app.sessions.try_lock().ok()?;
        for (session, claude) in sessions.iter() {
            let commands = claude.background_commands();
            if !commands.is_empty() {
                threads.insert(session.clone(), commands);
            }
        }
    }
    if let Some(codex) = app.codex.try_lock().ok()?.as_ref() {
        codex.collect_background_commands(&mut threads);
    }
    Some(threads)
}

#[derive(Default)]
pub(crate) struct Feed(Option<Threads>);

impl Feed {
    pub(crate) fn latest(&mut self, app: &Workspace) -> Option<ServerEvent> {
        let mut threads = current(app)?;
        for commands in threads.values_mut() {
            commands.sort_unstable_by(|a, b| {
                (a.started_at_ms, &a.id, &a.summary).cmp(&(b.started_at_ms, &b.id, &b.summary))
            });
        }
        if self.0.as_ref() == Some(&threads) {
            return None;
        }
        self.0 = Some(threads.clone());
        Some(ServerEvent::BackgroundCommands { threads })
    }
}

fn command_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn summary(command: &str) -> String {
    // Shell operands can contain arbitrary credentials. Keep a recognizable
    // command prefix from fixed vocabulary and discard assignments and operands.
    let Some(words) = shlex::split(command) else {
        return String::new();
    };
    let words = match words.as_slice() {
        [shell, flags, script]
            if matches!(shell.rsplit('/').next(), Some("bash" | "sh" | "zsh"))
                && matches!(flags.as_str(), "-c" | "-lc") =>
        {
            let Some(words) = shlex::split(script) else {
                return String::new();
            };
            words
        }
        _ => words,
    };
    let mut words = words.iter().map(String::as_str).peekable();
    if words.peek() == Some(&"env") {
        words.next();
    }
    while words.next_if(|word| word.contains('=')).is_some() {}
    let Some(program) = words
        .next()
        .and_then(|word| word.rsplit('/').next())
        .filter(|word| {
            matches!(
                *word,
                "cargo"
                    | "rustc"
                    | "git"
                    | "gh"
                    | "make"
                    | "cmake"
                    | "ninja"
                    | "just"
                    | "pytest"
                    | "python"
                    | "python3"
                    | "node"
                    | "npm"
                    | "pnpm"
                    | "yarn"
                    | "go"
                    | "uv"
                    | "docker"
                    | "kubectl"
                    | "sleep"
                    | "tail"
                    | "watch"
                    | "while"
                    | "bash"
                    | "sh"
                    | "curl"
                    | "rg"
                    | "find"
                    | "ls"
                    | "echo"
                    | "true"
            )
        })
    else {
        return String::new();
    };
    let mut label = program.to_owned();
    for word in words {
        if !matches!(
            word,
            "test"
                | "check"
                | "build"
                | "run"
                | "status"
                | "diff"
                | "log"
                | "clippy"
                | "fmt"
                | "lint"
                | "install"
                | "exec"
                | "-q"
                | "--quiet"
                | "--release"
                | "--workspace"
                | "--all-targets"
                | "--all-features"
                | "--offline"
                | "--locked"
                | "-p"
                | "--package"
                | "-f"
        ) || label.len() + word.len() + 1 > 78
        {
            label.push_str(" …");
            break;
        }
        label.push(' ');
        label.push_str(word);
    }
    label
}

fn running(id: &str, summary: String, started_at_ms: u64) -> BackgroundCommand {
    BackgroundCommand {
        id: id.into(),
        summary,
        started_at_ms,
        state: BackgroundCommandState::Running,
    }
}

fn description(value: &Value) -> Option<String> {
    let text = value.as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

#[derive(Default)]
pub(crate) struct Claude {
    pending: HashMap<String, PendingClaudeCommand>,
    running: BTreeMap<String, BackgroundCommand>,
    finished_before_result: HashSet<String>,
}

struct PendingClaudeCommand {
    description: Option<String>,
    summary: String,
    monitor: bool,
}

impl Claude {
    pub(crate) fn running(&self) -> impl Iterator<Item = &BackgroundCommand> {
        self.running.values()
    }

    pub(crate) fn observe(&mut self, event: &Value) {
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("assistant"), _) => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    let monitor = block["name"] == "Monitor";
                    if block["type"] != "tool_use"
                        || !(monitor
                            || block["name"] == "Bash"
                                && block["input"]["run_in_background"] == true)
                    {
                        continue;
                    }
                    if let Some(id) = block["id"].as_str().and_then(command_id) {
                        let mut summary = block["input"]["command"]
                            .as_str()
                            .map(summary)
                            .unwrap_or_default();
                        if monitor && summary.is_empty() {
                            summary = "Monitor".into();
                        }
                        self.pending.entry(id.into()).or_insert_with(|| PendingClaudeCommand {
                            description: description(&block["input"]["description"]),
                            summary,
                            monitor,
                        });
                    }
                }
            }
            (Some("user"), _) => {
                let origin = event["origin"]["kind"].as_str();
                match &event["message"]["content"] {
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("tool_result") => self.tool_result(block),
                                Some("text") => self.notice_text(block["text"].as_str(), origin),
                                _ => {}
                            }
                        }
                    }
                    Value::String(text) => self.notice_text(Some(text), origin),
                    _ => {}
                }
            }
            (Some("system"), Some("task_started")) if event["task_type"] == "local_bash" => {
                let tool = event["tool_use_id"].as_str().unwrap_or_default();
                let backgrounded = event["is_backgrounded"].as_bool();
                if let Some(id) = event["task_id"].as_str().and_then(command_id) {
                    // The pending call already identifies an explicitly backgrounded
                    // Bash or a Monitor, including CLI events without this flag.
                    if backgrounded != Some(false) {
                        self.start(tool, Some(id), description(&event["description"]));
                    }
                }
            }
            // Progress and notification summaries describe task output. The title
            // comes from the matched tool call or task start and stays stable.
            (Some("system"), Some("task_notification")) => {
                self.finish(event["task_id"].as_str(), event["status"].as_str());
            }
            (Some("result"), _) => {
                self.pending.clear();
                self.finished_before_result.clear();
            }
            _ => {}
        }
    }

    fn tool_result(&mut self, block: &Value) {
        let Some(tool) = block["tool_use_id"]
            .as_str()
            .filter(|id| self.pending.contains_key(*id))
        else {
            return;
        };
        let output = match &block["content"] {
            Value::String(text) => Some(text.as_str()),
            Value::Array(parts) if parts.first().is_some_and(|part| part["type"] == "text") => {
                parts[0]["text"].as_str()
            }
            _ => None,
        };
        let task = output
            .filter(|_| block["is_error"] != true)
            .and_then(|text| {
                text.trim_start()
                    .strip_prefix("Command running in background with ID: ")
            })
            .and_then(|text| text.split_whitespace().next())
            .and_then(|id| command_id(id.trim_end_matches('.')));
        if task.is_some() || block["is_error"] == true || !self.pending[tool].monitor {
            self.start(tool, task, None);
        }
    }

    fn start(&mut self, tool: &str, task: Option<&str>, task_description: Option<String>) {
        if let Some(call) = self.pending.remove(tool) {
            if let Some(id) = task.filter(|id| !self.finished_before_result.contains(*id)) {
                self.running.entry(id.into()).or_insert_with(|| {
                    let title = call.description.or(task_description).unwrap_or(call.summary);
                    running(id, title, now_ms())
                });
            }
        }
    }

    fn notice_text(&mut self, text: Option<&str>, origin: Option<&str>) {
        let Some(body) = text.and_then(|text| match origin {
            Some("task-notification") => {
                text.split_once("<task-notification>").map(|(_, body)| body)
            }
            None => text.trim_start().strip_prefix("<task-notification>"),
            _ => None,
        }) else {
            return;
        };
        let Some((notice, _)) = body.split_once("</task-notification>") else {
            return;
        };
        let field = |open, close| {
            let (_, tail) = notice.split_once(open)?;
            Some(tail.split_once(close)?.0.trim())
        };
        self.finish(
            field("<task-id>", "</task-id>"),
            field("<status>", "</status>"),
        );
    }

    fn finish(&mut self, task: Option<&str>, status: Option<&str>) {
        if matches!(status, Some("completed" | "failed" | "stopped")) {
            if let Some(id) = task.and_then(command_id) {
                self.running.remove(id);
                if !self.pending.is_empty() {
                    self.finished_before_result.insert(id.into());
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Codex {
    pending: HashMap<String, (String, BackgroundCommand)>,
    running: BTreeMap<String, BackgroundCommand>,
}

impl Codex {
    pub(crate) fn running(&self) -> impl Iterator<Item = &BackgroundCommand> {
        self.running.values()
    }

    pub(crate) fn observe(&mut self, method: &str, params: &Value) {
        let item = &params["item"];
        match method {
            "item/started"
                if item["type"] == "commandExecution" && item["status"] == "inProgress" =>
            {
                if let (Some(item_id), Some(turn), Some(process), Some(command)) = (
                    item["id"].as_str().and_then(command_id),
                    params["turnId"].as_str(),
                    item["processId"].as_str().and_then(command_id),
                    item["command"].as_str(),
                ) {
                    let started_at_ms = params["startedAtMs"].as_u64().unwrap_or_else(now_ms);
                    self.pending.entry(item_id.into()).or_insert_with(|| {
                        (
                            turn.into(),
                            running(process, summary(command), started_at_ms),
                        )
                    });
                }
            }
            "item/completed" if item["type"] == "commandExecution" => {
                if let Some(id) = item["id"].as_str() {
                    self.pending.remove(id);
                    self.running.remove(id);
                }
            }
            "turn/completed" if params["turn"]["status"] == "completed" => {
                // A PTY command that outlives its originating turn has yielded
                // an exec session. Its item/completed arrives when it exits.
                let turn = params["turn"]["id"].as_str();
                self.pending.retain(|id, (origin, command)| {
                    if Some(origin.as_str()) == turn {
                        self.running.insert(id.clone(), command.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            "turn/completed" | "thread/reverted" => *self = Self::default(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash_call(tool: &str, background: bool) -> Value {
        json!({
            "type": "assistant", "message": {"content": [{
                "type": "tool_use", "name": "Bash", "id": tool,
                "input": {
                    "command": "TEST_TOKEN='private value' cargo test -q -p private-package",
                    "run_in_background": background
                }
            }]}
        })
    }

    fn bash_result(tool: &str, task: &str) -> Value {
        json!({
            "type": "user", "message": {"content": [{
                "type": "tool_result", "tool_use_id": tool,
                "content": [{"type": "text", "text": format!("Command running in background with ID: {task}. Output is being written to a file.")}]
            }]}
        })
    }

    #[test]
    fn claude_correlates_background_results_and_removes_terminal_tasks() {
        let mut state = Claude::default();
        state.observe(&bash_result("unknown", "bunknown"));
        state.observe(&bash_call("foreground", false));
        state.observe(&bash_result("foreground", "bforeground"));
        assert_eq!(state.running().count(), 0);

        for status in ["completed", "failed", "stopped"] {
            state.observe(&bash_call("toolu_1", true));
            assert_eq!(state.running().count(), 0);
            state.observe(&bash_result("toolu_1", "btest"));
            state.observe(&json!({"type": "result", "subtype": "success"}));
            let command = state.running().next().unwrap();
            assert_eq!(command.id, "btest");
            assert_eq!(command.summary, "cargo test -q -p …");
            assert_eq!(command.state, BackgroundCommandState::Running);
            assert!(command.started_at_ms > 0);

            state.observe(&json!({
                "type": "system", "subtype": "task_notification",
                "task_id": "bunrelated", "status": status
            }));
            assert_eq!(state.running().count(), 1);
            state.observe(&json!({
                "type": "user", "message": {"content": [{
                    "type": "text",
                    "text": format!("<task-notification>\n<task-id>btest</task-id>\n<status>{status}</status>\n</task-notification>")
                }]}
            }));
            assert_eq!(state.running().count(), 0);
        }
    }

    #[test]
    fn claude_titles_the_background_task_from_its_tool_call() {
        let mut state = Claude::default();
        let mut foreground = bash_call("foreground", false);
        foreground["message"]["content"][0]["input"]["description"] = json!("List fixture files");
        state.observe(&foreground);
        state.observe(&bash_result("foreground", "bforeground"));

        let mut call = bash_call("toolu_wait", true);
        call["message"]["content"][0]["input"]["command"] = json!("while :; do gh run view; sleep 120; done");
        call["message"]["content"][0]["input"]["description"] = json!("  Wait for\n the validation run  ");
        state.observe(&call);
        let started = |tool, task| json!({
            "type": "system", "subtype": "task_started", "task_type": "local_bash",
            "task_id": task, "tool_use_id": tool, "description": "CLI task description",
            "summary": "A different activity's report",
        });
        state.observe(&started("foreground", "bforeground"));
        assert_eq!(state.running().count(), 0);
        state.observe(&started("toolu_wait", "bwait"));
        state.observe(&bash_result("toolu_wait", "bwait"));
        state.observe(&json!({"type": "result", "subtype": "success"}));
        state.observe(&json!({
            "type": "system", "subtype": "task_progress", "task_id": "bwait",
            "description": "Still polling", "summary": "Progress text for this command",
        }));
        let commands: Vec<_> = state.running().collect();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].id, "bwait");
        assert_eq!(commands[0].summary, "Wait for the validation run");
    }

    #[test]
    fn claude_falls_back_to_the_matched_start_description_or_command_prefix() {
        let mut state = Claude::default();
        let mut call = bash_call("toolu_loop", true);
        call["message"]["content"][0]["input"]["command"] = json!("while :; do sleep 120; done");
        call["message"]["content"][0]["input"]["description"] = json!(" \n ");
        state.observe(&call);
        state.observe(&bash_result("toolu_loop", "bloop"));
        assert_eq!(state.running().next().unwrap().summary, "while …");

        state.observe(&bash_call("toolu_cli", true));
        state.observe(&json!({
            "type": "system", "subtype": "task_started", "task_type": "local_bash",
            "task_id": "bcli", "tool_use_id": "toolu_cli", "description": "Run fixture tests",
        }));
        assert_eq!(state.running().find(|command| command.id == "bcli").unwrap().summary, "Run fixture tests");
    }

    #[test]
    fn claude_tracks_monitor_and_ignores_a_result_after_early_completion() {
        let mut state = Claude::default();
        state.observe(&json!({
            "type": "assistant", "message": {"content": [{
                "type": "tool_use", "name": "Monitor", "id": "toolu_watch",
                "input": {"command": "tail -f private.log"}
            }]}
        }));
        state.observe(&json!({
            "type": "system", "subtype": "task_started", "task_type": "local_bash",
            "task_id": "bwatch", "tool_use_id": "toolu_watch"
        }));
        assert_eq!(state.running().next().unwrap().summary, "tail -f …");
        state.observe(&json!({
            "type": "system", "subtype": "task_notification",
            "task_id": "bwatch", "status": "stopped"
        }));
        assert_eq!(state.running().count(), 0);

        state.observe(&bash_call("toolu_fast", true));
        state.observe(&json!({
            "type": "system", "subtype": "task_notification",
            "task_id": "bfast", "status": "completed"
        }));
        state.observe(&bash_result("toolu_fast", "bfast"));
        assert_eq!(state.running().count(), 0);
    }

    #[test]
    fn codex_only_reports_processes_that_outlive_their_turn() {
        let mut state = Codex::default();
        let started = |id, process| {
            json!({
                "turnId": "turn1", "startedAtMs": 1000,
                "item": {"type": "commandExecution", "id": id, "processId": process,
                    "command": "/bin/bash -lc 'env API_TOKEN=private curl https://private'",
                    "status": "inProgress"}
            })
        };
        let completed = |id| json!({"item": {"type": "commandExecution", "id": id}});
        state.observe("item/started", &started("call1", Some("42")));
        state.observe("item/started", &started("call2", Some("43")));
        state.observe("item/started", &started("call3", None));
        state.observe("item/completed", &completed("call2"));
        assert_eq!(state.running().count(), 0);
        state.observe(
            "turn/completed",
            &json!({"turn": {"id": "turn1", "status": "completed"}}),
        );
        assert_eq!(
            state.running().cloned().collect::<Vec<_>>(),
            [running("42", "curl …".into(), 1000)]
        );
        state.observe("turn/started", &json!({"turn": {"id": "turn2"}}));
        assert_eq!(state.running().count(), 1);
        state.observe("item/completed", &completed("call1"));
        assert_eq!(state.running().count(), 0);
    }
}
