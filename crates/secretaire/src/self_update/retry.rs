//! Retry failures reported by a Cargo libtest run without rerunning its passing tests.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitStatus;

use tokio::process::Command;

use super::CommandLine;

#[derive(Default)]
pub(super) struct Report {
    pub(super) passed: usize,
    pub(super) command_error: bool,
    names: BTreeSet<String>,
    listed_names: BTreeSet<String>,
    failures: BTreeSet<String>,
    in_failure_list: bool,
    invalid: bool,
}

impl Report {
    pub(super) fn observe(&mut self, line: &str) {
        let text = line.trim();
        if let Some(summary) = text.strip_prefix("test result: ") {
            self.in_failure_list = false;
            if let Some((result, counts)) = summary.split_once(". ") {
                let mut counts = counts.split("; ");
                let passed = counts.next().and_then(|count| test_count(count, " passed"));
                let failed = counts.next().and_then(|count| test_count(count, " failed"));
                self.passed += passed.unwrap_or(0);
                if result == "FAILED" {
                    // Prefer libtest's final list over names in captured output.
                    let names = if self.listed_names.is_empty() {
                        &mut self.names
                    } else {
                        &mut self.listed_names
                    };
                    if failed == Some(names.len()) && !names.is_empty() {
                        self.failures.append(names);
                    } else {
                        self.invalid = true;
                    }
                }
            }
            self.names.clear();
            self.listed_names.clear();
            return;
        }

        if text == "Caused by:"
            || text.strip_prefix("error: ").is_some_and(|error| {
                !error.starts_with("test failed, to rerun pass ")
                    && !error.starts_with("doctest failed, to rerun pass ")
                    && !error.ends_with(" targets failed:")
                    && !error.ends_with(" target failed:")
            })
        {
            self.command_error = true;
        }

        let name = text
            .strip_prefix("test ")
            .and_then(|test| test.strip_suffix(" ... FAILED"))
            .or_else(|| text.strip_suffix(" --- FAILED"))
            .or_else(|| text.strip_prefix("---- ")?.strip_suffix(" stdout ----"));
        if text == "failures:" {
            self.in_failure_list = true;
            self.listed_names.clear();
        } else if !text.is_empty() {
            // The summary list is indented. The earlier heading with the same
            // text is followed by the individual tests' captured output.
            if self.in_failure_list && line.starts_with(char::is_whitespace) {
                if !text.starts_with('-') {
                    self.listed_names.insert(text.to_owned());
                }
                return;
            }
            self.in_failure_list = false;
            if let Some(name) = name {
                if !name.starts_with('-') {
                    self.names.insert(name.to_owned());
                }
            }
        }
    }

    pub(super) fn failures(&self) -> Vec<&str> {
        if self.invalid {
            Vec::new()
        } else {
            self.failures.iter().map(String::as_str).collect()
        }
    }
}

fn test_count(summary: &str, suffix: &str) -> Option<usize> {
    summary.strip_suffix(suffix)?.parse().ok()
}

pub(super) struct CargoTest {
    cargo: String,
    args: Vec<String>,
    harness: Vec<String>,
}

impl CargoTest {
    pub(super) fn after_failure(
        command: &CommandLine,
        status: ExitStatus,
        stdout: &Report,
        stderr: &Report,
    ) -> Option<Self> {
        if status.success()
            || status.code().is_none()
            || stderr.command_error
            || stdout.invalid
            || stdout.failures.is_empty()
        {
            return None;
        }
        Self::from_command(command)
    }

    fn from_command(command: &CommandLine) -> Option<Self> {
        let shell;
        let args = match command {
            CommandLine::Argv(args) => args,
            CommandLine::Shell(script) => {
                // Only reconstruct literal invocations. Shell expansions and
                // compound commands may do more than run Cargo.
                if script.contains([
                    '$', '`', '\\', '\n', ';', '&', '|', '<', '>', '(', ')', '*', '?', '[', '~',
                    '#',
                ]) {
                    return None;
                }
                shell = shlex::split(script)?;
                &shell
            }
        };
        let [cargo, subcommand, args @ ..] = args.as_slice() else {
            return None;
        };
        if Path::new(cargo).file_name()? != "cargo" || subcommand != "test" {
            return None;
        }
        let mut cargo_args = Vec::new();
        let mut harness = Vec::new();
        let mut args = args.iter();
        for arg in args.by_ref() {
            if arg == "--" {
                break;
            }
            cargo_args.push(arg.clone());
        }
        let args = args.cloned().collect::<Vec<_>>();
        let cargo_args = without_filter(
            &cargo_args,
            &[
                "-p",
                "--package",
                "--exclude",
                "--bin",
                "--example",
                "--test",
                "--bench",
                "-F",
                "--features",
                "-j",
                "--jobs",
                "--profile",
                "--target",
                "--target-dir",
                "--manifest-path",
                "--color",
                "--config",
                "-Z",
                "--message-format",
                "--lockfile-path",
            ],
        )?;
        for arg in without_filter(
            &args,
            &[
                "--skip",
                "--test-threads",
                "--format",
                "--color",
                "--logfile",
                "--shuffle-seed",
                "-Z",
            ],
        )? {
            if arg != "--exact" {
                harness.push(arg);
            }
        }
        Some(Self {
            cargo: cargo.clone(),
            args: cargo_args,
            harness,
        })
    }

