mod discovery;
mod output;

use std::{
    collections::HashMap,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{ServerEvent, TestRun, TestState, TestSuite};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::Command,
    task::JoinHandle,
    time::Instant,
};

use crate::{cancellation::Cancellation, Workspace};
use discovery::Suite;
use output::{failure, Parsed};

#[derive(Default)]
pub(crate) struct Tests {
    jobs: Mutex<Jobs>,
    save_error: Mutex<Option<String>>,
}

#[derive(Default)]
struct Jobs {
    revision: u64,
    latest: HashMap<String, Job>,
}

struct Job {
    cancel: Arc<Cancellation>,
    run: TestRun,
}

const TEST_RUNS: &str = "project-test-runs.json";

impl Tests {
    pub(crate) fn load(state_dir: &Path) -> Self {
        let runs: Vec<TestRun> = std::fs::read(state_dir.join(TEST_RUNS))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let revision = runs
            .iter()
            .map(|run| run.revision)
            .max()
            .unwrap_or_default();
        let latest = runs
            .into_iter()
            .filter(|run| run.state != TestState::Running)
            .map(|run| {
                (
                    run.suite.clone(),
                    Job {
                        cancel: Arc::default(),
                        run,
                    },
                )
            })
            .collect();
        Self {
            jobs: Mutex::new(Jobs { revision, latest }),
            save_error: Default::default(),
        }
    }
}

impl Workspace {
    pub(crate) async fn test_suites(&self) -> Result<Vec<TestSuite>> {
        Ok(self
            .discover_tests()
            .await?
            .into_iter()
            .map(|suite| suite.info)
            .collect())
    }

    async fn discover_tests(&self) -> Result<Vec<Suite>> {
        let root = self.project_dir.clone();
        tokio::task::spawn_blocking(move || discovery::discover(&root)).await?
    }

    pub(crate) async fn run_tests(
        self: &Arc<Self>,
        selected: Option<&str>,
        filter: Option<String>,
    ) -> Result<Vec<JoinHandle<TestRun>>> {
        let work = Arc::new(self.admit_work()?);
        let suites: Vec<_> = self
            .discover_tests()
            .await?
            .into_iter()
            .filter(|suite| selected.is_none_or(|id| suite.info.id == id))
            .collect();
        ensure!(
            selected.is_none() || !suites.is_empty(),
            "テストが見つからない: {}",
            selected.unwrap_or_default()
        );
        let mut jobs = self.tests.jobs.lock().unwrap();
        for suite in &suites {
            ensure!(
                !jobs
                    .latest
                    .get(&suite.info.id)
                    .is_some_and(|job| job.run.state == TestState::Running),
                "テストは実行中: {}",
                suite.info.id
            );
        }
        if selected.is_none() {
            jobs.latest.retain(|id, job| {
                job.run.state == TestState::Running
                    || suites.iter().any(|suite| &suite.info.id == id)
            });
        }
        Ok(suites
            .into_iter()
            .map(|suite| {
                let cancel = Arc::new(Cancellation::default());
                let mut run = TestRun {
                    suite: suite.info.id.clone(),
                    revision: 0,
                    state: TestState::Running,
                    passed: 0,
                    failed: 0,
                    failures: Vec::new(),
                    output: String::new(),
                };
                jobs.latest.insert(
                    run.suite.clone(),
                    Job {
                        cancel: cancel.clone(),
                        run: run.clone(),
                    },
                );
                self.publish_test(&mut jobs, &mut run);
                let workspace = self.clone();
                let filter = filter.clone();
                let work = work.clone();
                tokio::spawn(async move {
                    let _work = work;
                    let context = RunContext { root: &workspace.project_dir, publish: Some(&workspace) };
                    execute(&context, &suite, filter.as_deref(), &cancel, &mut run).await;
                    workspace.report_test(&mut run);
                    run
                })
            })
            .collect())
    }

    pub(crate) fn stop_tests(&self, suite: &str) {
        if let Some(job) = self.tests.jobs.lock().unwrap().latest.get(suite) {
            job.cancel.cancel();
        }
    }

    pub(crate) fn stop_tests_for_swap(&self) {
        for job in self.tests.jobs.lock().unwrap().latest.values() {
            if job.run.state == TestState::Running {
                job.cancel.cancel();
            }
        }
    }

