//! Chat summaries and grouping shared by the client and platform-independent tests.

use fuzukue_proto::Actor;
use serde_json::Value;

use crate::truncate;

pub fn is_json_document(text: &str) -> bool {
    let text = text.trim();
    text.starts_with(['{', '[']) && serde_json::from_str::<Value>(text).is_ok()
}

/// Returns the shell body of an app-server Bash wrapper, or the original
/// command. Shell quoting is decoded once, without expanding or running it.
fn bash_command(command: &str) -> String {
    let mut words = shlex::Shlex::new(command);
    if let Some(shell) = words.next() {
        let shell = shell.rsplit('/').next().unwrap_or(&shell);
        if shell == "bash" && matches!(words.next().as_deref(), Some("-lc" | "-cl" | "-c")) {
            if let Some(body) = words.next() {
                if words.next().is_none() && !words.had_error {
                    return body;
                }
            }
        }
    }
    command.to_owned()
}

pub fn tool_paths(input: &Value) -> impl Iterator<Item = &str> {
    let path = ["file_path", "path", "file", "notebook_path"]
        .into_iter()
        .find_map(|key| input[key].as_str());
    let paths = input["paths"].as_array().filter(|_| path.is_none());
    path.into_iter()
        .chain(paths.into_iter().flatten().filter_map(Value::as_str))
}

pub fn tool_summary(name: &str, input: &Value) -> String {
    let summary = match name.to_ascii_lowercase().as_str() {
        "bash" | "exec" | "exec_command" | "shell_command" => {
            if let Some(command) = input["command"].as_str().or_else(|| input.as_str()) {
                return truncate(&format!("$ {}", one_line(&bash_command(command))), 160);
            }
            name.to_owned()
        }
        "read" | "write" | "edit" | "multiedit" | "notebookedit" | "read_file" | "write_file"
        | "edit_file" | "view_image" => {
            let paths = tool_paths(input).collect::<Vec<_>>();
            if paths.is_empty() {
                name.to_owned()
            } else {
                format!("{name} {}", paths.join(", "))
            }
        }
        _ => {
            let args = match input {
                Value::Object(args) => {
                    let mut entries = args.iter().collect::<Vec<_>>();
                    entries.sort_unstable_by_key(|(key, _)| *key);
                    let mut values = entries
                        .into_iter()
                        .take(4)
                        .map(|(key, value)| format!("{key}={}", short_arg(value)))
                        .collect::<Vec<_>>();
                    if args.len() > values.len() {
                        values.push("…".into());
                    }
                    values.join(", ")
                }
                Value::Null => String::new(),
                other => short_arg(other),
            };
            format!("{name}({args})")
        }
    };
    truncate(&one_line(&summary), 160)
}

fn short_arg(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", truncate(&one_line(s), 32)),
        other => truncate(&other.to_string(), 32),
    }
}

fn one_line(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
}

pub fn tool_result_summary(output: &str) -> String {
    let mut lines = output.lines();
    let mut first = truncate(&one_line(lines.next().unwrap_or_default()), 160);
    if lines.next().is_some() && !first.ends_with('…') {
        first.push('…');
    }
    first
}

/// A batch build report lists failed papers immediately after its heading.
pub fn paper_batch_failure_summary<'a>(name: &str, output: &'a str) -> Option<&'a str> {
    if !matches!(name, "mcp__fuzukue__build_papers" | "fuzukue.build_papers") {
        return None;
    }
    let mut lines = output.lines();
    let heading = lines.next()?;
    (lines.next()? == "" && lines.next()?.starts_with("- ")).then_some(heading)
}

/// Tracks adjacent rows within a conversation. Results update existing rows
/// and do not participate in grouping.
#[derive(Default)]
pub struct ChatGrouping {
    actor: Option<Actor>,
    last_was_tool: bool,
}

pub struct RowPlacement {
    pub join_tools: bool,
    pub show_actor: bool,
}

impl ChatGrouping {
    pub fn push(&mut self, actor: &Actor, is_tool: bool) -> RowPlacement {
        let same_turn = self.actor.as_ref() == Some(actor);
        let join_tools = same_turn && is_tool && self.last_was_tool;
        self.last_was_tool = is_tool;
        if !same_turn {
            self.actor = Some(actor.clone());
        }
        RowPlacement {
            join_tools,
            show_actor: !same_turn || matches!(actor, Actor::User | Actor::System),
        }
    }