    pub(super) fn command(&self, name: &str) -> Command {
        let mut command = Command::new(&self.cargo);
        command
            .arg("test")
            .args(&self.args)
            .arg("--")
            .args(&self.harness)
            .args(["--exact", name]);
        command
    }
}

fn without_filter(args: &[String], value_options: &[&str]) -> Option<Vec<String>> {
    let mut filtered = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.starts_with('-') {
            filtered.push(arg.clone());
            if value_options.contains(&arg.as_str()) {
                filtered.push(args.next()?.clone());
            }
        }
    }
    Some(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(output: &str) -> Report {
        let mut report = Report::default();
        for line in output.lines() {
            report.observe(line);
        }
        report
    }

    #[test]
    fn parses_single_timing_test_failure() {
        let output = "running 2 tests
test tests::tool_result_is_replayed ... ok
test tests::record_clock_does_not_drift ... FAILED

failures:

---- tests::record_clock_does_not_drift stdout ----
thread 'tests::record_clock_does_not_drift' (103) panicked at src/main.rs:15:9:
assertion failed: stamp.elapsed() < std::time::Duration::from_millis(10)

failures:
    tests::record_clock_does_not_drift

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s";
        assert_eq!(
            report(output).failures(),
            ["tests::record_clock_does_not_drift"]
        );
    }

    #[test]
    fn parses_two_test_failures_from_quiet_summary() {
        let output = "running 3 tests
graph::left --- FAILED
graph::right --- FAILED
.
failures:

---- graph::left stdout ----
---- graph::right stdout ----

failures:
    graph::left
    graph::right

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s";
        assert_eq!(report(output).failures(), ["graph::left", "graph::right"]);
    }

    #[test]
    fn ignores_compile_errors_empty_output_and_aborted_binaries() {
        let compile = report(
            "error[E0308]: mismatched types\nerror: could not compile `retry-fixture` (bin \"retry-fixture\" test) due to 1 previous error",
        );
        assert!(compile.command_error);
        assert!(compile.failures().is_empty());
        assert!(report("").failures().is_empty());
        let aborted = report(
            "test graph::left ... FAILED\nerror: test failed, to rerun pass `--lib`\n\nCaused by:\n  process didn't exit successfully",
        );
        assert!(aborted.command_error);
        assert!(aborted.failures().is_empty());
    }

    #[test]
    fn retries_only_completed_cargo_test_failures() {
        use std::os::unix::process::ExitStatusExt;

        let cargo = CommandLine::Shell("cargo test --locked".into());
        let failure =
            report("test graph::left ... FAILED\ntest result: FAILED. 0 passed; 1 failed");
        let expected_stderr = report("error: test failed, to rerun pass `--test graph`");
        let exit = ExitStatus::from_raw(101 << 8);
        assert!(CargoTest::after_failure(&cargo, exit, &failure, &expected_stderr).is_some());
        for (exit, stdout, stderr) in [
            (ExitStatus::from_raw(0), &failure, &expected_stderr),
            (ExitStatus::from_raw(9), &failure, &expected_stderr),
            (exit, &Report::default(), &expected_stderr),
            (
                exit,
                &failure,
                &report("error: could not compile `example`"),
            ),
            (
                exit,
                &failure,
                &report("Caused by:\n  process didn't exit successfully"),
            ),
        ] {
            assert!(CargoTest::after_failure(&cargo, exit, stdout, stderr).is_none());
        }
    }

    #[test]
    fn retries_literal_cargo_commands_with_an_exact_replacement_filter() {
        let command = CommandLine::Shell(
            "cargo test --locked -p secretaire --test graph old -- --exact --test-threads 1 old"
                .into(),
        );
        let retry = CargoTest::from_command(&command).unwrap();
        let command = retry.command("graph::left");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            [
                "test",
                "--locked",
                "-p",
                "secretaire",
                "--test",
                "graph",
                "--",
                "--test-threads",
                "1",
                "--exact",
                "graph::left"
            ],
        );
        for script in [
            "cargo test && make",
            "cargo test -p \"$PACKAGE\"",
            "make test",
            "",
        ] {
            assert!(CargoTest::from_command(&CommandLine::Shell(script.into())).is_none());
        }
    }
}
