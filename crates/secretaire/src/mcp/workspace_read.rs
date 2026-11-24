//! Read-only workspace MCP tools. Every caller supplies its own filesystem scope.

pub(crate) mod git_read;
mod history;
mod paths;
mod plan_status;
pub(super) mod range;
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

use self::range::{IntegerRange, optional_integer};

/// Open a checked source path without following links.
pub(crate) fn open_regular_source(path: &Path) -> Result<std::fs::File> {
    let file = paths::open(path)?;
    paths::regular(&file)?;
    Ok(file)
}

const MAX_BYTES: usize = 64 * 1024;
const READ_SCAN_BYTES: usize = 16 * 1024 * 1024;
const LINE_WINDOW: usize = 400;
const DEFAULT_START_LINE: usize = 1;
const DEFAULT_DEPTH: usize = 1;
// At least one scanned byte is needed per line.
const READ_LINE: IntegerRange = IntegerRange::new(1, READ_SCAN_BYTES);
const TAIL_LINES: IntegerRange = IntegerRange::new(1, LINE_WINDOW);
const READ_BYTES: IntegerRange = IntegerRange::new(1, MAX_BYTES);
const LIST_DEPTH: IntegerRange = IntegerRange::new(1, 3);
const DEADLINE: Duration = Duration::from_secs(5);
const TRUNCATED: &str = "… [truncated]";

pub(crate) const GUIDANCE: &str = "\n\nUse fuzukue fs_read (start_line/end_line or tail_lines), fs_list, fs_search, and git_show instead of head/tail/sed/cat, ls/find, rg/grep, and git show in the shell.";

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
            Self::Read => {
                let mut end_line = READ_LINE.schema();
                let mut start_line = READ_LINE.property(DEFAULT_START_LINE);
                start_line["minimum"] = json!(-(READ_SCAN_BYTES as i64));
                start_line["description"] = json!(harness_text!(
                    "One-based start. Use -1 for the last line or -20 to start 20 lines from the end. Zero is treated as 1. Integers outside the range are clamped to the nearest bound.",
                    "1 始まりの開始行。-1 は最終行、-20 は末尾から数えて 20 行目。0 は 1 として扱う。範囲外の整数は最寄りの境界値に丸める。"
                ));
                let mut tail_lines = TAIL_LINES.schema();
                tail_lines["description"] = json!(harness_text!(
                    "Read the last N lines. Omit to use the default head or a line range. Do not combine with start_line or end_line. Integers outside the range are clamped to the nearest bound.",
                    "末尾の N 行を読む。省略時は先頭または指定の行範囲を読む。start_line、end_line と併用できない。範囲外の整数は最寄りの境界値に丸める。"
                ));
                let offset = LINE_WINDOW - 1;
                end_line["description"] = json!(harness_format!(
                    "Defaults to the resolved start line plus {}, capped at the maximum. Integers outside the range are clamped to the nearest bound. The resulting end_line must be at least the resolved start line.",
                    "省略時は解決後の開始行に {} を足した値を上限で抑える。範囲外の整数は最寄りの境界値に丸める。丸めた後の end_line は解決後の開始行以上とする。",
                    offset
                ));
                (
                    harness_text!(
                        "Use instead of head, tail, sed -n, or cat. Pass path alone for a numbered preview of the first 400 UTF-8 lines, end_line=20 for the first 20, start_line=40 and end_line=60 for an inclusive range, or tail_lines=20 for the last 20. Returns total_lines when the file fits the 16 MiB scan limit, which is required for end-relative reads.",
                        "head、tail、sed -n、cat の代わりに使う。path だけで先頭から最大 400 行の UTF-8 テキストを行番号付きで読み、end_line=20 で先頭 20 行、start_line=40 と end_line=60 で両端を含む範囲、tail_lines=20 で末尾 20 行を読む。16 MiB の走査上限内で全体を読めた場合は total_lines を返す。末尾からの指定には全体の読み取りが必要。"
                    ),
                    json!({"path": {"type": "string", "description": scoped_path},
                        "start_line": start_line, "end_line": end_line, "tail_lines": tail_lines,
                        "max_bytes": READ_BYTES.property(MAX_BYTES)}),
                    &["path"],
                )
            }
            Self::List => (
                harness_text!(
                    "Use instead of ls or a shallow find. Lists paths, types, sizes in bytes, and mtimes in Unix seconds without following symlinks. Try path=\"src\", depth=2, glob=\"*.rs\" to find Rust files.",
                    "ls や浅い find の代わりに使う。パス、種類、バイト数、更新時刻（Unix 秒）を一覧する。シンボリックリンクはたどらない。たとえば path=\"src\"、depth=2、glob=\"*.rs\" で Rust ファイルを探す。"
                ),
                json!({"path": {"type": "string", "default": ".", "description": scoped_path}, "depth": LIST_DEPTH.property(DEFAULT_DEPTH),
                    "glob": {"type": "string"}, "include_hidden": {"type": "boolean", "default": false},
                    "include_ignored": {"type": "boolean", "default": false,
                        "description": harness_text!("Include target, node_modules, and the .git entry. Git metadata stays protected.",
                            "target、node_modules、.git の項目も含める。Git の内部データは保護される。")}}),
                &[],
            ),
            Self::Search => (
                harness_text!(
                    "Use instead of rg or grep for a single-line regex or literal search in scoped UTF-8 files. Try pattern=\"TODO\", glob=\"*.rs\", context=2 (rg -C 2). Use output_mode=files_with_matches for rg -l or output_mode=count for rg -c. Traversal skips hidden and generated directories.",
                    "rg や grep の代わりに使い、作業範囲内の UTF-8 ファイルを一行の正規表現または文字列で検索する。たとえば pattern=\"TODO\"、glob=\"*.rs\"、context=2 は rg -C 2 に相当する。rg -l は output_mode=files_with_matches、rg -c は output_mode=count を指定する。再帰検索では隠し項目と生成先ディレクトリを除く。"
                ),
                json!({"pattern": {"type": "string", "maxLength": 4096}, "path": {"type": "string", "default": ".", "description": scoped_path},
                    "glob": {"type": "string"}, "fixed_string": {"type": "boolean", "default": false},
                    "ignore_case": {"type": "boolean", "default": false}, "context": search::CONTEXT.property(search::DEFAULT_CONTEXT),
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "default": "content",
                        "description": harness_text!(
                            "Content returns matching lines and context. Files with matches returns paths once each. Count returns matching-line counts per matching file. max_results caps lines in content mode and files in the other modes.",
                            "content は一致行と前後の行、files_with_matches は重複のないパス、count は一致したファイルごとの一致行数を返す。max_results は content では行数、他のモードではファイル数を制限する。"
                        )},
                    "max_results": search::RESULTS.property(search::DEFAULT_RESULTS)}),
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
    #[serde(default, deserialize_with = "optional_integer")]
    start_line: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    end_line: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    tail_lines: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    max_bytes: Option<i128>,
}

