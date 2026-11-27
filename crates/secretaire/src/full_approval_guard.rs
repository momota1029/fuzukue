//! Operations that stay with a person while a thread or workspace runs hands-off.
//!
//! Shell commands are parsed, and the programs they name and the arguments of
//! known programs are checked. A line that cannot be read, a program name taken
//! from a variable or substitution, and a wrapper that is not understood ask a
//! person. The check catches honest mistakes only. A script file, an interpreter
//! one-liner or a build step runs unexamined, so it is not isolation.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

#[path = "full_approval_parse.rs"]
mod parse;
#[path = "full_approval_programs.rs"]
mod programs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    ExternalWrite,
    DestructiveGit,
    Administrator,
    Credentials,
    Unclassified,
}

impl Reason {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::ExternalWrite => harness_text!(
                "it pushes, publishes or writes to an external service", "プッシュ、公開、外部サービスへの書き込みを含む"
            ),
            Self::DestructiveGit => harness_text!(
                "it can destroy work or Git history", "作業内容や Git の履歴を失うことがある"
            ),
            Self::Administrator => harness_text!("it needs administrator rights", "管理者権限が必要"),
            Self::Credentials => harness_text!(
                "it touches server settings or credentials", "サーバーの設定や認証情報に触れる"
            ),
            Self::Unclassified => harness_text!("it cannot be checked automatically", "内容を自動で判定できない"),
        }
    }

    /// Whether the question may be about a push, so it lists recorded findings.
    pub(crate) fn may_push(self) -> bool {
        matches!(self, Self::ExternalWrite | Self::Unclassified)
    }
}

/// Backend tools that act on this machine only. Their paths and commands are
/// checked by [`request`]. Every other native tool, such as web access and
/// messaging, stays with a person.
const LOCAL_TOOLS: &[&str] = &[
    "Read", "Grep", "Glob", "LS", "LSP", "Edit", "MultiEdit", "Write", "NotebookRead", "NotebookEdit",
    "Bash", "BashOutput", "KillShell", "KillBash", "Monitor", "TodoWrite", "Agent", "Task", "TaskCreate",
    "TaskGet", "TaskList", "TaskOutput", "TaskStop", "TaskUpdate", "ToolSearch", "Skill",
];

/// Whether an automatic mode may grant a backend tool at all. The server's own
/// tools have separate rules. Tools of other MCP servers stay with a person.
pub(crate) fn automatic_tool(tool: &str) -> bool {
    tool.starts_with("mcp__fuzukue__") || LOCAL_TOOLS.contains(&tool)
}

/// Check one backend tool request. `protected` are the server's state and
/// configuration directories.
pub(crate) fn request(tool: &str, input: &Value, protected: &[PathBuf]) -> Option<Reason> {
    let paths = ["file_path", "notebook_path", "path", "cwd", "grantRoot"];
    if paths.iter().filter_map(|key| input[*key].as_str()).any(|path| location(path, protected))
        || tool == "Glob" && input["pattern"].as_str().is_some_and(|path| credential(path, protected))
    {
        return Some(Reason::Credentials);
    }
    match tool {
        "item/permissions/requestApproval" => permissions(&input["permissions"], protected),
        "item/fileChange/requestApproval" => file_changes(input, protected),
        // A file's content can quote any command, so only commands are scanned.
        "Bash" | "exec" | "Monitor" | "item/commandExecution/requestApproval" => {
            let network = (!input["networkApprovalContext"].is_null()).then_some(Reason::ExternalWrite);
            let found = match &input["command"] {
                Value::String(text) => command(text, protected),
                Value::Array(words) => {
                    match words.iter().map(|word| word.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>() {
                        Some(words) => argv(&words, protected),
                        None => Some(Reason::Unclassified),
                    }
                }
                // A monitor can watch something other than a local command.
                _ => network.or(Some(Reason::Unclassified)),
            };
            found.or(network)
        }
        _ => None,
    }
}

/// The paths a Codex patch changes. Without them there is nothing to check.
fn file_changes(input: &Value, protected: &[PathBuf]) -> Option<Reason> {
    let Some(changes) = input["changes"].as_array().filter(|changes| !changes.is_empty()) else {
        return Some(Reason::Unclassified);
    };
    let git = |path: &Path| path.components().any(|part| part.as_os_str().eq_ignore_ascii_case(".git"));
    for change in changes {
        let Some(path) = change["path"].as_str() else { return Some(Reason::Unclassified) };
        let moved = [&change["kind"]["move_path"], &change["kind"]["movePath"], &change["move_path"]]
            .into_iter().filter_map(Value::as_str);
        for path in std::iter::once(path).chain(moved) {
            let full = input["cwd"].as_str().filter(|_| Path::new(path).is_relative())
                .map_or_else(|| PathBuf::from(path), |cwd| Path::new(cwd).join(path));
            let Some(text) = full.to_str() else { return Some(Reason::Unclassified) };
            if location(text, protected) {
                return Some(Reason::Credentials);
            }
            if git(&full) || resolve(&full).is_some_and(|real| git(&real)) {
                return Some(Reason::DestructiveGit);
            }
        }
    }
    None
}

/// The path after following the links among its existing parts.
fn resolve(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|existing| {
        let real = existing.canonicalize().ok()?;
        Some(real.join(path.strip_prefix(existing).ok()?))
    })
}