    pub(crate) fn test_snapshots(&self) -> Vec<ServerEvent> {
        self.tests
            .jobs
            .lock()
            .unwrap()
            .latest
            .values()
            .map(|job| ServerEvent::TestRun {
                run: job.run.clone(),
            })
            .collect()
    }

    pub(crate) fn has_running_tests(&self) -> bool {
        self.running_test_count() != 0
    }

    pub(crate) fn running_test_count(&self) -> usize {
        self.tests.jobs.lock().unwrap().latest.values()
            .filter(|job| job.run.state == TestState::Running).count()
    }

    fn report_test(&self, run: &mut TestRun) {
        let mut jobs = self.tests.jobs.lock().unwrap();
        self.publish_test(&mut jobs, run);
        if run.state != TestState::Running {
            let mut save_error = self.tests.save_error.lock().unwrap();
            *save_error = self.save_test_runs(&jobs).err().map(|error| format!("{error:#}"));
            if let Some(error) = save_error.as_deref() {
                tracing::warn!("save workspace test results: {error}");
            }
        }
    }

    pub(crate) fn retry_test_saves(&self) {
        let jobs = self.tests.jobs.lock().unwrap();
        let mut error = self.tests.save_error.lock().unwrap();
        if error.is_some() {
            *error = self.save_test_runs(&jobs).err().map(|error| format!("{error:#}"));
        }
    }

    pub(crate) fn test_save_error(&self) -> Option<String> {
        self.tests.save_error.lock().unwrap().clone()
    }

    fn save_test_runs(&self, jobs: &Jobs) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)?;
        let runs: Vec<_> = jobs
            .latest
            .values()
            .map(|job| &job.run)
            .filter(|run| run.state != TestState::Running)
            .collect();
        let mut file = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        serde_json::to_writer(file.as_file_mut(), &runs)?;
        file.persist(self.state_dir.join(TEST_RUNS))?;
        Ok(())
    }

    fn publish_test(&self, jobs: &mut Jobs, run: &mut TestRun) {
        jobs.revision += 1;
        run.revision = jobs.revision;
        jobs.latest.get_mut(&run.suite).unwrap().run = run.clone();
        self.hub
            .publish_snapshot(ServerEvent::TestRun { run: run.clone() });
    }
}

pub(crate) async fn run_in_worktree(root: &Path) -> Result<Vec<TestRun>> {
    let path = root.to_owned();
    let suites = tokio::task::spawn_blocking(move || discovery::discover(&path)).await??;
    let context = RunContext { root, publish: None };
    let cancel = Cancellation::default();
    let mut runs = Vec::new();
    for suite in suites {
        let mut run = TestRun {
            suite: suite.info.id.clone(),
            revision: 0,
            state: TestState::Running,
            passed: 0,
            failed: 0,
            failures: Vec::new(),
            output: String::new(),
        };
        execute(&context, &suite, None, &cancel, &mut run).await;
        runs.push(run);
    }
    Ok(runs)
}

struct RunContext<'a> {
    root: &'a Path,
    publish: Option<&'a Workspace>,
}

