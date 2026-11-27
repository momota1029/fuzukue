use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::TestSuite;
use serde::Deserialize;
use walkdir::WalkDir;

#[derive(Debug)]
pub(super) struct Suite {
    pub info: TestSuite,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub audit: Option<PathBuf>,
    pub timeout: Duration,
    pub env: BTreeMap<String, String>,
    pub seed: Option<String>,
    pub expect: Option<PathBuf>,
    pub abs_tol: f64,
    pub rel_tol: f64,
    /// Run with a few inherited environment variables only.
    pub scrubbed: bool,
}

#[derive(Default, Deserialize)]
struct Config {
    tests: Option<BTreeMap<String, Override>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CommandLine {
    Text(String),
    Args(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    command: CommandLine,
    cwd: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    seed: Option<toml::Value>,
    expect: Option<String>,
    abs_tol: Option<f64>,
    rel_tol: Option<f64>,
}

impl Suite {
    fn new(root: &Path, cwd: &Path, id: String, kind: &str, command: Vec<String>) -> Self {
        let cwd_label = cwd.strip_prefix(root).unwrap().to_string_lossy();
        let command_label = shlex::try_join(command.iter().map(String::as_str))
            .unwrap_or_else(|_| command.join(" "));
        Self {
            info: TestSuite {
                id,
                kind: kind.into(),
                cwd: if cwd_label.is_empty() {
                    ".".into()
                } else {
                    cwd_label.into_owned()
                },
                command: command_label,
                unavailable: None,
            },
            cwd: cwd.into(),
            command,
            audit: None,
            timeout: Duration::from_secs(600),
            env: BTreeMap::new(),
            seed: None,
            expect: None,
            abs_tol: 1e-8,
            rel_tol: 1e-6,
            scrubbed: false,
        }
    }

    pub(super) fn missing_runner(&self) -> Option<String> {
        let program = Path::new(&self.command[0]);
        let executable = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        let available = if self.command[0].contains('/') {
            executable(&self.cwd.join(program))
        } else {
            let path = self
                .env
                .get("PATH")
                .map(Into::into)
                .or_else(|| std::env::var_os("PATH"));
            path.is_some_and(|path| {
                std::env::split_paths(&path)
                    .any(|dir| executable(&self.cwd.join(dir).join(program)))
            })
        };
        (!available).then(|| self.command[0].clone())
    }
}

fn configured(root: &Path, dir: &Path, name: &str, cfg: Override) -> Result<Suite> {
    ensure!(!name.trim().is_empty(), "テスト名が空");
    let command = match cfg.command {
        CommandLine::Text(text) => shlex::split(&text).context("command の引用符が不正")?,
        CommandLine::Args(args) => args,
    };
    ensure!(
        command.first().is_some_and(|arg| !arg.is_empty()),
        "command が空"
    );
    let cwd = crate::safe_join(
        root,
        dir.join(cfg.cwd.as_deref().unwrap_or("."))
            .to_string_lossy()
            .as_ref(),
    )?;
    ensure!(cwd.is_dir(), "cwd がディレクトリではない");
    let prefix = dir.strip_prefix(root)?;
    let id = if prefix.as_os_str().is_empty() {
        name.into()
    } else {
        format!("{}:{name}", prefix.display())
    };
    let mut suite = Suite::new(
        root,
        &cwd,
        id,
        if cfg.expect.is_some() {
            "numeric"
        } else {
            "custom"
        },
        command,
    );
    let timeout = cfg.timeout.unwrap_or(600);
    ensure!(timeout > 0, "timeout は正の秒数");
    suite.timeout = Duration::from_secs(timeout);
    suite.env = cfg.env;
    suite.seed = match cfg.seed {
        Some(toml::Value::String(value)) => Some(value),
        Some(toml::Value::Integer(value)) => Some(value.to_string()),
        None => None,
        _ => bail!("seed は整数か文字列"),
    };
    // Resolve again at run time so a replaced symlink cannot point outside the workspace.
    suite.expect = cfg.expect.map(PathBuf::from);
    suite.abs_tol = cfg.abs_tol.unwrap_or(suite.abs_tol);
    suite.rel_tol = cfg.rel_tol.unwrap_or(suite.rel_tol);
    ensure!(
        [suite.abs_tol, suite.rel_tol]
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0),
        "許容誤差は有限の非負数"
    );
    Ok(suite)
}

fn read(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name)).ok()
}

struct CargoWorkspace {
    dir: PathBuf,
    members: Vec<glob::Pattern>,
    exclude: Vec<glob::Pattern>,
}

impl CargoWorkspace {
    fn contains(&self, dir: &Path) -> bool {
        let Ok(relative) = dir.strip_prefix(&self.dir) else {
            return false;
        };
        !self.exclude.iter().any(|p| p.matches_path(relative))
            && self.members.iter().any(|p| {
                p.matches_path_with(
                    relative,
                    glob::MatchOptions {
                        require_literal_separator: true,
                        ..glob::MatchOptions::new()
                    },
                )
            })
    }
}

fn keep_entry(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.depth() == 0
        || (!entry.file_type().is_symlink()
            && (!entry.file_type().is_dir()
                || (!name.starts_with('.')
                    && !matches!(
                        name.as_ref(),
                        "target"
                            | "node_modules"
                            | "__pycache__"
                            | "vendor"
                            | "venv"
                            | "build"
                            | "dist"
                    ))))
}

fn has_pytest(dir: &Path, python_tests: &BTreeSet<PathBuf>) -> bool {
    dir.join("pytest.ini").is_file()
        || read(dir, "setup.cfg")
            .is_some_and(|text| text.lines().any(|line| line.trim() == "[tool:pytest]"))
        || dir.join("pyproject.toml").is_file()
        || python_tests.contains(dir)
}

