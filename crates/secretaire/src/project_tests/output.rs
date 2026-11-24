use std::{collections::BTreeMap, path::Path, sync::LazyLock};

use anyhow::{bail, ensure, Result};
use fuzukue_proto::{TestFailure, TestRun};
use regex::Regex;

const MAX_FAILURES: usize = 12;
const LOG_BYTES: usize = 24 * 1024;

pub(super) fn failure(run: &mut TestRun, file: Option<String>, line: Option<u32>, message: &str) {
    let message: String = message.trim().chars().take(240).collect();
    if run.failures.len() < MAX_FAILURES
        && !run
            .failures
            .iter()
            .any(|f| f.file == file && f.line == line && (file.is_some() || f.message == message))
    {
        run.failures.push(TestFailure {
            file,
            line,
            message,
        });
    }
}

#[derive(Default)]
pub(super) struct Parsed {
    pub counts: Option<(usize, usize)>,
    pub stdout: String,
    pub stdout_truncated: bool,
    failed_test: String,
}

impl Parsed {
    pub(super) fn line(
        &mut self,
        root: &Path,
        cwd: &Path,
        kind: &str,
        line: &str,
        run: &mut TestRun,
    ) {
        static ANSI: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap());
        static CARGO: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^test result: .*? (\d+) passed; (\d+) failed;").unwrap());
        static PYTEST: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(\d+) (passed|failed|errors?)\b").unwrap());
        static PYTEST_DURATION: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^\d+ .* in \d+(?:\.\d+)?s$").unwrap());
        static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?:^|\s|\()([^\s():]+):(\d+)(?::\d+)?(?:[:\s)]|$)").unwrap()
        });
        static PANIC: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^thread '([^']+)' .*panicked at (.+):(\d+):\d+:").unwrap()
        });
        let line = ANSI.replace_all(line, "");
        run.output.push_str(&line);
        run.output.push('\n');
        if run.output.len() > LOG_BYTES {
            let cut = run.output.ceil_char_boundary(run.output.len() - LOG_BYTES);
            run.output.drain(..cut);
        }

        if kind == "cargo" {
            if let Some(c) = CARGO.captures(&line) {
                let (passed, failed) = self.counts.get_or_insert_default();
                *passed = passed.saturating_add(c[1].parse::<usize>().unwrap_or(0));
                *failed = failed.saturating_add(c[2].parse::<usize>().unwrap_or(0));
            }
        } else if kind == "pytest"
            && ((line.starts_with('=') && line.ends_with('='))
                || PYTEST_DURATION.is_match(line.trim()))
        {
            let mut counts = (0, 0usize);
            let mut found = false;
            for c in PYTEST.captures_iter(&line) {
                found = true;
                let count = c[1].parse::<usize>().unwrap_or(0);
                if &c[2] == "passed" {
                    counts.0 = count;
                } else {
                    counts.1 = counts.1.saturating_add(count);
                }
            }
            if found {
                self.counts = Some(counts);
            }
        }

        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            self.failed_test = name.into();
        }
        if let Some(c) = PANIC.captures(&line) {
            let name = if c[1].starts_with('<') {
                self.failed_test.as_str()
            } else {
                &c[1]
            };
            located(root, cwd, &c[2], &c[3], &format!("{name}: panic"), run);
        } else if let Some(c) = LOCATION.captures(&line) {
            let message = if kind == "pytest" && !self.failed_test.is_empty() {
                format!("{}: {}", self.failed_test, line.trim())
            } else {
                line.trim().into()
            };
            located(root, cwd, &c[1], &c[2], &message, run);
        } else if let Some(message) = line.strip_prefix("--- FAIL") {
            failure(run, None, None, message.trim_start_matches(':'));
        } else if kind == "pytest" {
            // pytest's short summary can still name a file when the traceback has no line.
            if let Some((path, _)) = line
                .strip_prefix("FAILED ")
                .and_then(|s| s.split_once("::"))
            {
                if let Some(file) = relative_file(root, cwd, path) {
                    if !run
                        .failures
                        .iter()
                        .any(|f| f.file.as_deref() == Some(&file))
                    {
                        failure(run, Some(file), None, &line);
                    }
                }
            }
        }
    }
}

fn relative_file(root: &Path, cwd: &Path, path: &str) -> Option<String> {
    for dir in [cwd, root] {
        if let Ok(full) = dir.join(path).canonicalize() {
            if full.is_file() {
                if let Ok(relative) = full.strip_prefix(root) {
                    return Some(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

fn located(root: &Path, cwd: &Path, path: &str, number: &str, message: &str, run: &mut TestRun) {
    if let (Some(file), Ok(line @ 1..)) = (relative_file(root, cwd, path), number.parse::<u32>()) {
        failure(run, Some(file), Some(line), message);
    }
}

pub(super) fn numbers(text: &str, strict: bool) -> Result<BTreeMap<String, f64>> {
    let text = text.trim();
    if text.starts_with('{') {
        let numbers: BTreeMap<String, f64> = serde_json::from_str(text)?;
        ensure!(
            numbers.values().all(|n| n.is_finite()),
            "有限でない数値がある"
        );
        return Ok(numbers);
    }
    let mut numbers = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if matches!(line, "key,value" | "key, value") {
            continue;
        }
        let pair = line
            .split_once([',', '=', ':'])
            .or_else(|| line.split_once(char::is_whitespace));
        if let Some((key, value)) = pair {
            let key = key.trim().trim_matches('"');
            if let Ok(value) = value.trim().trim_matches('"').parse::<f64>() {
                if !key.is_empty() && value.is_finite() {
                    ensure!(
                        numbers.insert(key.to_owned(), value).is_none(),
                        "数値のキーが重複: {key}"
                    );
                    continue;
                }
            }
        }
        if strict {
            bail!("期待値はキーと有限の数値の組で指定する: {line}");
        }
    }
    Ok(numbers)
}

pub(super) fn compare(
    expected: &BTreeMap<String, f64>,
    actual: &BTreeMap<String, f64>,
    abs: f64,
    rel: f64,
    run: &mut TestRun,
) {
    for (key, want) in expected {
        match actual.get(key) {
            Some(got) if got == want || (got - want).abs() <= abs.max(rel * want.abs()) => {
                run.passed += 1
            }
            value => {
                run.failed += 1;
                let got = value.map_or_else(|| "出力なし".into(), |n| n.to_string());
                failure(
                    run,
                    None,
                    None,
                    &format!("{key}: 期待値 {want} / 実行結果 {got}"),
                );
            }
        }
    }
}