fn read(scope: &ReadScope, args: ReadArgs) -> Result<Value> {
    let from_end = args.tail_lines.is_some() || args.start_line.is_some_and(|start| start < 0);
    let max_bytes = READ_BYTES.resolve(args.max_bytes, MAX_BYTES);
    ensure!(
        args.tail_lines.is_none() || (args.start_line.is_none() && args.end_line.is_none()),
        "Use fs_read tail_lines on its own, or specify start_line and optionally end_line"
    );
    let path = scope.resolve(&args.path)?;
    paths::media(&args.path)?;
    paths::media(&path)?;
    let file = paths::open(&path)?;
    ensure!(!file.metadata()?.is_dir(), "Use fs_list to list a directory");
    paths::regular(&file)?;
    let mut bytes = Vec::new();
    file.take((READ_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let scan_capped = bytes.len() > READ_SCAN_BYTES;
    ensure!(
        !from_end || !scan_capped,
        "fs_read cannot locate the end of a file beyond its 16 MiB scan limit. Use a positive start_line to read within the scanned prefix."
    );
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
    let total_lines = (!scan_capped).then(|| text.lines().count());
    let start = if from_end {
        let offset = match args.tail_lines {
            Some(lines) => TAIL_LINES.clamp(lines),
            None => READ_LINE.clamp(-args.start_line.unwrap_or_default()),
        };
        total_lines.unwrap_or_default().saturating_sub(offset) + 1
    } else {
        READ_LINE.resolve(args.start_line, DEFAULT_START_LINE)
    };
    let end = READ_LINE.resolve(args.end_line, start + LINE_WINDOW - 1);
    ensure!(
        end >= start,
        "end_line must be at least start_line after clamping and resolving any end-relative offset"
    );
    let mut content = String::new();
    let mut used = 0;
    let mut next = None;
    let mut clipped_line = false;
    for (index, line) in text.lines().enumerate().skip(start - 1) {
        let number = index + 1;
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
                next = Some(number + 1).filter(|&next| {
                    next <= READ_SCAN_BYTES && (scan_capped || total_lines.is_some_and(|total| next <= total))
                });
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
    Ok(json!({"path": args.path, "text": content, "truncated": truncated,
        "next_start_line": next, "total_lines": total_lines}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "dot")]
    path: PathBuf,
    #[serde(default, deserialize_with = "optional_integer")]
    depth: Option<i128>,
    glob: Option<String>,
    #[serde(default)]
    include_hidden: bool,
    #[serde(default)]
    include_ignored: bool,
}

fn list(scope: &ReadScope, args: ListArgs) -> Result<Value> {
    let depth = LIST_DEPTH.resolve(args.depth, DEFAULT_DEPTH);
    let path = scope.resolve(&args.path)?;
    let file = paths::open(&path)?;
    ensure!(file.metadata()?.is_dir(), "Use fs_read to read a UTF-8 file. fs_list expects a directory.");
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
            let data = json!({"path": relative.to_string_lossy(), "type": entry.kind,
                "size": entry.size, "mtime_unix_secs": entry.mtime_unix_secs});
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
