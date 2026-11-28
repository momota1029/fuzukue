//! Filesystem policy for a delegated job, independent of the front desk.
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) mod shell;
pub(crate) mod shell_rules;
pub(crate) mod workspace_read;

#[cfg(target_os = "linux")]
pub(crate) fn lower_child_cpu_priority(command: &mut tokio::process::Command) {
    let increment = std::env::var("FUZUKUE_WORKER_NICE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (1..=19).contains(value));
    if let Some(increment) = increment {
        // Only the child changes priority. Its tools inherit the new value.
        // Keep launching workers where the host disallows changing priority.
        unsafe {
            command.pre_exec(move || {
                let _ = rustix::process::nice(increment);
                Ok(())
            });
        }
    }
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    agents: Agents,
}

#[derive(Deserialize, Default)]
struct Agents {
    #[serde(default)]
    read_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Scope {
    pub(crate) cwd: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) memory_roots: Vec<PathBuf>,
}

impl Scope {
    pub(crate) fn open(project: &Path, cwd: &Path, scratch: &Path) -> Result<Self> {
        let config = match std::fs::read_to_string(fuzukue_proto::compat::workspace_config(project)) {
            Ok(text) => {
                toml::from_str::<Config>(&text).context("worker read roots in fuzukue.toml")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error.into()),
        };
        #[cfg(test)]
        let home = Some(scratch.join("test-home"));
        #[cfg(not(test))]
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut read_roots = ["/usr", "/etc", "/bin", "/sbin", "/lib", "/lib64", "/opt"]
            .into_iter()
            .map(PathBuf::from)
            .chain(home.iter().flat_map(|home| {
                // `.codex/packages` holds the Codex binary itself: its sandbox re-executes it,
                // so without read access every command in the worker fails.
                [".cargo", ".rustup", ".elan", ".cache", ".codex/packages", ".local/bin", ".nvm"]
                    .map(|name| home.join(name))
            }))
            .chain(tool_install_dir("codex"))
            .filter_map(|path| path.canonicalize().ok())
            .collect::<Vec<_>>();
        for root in config.agents.read_roots {
            let path = if let Ok(relative) = root.strip_prefix("~") {
                home.as_ref()
                    .context("HOME is required for ~/ in agents.read_roots")?
                    .join(relative)
            } else {
                project.join(&root)
            };
            read_roots.push(
                path.canonicalize()
                    .with_context(|| format!("worker read root {}", path.display()))?,
            );
        }
        read_roots.sort();
        read_roots.dedup();
        let project = project.canonicalize()?;
        #[cfg(not(test))]
        let global = global_memory_root();
        #[cfg(test)]
        let global = Some(scratch.canonicalize()?.join(".fuzukue/memory"));
        let mut memory_roots = vec![project.join(".fuzukue/memory")];
        memory_roots.extend(global);
        memory_roots.sort();
        memory_roots.dedup();
        Ok(Self {
            cwd: cwd.canonicalize()?,
            scratch: scratch.canonicalize()?,
            read_roots,
            memory_roots,
        })
    }

    pub(crate) fn prepare_memory_mounts(&self) -> Result<()> {
        for root in &self.memory_roots {
            ensure!(
                !self.writable().iter().any(|path| path.starts_with(root)),
                "worker directory is inside app-owned memory"
            );
            crate::memory::prepare_sandbox_mount(root)?;
        }
        Ok(())
    }

    pub(crate) fn writable(&self) -> [&Path; 2] {
        [&self.cwd, &self.scratch]
    }

    pub(crate) fn temp_env(&self) -> [(&'static str, &Path); 3] {
        ["TMPDIR", "TMP", "TEMP"].map(|name| (name, self.scratch.as_path()))
    }

    fn allows(&self, base: &Path, path: &Path, write: bool) -> bool {
        resolve(&base.join(path)).is_ok_and(|path| {
            if self.memory_roots.iter().any(|root| path.starts_with(root))
                || (write && crate::preferences::protected_write(&path)) {
                return false;
            }
            self.writable().iter().any(|root| path.starts_with(root))
                || !write && self.read_roots.iter().any(|root| path.starts_with(root))
        })
    }

    pub(crate) fn claude_settings(&self, executable: &Path) -> Value {
        json!({
            "permissions": { "ask": ["Bash"] },
            "hooks": { "PreToolUse": [{
                "matcher": "Read|Grep|Glob|Edit|MultiEdit|Write|NotebookEdit|Bash",
                "hooks": [{
                    "type": "command", "command": executable,
                    "args": ["worker-scope-hook"], "timeout": 10, "onFailure": "block"
                }]
            }] }
        })
    }

    pub(crate) fn claude_decision(&self, event: &Value) -> Value {
        let input = &event["tool_input"];
        let cwd = event["cwd"].as_str().map(Path::new).unwrap_or(&self.cwd);
        let file = |key: &str, write| {
            input[key]
                .as_str()
                .is_some_and(|path| !path.is_empty() && self.allows(cwd, Path::new(path), write))
        };
        let allowed = match event["tool_name"].as_str() {
            Some("Read") => file("file_path", false),
            Some("Grep" | "Glob") => {
                // Claude defaults searches to the current directory. The scope
                // check covers the search root, including its symlink target.
                let path = input["path"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .unwrap_or(".");
                let pattern = input["glob"].as_str().or_else(|| {
                    input["pattern"]
                        .as_str()
                        .filter(|_| event["tool_name"] == "Glob")
                });
                self.allows(cwd, Path::new(path), false)
                    && resolve(&cwd.join(path)).is_ok_and(|search| {
                        !self.memory_roots.iter().any(|root| root.starts_with(&search))
                    })
                    && pattern.is_none_or(|pattern| {
                        let pattern = Path::new(pattern);
                        !pattern.is_absolute()
                            && !pattern
                                .components()
                                .any(|part| part == Component::ParentDir)
                    })
            }
            Some("Write" | "Edit" | "MultiEdit") => file("file_path", true),
            Some("NotebookEdit") => file("notebook_path", true),
            Some("Bash") => {
                // The server checks the complete command and records the decision.
                // Claude's built-in allowances must not skip that check.
                return decision(
                    "ask",
                    "The workspace reviews each worker shell command.",
                );
            }
            _ => false,
        };
        if allowed {
            json!({})
        } else {
            decision("deny", "Path outside the worker filesystem scope.")
        }
    }

    /// A Read request for one of the job's own files, such as background command output.
    pub(crate) fn reads_own_file(&self, cwd: &Path, input: &Value) -> bool {
        let Some(fields) = input.as_object() else { return false };
        let known = fields.iter().all(|(key, value)| match key.as_str() {
            "file_path" => value.is_string(),
            "offset" | "limit" => value.as_u64().is_some(),
            _ => false,
        });
        known && input["file_path"].as_str().filter(|path| !path.is_empty()).is_some_and(|path| {
            cwd.join(path).canonicalize().is_ok_and(|path| path.is_file() && own_file(&self.scratch, &path))
        })
    }

}

fn global_memory_root() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    home.is_absolute().then(|| {
        home.canonicalize().unwrap_or(home).join(".fuzukue/memory")
    })
}

pub(crate) fn memory_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![workspace.join(".fuzukue/memory")];
    roots.extend(global_memory_root());
    let aliases: Vec<_> = roots.iter().filter_map(|path| resolve(path).ok()).collect();
    roots.extend(aliases);
    roots.sort();
    roots.dedup();
    roots
}

pub(crate) fn protected_roots(state_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![state_dir.to_path_buf()];
    roots.extend(global_memory_root());
    #[cfg(not(test))]
    {
        let bases = fuzukue_proto::compat::config_home().into_iter().chain([
            fuzukue_proto::compat::state_home(),
        ]);
        roots.extend(bases.flat_map(|base| {
            ["fuzukue", fuzukue_proto::compat::LEGACY_APP_DIR].map(|name| base.join(name))
        }));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            roots.extend([".config/fuzukue", ".local/state/fuzukue"].map(|path| home.join(path)));
        }
    }
    let aliases: Vec<_> = roots.iter().filter_map(|path| resolve(path).ok()).collect();
    roots.extend(aliases);
    roots
}