enum Execution {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

fn command(suite: &Suite, args: &[String], filter: Option<&str>) -> Command {
    let program = Path::new(&args[0]);
    let mut command = if args[0].contains('/') {
        Command::new(suite.cwd.join(program))
    } else {
        Command::new(program)
    };
    command
        .current_dir(&suite.cwd)
        .args(&args[1..])
        .envs(&suite.env);
    if let Some(seed) = &suite.seed {
        command.env("FUZUKUE_TEST_SEED", seed).env("LABO_TEST_SEED", seed);
    }
    if let Some(filter) = filter {
        command.env("FUZUKUE_TEST_FILTER", filter).env("LABO_TEST_FILTER", filter);
        if args == suite.command {
            match suite.info.kind.as_str() {
                "cargo" => {
                    command.args(["--", filter]);
                }
                "pytest" => {
                    command.args(["-k", filter]);
                }
                "npm" => {
                    command.args(["--", filter]);
                }
                _ => {}
            }
        }
    }
    command
}

async fn execute(
    context: &RunContext<'_>,
    suite: &Suite,
    filter: Option<&str>,
    cancel: &Cancellation,
    run: &mut TestRun,
) {
    let mut parsed = Parsed::default();
    let result = async {
        if let Some(program) = suite.missing_runner() {
            run.state = TestState::Unavailable;
            anyhow::bail!(
                "{program} が見つからない。実行コマンド: {}",
                suite.info.command
            );
        }
        let root = context.root;
        let expected = suite
            .expect
            .as_ref()
            .map(|path| {
                let path = crate::safe_join(root, &suite.cwd.join(path).to_string_lossy())?;
                let expected = output::numbers(&std::fs::read_to_string(path)?, true)?;
                ensure!(!expected.is_empty(), "期待値が空");
                Ok::<_, anyhow::Error>(expected)
            })
            .transpose()?;
        let deadline = Instant::now()
            .checked_add(suite.timeout)
            .context("timeout が大きすぎる")?;
        let mut cmd = command(suite, &suite.command, filter);
        let result = stream(
            context,
            suite,
            &mut cmd,
            deadline,
            cancel,
            &mut parsed,
            run,
        )
        .await?;
        let Execution::Exited(status) = result else {
            return Ok(result);
        };
        if !status.success() {
            return Ok(result);
        }
        if let Some(audit) = &suite.audit {
            let file = audit.strip_prefix(&suite.cwd)?.to_string_lossy();
            let path = crate::safe_join(root, &audit.to_string_lossy())?;
            let source = std::fs::read_to_string(path)?;
            let declarations: Vec<_> = source
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix("#print axioms ")
                        .and_then(|line| line.split_whitespace().next())
                        .map(str::to_owned)
                })
                .collect();
            let args = vec![
                "lake".into(),
                "env".into(),
                "lean".into(),
                file.into_owned(),
            ];
            let mut cmd = command(suite, &args, filter);
            parsed.stdout.clear();
            parsed.stdout_truncated = false;
            let result = stream(
                context,
                suite,
                &mut cmd,
                deadline,
                cancel,
                &mut parsed,
                run,
            )
            .await?;
            if matches!(result, Execution::Exited(status) if status.success()) {
                ensure!(
                    !declarations.is_empty() && !parsed.stdout_truncated,
                    "公理監査の宣言または出力が不正"
                );
                for (name, state) in crate::lean::parse_axioms(&parsed.stdout, &declarations) {
                    if state == "proved" {
                        run.passed += 1;
                    } else {
                        run.failed += 1;
                        let line = source.lines().position(|line| {
                            line.trim()
                                .strip_prefix("#print axioms ")
                                .and_then(|line| line.split_whitespace().next())
                                == Some(&name)
                        });
                        failure(
                            run,
                            Some(audit.strip_prefix(root)?.to_string_lossy().into_owned()),
                            line.map(|index| index as u32 + 1),
                            &format!("{name}: {state}"),
                        );
                    }
                }
            }
            return Ok(result);
        }
        if let Some(expected) = expected {
            ensure!(!parsed.stdout_truncated, "数値の標準出力が 1 MiB を超えた");
            let actual = output::numbers(&parsed.stdout, false)?;
            output::compare(&expected, &actual, suite.abs_tol, suite.rel_tol, run);
        }
        Ok(result)
    }
    .await;
    if let Some((passed, failed)) = parsed.counts {
        run.passed += passed;
        run.failed += failed;
    }
    let (state, message) = match result {
        Ok(Execution::Exited(status)) if status.success() && run.failed == 0 => {
            if parsed.counts.is_none() && suite.expect.is_none() && suite.audit.is_none() {
                run.passed = 1;
            }
            (TestState::Passed, None)
        }
        Ok(Execution::Exited(status)) => {
            run.failed = run.failed.max(1);
            (TestState::Failed, Some(format!("終了状態: {status}")))
        }
        Ok(Execution::Cancelled) => (TestState::Cancelled, Some("止めた".into())),
        Ok(Execution::TimedOut) => (
            TestState::TimedOut,
            Some(format!("{} 秒で時間切れ", suite.timeout.as_secs())),
        ),
        Err(error) => {
            if run.state != TestState::Unavailable {
                run.failed = run.failed.max(1);
            }
            let state = if run.state == TestState::Unavailable {
                TestState::Unavailable
            } else {
                TestState::Failed
            };
            (state, Some(format!("{error:#}")))
        }
    };
    run.state = state;
    if state == TestState::Passed {
        run.failures.clear();
    }
    if let Some(message) = message {
        if run.failures.is_empty() || matches!(state, TestState::Cancelled | TestState::TimedOut) {
            failure(run, None, None, &message);
        }
    }
}

