mod filters;
mod matcher;
mod schema;

pub(super) use schema::definition;

use super::range::{optional_integer, IntegerRange};
use super::{dot, paths, utf8_prefix, ReadScope, DEADLINE, MAX_BYTES, TRUNCATED};
use anyhow::{ensure, Context, Result};
use filters::Filters;
use matcher::{FileText, Hit, Matcher};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::AsyncReadExt;

const MAX_PATHS: usize = 16;
const FILE_BYTES: usize = 1024 * 1024;
const SEARCH_BYTES: usize = 32 * FILE_BYTES;
const MATCH_TEXT_BYTES: usize = 1000;
const MATCH_WINDOW_LINES: usize = 32;
const DEFAULT_CONTEXT: usize = 0;
const DEFAULT_RESULTS: usize = 100;
const CONTEXT: IntegerRange = IntegerRange::new(0, 5);
const RESULTS: IntegerRange = IntegerRange::new(1, 200);
const COUNT: IntegerRange = IntegerRange::new(1, 100_000);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    #[serde(default, deserialize_with = "optional_string")]
    pattern: Option<String>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default, deserialize_with = "optional_string")]
    path: Option<String>,
    #[serde(default)]
    paths: Vec<PathBuf>,
    glob: Option<String>,
    #[serde(default)]
    globs: Vec<String>,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    exclude_types: Vec<String>,
    #[serde(default)]
    fixed_string: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    smart_case: bool,
    #[serde(default)]
    word: bool,
    #[serde(default)]
    whole_line: bool,
    #[serde(default)]
    invert: bool,
    #[serde(default)]
    only_matching: bool,
    #[serde(default)]
    multiline: bool,
    #[serde(default)]
    include_hidden: bool,
    #[serde(default)]
    include_ignored: bool,
    #[serde(default = "enabled")]
    line_number: bool,
    #[serde(default)]
    column: bool,
    #[serde(default = "enabled")]
    with_path: bool,
    #[serde(default)]
    stats: bool,
    #[serde(default, deserialize_with = "optional_integer")]
    context: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    before_context: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    after_context: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    max_results: Option<i128>,
    #[serde(default, deserialize_with = "optional_integer")]
    max_count: Option<i128>,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    sort: Sort,
}

fn enabled() -> bool {
    true
}

fn optional_string<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<String>, D::Error> {
    String::deserialize(value).map(Some)
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    FilesWithoutMatches,
    Count,
    Files,
}

