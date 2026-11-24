//! Read-only workspace MCP tools. Every caller supplies its own filesystem scope.

pub(crate) mod git_read;
mod history;
mod paths;
mod plan_status;
pub(super) use paths::{open as open_no_symlinks, secret as secret_name};
mod review;
mod search;
#[cfg(test)]
mod tests;

pub(crate) use paths::ReadScope;
pub(super) use history::read_metadata as git_metadata;
pub(super) use paths::{check_git_path, worktree_relative};
pub(crate) use review::{ReviewLease, claude_config as review_claude_config, is_review_launch};
pub(super) use review::{dispatch as dispatch_review, is_review};

use anyhow::{Result, ensure};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Open a checked source path without following links.
pub(crate) fn open_regular_source(path: &Path) -> Result<std::fs::File> {
    let file = paths::open(path)?;
    paths::regular(&file)?;
    Ok(file)
}

const MAX_BYTES: usize = 64 * 1024;
const READ_SCAN_BYTES: usize = 16 * 1024 * 1024;
const LINE_WINDOW: usize = 400;
const DEADLINE: Duration = Duration::from_secs(5);
const TRUNCATED: &str = "… [truncated]";

pub(crate) const GUIDANCE: &str = "\n\nFor reading, listing and searching files use the fuzukue fs_read/fs_list/fs_search/git_show tools instead of sed/ls/rg/cat in the shell.";

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Read,
    List,
    Search,
    GitShow,
    PlanStatus,
}