/// [`credential`] for a file operand, including what its links point at.
fn location(path: &str, protected: &[PathBuf]) -> bool {
    credential(path, protected) || Path::new(path).is_absolute()
        && resolve(Path::new(path)).and_then(|real| real.to_str().map(str::to_owned))
            .is_some_and(|real| credential(&real, protected))
}

/// A Codex permission grant lifts the sandbox for later commands, which then
/// run without a request.
fn permissions(value: &Value, protected: &[PathBuf]) -> Option<Reason> {
    fn any(value: &Value, test: &dyn Fn(&str) -> bool) -> bool {
        match value {
            Value::String(text) => test(text),
            Value::Array(items) => items.iter().any(|item| any(item, test)),
            Value::Object(map) => map.iter().any(|(key, item)| test(key) || any(item, test)),
            _ => false,
        }
    }
    let network = &value["network"];
    if !(network.is_null() || network["enabled"] == false) {
        return Some(Reason::ExternalWrite);
    }
    any(value, &|text| location(text, protected) || text == "/").then_some(Reason::Credentials)
}

fn home() -> Option<String> {
    std::env::var("HOME").ok().filter(|home| home.len() > 1)
}

/// Whether a path names server state, server settings or a stored login.
pub(crate) fn credential(path: &str, protected: &[PathBuf]) -> bool {
    let expanded = match (path.strip_prefix('~'), home()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_owned(),
    };
    let path = Path::new(&expanded);
    let parts: Vec<_> = path.components().filter_map(|part| match part {
        Component::Normal(name) => name.to_str(),
        _ => None,
    }).collect();
    let stored = parts.iter().enumerate().any(|(index, part)| {
        let next = parts.get(index + 1).copied().unwrap_or_default();
        matches!(*part, ".ssh" | ".gnupg" | ".netrc" | ".git-credentials")
            || matches!((*part, next), (".config", "gh") | (".config", "fuzukue") | (".aws", "credentials")
                | (".codex", "auth.json") | (".claude", ".credentials.json") | (".docker", "config.json")
                | (".cargo", "credentials") | (".cargo", "credentials.toml") | (".config", "git"))
            || (*part == "state" && next == "fuzukue")
                && index > 0 && parts[index - 1] == ".local" && !job_file(&parts[index + 2..])
    });
    // The server keeps each job's temporary directory inside its state directory.
    let server = protected.iter().any(|root| path.strip_prefix(root).is_ok_and(|rest| {
        let rest: Vec<_> = rest.iter().filter_map(|part| part.to_str()).collect();
        !job_file(&rest)
    }));
    let own_job = protected.iter().any(|root| path.strip_prefix(root).is_ok_and(|rest| {
        job_file(&rest.iter().filter_map(|part| part.to_str()).collect::<Vec<_>>())
    }));
    server || stored && !own_job
}

/// `.../worker-XXXX/<file>` and `.../exec-XXXX/<file>` are a job's own files. Its top-level dot entries hold
/// copied backend logins.
fn job_file(parts: &[&str]) -> bool {
    parts.iter().enumerate().any(|(index, part)| {
        ["worker-", "exec-"].iter().any(|prefix| part.len() > prefix.len() && part.starts_with(prefix))
            && parts[index + 1..].first().is_none_or(|next| !next.starts_with('.'))
            && !parts[index + 1..].contains(&"..")
    })
}

/// Check one shell command line.
pub(crate) fn command(text: &str, protected: &[PathBuf]) -> Option<Reason> {
    programs::Check { protected, home: home() }.script(text, 0)
}

