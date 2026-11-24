use super::{
    DEADLINE, MAX_BYTES, ReadScope, TRUNCATED, dot, glob, matches_glob, paths, utf8_prefix,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const FILE_BYTES: usize = 1024 * 1024;
const SEARCH_BYTES: usize = 32 * FILE_BYTES;
const MATCH_TEXT_BYTES: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    pattern: String,
    #[serde(default = "dot")]
    path: PathBuf,
    glob: Option<String>,
    #[serde(default)]
    fixed_string: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    context: usize,
    max_results: Option<usize>,
}

#[derive(Default)]
struct Corpus {
    text: String,
    files: Vec<Source>,
    lines: usize,
    incomplete: bool,
}

struct Source {
    path: PathBuf,
    byte: usize,
    line: usize,
}

impl Corpus {
    fn add(&mut self, path: &Path, label: &Path, file: &File) -> Result<bool> {
        let meta = paths::regular(file)?;
        if meta.len() > FILE_BYTES as u64 || paths::media(path).is_err() {
            return Ok(true);
        }
        let mut bytes = Vec::new();
        file.take((FILE_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > FILE_BYTES {
            return Ok(true);
        }
        let Ok(text) = paths::text(&bytes) else {
            return Ok(true);
        };
        if text.is_empty() {
            return Ok(true);
        }
        if self.text.len() + text.len() + 1 > SEARCH_BYTES {
            return Ok(false);
        }
        self.files.push(Source {
            path: label.into(),
            byte: self.text.len(),
            line: self.lines,
        });
        self.lines += text.lines().count();
        self.text.push_str(text);
        if !text.ends_with('\n') {
            self.text.push('\n');
        }
        Ok(true)
    }

    fn collect(scope: &ReadScope, args: &Args, deadline: Instant) -> Result<Self> {
        let base = scope.resolve(&args.path)?;
        let handle = paths::open(&base)?;
        let glob = glob(args.glob.as_deref())?;
        let mut corpus = Self::default();
        if handle.metadata()?.is_dir() {
            let mut scan = paths::Scan::new(scope, 64, false, false, deadline);
            let mut skipped_at_depth = false;
            corpus.incomplete = !scan.visit(&base, &handle, &mut |entry| {
                let relative = entry
                    .path
                    .strip_prefix(&base)
                    .expect("scan returns descendants");
                if entry.kind == "directory" && relative.components().count() >= 64 {
                    skipped_at_depth = true;
                }
                if entry.kind != "file" || !matches_glob(glob.as_ref(), relative) {
                    return true;
                }
                match scope.audit_read(&entry.path)
                    .and_then(|()| paths::open(&entry.path))
                    .and_then(|file| corpus.add(&entry.path, relative, &file))
                {
                    Ok(has_space) => has_space,
                    Err(_) => {
                        corpus.incomplete = true;
                        true
                    }
                }
            })? || corpus.incomplete
                || skipped_at_depth;
        } else if matches_glob(
            glob.as_ref(),
            Path::new(base.file_name().context("Missing file name")?),
        ) {
            paths::media(&args.path)?;
            paths::media(&base)?;
            corpus.add(&base, &args.path, &handle)?;
        }
        Ok(corpus)
    }
}

pub(super) async fn search(scope: ReadScope, args: Args) -> Result<Value> {
    let limit = args.max_results.unwrap_or(100);
    ensure!(
        (1..=200).contains(&limit),
        "max_results must be between 1 and 200"
    );
    ensure!(args.context <= 5, "context must be at most 5");
    ensure!(
        args.pattern.len() <= 4096 && !args.pattern.contains(['\r', '\n', '\0']),
        "Supply a single-line pattern of at most 4096 bytes"
    );
    let path = args.path.clone();
    let deadline = Instant::now() + DEADLINE;
    let (corpus, args) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((Corpus::collect(&scope, &args, deadline)?, args))
    })
    .await??;
    let (lines, engine) = match ripgrep("rg", &corpus.text, &args, limit + 1).await? {
        Some(lines) => (lines, "ripgrep"),
        None => {
            let context = args.context;
            let (corpus, lines, timed_out) = tokio::task::spawn_blocking(move || {
                let (lines, timed_out) = builtin(
                    &corpus.text,
                    &args.pattern,
                    args.fixed_string,
                    args.ignore_case,
                    limit + 1,
                )?;
                Ok::<_, anyhow::Error>((corpus, lines, timed_out))
            })
            .await??;
            return Ok(render(
                path, corpus, lines, limit, context, "builtin", timed_out,
            ));
        }
    };
    Ok(render(
        path,
        corpus,
        lines,
        limit,
        args.context,
        engine,
        false,
    ))
}

