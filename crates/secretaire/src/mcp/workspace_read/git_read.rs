//! Scoped Git queries with a fixed argument grammar and isolated repository configuration.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{DEADLINE, MAX_BYTES, ReadScope, history, paths};

pub(crate) const NAME: &str = "git_read";
const OPERATIONS: &[&str] = &[
    "rev-parse",
    "status",
    "log",
    "diff",
    "show",
    "ls-files",
    "ls-tree",
    "cat-file",
    "merge-base",
    "describe",
    "branch",
    "tag",
    "remote",
    "blame",
    "config",
];
const SAFE_KEYS: &[&str] = &[
    "core.filemode",
    "core.ignorecase",
    "core.symlinks",
    "core.autocrlf",
    "core.eol",
];

pub(in crate::mcp) fn definition() -> Value {
    json!({
        "name": NAME,
        "description": harness_text!(
            "Run a bounded read-only Git query in the assigned checkout. Pass options as separate strings. Use -- before repository-relative literal paths.",
            "割り当てられたチェックアウトで、上限付きの読み取り専用 Git 照会を行う。引数は一つずつ文字列で渡す。リポジトリ相対のリテラルパスの前には -- を置く。"
        ),
        "annotations": {"readOnlyHint": true},
        "inputSchema": {
            "type": "object",
            "properties": {
                "subcommand": {"type": "string", "enum": OPERATIONS},
                "args": {"type": "array", "maxItems": 18, "items": {"type": "string", "maxLength": 4096},
                    "description": "rev-parse [HEAD | --verify REV | --abbrev-ref REV | --show-toplevel | --is-inside-work-tree]; status [--porcelain]; log [-n COUNT] [REV]; diff [--cached | REV [REV]] [-- PATH ...]; show [REV] [-- PATH ...]; ls-files [-- PATH ...]; ls-tree [-r] REV [-- PATH ...]; cat-file -t|-s|-p REV; merge-base [--is-ancestor] REV REV; describe [--tags] [--always] [REV]; branch --list; tag --list; remote -v; blame -L START,END [REV] -- PATH; config --get core.filemode|core.ignorecase|core.symlinks|core.autocrlf|core.eol."}
            },
            "required": ["subcommand"],
            "additionalProperties": false
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    subcommand: String,
    #[serde(default)]
    args: Vec<String>,
}

fn revision(rev: &str) -> Result<&str> {
    let base = ["^{commit}", "^{tree}", "^{blob}", "^{tag}", "^{}"]
        .iter()
        .find_map(|suffix| rev.strip_suffix(suffix))
        .unwrap_or(rev);
    ensure!(
        base.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && rev.len() <= 256
            && !base.contains("..")
            && !base.split('/').any(|part| part.is_empty() || part == ".")
            && base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_+.-^~".contains(&b)),
        "Supply a revision such as HEAD, HEAD~1, a ref, or HEAD^{{tree}}"
    );
    Ok(rev)
}

fn literal_path(scope: &ReadScope, name: &str) -> Result<()> {
    ensure!(
        !name.starts_with(':') && paths::worktree_relative(Path::new(name)),
        "Supply a repository-relative literal path without parent components"
    );
    let absolute = scope.cwd.join(name);
    scope.check(&absolute)?;
    paths::check_git_path(&absolute, &scope.cwd)
}

fn number(value: &str, max: u32) -> Result<u32> {
    let n: u32 = value
        .parse()
        .context("Expected a positive decimal integer")?;
    ensure!(
        value.bytes().all(|b| b.is_ascii_digit()) && (1..=max).contains(&n),
        "Number must be between 1 and {max}"
    );
    Ok(n)
}

const PATCH_OPTIONS: &[&str] = &[
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--ignore-submodules=all",
];

impl Args {
    fn command(&self, scope: &ReadScope) -> Result<Vec<String>> {
        ensure!(
            self.args.len() <= 18 && self.args.iter().all(|arg| arg.len() <= 4096),
            "Too many Git arguments or an argument is too long"
        );
        let args: Vec<_> = self.args.iter().map(String::as_str).collect();
        let (options, paths) = args
            .iter()
            .position(|&arg| arg == "--")
            .map_or((args.as_slice(), &[][..]), |at| {
                (&args[..at], &args[at + 1..])
            });
        let with_paths = matches!(
            self.subcommand.as_str(),
            "diff" | "show" | "ls-files" | "ls-tree" | "blame"
        );
        ensure!(
            with_paths || args.len() == options.len(),
            "This Git query does not accept a path separator"
        );
        for path in paths {
            literal_path(scope, path)?;
        }
        let subcommand = self.subcommand.as_str();
        let command: Vec<&str> = match (subcommand, options) {
            ("rev-parse", ["--show-toplevel" | "--is-inside-work-tree"]) => options.to_vec(),
            ("rev-parse", ["--abbrev-ref", rev]) => vec![
                "--abbrev-ref",
                "--verify",
                "--end-of-options",
                revision(rev)?,
            ],
            ("rev-parse", [] | ["--verify", _] | [_]) => {
                let rev = options.last().copied().unwrap_or("HEAD");
                vec!["--verify", "--end-of-options", revision(rev)?]
            }
            ("status", [] | ["--porcelain" | "--porcelain=v1"]) => {
                vec![
                    "--porcelain=v1",
                    "--untracked-files=normal",
                    "--ignore-submodules=all",
                ]
            }
            ("log", [] | [_] | ["-n", _] | ["-n", _, _]) => {
                let (limit, rev) = match options {
                    ["-n", limit, tail @ ..] => {
                        number(limit, 100)?;
                        (*limit, tail.first().copied().unwrap_or("HEAD"))
                    }
                    _ => ("20", options.first().copied().unwrap_or("HEAD")),
                };
                vec![
                    "--no-notes",
                    "--no-show-signature",
                    "--no-patch",
                    "--format=%h %aI %s",
                    "-n",
                    limit,
                    revision(rev)?,
                    "--",
                ]
            }
            ("diff", [] | [_] | [_, _]) => {
                let mut command = PATCH_OPTIONS.to_vec();
                if options == ["--cached"] {
                    command.push("--cached");
                } else {
                    for rev in options {
                        command.push(revision(rev)?);
                    }
                }
                command
            }
            ("show", [] | [_]) => {
                let mut command = PATCH_OPTIONS.to_vec();
                command.extend([
                    "--no-notes",
                    "--no-show-signature",
                    "--format=medium",
                    revision(options.first().copied().unwrap_or("HEAD"))?,
                ]);
                command
            }
            ("ls-files", []) => vec![],
            ("ls-tree", [rev] | ["-r", rev]) => {
                let mut command = vec!["--full-tree"];
                if options.len() == 2 {
                    command.push("-r");
                }
                command.push(revision(rev)?);
                command
            }
            ("cat-file", [mode @ ("-t" | "-s" | "-p"), rev]) => vec![mode, revision(rev)?],
            ("merge-base", [left, right] | ["--is-ancestor", left, right]) => {
                let mut command = Vec::new();
                if options.len() == 3 {
                    command.push("--is-ancestor");
                }
                command.extend([revision(left)?, revision(right)?]);
                command
            }
            ("describe", options) => {
                let (tags, rest) = match options {
                    ["--tags", rest @ ..] => (true, rest),
                    _ => (false, options),
                };
                let (always, rest) = match rest {
                    ["--always", rest @ ..] => (true, rest),
                    _ => (false, rest),
                };
                ensure!(
                    rest.len() <= 1,
                    "Expected describe [--tags] [--always] [REV]"
                );
                let mut command = Vec::new();
                command.extend(tags.then_some("--tags"));
                command.extend(always.then_some("--always"));
                command.push(revision(rest.first().copied().unwrap_or("HEAD"))?);
                command
            }
            ("branch" | "tag", ["--list"]) => {
                vec!["--list", "--no-color", "--format=%(refname:short)"]
            }
            ("remote", ["-v"]) => vec![],
            ("blame", ["-L", lines] | ["-L", lines, _]) if paths.len() == 1 => {
                let (start, end) = lines.split_once(',').context("Use -L START,END")?;
                let start = number(start, u32::MAX)?;
                let end = number(end, u32::MAX)?;
                ensure!(
                    end >= start && end - start < 1000,
                    "Supply an inclusive range of at most 1,000 lines"
                );
                vec![
                    "--no-textconv",
                    "-L",
                    lines,
                    revision(options.get(2).copied().unwrap_or("HEAD"))?,
                ]
            }
            ("config", ["--get", key]) if SAFE_KEYS.contains(key) => vec![],
            _ => bail!("Unsupported subcommand or arguments for git_read. See the tool schema."),
        };
        let mut command: Vec<String> = std::iter::once(subcommand)
            .chain(command)
            .map(str::to_owned)
            .collect();
        if with_paths {
            command.push("--".into());
            command.extend(paths.iter().map(|path| (*path).into()));
        }
        Ok(command)
    }
}

pub(in crate::mcp) struct Git {
    cwd: PathBuf,
    pub(in crate::mcp) metadata: PathBuf,
    pub(in crate::mcp) config_root: PathBuf,
    pub(in crate::mcp) view: tempfile::TempDir,
}

impl Git {
    pub(in crate::mcp) fn open(workspace: &Path, cwd: &Path) -> Result<Self> {
        let (metadata, common) = history::read_metadata(cwd)?;
        if metadata != common && cwd != workspace {
            let (_, workspace_common) = history::read_metadata(workspace)?;
            ensure!(
                common == workspace_common,
                "Git metadata belongs to a different workspace"
            );
        }
        history::check_objects(&common)?;
        let view = tempfile::Builder::new()
            .prefix("fuzukue-git-read-")
            .tempdir()?;
        // Keep the real per-worktree index and HEAD. Supply a separate common directory
        // with only Git's data stores and a config built from safe structural settings.
        for (name, directory) in [
            ("objects", true),
            ("refs", true),
            ("reftable", true),
            ("packed-refs", false),
            ("shallow", false),
        ] {
            let source = common.join(name);
            if check_entry(&source, directory)? {
                symlink(source, view.path().join(name))?;
            }
        }
        for name in ["HEAD", "index"] {
            check_entry(&metadata.join(name), false)?;
        }
        check_entry(&common.join("objects/info"), true)?;
        Ok(Self {
            cwd: cwd.to_owned(),
            metadata,
            config_root: common,
            view,
        })
    }

    pub(in crate::mcp) async fn config(&self, pattern: &str) -> Result<Vec<(String, String)>> {
        let path = self.config_root.join("config");
        if !check_entry(&path, false)? {
            return Ok(Vec::new());
        }
        let mut command = git_command(&self.cwd);
        // The explicit config file is parsed without includes or repository setup.
        command
            .env("GIT_DIR", self.view.path())
            .args(["config", "--no-includes", "--null", "--file"])
            .arg(path)
            .args(["--get-regexp", pattern]);
        let result = output(command).await?;
        ensure!(
            !result.truncated,
            "Git configuration exceeded the output limit"
        );
        if result.exit_code == 1 && result.bytes.is_empty() {
            return Ok(Vec::new());
        }
        ensure!(
            result.exit_code == 0,
            "Git could not read the allowed configuration"
        );
        Ok(result
            .bytes
            .split(|&b| b == 0)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| std::str::from_utf8(entry).ok()?.split_once('\n'))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }

    pub(in crate::mcp) async fn prepare_config(&self) -> Result<()> {
        let config = self.config(REPO_CONFIG).await?;
        fs::write(self.view.path().join("config"), safe_config(&config))?;
        Ok(())
    }

    pub(in crate::mcp) fn command(&self) -> Command {
        self.command_for(&self.cwd, &self.metadata)
    }

    pub(in crate::mcp) fn command_for(&self, cwd: &Path, metadata: &Path) -> Command {
        let mut command = git_command(cwd);
        command
            .env("GIT_COMMON_DIR", self.view.path())
            .arg("--git-dir")
            .arg(metadata)
            .arg("--work-tree")
            .arg(cwd);
        command
    }
}

pub(in crate::mcp) fn check_entry(path: &Path, directory: bool) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => {
            let entry = paths::open(path)?;
            let meta = entry.metadata()?;
            ensure!(
                if directory {
                    meta.is_dir()
                } else {
                    meta.is_file()
                },
                "Unexpected Git metadata type"
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

const REPO_CONFIG: &str = "^(core\\.(repositoryformatversion|filemode|ignorecase|symlinks|autocrlf|eol)|extensions\\.(objectformat|refstorage))$";

fn normalized_value(key: &str, value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    let valid = match key {
        "core.repositoryformatversion" => matches!(value.as_str(), "0" | "1"),
        "extensions.objectformat" => matches!(value.as_str(), "sha1" | "sha256"),
        "extensions.refstorage" => matches!(value.as_str(), "files" | "reftable"),
        "core.eol" => matches!(value.as_str(), "lf" | "crlf" | "native"),
        "core.autocrlf" if value == "input" => true,
        _ => matches!(
            value.as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
        ),
    };
    valid.then_some(value)
}

fn safe_config(config: &[(String, String)]) -> String {
    let mut result = String::new();
    for (key, value) in config {
        if let Some(value) = normalized_value(key, value) {
            let (section, name) = key.split_once('.').unwrap();
            result.push_str(&format!("[{section}]\n\t{name} = {value}\n"));
        }
    }
    result
}

fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::env::split_paths(&path).filter(|dir| dir.is_absolute()))
        .unwrap_or_default();
    command
        .current_dir(cwd)
        .env_clear()
        .env(
            "PATH",
            if path.is_empty() {
                "/usr/bin:/bin".into()
            } else {
                path
            },
        )
        .envs([
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_TERMINAL_PROMPT", "0"),
        ])
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

pub(in crate::mcp) struct Output {
    pub(in crate::mcp) bytes: Vec<u8>,
    pub(in crate::mcp) truncated: bool,
    pub(in crate::mcp) exit_code: i32,
}

pub(in crate::mcp) async fn output(mut command: Command) -> Result<Output> {
    let mut child = command.spawn().context("Could not start Git")?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("Git stdout is unavailable")?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_BYTES {
        child.kill().await?;
        bytes.truncate(MAX_BYTES);
        return Ok(Output {
            bytes,
            truncated: true,
            exit_code: 0,
        });
    }
    let exit_code = child.wait().await?.code().context("Git was terminated")?;
    Ok(Output {
        bytes,
        truncated: false,
        exit_code,
    })
}

fn remote_text(config: &[(String, String)]) -> String {
    let mut remotes = std::collections::BTreeMap::<_, (Vec<&str>, Vec<&str>)>::new();
    for (key, value) in config {
        if let Some((name, field)) = key
            .strip_prefix("remote.")
            .and_then(|key| key.rsplit_once('.'))
        {
            let (fetch, push) = remotes.entry(name).or_default();
            match field {
                "url" => fetch.push(value),
                "pushurl" => push.push(value),
                _ => {}
            }
        }
    }
    let mut text = String::new();
    for (name, (fetch, push)) in remotes {
        // URL paths can contain credentials too. Publish only a recognized scheme and host.
        let endpoint = |value: &str| -> String {
            url::Url::parse(value)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https" | "ssh" | "git"))
                .and_then(|url| Some(format!("{}://{}", url.scheme(), url.host_str()?)))
                .unwrap_or_else(|| "[redacted]".into())
        };
        if let Some(url) = fetch.first() {
            text.push_str(&format!("{name}\t{} (fetch)\n", endpoint(url)));
        }
        for url in if push.is_empty() { fetch } else { push } {
            text.push_str(&format!("{name}\t{} (push)\n", endpoint(url)));
        }
    }
    text
}

pub(in crate::mcp) async fn call(scope: ReadScope, args: &Value) -> Result<Value> {
    let args: Args = super::super::parse_args(args)?;
    let command = args.command(&scope)?;
    tokio::time::timeout(DEADLINE, async {
        let git = Git::open(&scope.workspace, &scope.cwd)?;
        let subcommand = args.subcommand.as_str();
        let (mut text, mut truncated, exit_code) = match subcommand {
            "remote" => {
                let config = git.config("^remote\\..*\\.(url|pushurl)$").await?;
                (remote_text(&config), false, 0)
            }
            "config" => {
                let key = &args.args[1];
                let config = git.config(REPO_CONFIG).await?;
                match config.iter().rev().find(|(name, _)| name == key) {
                    Some((_, value)) => {
                        ensure!(normalized_value(key, value).is_some(), "Git configuration value is invalid for this key");
                        (format!("{value}\n"), false, 0)
                    }
                    None => (String::new(), false, 1),
                }
            }
            _ => {
                git.prepare_config().await?;
                let mut process = git.command();
                process.args(&command);
                let result = output(process).await?;
                ensure!(result.exit_code == 0 || subcommand == "merge-base" && result.exit_code == 1,
                    "Git {subcommand} exited with status {}", result.exit_code);
                (String::from_utf8_lossy(&result.bytes).into_owned(), result.truncated, result.exit_code)
            }
        };
        if text.len() > MAX_BYTES {
            let end = (0..=MAX_BYTES).rev().find(|&at| text.is_char_boundary(at)).unwrap();
            text.truncate(end);
            truncated = true;
        }
        Ok(json!({"subcommand": subcommand, "text": text, "truncated": truncated, "exit_code": exit_code}))
    }).await.context("Git read timed out")?
}

#[cfg(test)]
pub(in crate::mcp) mod tests;