/// Check a program and its arguments that no shell reads.
pub(crate) fn argv(words: &[String], protected: &[PathBuf]) -> Option<Reason> {
    let words: Vec<_> = words.iter().map(|word| parse::Word::literal(word)).collect();
    let check = programs::Check { protected, home: home() };
    check.paths(&words).or_else(|| check.invocation(&parse::Simple { words, ..Default::default() }, 0))
}

/// Whether a Bash request only reads or searches files, so the file tools can
/// do the same. Anything else, including unknown options that run or write,
/// is false.
pub(crate) fn plain_read(input: &Value) -> bool {
    let known = input.as_object().is_some_and(|object| object.iter().all(|(key, value)| match key.as_str() {
        "command" | "description" => value.is_string(),
        "timeout" => value.as_u64().is_some(),
        "run_in_background" | "dangerouslyDisableSandbox" => value == false,
        _ => false,
    }));
    let Some(text) = input["command"].as_str().filter(|_| known) else { return false };
    // Only commands joined by `;`, `&&`, `||`, pipes and newlines, built from
    // literal words, without redirects, expansions, groups or background jobs.
    let Some(tree) = parse::tree(text) else { return false };
    let plain = parse::nodes(tree.root_node()).into_iter().all(|node| match node.kind() {
        "program" | "list" | "pipeline" | "command" | "command_name" | "word" | "number" | "string"
        | "string_content" | "raw_string" | "concatenation" => true,
        kind => !node.is_named() && matches!(kind, "&&" | "||" | ";" | "|" | "\n" | "\""),
    });
    let Some(script) = parse::script(text, None).filter(|_| plain) else { return false };
    let mut reads = false;
    script.commands.iter().all(|command| {
        if command.words.iter().any(|word| word.dynamic) {
            return false;
        }
        let words: Vec<_> = command.words.iter().map(|word| word.text.as_str()).collect();
        let Some((program, args)) = words.split_first() else { return false };
        let long = |names: &[&str]| args.iter().any(|arg| names.iter().any(|name| arg.starts_with(name)));
        let allowed = match *program {
            "cd" | "pwd" => return true,
            "ls" | "cat" | "head" | "grep" => true,
            "tail" => !long(&["--follow"]) && !args.iter().any(|arg| arg.starts_with('-') && arg.contains(['f', 'F'])),
            "rg" => !long(&["--pre", "--hostname-bin"]),
            "find" => !args.iter().any(|arg| ["-exec", "-ok", "-delete", "-fprint", "-fls"]
                .iter().any(|action| arg.starts_with(action))),
            "sed" => matches!(args.first(), Some(&"-n")) && args.get(1).is_some_and(|script| {
                script.ends_with('p') && script[..script.len() - 1].chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | '$'))
            }) && args[2..].iter().all(|arg| !arg.starts_with('-')),
            _ => false,
        };
        reads |= allowed;
        allowed
    }) && reads
}

const REFUSALS_PER_COMMAND: u8 = 2;
const REFUSALS_PER_RUN: usize = 12;

/// Plain reads answered with the name of a file tool instead of a card. The
/// bounds return an insisting agent to the ordinary question.
#[derive(Default)]
pub(crate) struct Refusals {
    commands: std::collections::HashMap<String, u8>,
    total: usize,
}

impl Refusals {
    pub(crate) fn refuse(&mut self, input: &Value, protected: Option<Reason>) -> Option<&'static str> {
        if protected.is_some() || !plain_read(input) || self.total >= REFUSALS_PER_RUN {
            return None;
        }
        let count = self.commands.entry(input["command"].as_str()?.trim().to_owned()).or_default();
        if *count >= REFUSALS_PER_COMMAND {
            return None;
        }
        *count += 1;
        self.total += 1;
        Some(harness_text!(
            "This command was not run. Read and search with the file tools, which need no approval: fs_list for ls and find; fs_search (or Grep and Glob) for rg and grep; fs_read or Read for cat, head, tail and sed -n; jq for JSON; git_log and git_show for history. If no tool can do it, run the same command again to ask the user.",
            "このコマンドは実行していない。読み取りと検索は、承認の要らないファイル用の道具を使う。ls と find は fs_list、rg と grep は fs_search（Grep・Glob も可）、cat・head・tail・sed -n は fs_read か Read、JSON は jq、履歴は git_log と git_show。道具でできない場合は、同じコマンドをもう一度実行すると利用者に確認する。"
        ))
    }
}

#[cfg(test)]
#[path = "full_approval_guard_tests.rs"]
mod tests;