    pub fn end_turn(&mut self, actor: &Actor) {
        if self.actor.as_ref() == Some(actor) {
            *self = Self::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarizes_bash_with_nested_quotes_and_escapes() {
        let input: Value = serde_json::from_str(
            r#"{"command":"/usr/bin/bash -lc \"rg -n '\\\"a b\\\"\\\\.lean' \\\"my file\\\"\""}"#,
        )
        .unwrap();
        assert_eq!(
            tool_summary("exec", &input),
            r#"$ rg -n '"a b"\.lean' "my file""#
        );
        assert_eq!(
            tool_summary(
                "Bash",
                &json!({"command": r#"bash -lc 'printf "%s\n" '\''x'\'''"#})
            ),
            r#"$ printf "%s\n" 'x'"#,
        );
        for command in ["rg 'a b'", "bash -lc 'echo $1' _ arg", "bash -lc 'unclosed"] {
            assert_eq!(
                tool_summary("Bash", &json!({"command": command})),
                format!("$ {command}")
            );
        }
        assert_eq!(
            tool_summary("Bash", &json!({"command": "echo one\necho two"})),
            "$ echo one echo two"
        );
    }

    #[test]
    fn summarizes_file_tools() {
        assert_eq!(
            tool_summary(
                "Read",
                &json!({"file_path": r"C:\a b\file.md", "limit": 10})
            ),
            r"Read C:\a b\file.md"
        );
        assert_eq!(
            tool_summary("edit", &json!({"paths": ["a.md", "日本語.md"]})),
            "edit a.md, 日本語.md"
        );
        assert_eq!(
            tool_summary("view_image", &json!({"path": "plot.png"})),
            "view_image plot.png"
        );
    }

    #[test]
    fn summarizes_mcp_args_with_short_values() {
        assert_eq!(
            tool_summary(
                "mcp__fuzukue__search",
                &json!({"query": "x\ny", "limit": 3, "refresh": false})
            ),
            r#"mcp__fuzukue__search(limit=3, query="x y", refresh=false)"#,
        );
        assert_eq!(
            tool_summary("fuzukue.search", &json!({"query": "字".repeat(60)})),
            format!("fuzukue.search(query=\"{}…\")", "字".repeat(32)),
        );
        assert_eq!(tool_summary("fuzukue.status", &Value::Null), "fuzukue.status()");
        assert_eq!(
            tool_summary(
                "fuzukue.lookup",
                &json!({"e": 5, "b": 2, "d": 4, "c": 3, "a": 1})
            ),
            "fuzukue.lookup(a=1, b=2, c=3, d=4, …)",
        );
    }

    #[test]
    fn previews_only_the_first_result_line() {
        assert_eq!(tool_result_summary("line 1\r\nline 2\r\n"), "line 1…");
        assert_eq!(tool_result_summary("one line\n"), "one line");
        assert_eq!(tool_result_summary(""), "");
        assert_eq!(
            tool_result_summary(&"字".repeat(161)),
            format!("{}…", "字".repeat(160))
        );
    }

    #[test]
    fn recognizes_batch_failure_summaries_for_both_adapters() {
        let ja = "失敗 1 / 全 3（成功 1・警告あり 1）";
        let en = "Failed 1 / 3 total (1 succeeded, 1 with warnings)";
        for (tool, heading) in [
            ("mcp__fuzukue__build_papers", ja),
            ("fuzukue.build_papers", en),
        ] {
            let output =
                format!("{heading}\n\n- a-paper: Undefined control sequence.\n\n| Deck | Status |");
            assert_eq!(paper_batch_failure_summary(tool, &output), Some(heading));
            assert_eq!(paper_batch_failure_summary("other", &output), None);
            assert_eq!(
                paper_batch_failure_summary(tool, "Failed 0 / 1 total\n\n| Deck |"),
                None
            );
            assert_eq!(
                paper_batch_failure_summary(tool, "| Deck |\n| --- |\n| a-paper |"),
                None
            );
        }
    }

    #[test]
    fn detects_complete_json_objects_and_arrays() {
        assert!(is_json_document(r#" {"findings":[{"text":"$\\mu$"}]} "#));
        assert!(is_json_document("\n[1, null]\n"));
        for text in [
            "[自動査読] 修正",
            "[link](url)",
            "{",
            "[1",
            "{} trailing",
            "true",
            "42",
            "\"\"",
        ] {
            assert!(!is_json_document(text), "{text:?}");
        }
    }

    #[test]
    fn groups_adjacent_calls_within_one_actor_turn() {
        let claude = Actor::Claude;
        let codex = Actor::Codex {
            thread_id: "one".into(),
            parent_tool_use: Some("tool".into()),
        };
        let another = Actor::Codex {
            thread_id: "two".into(),
            parent_tool_use: None,
        };
        let mut groups = ChatGrouping::default();

        let first = groups.push(&claude, true);
        assert!(!first.join_tools && first.show_actor);
        assert!(groups.push(&claude, true).join_tools);
        assert!(!groups.push(&claude, false).show_actor);
        let after_text = groups.push(&claude, true);
        assert!(!after_text.join_tools && !after_text.show_actor);
        assert!(groups.push(&codex, true).show_actor);
        groups.end_turn(&claude); // An interleaved turn cannot close the Codex group.
        assert!(groups.push(&codex, true).join_tools);
        assert!(!groups.push(&another, true).join_tools);
        assert!(!groups.push(&codex, true).join_tools);
        groups.end_turn(&codex);
        let next_turn = groups.push(&codex, true);
        assert!(!next_turn.join_tools && next_turn.show_actor);
    }
}