async fn stream(
    context: &RunContext<'_>,
    suite: &Suite,
    command: &mut Command,
    deadline: Instant,
    cancel: &Cancellation,
    parsed: &mut Parsed,
    run: &mut TestRun,
) -> Result<Execution> {
    if cancel.check().is_err() {
        return Ok(Execution::Cancelled);
    }
    if Instant::now() >= deadline {
        return Ok(Execution::TimedOut);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, job) = crate::job_process::Job::spawn(command, "test", None)
        .with_context(|| format!("テストを起動できない: {}", suite.info.command))?;
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut out_line = Line::default();
    let mut err_line = Line::default();
    let mut open = [true; 2];
    let mut status = None;
    let mut interval = tokio::time::interval(Duration::from_millis(120));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut changed = false;
    let mut forced = None;
    let result = loop {
        let next = tokio::select! {
            _ = cancel.cancelled(), if forced.is_none() => { forced = Some(Execution::Cancelled); None },
            _ = tokio::time::sleep_until(deadline), if forced.is_none() => { forced = Some(Execution::TimedOut); None },
            waited = child.wait(), if status.is_none() => {
                match waited {
                    Ok(exit) => {
                        status = Some(exit);
                        if let Err(error) = job.stop() {
                            break Err(error).context("テストの子プロセスを停止できない");
                        }
                    }
                    Err(error) => break Err(error.into()),
                }
                None
            }
            _ = interval.tick() => {
                if let Some(workspace) = context.publish.filter(|_| changed) {
                    workspace.report_test(run);
                    changed = false;
                }
                None
            }
            bytes = out_line.read(&mut stdout), if open[0] => Some((0, bytes)),
            bytes = err_line.read(&mut stderr), if open[1] => Some((1, bytes)),
        };
        if let Some((channel, bytes)) = next {
            let line = if channel == 0 {
                &mut out_line
            } else {
                &mut err_line
            };
            match bytes {
                Ok(0) => {
                    open[channel] = false;
                    line.finish(|text| consume(context, suite, parsed, run, channel, text));
                }
                Ok(bytes) => line.drain(bytes, |text| {
                    consume(context, suite, parsed, run, channel, text)
                }),
                Err(error) => break Err(error.into()),
            }
            changed = true;
        }
        if let Some(forced) = forced {
            // Close the readers on stop even if a descendant has moved to a different group.
            break Ok(forced);
        }
        if let Some(status) = status.filter(|_| !open[0] && !open[1]) {
            return Ok(Execution::Exited(status));
        }
    };
    job.stop().context("テストを停止できない")?;
    child.wait().await?;
    result
}

struct Line {
    buffer: Vec<u8>,
    chunk: Vec<u8>,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            buffer: Vec::new(),
            // Branch review carries both pipe buffers across awaits when running tests.
            chunk: vec![0; 8192],
        }
    }
}

impl Line {
    async fn read(
        &mut self,
        reader: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> std::io::Result<usize> {
        reader.read(&mut self.chunk).await
    }

    fn finish(&mut self, mut emit: impl FnMut(&str)) {
        if !self.buffer.is_empty() {
            emit(String::from_utf8_lossy(&self.buffer).trim_end_matches('\r'));
            self.buffer.clear();
        }
    }

    fn drain(&mut self, bytes: usize, mut emit: impl FnMut(&str)) {
        for segment in self.chunk[..bytes].split_inclusive(|b| *b == b'\n') {
            let line = segment.strip_suffix(b"\n").unwrap_or(segment);
            let remaining = (64 * 1024usize).saturating_sub(self.buffer.len());
            self.buffer
                .extend_from_slice(&line[..line.len().min(remaining)]);
            if line.len() < segment.len() {
                emit(String::from_utf8_lossy(&self.buffer).trim_end_matches('\r'));
                self.buffer.clear();
            }
        }
    }
}

fn consume(
    context: &RunContext<'_>,
    suite: &Suite,
    parsed: &mut Parsed,
    run: &mut TestRun,
    channel: usize,
    text: &str,
) {
    if channel == 0 && (suite.expect.is_some() || suite.audit.is_some()) {
        if parsed.stdout.len() + text.len() < 1024 * 1024 && !parsed.stdout_truncated {
            parsed.stdout.push_str(text);
            parsed.stdout.push('\n');
        } else {
            parsed.stdout_truncated = true;
        }
    }
    parsed.line(
        context.root,
        &suite.cwd,
        &suite.info.kind,
        text,
        run,
    );
}
