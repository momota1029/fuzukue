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
        Ok(Self {
            cwd: cwd.canonicalize()?,
            scratch: scratch.canonicalize()?,
            read_roots,
        })
    }

    pub(crate) fn writable(&self) -> [&Path; 2] {
        [&self.cwd, &self.scratch]
    }

    pub(crate) fn temp_env(&self) -> [(&'static str, &Path); 3] {
        ["TMPDIR", "TMP", "TEMP"].map(|name| (name, self.scratch.as_path()))
    }

    fn allows(&self, base: &Path, path: &Path, write: bool) -> bool {
        resolve(&base.join(path)).is_ok_and(|path| {
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

}

pub(crate) fn protected_roots(state_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![state_dir.to_path_buf()];
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

pub(crate) const SHELL_READ_GUIDANCE: &str =
    "\n\nFor shell reads, pass paths directly to commands (e.g. `sed -n '1,40p' dir/file`, `rg -n x dir/file`). Avoid `cd dir && cmd`.";

pub(crate) fn append_environment_notes(instructions: &mut String, workspace: &Path) -> Result<()> {
    let (_, en) = include_str!("../../../rules/worker-environment.md")
        .split_once("\n<!-- English -->\n")
        .expect("worker environment notes must contain both languages");
    instructions.push_str("\n\n");
    instructions.push_str(en.trim());
    instructions.push_str(SHELL_READ_GUIDANCE);

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
fn resolve(path: &Path) -> Result<PathBuf> {
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
