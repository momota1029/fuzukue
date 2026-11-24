//! Attribute reportable worker events and collect them for the next desk turn.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use fuzukue_proto::{Envelope, ServerEvent};
use tokio::time::Instant;

use super::{activity, enqueue_desk, Activity, App, DeskTurn};

const THREAD_INTERVAL: Duration = Duration::from_secs(90);
const FAILURE_INTERVAL: Duration = Duration::from_secs(20);
const PROSE_IDLE: Duration = Duration::from_secs(2);
const PROSE_LIMIT: usize = 8192;
const DIGEST_LIMIT: usize = 1500;
const FAILURE_LIMIT: usize = 240;

pub(super) fn enabled() -> bool {
    !fuzukue_proto::compat::env("FUZUKUE_WORKER_RELAY").is_ok_and(|value| value == "0")
}

#[derive(Clone, Default)]
pub(crate) struct Digest {
    title: Option<String>,
    revision: u64,
    prose: String,
    paths: BTreeSet<String>,
    failures: VecDeque<String>,
}

impl Digest {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    fn has_progress(&self) -> bool {
        !self.prose.is_empty() || !self.paths.is_empty()
    }

    fn is_empty(&self) -> bool {
        !self.has_progress() && self.failures.is_empty()
    }

    fn merge(&mut self, newer: Self) {
        self.title = newer.title.or(self.title.take());
        self.revision = self.revision.max(newer.revision);
        if !newer.prose.is_empty() {
            self.prose = newer.prose;
        }
        self.paths.extend(newer.paths);
        self.failures.extend(newer.failures);
    }

    fn render(&self) -> String {
        let mut parts = Vec::new();
        if let Some(failure) = self.failures.front() {
            parts.push(format!("Failure\n{failure}"));
        }
        if !self.paths.is_empty() {
            let paths = self.paths.iter().cloned().collect::<Vec<_>>().join(", ");
            let paths: String = paths.chars().take(600).collect();
            parts.push(format!("Changed files: {paths}"));
        }
        if !self.prose.is_empty() {
            parts.push(self.prose.clone());
        }
        parts.join("\n").chars().take(DIGEST_LIMIT).collect()
    }
}

pub(super) type Digests = BTreeMap<String, Digest>;

fn render(digests: &Digests) -> String {
    digests.iter().map(|(job, digest)| {
        let title = digest.title.as_deref().unwrap_or(job);
        let heading = if digest.title.is_some() { format!("{title} ({job})") } else { title.into() };
        format!("## {heading}\n{}", digest.render())
    }).collect::<Vec<_>>().join("\n\n")
}

pub(super) fn prompt(digests: &Digests) -> String {
    format!("{}\n\n{}", crate::front_desk::worker_digest_instruction(), render(digests))
}

#[derive(Default)]
pub(super) struct Queue {
    pending: Digests,
    next_turn: Option<Instant>,
    next_failure: HashMap<String, Instant>,
}

impl Queue {
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn has_job(&self, job: &str) -> bool {
        self.pending.contains_key(job)
    }

    pub(super) fn discard(&mut self, job: &str) {
        self.pending.remove(job);
        self.next_failure.remove(job);
    }

    pub(super) fn enqueue(&mut self, digests: Digests) {
        for (job, digest) in digests {
            self.pending.entry(job).or_default().merge(digest);
        }
    }

    // A backend can defer delivery in favor of user input. New arrivals win
    // for prose, while the older unsent failure keeps its place in line.
    pub(super) fn restore(&mut self, mut older: Digests) {
        for (job, newer) in std::mem::take(&mut self.pending) {
            older.entry(job).or_default().merge(newer);
        }
        self.pending = older;
    }

    pub(super) fn cooling_down(&self, now: Instant) -> bool {
        self.next_turn.is_some_and(|deadline| deadline > now)
    }

    pub(super) fn deadline(&self, now: Instant) -> Option<Instant> {
        self.pending.iter().flat_map(|(job, digest)| {
            let progress = digest.has_progress().then(|| self.next_turn.unwrap_or(now));
            let failure = (!digest.failures.is_empty()).then(|| self.next_failure.get(job).copied().unwrap_or(now));
            progress.into_iter().chain(failure)
        }).min()
    }

