use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub(super) type Arguments = BTreeMap<String, String>;

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Config {
    pub(super) recipes: BTreeMap<String, Recipe>,
    pub(super) recipe_runner: Runner,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Runner {
    pub(super) allow_unisolated: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recipe {
    command: Vec<String>,
    pub(super) cwd: PathBuf,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    pub(super) timeout_secs: u32,
    pub(super) max_memory_mib: u32,
    #[serde(default)]
    pub(super) args: BTreeMap<String, Argument>,
    pub(super) queue: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Argument {
    kind: Kind,
    #[serde(default)]
    required: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    TestName,
}

pub(super) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

impl Config {
    pub(super) fn parse(source: &str) -> Result<Self> {
        let config: Self = toml::from_str(source).context("invalid server recipe configuration")?;
        for (name, recipe) in &config.recipes {
            ensure!(valid_name(name), "invalid recipe name: {name}");
            recipe
                .validate()
                .with_context(|| format!("[recipes.{name}]"))?;
        }
        Ok(config)
    }
}

impl Recipe {
    fn validate(&self) -> Result<()> {
        let program = self
            .command
            .first()
            .context("command must be a nonempty argv array")?;
        ensure!(
            !program.is_empty() && placeholder(program).is_none(),
            "the executable must be fixed"
        );
        ensure!(
            self.command.iter().all(|part| !part.contains('\0')),
            "command contains NUL"
        );
        ensure!(
            !self.cwd.as_os_str().is_empty()
                && self
                    .cwd
                    .components()
                    .all(|part| { matches!(part, Component::Normal(_) | Component::CurDir) }),
            "cwd must be workspace-relative without parent components"
        );
        ensure!(
            self.timeout_secs > 0 && self.max_memory_mib > 0,
            "timeout_secs and max_memory_mib must be positive"
        );
        ensure!(valid_name(&self.queue), "invalid queue name");
        for arg in self
            .command
            .iter()
            .skip(1)
            .filter_map(|part| placeholder(part))
        {
            ensure!(
                self.args.contains_key(arg),
                "undeclared argv placeholder {arg}"
            );
        }
        for name in self.args.keys() {
            ensure!(valid_name(name), "invalid argument name: {name}");
            ensure!(
                self.command
                    .iter()
                    .skip(1)
                    .any(|part| placeholder(part) == Some(name)),
                "unused declared argument: {name}"
            );
        }
        for (name, value) in &self.env {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    && !name.starts_with("FUZUKUE_")
                    && !name.starts_with("XDG_")
                    && !matches!(name.as_str(), "HOME" | "TMPDIR" | "TMP" | "TEMP" | "PATH")
                    && !value.contains('\0'),
                "invalid or reserved environment variable: {name}",
            );
        }
        Ok(())
    }

    pub(super) fn argv(&self, args: &Arguments) -> Result<Vec<String>> {
        for (name, value) in args {
            let argument = self
                .args
                .get(name)
                .with_context(|| format!("undeclared argument: {name}"))?;
            ensure!(argument.kind.accepts(value), "invalid value for {name}");
        }
        for (name, argument) in &self.args {
            ensure!(
                !argument.required || args.contains_key(name),
                "missing required argument: {name}"
            );
        }
        Ok(self
            .command
            .iter()
            .filter_map(|part| match placeholder(part) {
                Some(name) => args.get(name).cloned(),
                None => Some(part.clone()),
            })
            .collect())
    }
}

impl Kind {
    fn accepts(self, value: &str) -> bool {
        match self {
            // A filter is a positional argument. Reject leading '-' for commands that parse options.
            Self::TestName => {
                !value.is_empty()
                    && value.len() <= 256
                    && !value.starts_with('-')
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-/".contains(&byte))
            }
        }
    }
}

fn placeholder(part: &str) -> Option<&str> {
    part.strip_prefix('{')?.strip_suffix('}')
}

pub(super) fn inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root) || path.canonicalize().is_ok_and(|path| path.starts_with(root))
}