impl OutputMode {
    fn result_key(self) -> &'static str {
        match self {
            Self::Content => "matches",
            Self::Count => "counts",
            Self::Files | Self::FilesWithMatches | Self::FilesWithoutMatches => "files",
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sort {
    #[default]
    Path,
    Modified,
}

struct Candidate {
    path: PathBuf,
    label: PathBuf,
    modified: (i64, u32),
}

fn candidates(
    scope: &ReadScope,
    args: &Args,
    filters: &Filters,
    root: &Path,
    base: PathBuf,
    handle: File,
    qualify: bool,
    deadline: Instant,
) -> Result<(Vec<Candidate>, Option<File>, bool)> {
    let mut candidates = Vec::new();
    if !handle.metadata()?.is_dir() {
        if filters.matches(Path::new(
            base.file_name().context("path has no file name")?,
        )) {
            paths::regular(&handle)?;
            if args.output_mode != OutputMode::Files {
                paths::media(root)?;
                paths::media(&base)?;
            }
            candidates.push(Candidate {
                path: base,
                label: root.to_path_buf(),
                modified: (0, 0),
            });
        }
        return Ok((candidates, Some(handle), true));
    }

    let mut scan =
        paths::Scan::for_search(scope, args.include_hidden, args.include_ignored, deadline);
    let complete = scan.visit(&base, &handle, &mut |entry| {
        let relative = entry
            .path
            .strip_prefix(&base)
            .expect("scan returns descendants");
        if entry.kind == "file" && filters.matches(relative) {
            let label = if qualify { root.join(relative) } else { relative.into() };
            candidates.push(Candidate {
                path: entry.path,
                label,
                modified: (entry.mtime_unix_secs, entry.mtime_nanos),
            });
        }
        true
    })?;
    candidates.sort_unstable_by(|a, b| match args.sort {
        Sort::Path => a.label.as_os_str().cmp(b.label.as_os_str()),
        Sort::Modified => a
            .modified
            .cmp(&b.modified)
            .then_with(|| a.label.as_os_str().cmp(b.label.as_os_str())),
    });
    Ok((candidates, None, complete))
}

enum FileRead {
    Skipped,
    BudgetExceeded,
    Bytes(Vec<u8>),
}

fn read_file(file: &File, scanned: &mut usize) -> Result<FileRead> {
    let remaining = SEARCH_BYTES - *scanned;
    let size = paths::regular(file)?.len();
    if size > FILE_BYTES as u64 {
        return Ok(FileRead::Skipped);
    }
    if size > remaining as u64 {
        return Ok(FileRead::BudgetExceeded);
    }
    let mut bytes = Vec::new();
    let read = file
        .take((FILE_BYTES + 1).min(remaining) as u64)
        .read_to_end(&mut bytes);
    *scanned += bytes.len();
    read?;
    if bytes.len() == remaining && file.metadata()?.len() > remaining as u64 {
        return Ok(FileRead::BudgetExceeded);
    }
    if bytes.len() > FILE_BYTES {
        return Ok(FileRead::Skipped);
    }
    Ok(FileRead::Bytes(bytes))
}

#[derive(Default, Serialize)]
struct Stats {
    files_searched: usize,
    files_matched: usize,
    bytes_scanned: usize,
    elapsed_ms: u128,
}

struct Results {
    rows: Vec<Value>,
    bytes: usize,
    limit: usize,
    root_start: usize,
    full: bool,
    truncated: bool,
}

impl Results {
    fn push(&mut self, make_row: impl FnOnce() -> Value) {
        if self.full || self.rows.len() - self.root_start == self.limit {
            self.full = true;
            self.truncated = true;
            return;
        }
        let row = make_row();
        let bytes = row.to_string().len() + 1;
        if bytes > MAX_BYTES - self.bytes {
            self.full = true;
            self.truncated = true;
        } else {
            self.bytes += bytes;
            self.rows.push(row);
        }
    }
}

struct Content {
    before: usize,
    after: usize,
    line_number: bool,
    column: bool,
    with_path: bool,
    only_matching: bool,
    multiline: bool,
}

impl Content {
    fn new(args: &Args) -> Self {
        let context = CONTEXT.resolve(args.context, DEFAULT_CONTEXT);
        Self {
            before: CONTEXT.resolve(args.before_context, context),
            after: CONTEXT.resolve(args.after_context, context),
            line_number: args.line_number,
            column: args.column,
            with_path: args.with_path,
            only_matching: args.only_matching,
            multiline: args.multiline,
        }
    }

    fn push(&self, output: &mut Results, path: &Path, hit: Hit<'_>, lines: &[&str]) {
        let mut clipped = false;
        output.push(|| {
            let mut row = json!({});
            if self.with_path {
                row["path"] = json!(path.to_string_lossy());
            }
            if self.line_number {
                row["line"] = json!(hit.first + 1);
                if self.multiline {
                    row["end_line"] = json!(hit.last + 1);
                }
            }
            if self.column {
                row["column"] = json!(hit.column);
            }
            if self.only_matching {
                let text = hit.text.expect("a substring hit");
                let prefix = utf8_prefix(text, MATCH_TEXT_BYTES);
                clipped = prefix.len() < text.len();
                row["text"] = json!(prefix);
                row["truncated"] = json!(clipped);
            } else {
                let end = (hit.last + 1 + self.after).min(lines.len());
                let visible_end = (hit.first + MATCH_WINDOW_LINES).min(hit.last + 1);
                clipped = visible_end <= hit.last;
                let window: Vec<_> = (hit.first.saturating_sub(self.before)..visible_end)
                    .chain(hit.last + 1..end)
                    .map(|i| {
                        let text = lines[i];
                        let prefix = utf8_prefix(text, MATCH_TEXT_BYTES);
                        let line_clipped = prefix.len() < text.len();
                        clipped |= line_clipped;
                        let mut row = json!({"text": prefix, "truncated": line_clipped});
                        if self.line_number {
                            row["line"] = json!(i + 1);
                        }
                        row
                    })
                    .collect();
                row["context"] = json!(window);
                if self.multiline {
                    row["truncated"] = json!(clipped);
                }
            }
            row
        });
        output.truncated |= clipped;
    }
}

pub(super) async fn search(scope: ReadScope, input: &Value) -> Result<Value> {
    let args = super::parse_named_args(input).context(
        "fs_search takes named arguments. Pass option-like search text in pattern. The tool description maps grep flags to arguments",
    )?;
    tokio::task::spawn_blocking(move || run(&scope, &args)).await?
}

fn run(scope: &ReadScope, args: &Args) -> Result<Value> {
    let start = Instant::now();
    let deadline = start + DEADLINE;
    let filters = Filters::new(args)?;
    let matcher = if args.output_mode == OutputMode::Files {
        ensure!(
            args.pattern.is_none() && args.patterns.is_empty(),
            "Omit pattern and patterns in output_mode=files, or use output_mode=files_with_matches"
        );
        None
    } else {
        Some(Matcher::new(args)?)
    };
    ensure!(args.paths.len() <= MAX_PATHS, "paths accepts at most {MAX_PATHS} entries");
    let mut roots: Vec<_> = args.path.iter().map(PathBuf::from)
        .chain(args.paths.iter().cloned())
        .collect();
    if roots.is_empty() {
        roots.push(dot());
    }
    // Check every explicit root before scanning so truncation cannot hide an invalid path.
    let opened = roots.iter().map(|root| {
        let base = scope.resolve_checked(root, false)
            .with_context(|| format!("Invalid path {}", root.display()))?;
        let handle = paths::open(&base)
            .with_context(|| format!("Could not open path {}", root.display()))?;
        Ok((base, handle))
    }).collect::<Result<Vec<_>>>()?;
    let mut output = Results {
        rows: Vec::new(),
        bytes: 0,
        limit: RESULTS.resolve(args.max_results, DEFAULT_RESULTS),
        root_start: 0,
        full: false,
        truncated: false,
    };
    let content = Content::new(args);
    let max_count = args
        .max_count
        .map_or(usize::MAX, |count| COUNT.clamp(count));
    let mut stats = Stats::default();
    for (root, (base, handle)) in roots.iter().zip(opened) {
        if Instant::now() >= deadline {
            output.truncated = true;
            break;
        }
        scope.audit_read(&base)
            .with_context(|| format!("Invalid path {}", root.display()))?;
        let (candidates, direct, complete) =
            candidates(scope, args, &filters, root, base, handle, !args.paths.is_empty(), deadline)
                .with_context(|| format!("Could not search path {}", root.display()))?;
        output.truncated |= !complete;
        output.root_start = output.rows.len();
        output.full = false;
        for candidate in candidates {
            if Instant::now() >= deadline {
                output.truncated = true;
                break;
            }
            let label = candidate.label.to_string_lossy();
            let Some(matcher) = &matcher else {
                output.push(|| json!(label));
                if output.full {
                    break;
                }
                continue;
            };
            if paths::media(&candidate.path).is_err() {
                continue;
            }
            let read = if let Some(file) = &direct {
                read_file(file, &mut stats.bytes_scanned).context("Could not read path")?
            } else {
                match scope
                    .audit_read(&candidate.path)
                    .and_then(|()| paths::open(&candidate.path))
                    .and_then(|file| read_file(&file, &mut stats.bytes_scanned))
                {
                    Ok(read) => read,
                    Err(_) => {
                        output.truncated = true;
                        continue;
                    }
                }
            };
            let bytes = match read {
                FileRead::Skipped => continue,
                FileRead::BudgetExceeded => {
                    output.truncated = true;
                    break;
                }
                FileRead::Bytes(bytes) => bytes,
            };
            let Ok(text) = paths::text(&bytes) else {
                continue;
            };
            stats.files_searched += 1;
            let file = FileText::new(text);
            let mut count = 0;
            let mut last_counted = None;
            let complete = matcher.visit(&file, deadline, |hit| {
                if last_counted.is_none_or(|last| last < hit.first) {
                    if count == max_count {
                        output.truncated = true;
                        return false;
                    }
                    count += 1;
                }
                last_counted = Some(last_counted.map_or(hit.last, |last| hit.last.max(last)));
                if args.output_mode == OutputMode::Content
                    && (!args.only_matching || hit.text.is_some_and(|text| !text.is_empty()))
                {
                    content.push(&mut output, &candidate.label, hit, &file.lines);
                }
                !matches!(
                    args.output_mode,
                    OutputMode::FilesWithMatches | OutputMode::FilesWithoutMatches
                ) && (!output.full || args.stats)
            });
            output.truncated |= !complete;
            stats.files_matched += usize::from(count != 0);
            match args.output_mode {
                OutputMode::FilesWithMatches if count != 0 => output.push(|| json!(label)),
                OutputMode::FilesWithoutMatches if complete && count == 0 => {
                    output.push(|| json!(label))
                }
                OutputMode::Count if count != 0 => output.push(|| {
                    let mut row = json!({"count": count});
                    if args.with_path {
                        row["path"] = json!(label);
                    }
                    row
                }),
                _ => {}
            }
            if !complete || (output.full && !args.stats) {
                break;
            }
        }
    }

    let mut response = json!({
        "path": roots[0],
        (args.output_mode.result_key()): output.rows,
        "engine": "builtin",
        "truncated": output.truncated,
        "note": output.truncated.then_some(format!("{TRUNCATED} Narrow path, globs, pattern, or context.")),
    });
    if !args.paths.is_empty() {
        response["paths"] = json!(roots);
    }
    if args.stats {
        stats.elapsed_ms = start.elapsed().as_millis();
        response["stats"] = json!(stats);
    }
    Ok(response)
}

pub(super) async fn collect(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count.min(limit.saturating_sub(bytes.len()))]);
    }
}