impl Tool {
    pub(crate) const ALL: [Self; 5] = [Self::Read, Self::List, Self::Search, Self::GitShow, Self::PlanStatus];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "fs_read",
            Self::List => "fs_list",
            Self::Search => "fs_search",
            Self::GitShow => "git_show",
            Self::PlanStatus => "plan_status",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(super) fn definition(self) -> Value {
        let scoped_path = harness_text!(
            "Relative to the assigned working directory, or absolute within an allowed read root.",
            "割り当てられた作業ディレクトリからの相対パス、または読み取り範囲内の絶対パス。"
        );
        let (description, properties, required): (_, _, &[&str]) = match self {
            Self::Read => (
                harness_text!(
                    "Read numbered UTF-8 lines in the workspace scope. Line ranges are one-based and inclusive.",
                    "作業範囲内の UTF-8 テキストを行番号付きで読む。行範囲は 1 始まりで両端を含む。"
                ),
                json!({"path": {"type": "string", "description": scoped_path}, "start_line": {"type": "integer", "minimum": 1, "default": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_BYTES, "default": MAX_BYTES}}),
                &["path"],
            ),
            Self::List => (
                harness_text!(
                    "List workspace entries without following symlinks. Globs match paths relative to the requested directory.",
                    "作業範囲内の項目を一覧する。シンボリックリンクはたどらない。glob は起点ディレクトリからの相対パスに適用する。"
                ),
                json!({"path": {"type": "string", "default": ".", "description": scoped_path}, "depth": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1},
                    "glob": {"type": "string"}, "include_hidden": {"type": "boolean", "default": false},
                    "include_ignored": {"type": "boolean", "default": false,
                        "description": harness_text!("Include target, node_modules, and the .git entry. Git metadata stays protected.",
                            "target、node_modules、.git の項目も含める。Git の内部データは保護される。")}}),
                &[],
            ),
            Self::Search => (
                harness_text!(
                    "Search scoped UTF-8 files with a single-line regex or literal pattern. Hidden and generated directories are skipped during traversal.",
                    "作業範囲内の UTF-8 ファイルを一行の正規表現または文字列で検索する。再帰検索では隠し項目と生成先ディレクトリを除く。"
                ),
                json!({"pattern": {"type": "string", "maxLength": 4096}, "path": {"type": "string", "default": ".", "description": scoped_path},
                    "glob": {"type": "string"}, "fixed_string": {"type": "boolean", "default": false},
                    "ignore_case": {"type": "boolean", "default": false}, "context": {"type": "integer", "minimum": 0, "maximum": 5, "default": 0},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "default": 100}}),
                &["pattern"],
            ),
            Self::PlanStatus => return plan_status::definition(self.name()),
            Self::GitShow => (
                harness_text!(
                    "Read a commit or a UTF-8 file at that commit. For an external folder, supply the absolute repository path; the file path is relative to that repository.",
                    "コミットの情報、またはその版の UTF-8 ファイル一件を読む。外部のフォルダーにはリポジトリの絶対パスを指定する。ファイルのパスはリポジトリからの相対パス。"
                ),
                json!({"rev": {"type": "string"}, "path": {"type": "string"}, "repository": {"type": "string"}}),
                &["rev"],
            ),
        };
        json!({"name": self.name(), "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required, "additionalProperties": false,
        }})
    }

    pub(super) async fn call(self, scope: ReadScope, args: &Value) -> Result<Value> {
        match self {
            Self::Read => {
                let args = super::parse_args(args)?;
                tokio::task::spawn_blocking(move || read(&scope, args)).await?
            }
            Self::List => {
                let args = super::parse_args(args)?;
                tokio::task::spawn_blocking(move || list(&scope, args)).await?
            }
            Self::Search => search::search(scope, super::parse_args(args)?).await,
            Self::GitShow => history::show(scope, super::parse_args(args)?).await,
            Self::PlanStatus => {
                let args = super::parse_args(args)?;
                tokio::task::spawn_blocking(move || plan_status::status(&scope, args)).await?
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: PathBuf,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

fn read(scope: &ReadScope, args: ReadArgs) -> Result<Value> {
    let start = args.start_line.unwrap_or(1);
    let end = args
        .end_line
        .unwrap_or_else(|| start.saturating_add(LINE_WINDOW - 1));
    let max_bytes = args.max_bytes.unwrap_or(MAX_BYTES);
    ensure!(
        start > 0 && end >= start,
        "Use a positive and increasing line range"
    );
    ensure!(
        (1..=MAX_BYTES).contains(&max_bytes),
        "max_bytes must be between 1 and {MAX_BYTES}"
    );
    let path = scope.resolve(&args.path)?;
    paths::media(&args.path)?;
    paths::media(&path)?;
    let file = paths::open(&path)?;
    paths::regular(&file)?;
    let mut bytes = Vec::new();
    file.take((READ_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let scan_capped = bytes.len() > READ_SCAN_BYTES;
    if scan_capped {
        bytes.truncate(READ_SCAN_BYTES);
        // The final code point may cross the scan boundary.
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    let text = paths::text(&bytes)?;
    let mut content = String::new();
    let mut used = 0;
    let mut next = None;
    let mut clipped_line = false;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if number < start {
            continue;
        }
        if number > end || index - (start - 1) >= LINE_WINDOW {
            next = Some(number);
            break;
        }
        let width = line.len() + 1;
        if width > max_bytes - used {
            if used == 0 {
                let prefix = utf8_prefix(line, max_bytes);
                content.push_str(&format!("{number}: {prefix}\n"));
                clipped_line = true;
                // Resume at the next complete line, if the file has one.
                next =
                    Some(number + 1).filter(|_| text.lines().nth(number).is_some() || scan_capped);
            } else {
                next = Some(number);
            }
            break;
        }
        content.push_str(&format!("{number}: {line}\n"));
        used += width;
    }
    let truncated = next.is_some() || scan_capped || clipped_line;
    if truncated {
        content.push_str(TRUNCATED);
        if clipped_line {
            content.push_str(" The displayed line was clipped.");
        }
        if let Some(line) = next {
            content.push_str(&format!(" Continue with start_line={line}."));
        } else if scan_capped {
            content.push_str(" Reached the 16 MiB file scan limit.");
        }
    }
    Ok(json!({"path": args.path, "text": content, "truncated": truncated, "next_start_line": next}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "dot")]
    path: PathBuf,
    depth: Option<i64>,
    glob: Option<String>,
    #[serde(default)]
    include_hidden: bool,
    #[serde(default)]
    include_ignored: bool,
}

fn list(scope: &ReadScope, args: ListArgs) -> Result<Value> {
    let depth = args.depth.unwrap_or(1).clamp(1, 3) as usize;
    let path = scope.resolve(&args.path)?;
    let file = paths::open(&path)?;
    ensure!(file.metadata()?.is_dir(), "Select a directory to list");
    let glob = glob(args.glob.as_deref())?;
    let mut scan = paths::Scan::new(
        scope,
        depth,
        args.include_hidden,
        args.include_ignored,
        Instant::now() + DEADLINE,
    );
    let mut entries = Vec::new();
    let mut output_bytes = 0;
    let complete = scan.visit(&path, &file, &mut |entry| {
        let relative = entry
            .path
            .strip_prefix(&path)
            .expect("scan returns descendants");
        if matches_glob(glob.as_ref(), relative) {
            let data =
                json!({"path": relative.to_string_lossy(), "type": entry.kind, "size": entry.size});
            output_bytes += data.to_string().len();
            if entries.len() >= 500 || output_bytes > MAX_BYTES {
                return false;
            }
            entries.push(data);
        }
        true
    })?;
    Ok(
        json!({"path": args.path, "entries": entries, "truncated": !complete,
        "note": (!complete).then_some("… [truncated] Narrow path or glob to see more entries.")}),
    )
}

fn dot() -> PathBuf {
    ".".into()
}

fn glob(pattern: Option<&str>) -> Result<Option<GlobMatcher>> {
    pattern
        .map(|pattern| {
            ensure!(pattern.len() <= 1024, "glob is too long");
            Ok(GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()?
                .compile_matcher())
        })
        .transpose()
}

fn matches_glob(glob: Option<&GlobMatcher>, path: &Path) -> bool {
    glob.is_none_or(|glob| {
        glob.is_match(path) || path.file_name().is_some_and(|name| glob.is_match(name))
    })
}

fn utf8_prefix(text: &str, limit: usize) -> &str {
    &text[..text.floor_char_boundary(limit.min(text.len()))]
}