    pub(super) fn take(&mut self, now: Instant) -> Digests {
        // Any eligible failure opens a turn for all pending progress, but a
        // job's later failures still wait for its own deadline.
        let mut ready = Digests::new();
        self.pending.retain(|job, digest| {
            let failure = if self.next_failure.get(job).is_none_or(|deadline| *deadline <= now) {
                digest.failures.pop_front()
            } else {
                None
            };
            let batch = Digest {
                title: digest.title.clone(),
                revision: digest.revision,
                prose: std::mem::take(&mut digest.prose),
                paths: std::mem::take(&mut digest.paths),
                failures: failure.into_iter().collect(),
            };
            if !batch.is_empty() {
                ready.insert(job.clone(), batch);
            }
            !digest.is_empty()
        });
        ready
    }

    pub(super) fn sent(&mut self, batch: &Digests, now: Instant) {
        self.next_turn = Some(now + THREAD_INTERVAL);
        self.next_failure.retain(|_, deadline| *deadline > now);
        for (job, digest) in batch {
            if !digest.failures.is_empty() {
                self.next_failure.insert(job.clone(), now + FAILURE_INTERVAL);
            }
        }
    }
}

#[derive(Default)]
struct Batch {
    prose_item: String,
    prose: String,
    prose_truncated: bool,
    prose_deadline: Option<Instant>,
    last_findings: String,
    calls: HashMap<String, (String, serde_json::Value)>,
    seen_failures: HashSet<String>,
}

impl Batch {
    fn finish_prose(&mut self, digest: &mut Digest) {
        if self.prose_deadline.take().is_none() {
            return;
        }
        let findings = crate::front_desk::worker_findings(&self.prose);
        if !findings.is_empty() && findings != self.last_findings {
            self.last_findings.clone_from(&findings);
            digest.prose = findings;
        }
    }

    fn observe(&mut self, event: &ServerEvent, session: &str, job: &str, now: Instant, digest: &mut Digest) {
        match activity(event, session, job) {
            Some(Activity::Prose { item, text }) if !item.starts_with("plan:") && !item.contains(":summary:") => {
                if self.prose_item != item {
                    self.finish_prose(digest);
                    self.prose_item = item.into();
                    self.prose.clear();
                    self.prose_truncated = false;
                }
                if !self.prose_truncated {
                    if self.prose.len() + text.len() <= PROSE_LIMIT {
                        self.prose.push_str(text);
                        self.prose_deadline = Some(now + PROSE_IDLE);
                    } else {
                        // Keep Markdown boundaries intact instead of reading a
                        // truncated code block or sentence as an update.
                        self.prose.clear();
                        self.prose_truncated = true;
                        self.prose_deadline = None;
                    }
                }
            }
            Some(Activity::Tool { item, name, input }) => {
                self.finish_prose(digest);
                self.calls.insert(item.into(), (name.into(), input.clone()));
            }
            _ => {}
        }
        if let ServerEvent::ToolResult { session: id, item, ok, output } = event {
            if id != session {
                return;
            }
            if let Some((name, input)) = self.calls.remove(item) {
                if *ok {
                    digest.paths.extend(crate::thread_artifacts::tool_paths(&name, Some(&input), Some(output))
                        .into_iter().filter(|path| !path.trim().is_empty()));
                } else {
                    let line = output.lines().find(|line| {
                        let line = line.trim();
                        !line.is_empty() && line != "null"
                    });
                    let failure: String = line.unwrap_or(&name).chars().take(FAILURE_LIMIT).collect();
                    if self.seen_failures.insert(failure.trim().into()) {
                        digest.failures.push_back(failure);
                    }
                }
            }
        }
    }
}