pub(super) fn discover(root: &Path) -> Result<Vec<Suite>> {
    let mut dirs = Vec::new();
    let mut python_tests = BTreeSet::new();
    let mut audit_files = Vec::new();
    for entry in WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(keep_entry)
        .filter_map(Result::ok)
    {
        if entry.file_type().is_dir() {
            dirs.push(entry.into_path());
        } else if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with("test_") && name.ends_with(".py") {
                for tests in entry
                    .path()
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| *dir != root)
                    .filter(|dir| dir.ends_with("tests"))
                {
                    if let Some(parent) = tests.parent() {
                        python_tests.insert(parent.to_path_buf());
                    }
                }
            }
            let name = name.to_ascii_lowercase();
            if name.ends_with(".lean")
                && (name.contains("audit") || name.contains("axiom"))
                && fs::read_to_string(entry.path())
                    .is_ok_and(|text| text.contains("#print axioms "))
            {
                audit_files.push(entry.into_path());
            }
        }
    }
    let mut suites = BTreeMap::new();
    let mut overridden = BTreeSet::new();
    let mut cargo_roots = Vec::new();
    let mut cargo_members = BTreeSet::new();
    let mut lake_projects = Vec::new();
    for dir in &dirs {
        let config = fuzukue_proto::compat::workspace_config(dir);
        if config.is_file() {
            let path = crate::safe_join(root, &config.to_string_lossy())?;
            let cfg: Config = toml::from_str(&fs::read_to_string(&path)?)
                .with_context(|| format!("{} のテスト設定を読めない", path.display()))?;
            if let Some(tests) = cfg.tests {
                overridden.insert(dir.clone());
                for (name, cfg) in tests {
                    let suite = configured(root, dir, &name, cfg)
                        .with_context(|| format!("{} のテスト {name}", path.display()))?;
                    overridden.insert(suite.cwd.clone());
                    ensure!(
                        suites.insert(suite.info.id.clone(), suite).is_none(),
                        "テスト名が重複: {name}"
                    );
                }
            }
        }
        if let Some(text) = read(dir, "Cargo.toml") {
            cargo_members.insert(dir.clone());
            if let Some(workspace) = text
                .parse::<toml::Table>()
                .ok()
                .and_then(|mut table| table.remove("workspace"))
            {
                let patterns = |key| {
                    workspace
                        .get(key)
                        .and_then(toml::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(toml::Value::as_str)
                        .filter_map(|p| {
                            glob::Pattern::new(p.trim_start_matches("./").trim_end_matches('/'))
                                .ok()
                        })
                        .collect()
                };
                cargo_roots.push(CargoWorkspace {
                    dir: dir.clone(),
                    members: patterns("members"),
                    exclude: patterns("exclude"),
                });
            }
        }
        if dir.join("lakefile.lean").is_file() || dir.join("lakefile.toml").is_file() {
            lake_projects.push(dir.clone());
        }
    }
    for dir in dirs.iter().filter(|dir| !overridden.contains(*dir)) {
        let rel = dir.strip_prefix(root)?;
        let mut add = |kind: &str, args: &[&str], audit: Option<&PathBuf>| -> Result<()> {
            let id = if rel.as_os_str().is_empty() {
                kind.into()
            } else {
                format!("{}:{kind}", rel.display())
            };
            let mut suite = Suite::new(
                root,
                dir,
                id.clone(),
                kind,
                args.iter().map(|s| (*s).into()).collect(),
            );
            suite.audit = audit.cloned();
            if let Some(audit) = audit {
                let file = audit.strip_prefix(dir)?.to_string_lossy();
                suite
                    .info
                    .command
                    .push_str(&format!(" && lake env lean {}", shlex::try_quote(&file)?));
            }
            suites.entry(id).or_insert(suite);
            Ok(())
        };
        if cargo_members.contains(dir)
            && !cargo_roots
                .iter()
                .any(|ws| ws.contains(dir) && !overridden.contains(&ws.dir))
        {
            if cargo_roots.iter().any(|ws| &ws.dir == dir) {
                add("cargo", &["cargo", "test", "--workspace"], None)?;
            } else {
                add("cargo", &["cargo", "test"], None)?;
            }
        }
        if has_pytest(dir, &python_tests) {
            add("pytest", &["pytest", "-rA"], None)?;
        }
        if dir.join("DESCRIPTION").is_file() {
            add("r", &["Rscript", "-e", "testthat::test_local()"], None)?;
        }
        if dir.join("Project.toml").is_file() {
            add(
                "julia",
                &["julia", "--project", "-e", "using Pkg; Pkg.test()"],
                None,
            )?;
        }
        if lake_projects.contains(dir) {
            let audit = audit_files.iter().find(|file| {
                lake_projects
                    .iter()
                    .rev()
                    .find(|project| file.starts_with(project))
                    == Some(&dir)
            });
            add("lean", &["lake", "build"], audit)?;
        }
        if read(dir, "Makefile").is_some_and(|text| {
            text.lines().any(|line| {
                !line.starts_with('\t')
                    && line.split_once(':').is_some_and(|(targets, rest)| {
                        !rest.trim_start().starts_with('=')
                            && targets.split_whitespace().any(|target| target == "test")
                    })
            })
        }) {
            add("make", &["make", "test"], None)?;
        }
        if read(dir, "package.json")
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|json| json["scripts"]["test"].as_str().map(str::to_owned))
            .is_some_and(|script| !script.trim().is_empty())
        {
            add("npm", &["npm", "test"], None)?;
        }
    }
    Ok(suites
        .into_values()
        .map(|mut suite| {
            suite.info.unavailable = suite.missing_runner();
            suite
        })
        .collect())
}