/// A resolved path inside the job's temporary directory, such as background
/// command output. Top-level dot entries hold copied backend logins and stay excluded.
pub(crate) fn own_file(scratch: &Path, canonical: &Path) -> bool {
    canonical.strip_prefix(scratch).is_ok_and(|relative| {
        relative.components().next().is_none_or(|first| {
            matches!(first, Component::Normal(name) if !name.to_string_lossy().starts_with('.'))
        })
    })
}

pub(crate) const SHELL_READ_GUIDANCE: &str =
    "\n\nFor shell reads, pass paths directly to commands (e.g. `sed -n '1,40p' dir/file`, `rg -n x dir/file`). Avoid `cd dir && cmd`.";

pub(crate) const WORKER_READ_GUIDANCE: &str =
    "\n\nRead files with the file-reading tool (Read, or fs_read when available) instead of `cat`, `head`, or `tail` in the shell. This includes your own background command output and other files in the job's temporary directory, which never need approval.";

pub(crate) fn append_environment_notes(instructions: &mut String, workspace: &Path) -> Result<()> {
    let (_, en) = include_str!("../../../rules/worker-environment.md")
        .split_once("\n<!-- English -->\n")
        .expect("worker environment notes must contain both languages");
    instructions.push_str("\n\n");
    instructions.push_str(en.trim());
    instructions.push_str(SHELL_READ_GUIDANCE);
    instructions.push_str(WORKER_READ_GUIDANCE);

    let path = fuzukue_proto::compat::workspace_data(workspace, "worker-notes.md");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    };
    ensure!(metadata.is_file(), "{} is not a regular file", path.display());
    let path = path.canonicalize()?;
    ensure!(path.starts_with(workspace.canonicalize()?), "worker notes escape the workspace");
    let notes = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if !notes.trim().is_empty() {
        instructions.push_str("\n\n");
        instructions.push_str(notes.trim());
    }
    Ok(())
}