pub(super) async fn relay_progress(
    app: Arc<App>, session: String, job: String, mut events: tokio::sync::broadcast::Receiver<Envelope>,
) {
    let mut batch = Batch::default();
    loop {
        let mut digest = Digest::default();
        let deadline = batch.prose_deadline;
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => batch.observe(&event.event, &session, &job, Instant::now(), &mut digest),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => batch.finish_prose(&mut digest)
        }
        if !digest.is_empty() {
            digest.title = app.delegates.jobs.get(&job).and_then(|record| record.lock().unwrap().display.label.clone());
            digest.revision = app.delegates.jobs.digest_ready(&job).unwrap_or_default();
            enqueue_desk(&app, &session, DeskTurn::WorkerDigest(Digests::from([(job.clone(), digest)])));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegate::DeskQueue;

    #[test]
    fn relay_requests_a_japanese_summary_of_english_prose() {
        let evidence = "I verified that the two declarations have the same hypotheses.";
        let digest = Digest { prose: evidence.into(), ..Default::default() };
        let prompt = prompt(&Digests::from([("delegate:test".into(), digest)]));
        assert!(prompt.contains(evidence));
        assert!(prompt.contains(harness_text!("English updates", "英語の進捗")));
        assert!(prompt.contains(harness_text!("Japanese for a Japanese conversation", "日本語にまとめ")));
        assert!(prompt.contains("<silent/>"));
    }
    use fuzukue_proto::Actor;

    const SESSION: &str = "thread-test";
    const JOB: &str = "delegate:worker";

    fn prose(id: &str, text: &str) -> ServerEvent {
        ServerEvent::TextDelta {
            session: SESSION.into(), item: format!("{JOB}:{id}"), actor: Actor::System, text: text.into(),
        }
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> ServerEvent {
        ServerEvent::ToolCall {
            session: SESSION.into(), item: id.into(),
            actor: Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some(JOB.into()) },
            name: name.into(), input,
        }
    }

    fn result(id: &str, ok: bool, output: &str) -> ServerEvent {
        ServerEvent::ToolResult {
            session: SESSION.into(), item: id.into(), ok, output: output.into(),
        }
    }

    #[test]
    fn digest_emission_requires_a_result_edit_or_failure() {
        let empty = serde_json::Value::Null;
        let examples = [
            ("read, search and list", vec![
                call("bash", "Bash", empty.clone()), result("bash", true, "read a file"),
                call("search", "fuzukue.search_library", empty.clone()), result("search", true, "matches"),
                call("list", "fuzukue.list_tests", empty.clone()), result("list", true, "test names"),
            ], ""),
            ("edit without a path", vec![
                call("edit", "Edit", empty.clone()), result("edit", true, "saved"),
            ], ""),
            ("attempted edit", vec![call("edit", "Edit", serde_json::json!({"file_path": "src/main.rs"}))], ""),
            ("completed edits", vec![
                call("write", "Write", serde_json::json!({"file_path": "src/main.rs"})), result("write", true, "saved"),
                call("edit", "edit", serde_json::json!({"paths": ["src/lib.rs", "src/main.rs"]})), result("edit", true, "saved"),
            ], "Changed files: src/lib.rs, src/main.rs"),
            ("unrelated failure", vec![result("other-job", false, "unrelated")], ""),
            ("failure with null output", vec![
                call("test", "fuzukue.run_recipe", empty.clone()), result("test", false, "null"),
            ], "Failure\nfuzukue.run_recipe"),
            ("failure with duplicate", vec![
                call("test", "exec", empty.clone()), result("test", false, "  error: expected x  \nsecond line"),
                call("retry", "exec", empty), result("retry", false, "  error: expected x  \nretry"),
            ], "Failure\n  error: expected x  "),
            ("plan prose", vec![prose("plan", "次はテストが失敗した原因を追って、修正できる場所を見つけるねぇ。")], ""),
            ("quoted code", vec![prose("quote", "> Fixed the retry queue and confirmed all three test cases passed.")], ""),
            ("result prose across deltas", vec![prose("answer", "3 件"), prose("answer", "通過した")], "3 件通過した"),
        ];
        for (name, events, want) in examples {
            let mut batch = Batch::default();
            let mut digest = Digest::default();
            let now = Instant::now();
            for event in events {
                batch.observe(&event, SESSION, JOB, now, &mut digest);
            }
            batch.finish_prose(&mut digest);
            assert_eq!(digest.is_empty(), want.is_empty(), "{name}");
            assert_eq!(digest.render(), want, "{name}");
        }
    }

    fn update(job: &str, title: &str, text: &str) -> DeskTurn {
        DeskTurn::WorkerDigest(Digests::from([(job.into(), Digest {
            title: Some(title.into()), prose: text.into(), ..Default::default()
        })]))
    }

    fn failed(job: &str, text: &str) -> DeskTurn {
        DeskTurn::WorkerDigest(Digests::from([(job.into(), Digest {
            failures: VecDeque::from([text.into()]), ..Default::default()
        })]))
    }

    #[test]
    fn desk_limits_progress_per_thread_and_preserves_the_deadline_when_empty() {
        let mut desk = DeskQueue::default();
        let mut other_thread = DeskQueue::default();
        let now = Instant::now();
        desk.enqueue(update(JOB, "Tests", "3 tests passed"));
        let DeskTurn::WorkerDigest(first) = desk.next(now).unwrap().unwrap() else { panic!("missing digest") };
        desk.relay.sent(&first, now);
        assert!(matches!(desk.next(now), Ok(None)));
        assert!(desk.relay.cooling_down(now));

        let early = now + Duration::from_secs(89);
        let ready = now + THREAD_INTERVAL;
        desk.enqueue(update("other-job", "Cleanup", "Removed obsolete fixtures"));
        other_thread.enqueue(update(JOB, "Tests", "3 tests passed"));
        assert!(matches!(desk.next(early), Err(deadline) if deadline == ready));
        assert!(matches!(other_thread.next(early), Ok(Some(DeskTurn::WorkerDigest(_)))));
        assert!(matches!(desk.next(ready), Ok(Some(DeskTurn::WorkerDigest(_)))));
    }

    #[test]
    fn desk_merges_pending_jobs_into_one_turn_with_titles() {
        let mut desk = DeskQueue::default();
        let now = Instant::now();
        desk.enqueue(update(JOB, "Tests", "3 tests passed"));
        let DeskTurn::WorkerDigest(older) = desk.next(now).unwrap().unwrap() else { panic!("missing digest") };
        desk.enqueue(update("other-job", "Cleanup", "Removed obsolete fixtures"));
        desk.enqueue(update(JOB, "Tests", "4 tests passed"));
        desk.relay.restore(older);
        let DeskTurn::WorkerDigest(batch) = desk.next(now).unwrap().unwrap() else { panic!("missing digest") };
        assert_eq!(render(&batch),
            "## Tests (delegate:worker)\n4 tests passed\n\n## Cleanup (other-job)\nRemoved obsolete fixtures");
        assert!(matches!(desk.next(now), Ok(None)));
    }

    #[test]
    fn failures_bypass_the_thread_limit_with_one_per_job_every_twenty_seconds() {
        let mut desk = DeskQueue::default();
        let now = Instant::now();
        desk.enqueue(update(JOB, "Tests", "3 tests passed"));
        let DeskTurn::WorkerDigest(first) = desk.next(now).unwrap().unwrap() else { panic!("missing digest") };
        desk.relay.sent(&first, now);

        let failure_at = now + Duration::from_secs(4);
        desk.enqueue(failed(JOB, "first"));
        desk.enqueue(failed(JOB, "second"));
        let DeskTurn::WorkerDigest(batch) = desk.next(failure_at).unwrap().unwrap() else { panic!("missing failure") };
        assert_eq!(batch[JOB].render(), "Failure\nfirst");
        desk.relay.sent(&batch, failure_at);

        let ready = failure_at + FAILURE_INTERVAL;
        desk.enqueue(update(JOB, "Tests", "4 tests passed"));
        assert!(matches!(desk.next(ready - Duration::from_millis(1)), Err(deadline) if deadline == ready));
        desk.enqueue(failed("other-job", "other failure"));
        let DeskTurn::WorkerDigest(other) = desk.next(ready - Duration::from_secs(1)).unwrap().unwrap() else {
            panic!("missing failure from other job")
        };
        assert_eq!(other[JOB].render(), "4 tests passed");
        assert_eq!(other["other-job"].render(), "Failure\nother failure");
        desk.relay.sent(&other, ready - Duration::from_secs(1));

        let DeskTurn::WorkerDigest(later) = desk.next(ready).unwrap().unwrap() else { panic!("missing delayed failure") };
        assert_eq!(later[JOB].render(), "Failure\nsecond");
    }

    #[test]
    fn final_report_supersedes_its_pending_digests_and_bypasses_the_limit() {
        let mut desk = DeskQueue::default();
        let now = Instant::now();
        desk.enqueue(update(JOB, "Tests", "3 tests passed"));
        let DeskTurn::WorkerDigest(first) = desk.next(now).unwrap().unwrap() else { panic!("missing digest") };
        desk.relay.sent(&first, now);
        desk.enqueue(failed(JOB, "superseded"));
        desk.enqueue(update(JOB, "Tests", "4 tests passed"));
        desk.enqueue(update("other-job", "Cleanup", "Removed obsolete fixtures"));
        desk.enqueue(DeskTurn::Report { job: JOB.into(), text: "done".into(), plan_failed: false });
        desk.enqueue(update(JOB, "Tests", "late draft"));

        assert!(matches!(desk.next(now), Ok(Some(DeskTurn::Report { job, .. })) if job == JOB));
        assert!(matches!(desk.next(now), Err(deadline) if deadline == now + THREAD_INTERVAL));
        let DeskTurn::WorkerDigest(batch) = desk.next(now + THREAD_INTERVAL).unwrap().unwrap() else {
            panic!("the other job's digest should remain")
        };
        assert_eq!(batch.keys().map(String::as_str).collect::<Vec<_>>(), ["other-job"]);
    }

    #[tokio::test]
    async fn relay_turns_skip_the_nudge_and_leave_recent_requests_alone() -> anyhow::Result<()> {
        use crate::test_support::TestDir;
        use fuzukue_proto::AgentBackend;

        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots");
        for backend in AgentBackend::ALL {
            let project = TestDir::new();
            let state = TestDir::new();
            let mut app = App::new(project.path().into(), "127.0.0.1:0".into(), state.path().into(), Arc::new(crate::Hub::new()));
            app.claude_program = Some(fixtures.join("claude"));
            app.front_desk_nudge = true;
            let session = app.hub.edit_threads(|store| {
                let session = store.create_titled(None, Some("Relay fixture".into()))?;
                if backend == AgentBackend::Codex {
                    let settings = fuzukue_proto::ChatSettings { model: Some("gpt-6.2-sol".into()), ..Default::default() };
                    store.set_roles(&session, Some(settings), None)?;
                }
                Ok(session)
            })?;
            if backend == AgentBackend::Codex {
                let mut command = tokio::process::Command::new(fixtures.join("codex"));
                command.arg("app-server").env_remove("FUZUKUE_CODEX_SHOT");
                *app.codex.get_mut() = Some(crate::Codex::spawn_command(
                    command, &app.project_dir, app.hub.clone(), app.usage.clone(), app.session_stats.clone(),
                ).await?);
            }
            let app = Arc::new(app);
            app.hub.publish(crate::prompt_event(&session, Actor::User, "仮定を確かめて"));
            app.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: false });
            let (_, mut events) = app.hub.subscribe(u64::MAX);
            {
                let _start = app.prompt_start.lock().await;
                enqueue_desk(&app, &session, update(JOB, "Tests", "front-desk-test:promise"));
                enqueue_desk(&app, &session, update("other-job", "Cleanup", "Removed obsolete fixtures"));
            }
            let reply = tokio::time::timeout(Duration::from_secs(5), async {
                let mut reply = String::new();
                loop {
                    match events.recv().await?.event {
                        ServerEvent::TextDelta { session: id, actor, text, .. }
                            if id == session && crate::routing::is_partner(&actor) => reply.push_str(&text),
                        ServerEvent::TurnEnded { session: id, .. } if id == session => return anyhow::Ok(reply),
                        _ => {}
                    }
                }
            }).await??;
            assert!(crate::front_desk::needs_nudge(&reply, false, crate::front_desk::TurnKind::Conversation), "{backend:?}: {reply}");
            assert_eq!(app.hub.recent_requests(&session), ["仮定を確かめて"]);
            let extra_turn = tokio::time::timeout(Duration::from_millis(150), async {
                loop {
                    if matches!(events.recv().await.unwrap().event, ServerEvent::TurnActivity { session: id, running: true } if id == session) {
                        return;
                    }
                }
            }).await;
            assert!(extra_turn.is_err(), "unexpected follow-up turn for {backend:?}");
        }
        Ok(())
    }
}