// Match a bounded UTF-8 stream assembled from files opened by ReadScope. rg
// never receives an on-disk search path, so ignore files, symlink races, and
// option-like paths cannot change what it reads.
async fn ripgrep(
    program: &str,
    corpus: &str,
    args: &Args,
    limit: usize,
) -> Result<Option<Vec<usize>>> {
    let mut command = Command::new(program);
    command
        .args([
            "--no-config",
            "--no-follow",
            "--max-filesize",
            "1M",
            "--color",
            "never",
            "--line-number",
            "--no-filename",
            "--no-heading",
            "--crlf",
            "--max-count",
        ])
        .arg(limit.to_string());
    if args.fixed_string {
        command.arg("--fixed-strings");
    }
    if args.ignore_case {
        command.arg("--ignore-case");
    }
    command.args(["-e", &args.pattern, "--", "-"]);
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not start ripgrep"),
    };
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, output, error, ()) = tokio::time::timeout(DEADLINE, async {
        tokio::try_join!(
            child.wait(),
            collect(stdout, SEARCH_BYTES + 16 * limit),
            collect(stderr, 4096),
            async {
                match stdin.write_all(corpus.as_bytes()).await {
                    Err(error) if error.kind() != ErrorKind::BrokenPipe => Err(error),
                    _ => {
                        drop(stdin);
                        Ok(())
                    }
                }
            },
        )
    })
    .await
    .context("Workspace search timed out")??;
    match status.code() {
        Some(0 | 1) => {}
        _ => bail!("ripgrep failed: {}", String::from_utf8_lossy(&error).trim()),
    }
    let lines = std::str::from_utf8(&output)?
        .lines()
        .map(|line| {
            let (number, _) = line
                .split_once(':')
                .context("Invalid ripgrep line number")?;
            Ok(number
                .parse::<usize>()?
                .checked_sub(1)
                .context("Invalid ripgrep line number")?)
        })
        .collect::<Result<_>>()?;
    Ok(Some(lines))
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

fn builtin(
    text: &str,
    pattern: &str,
    fixed: bool,
    ignore_case: bool,
    limit: usize,
) -> Result<(Vec<usize>, bool)> {
    let pattern = if fixed {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let matcher = regex::RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .size_limit(2 * 1024 * 1024)
        .build()?;
    let deadline = Instant::now() + DEADLINE;
    let mut matches = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if Instant::now() >= deadline {
            return Ok((matches, true));
        }
        if matcher.is_match(line) {
            matches.push(index);
            if matches.len() == limit {
                break;
            }
        }
    }
    Ok((matches, false))
}

fn render(
    path: PathBuf,
    corpus: Corpus,
    lines: Vec<usize>,
    limit: usize,
    context: usize,
    engine: &str,
    timed_out: bool,
) -> Value {
    let mut results = Vec::new();
    let mut truncated = corpus.incomplete || lines.len() > limit || timed_out;
    let mut output_bytes = 0;
    let mut sources = corpus.files.iter().peekable();
    let mut matches = lines.into_iter().take(limit).peekable();
    while let Some(file) = sources.next() {
        let end = sources.peek().map_or(corpus.text.len(), |next| next.byte);
        let last_line = sources.peek().map_or(corpus.lines, |next| next.line);
        if matches.peek().is_none_or(|line| *line >= last_line) {
            continue;
        }
        let local = corpus.text[file.byte..end].lines().collect::<Vec<_>>();
        while matches.peek().is_some_and(|line| *line < last_line) {
            let index = matches.next().unwrap() - file.line;
            let first = index.saturating_sub(context);
            let last = (index + context + 1).min(local.len());
            let window = (first..last)
                .map(|i| {
                    let line = local[i];
                    let prefix = utf8_prefix(line, MATCH_TEXT_BYTES);
                    json!({"line": i + 1, "text": prefix, "truncated": prefix.len() < line.len()})
                })
                .collect::<Vec<_>>();
            let data =
                json!({"path": file.path.to_string_lossy(), "line": index + 1, "context": window});
            output_bytes += data.to_string().len();
            if output_bytes > MAX_BYTES {
                truncated = true;
                break;
            }
            results.push(data);
        }
        if output_bytes > MAX_BYTES {
            break;
        }
    }
    json!({"path": path, "matches": results, "engine": engine, "truncated": truncated,
        "note": truncated.then(|| format!("{TRUNCATED} Narrow path, glob, pattern, or context."))})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_ripgrep_allows_builtin_literal_and_regex_search() -> Result<()> {
        let args = Args {
            pattern: "-x".into(),
            path: dot(),
            glob: None,
            fixed_string: true,
            ignore_case: true,
            context: 0,
            max_results: Some(1),
        };
        let text = "before\n-X\r\n-x\n";
        assert!(
            ripgrep("/missing/fs-search-rg", text, &args, 2)
                .await?
                .is_none()
        );
        assert_eq!(
            builtin(text, &args.pattern, true, true, 2)?,
            (vec![1, 2], false)
        );
        assert_eq!(
            builtin(text, "^before$", false, false, 2)?,
            (vec![0], false)
        );
        Ok(())
    }
}