fn decision(behavior: &str, reason: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse", "permissionDecision": behavior, "permissionDecisionReason": reason
    }})
}

// Resolve existing components before processing '..', and permit missing
// destinations for Write/Edit. A dangling symlink is never a missing destination.
pub(crate) fn resolve(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.is_symlink() => resolved = resolved.canonicalize()?,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

pub(crate) fn claude_hook() -> Result<()> {
    let scope: Scope = serde_json::from_str(&fuzukue_proto::compat::env("FUZUKUE_WORKER_SCOPE")?)?;
    let input = serde_json::from_reader(std::io::stdin().lock())?;
    println!("{}", scope.claude_decision(&input));
    Ok(())
}

/// The directory holding the real binary of a tool found on PATH (after symlinks),
/// so a sandboxed worker can still execute it.
fn tool_install_dir(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|found| found.canonicalize().ok())
        .and_then(|real| real.parent().map(Path::to_path_buf))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_roots_are_excluded_from_worker_access() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let project = fixture.path().join("project");
        let scratch = fixture.path().join("scratch");
        std::fs::create_dir(&project)?;
        std::fs::create_dir(&scratch)?;
        let scope = Scope::open(&project, &project, &scratch)?;
        let event = json!({"cwd": project, "tool_name": "Grep", "tool_input": {"path": "."}});
        assert_eq!(scope.claude_decision(&event)["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(shell::check(&scope, &json!({"command": "find . -type f"}), &[]).is_none());
        if Path::new("/usr/bin/ls").is_file() {
            assert!(shell::check(&scope, &json!({"command": "ls ."}), &[]).is_some());
        }
        assert!(shell::check(&scope, &json!({"command": format!(
            "mkdir -p {}/.fuzukue/memory/items", scratch.display()
        )}), &[]).is_none());
        std::fs::create_dir_all(project.join(".fuzukue/memory/items"))?;
        std::fs::write(project.join(".fuzukue/memory/index.jsonl"), "private")?;
        let global_memory = scratch.join(".fuzukue/memory");
        std::fs::create_dir_all(&global_memory)?;
        std::fs::write(global_memory.join("index.jsonl"), "private")?;
        for path in [project.join(".fuzukue/memory"), global_memory] {
            for tool in ["Read", "Write", "Edit"] {
                let event = json!({"cwd": project, "tool_name": tool,
                    "tool_input": {"file_path": path.join("index.jsonl")}});
                assert_eq!(scope.claude_decision(&event)["hookSpecificOutput"]["permissionDecision"], "deny");
            }
        }
        assert!(shell::check(&scope, &json!({"command": "cat .fuzukue/memory/index.jsonl"}), &[]).is_none());
        Ok(())
    }

    #[test]
    fn claude_guard_checks_file_tools_and_routes_shell_to_server_review() -> Result<()>
    {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let project = root.join("project");
        let worktree = root.join("project-issue-42");
        let extra = root.join("references");
        let scratch = root.join("scratch");
        for path in [&project, &worktree, &extra, &scratch] {
            std::fs::create_dir(path)?;
        }
        std::fs::write(
            project.join("fuzukue.toml"),
            "[agents]\nread_roots = ['../references']",
        )?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&project, worktree.join("escape"))?;

        let scope = Scope::open(&project, &worktree, &scratch)?;
        let check = |tool, input: Value, expected| {
            let event = json!({"cwd": worktree, "tool_name": tool, "tool_input": input});
            assert_eq!(
                scope.claude_decision(&event)["hookSpecificOutput"]["permissionDecision"].as_str(),
                expected,
                "{event}"
            );
        };
        check(
            "Read",
            json!({"file_path": project.join("fuzukue.toml")}),
            Some("deny"),
        );
        check("Read", json!({"file_path": extra.join("source.md")}), None);
        check(
            "Write",
            json!({"file_path": extra.join("new.md")}),
            Some("deny"),
        );
        check("Write", json!({"file_path": scratch.join("new.md")}), None);
        for tool in ["Write", "Edit", "MultiEdit"] {
            check(tool, json!({"file_path": worktree.join(".fuzukue/rules/private.md")}), Some("deny"));
        }
        check("NotebookEdit", json!({"notebook_path": worktree.join(".fuzukue/rules/private.ipynb")}), Some("deny"));
        check(
            "Edit",
            json!({"file_path": "../project/fuzukue.toml"}),
            Some("deny"),
        );
        check(
            "NotebookEdit",
            json!({"notebook_path": "notebooks/new.ipynb"}),
            None,
        );
        check("Glob", json!({"pattern": "**/*.md"}), None);
        check("Glob", json!({"pattern": "../project/*.md"}), Some("deny"));
        check(
            "Grep",
            json!({"path": project, "pattern": "test"}),
            Some("deny"),
        );
        #[cfg(unix)]
        check(
            "Read",
            json!({"file_path": "escape/fuzukue.toml"}),
            Some("deny"),
        );
        check("Bash", json!({"command": "cat ./cards/sample.md"}), Some("ask"));
        check(
            "Bash",
            json!({"command": format!("printf x >'{}'", project.join("out").display())}),
            Some("ask"),
        );
        check(
            "Bash",
            json!({"command": "cd ../project && pwd"}),
            Some("ask"),
        );
        Ok(())
    }
}
