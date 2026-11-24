//! Review one live approval raised by a delegated worker.

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuzukue_proto::{Actor, ServerEvent, WorkerApprovalPolicy};
use serde::Deserialize;
use serde_json::{json, Value};

use super::touches::WriteSet;
use crate::worker_scope::protected_roots;
use crate::{Hub, Workspace};

const TIMEOUT: Duration = Duration::from_secs(25);
const RULES: &str = "Your review applies to one structured file read inside the assigned worktree. Always escalate requests involving Fuzukue configuration or state, writes outside the worktree or declared touches, writes or execution in read-only roots, Git commits or pushes, network egress, service installation or removal, destructive deletion of non-temporary files, server restarts or swaps, GitHub writes, publication, signing, and human-only operations.";

pub(super) struct JobScope {
    pub title: String,
    pub prompt: String,
    pub root: PathBuf,
    pub writes: Arc<WriteSet>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Owner {
    session: String,
    job: String,
    backend_id: String,
}

impl Owner {
    pub(super) fn new(session: &str, job: &str, backend_id: &str) -> Self {
        Self { session: session.into(), job: job.into(), backend_id: backend_id.into() }
    }

    pub(super) fn session(&self) -> &str {
        &self.session
    }

    pub(super) fn job(&self) -> &str {
        &self.job
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Accept,
    Decline,
    Escalate,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    decision: Verdict,
    reason: String,
}

pub(super) struct Decision {
    request_id: String,
    owner: Owner,
    policy: WorkerApprovalPolicy,
    answer: Answer,
    path: PathBuf,
}

impl Decision {
    pub(super) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(super) fn owner(&self) -> &Owner {
        &self.owner
    }

    fn value(&self) -> &'static str {
        match self.answer.decision {
            Verdict::Accept => "accept",
            Verdict::Decline => "decline",
            Verdict::Escalate => unreachable!("escalated requests remain pending"),
        }
    }

    pub(super) fn audit(&self, hub: &Hub) {
        let who = match self.policy {
            WorkerApprovalPolicy::Advisor => "Front desk advisor",
            _ => "Workspace worker approval policy (auto)",
        };
        let session = &self.owner.session;
        let item = format!("{}:approval:{}", self.owner.job, self.request_id);
        let reason = &self.answer.reason;
        hub.publish(ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), actor: Actor::System,
            name: "worker_approval".into(),
            input: json!({
                "automatic": true, "decision": self.value(), "decided_by": who,
                "reason": reason, "rule": "structured worktree file read", "request_id": self.request_id,
                "operation": format!("Read {}", self.path.display()),
            }),
        });
        hub.publish(ServerEvent::ToolResult {
            session: session.clone(), item, ok: true,
            output: format!("{who} · {} · {reason}", self.value()),
        });
    }
}

async fn decide<Fut>(
    policy: WorkerApprovalPolicy, timeout: Duration,
    advise: impl FnOnce() -> Fut,
) -> Option<Answer>
where
    Fut: Future<Output = anyhow::Result<String>>,
{
    match policy {
        WorkerApprovalPolicy::Ask => None,
        WorkerApprovalPolicy::Auto => Some(Answer {
            decision: Verdict::Accept,
            reason: "The workspace policy automatically allows eligible file reads.".into(),
        }),
        WorkerApprovalPolicy::Advisor => {
            let reply = tokio::time::timeout(timeout, advise()).await.ok()?.ok()?;
            let mut answer = serde_json::from_str::<Answer>(&reply).ok()?;
            answer.reason = answer.reason.trim().to_owned();
            (answer.decision != Verdict::Escalate && !answer.reason.is_empty() && answer.reason.len() <= 1200)
                .then_some(answer)
        }
    }
}

pub(super) async fn request(
    app: &Workspace, owner: Owner, tool: &str, input: &Value, human_only: bool,
) -> anyhow::Result<String> {
    let session = owner.session.clone();
    request_with(app, owner, tool, input, human_only, |data| async move {
        crate::planner::worker_approval(app, &session, &data, RULES, Duration::from_secs(12)).await
    }).await
}

async fn request_with<Fut>(
    app: &Workspace, owner: Owner, tool: &str, input: &Value, human_only: bool,
    advise: impl FnOnce(Value) -> Fut,
) -> anyhow::Result<String>
where
    Fut: Future<Output = anyhow::Result<String>>,
{
    // Register the human waiter first. This is a live approval throughout review and drain.
    let (wait, mut reply) = super::open_approval(app, &owner.session, &owner.job, Actor::Claude,
        format!("{tool} {input}").into(), Some(owner.clone()));
    let policy = *app.worker_approval_policy.lock().unwrap();
    let path = || {
        let scope = app.delegates.approval_scope(&owner.session, &owner.job)?;
        let path = checked_read(tool, input, human_only, &scope.root, &protected_roots(&app.state_dir))?;
        Some((scope, path))
    };
    if policy != WorkerApprovalPolicy::Ask {
        if let Some((scope, reviewed_path)) = path() {
            let answer = tokio::select! {
                biased;
                reply = &mut reply => return Ok(reply?.decision),
                answer = decide(policy, TIMEOUT, || advise(json!({
                    "request_id": wait.id, "job": owner.job, "title": scope.title, "assignment": scope.prompt,
                    "operation": "Read", "path": reviewed_path, "input": input,
                    "worktree": scope.root, "declared_touches": scope.writes.declared_globs(),
                }))) => answer,
            };
            if let Some(answer) = answer {
                // The policy and paths can change while the advisor is running. The same
                // lock that consumes a human reply arbitrates the exact pending request.
                let current = app.worker_approval_policy.lock().unwrap();
                if *current == policy && path().is_some_and(|(_, path)| path == reviewed_path) {
                    let review = Decision { request_id: wait.id.clone(), owner, policy, answer, path: reviewed_path };
                    app.delegates.complete_approval(&wait.id, review.value(), Some(review))?;
                }
            }
        }
    }
    Ok(reply.await?.decision)
}

fn checked_read(tool: &str, input: &Value, human_only: bool, root: &Path, protected: &[PathBuf]) -> Option<PathBuf> {
    // Permission for a shell or a write can lift backend protections. Only Read with
    // a single file operand and known read-only options has sufficient metadata.
    if human_only || tool != "Read" || !input.as_object()?.iter().all(|(key, value)| {
        match key.as_str() {
            "file_path" => value.is_string(),
            "offset" | "limit" => value.as_u64().is_some(),
            _ => false,
        }
    }) {
        return None;
    }
    let path = input["file_path"].as_str()?;
    if path.is_empty() || path.starts_with('~') || path.len() > 4096 || path.chars().any(char::is_control) {
        return None;
    }
    let path = Path::new(path);
    if path.components().any(|part| !matches!(part, Component::RootDir | Component::Prefix(_) | Component::CurDir | Component::Normal(_))) {
        return None;
    }
    let path = root.join(path);
    let relative = path.strip_prefix(root).ok()?;
    if relative.components().any(|part| part.as_os_str() == ".git") {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    if canonical != path || protected.iter().any(|root| canonical.starts_with(root)) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return None;
        }
    }
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegate::RunningJob;
    use anyhow::Result;
    use futures_util::poll;
    use std::task::Poll;

    const SESSION: &str = "worker-approval-test";
    const JOB: &str = "delegate:fixture";

    fn fixture(policy: WorkerApprovalPolicy) -> Result<(tempfile::TempDir, Workspace)> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("worktree");
        let state = temp.path().join("state");
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&state)?;
        std::fs::write(root.join("README.md"), "worktree file")?;
        std::fs::write(state.join("private.txt"), "state")?;
        crate::state::save_worker_approval_policy(&state, policy)?;
        let hub = Arc::new(Hub::with_timeline(&state));
        let app = Workspace::new(root.clone(), String::new(), state, hub);
        let writes = Arc::new(WriteSet::new(&root, Some(vec!["README.md".into()]), "", || None)?);
        app.delegates.running.lock().unwrap().insert(JOB.into(), Arc::new(RunningJob {
            session: SESSION.into(), key: String::new(), approach: String::new(),
            summary: Default::default(), retry: Default::default(), tools: Vec::new(),
            input: Default::default(), cancel: Default::default(), started: true.into(),
            drain_notice: Default::default(), capture_scope: Default::default(),
            workspace_read_scope: Default::default(),
            workspace_write_scope: Default::default(),
            approval_scope: std::sync::Mutex::new(Some(Arc::new(JobScope {
                title: "Read project notes".into(), prompt: "Inspect README.md.".into(), root, writes,
            }))),
        }));
        Ok((temp, app))
    }

    fn pending(app: &Workspace) -> String {
        let (events, _) = app.hub.subscribe_client(0, true);
        let mut requests = events.into_iter().filter_map(|env| match env.event {
            ServerEvent::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        });
        let id = requests.next().expect("live approval is replayed");
        assert!(requests.next().is_none());
        assert!(app.hub.live_approval_threads().contains(SESSION));
        id
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn remember_shell_requires_a_live_eligible_request_and_replies_with_checked_input() -> Result<()> {
        let (_temp, app) = fixture(WorkerApprovalPolicy::Ask)?;
        let root = &app.project_dir;
        let scope = crate::worker_scope::Scope::open(root, root, root)?;
        let shell = |command: &str| {
            super::super::request_shell_approval(&app, SESSION, JOB, "backend", &scope, json!({"command": command}))
        };
        let rejected = shell("rm README.md");
        tokio::pin!(rejected);
        assert!(poll!(rejected.as_mut()).is_pending());
        let rejected_id = pending(&app);
        assert!(crate::remember_shell_approval(&app, &rejected_id, None).await.is_err());
        assert!(app.shell_approval_rules.list().is_empty());
        assert!(app.delegates.resolve_approval(&rejected_id, "decline", false)?);
        assert!(rejected.await?.is_none());

        let request = shell("cat README.md");
        tokio::pin!(request);
        assert!(poll!(request.as_mut()).is_pending());
        let id = pending(&app);
        let events = app.hub.subscribe_client(0, true).0;
        assert!(events.iter().any(|env| matches!(&env.event, ServerEvent::ApprovalRequested {
            request_id, shell_directory: Some(directory), shell_prefix: Some(prefix), ..
        } if request_id == &id && directory == root.to_str().unwrap() && prefix == "cat")));
        assert!(crate::remember_shell_approval(&app, &id, Some("sh")).await.is_err());
        assert!(app.shell_approval_rules.list().is_empty());
        crate::remember_shell_approval(&app, &id, Some("cat")).await?;
        assert!(request.await?.unwrap()["command"].as_str().unwrap().contains("/usr/bin/cat"));
        assert!(crate::remember_shell_approval(&app, &id, Some("cat")).await.is_err());
        let saved = app.shell_approval_rules.list();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].prefix.as_deref(), Some("cat"));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn worker_approval_matrix_uses_only_eligible_actions_and_fails_closed() -> Result<()> {
        use WorkerApprovalPolicy::*;
        let read = json!({"file_path": "README.md"});
        let prohibited = [
            ("Read", json!({"file_path": "../state/private.txt"}), false),
            ("Write", json!({"file_path": "../outside.md"}), false),
            ("Edit", json!({"file_path": "undeclared.md"}), false),
            ("Write", json!({"file_path": "README.md"}), false),
            ("Write", json!({"file_path": "/home/owner/labo/card.md"}), false),
            ("Bash", json!({"command": "cd ~/projects/compiler && ./tool"}), false),
            ("Bash", json!({"command": "git commit -m draft"}), false),
            ("Bash", json!({"command": "git push"}), false),
            ("Bash", json!({"command": "curl https://example.invalid"}), false),
            ("Bash", json!({"command": "systemctl enable secretaire"}), false),
            ("Bash", json!({"command": "systemctl disable secretaire"}), false),
            ("Bash", json!({"command": "rm -rf notes"}), false),
            ("Bash", json!({"command": "systemctl restart secretaire"}), false),
            ("Bash", json!({"command": "secretaire --swap"}), false),
            ("Bash", json!({"command": "gh issue edit 1"}), false),
            ("Bash", json!({"command": "cargo publish"}), false),
            ("Bash", json!({"command": "gpg --sign artifact"}), false),
            ("Bash", json!({"command": "cargo build"}), false),
            ("Bash", json!({"command": "cargo test"}), false),
            ("Bash", json!({"command": "/usr/bin/cat README.md"}), false),
            ("mcp__fuzukue__file_inbox", json!({}), true),
            ("Read", read.clone(), true),
            ("Read", json!({"file_path": "README.md", "extra": "unrecognized"}), false),
        ];
        let replies = [
            ("accept", Some(r#"{"decision":"accept","reason":"The assignment names this file."}"#)),
            ("decline", Some(r#"{"decision":"decline","reason":"Already checked."}"#)),
            ("human", Some(r#"{"decision":"escalate","reason":"Need the human."}"#)),
            ("human", Some(r#"{"decision":"accept","reason":" "}"#)),
            ("human", Some(r#"{"decision":"accept","reason":"OK","scope":"session"}"#)),
            ("human", Some("malformed response")),
            ("human", Some("")),
            ("human", None),
        ];
        for policy in [Ask, Advisor, Auto] {
            let (_temp, app) = fixture(policy)?;
            for (tool, input, human_only, eligible) in prohibited.iter().map(|(tool, input, human_only)| {
                (*tool, input, *human_only, false)
            }).chain(std::iter::once(("Read", &read, false, true))) {
                for (advisor_outcome, answer) in replies {
                    let calls = std::cell::Cell::new(0);
                    let request = request_with(&app, Owner::new(SESSION, JOB, "backend-request"),
                        tool, input, human_only, |data| {
                            calls.set(calls.get() + 1);
                            assert_eq!(data["operation"], "Read");
                            async move {
                                match answer {
                                    Some("") => anyhow::bail!("fixture advisor error"),
                                    Some(answer) => Ok(answer.to_owned()),
                                    None => std::future::pending().await,
                                }
                            }
                        });
                    tokio::pin!(request);
                    let expected = match (policy, eligible) {
                        (Auto, true) => "accept",
                        (Advisor, true) => advisor_outcome,
                        _ => "human",
                    };
                    if expected == "human" {
                        assert!(poll!(request.as_mut()).is_pending());
                        if answer.is_none() {
                            tokio::time::advance(TIMEOUT).await;
                            assert!(poll!(request.as_mut()).is_pending());
                        }
                        let id = pending(&app);
                        assert!(app.delegates.resolve_approval(&id, "decline", false)?);
                        assert_eq!(request.await?, "decline");
                        assert!(!app.delegates.resolve_approval(&id, "accept", false)?);
                    } else {
                        let Poll::Ready(result) = poll!(request.as_mut()) else { panic!("automatic result pending") };
                        assert_eq!(result?, expected);
                    }
                    assert_eq!(calls.get(), u32::from(policy == Advisor && eligible));
                    assert!(app.hub.live_approval_threads().is_empty());
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn checked_reads_reject_protected_paths_links_and_nonfiles() -> Result<()> {
        let (_temp, app) = fixture(WorkerApprovalPolicy::Ask)?;
        let root = &app.project_dir;
        let checked = |name: &str, protected: &[PathBuf]| checked_read("Read", &json!({"file_path": name}), false, root, protected);
        assert!(checked("README.md", &[]).is_some());
        assert!(checked("README.md", &[root.clone()]).is_none());
        assert!(checked(".", &[]).is_none());
        assert!(checked("../state/private.txt", &[]).is_none());
        assert!(checked("missing.md", &[]).is_none());
        std::os::unix::fs::symlink(root.join("README.md"), root.join("alias.md"))?;
        std::os::unix::fs::symlink(&app.state_dir, root.join("alias-state"))?;
        assert!(checked("alias.md", &[]).is_none());
        assert!(checked("alias-state/private.txt", &[]).is_none());
        std::fs::hard_link(root.join("README.md"), root.join("linked.md"))?;
        assert!(checked("README.md", &[]).is_none());
        assert!(checked("linked.md", &[]).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn human_reply_wins_and_advisor_audit_replays_for_the_exact_request() -> Result<()> {
        let (_temp, app) = fixture(WorkerApprovalPolicy::Advisor)?;
        let read = json!({"file_path":"README.md"});
        let (release, advisor) = tokio::sync::oneshot::channel();
        let first = request_with(&app, Owner::new(SESSION, JOB, "backend-id"), "Read", &read, false, |_| async {
            Ok(advisor.await?)
        });
        tokio::pin!(first);
        assert!(poll!(first.as_mut()).is_pending());
        let first_id = pending(&app);
        assert!(app.delegates.resolve_approval(&first_id, "accept", false)?);
        assert!(release.send(r#"{"decision":"decline","reason":"Late reply"}"#.into()).is_ok());
        assert_eq!(first.await?, "accept");

        let (release, advisor) = tokio::sync::oneshot::channel();
        let second = request_with(&app, Owner::new(SESSION, JOB, "backend-id"), "Read", &read, false, |data| async move {
            assert_eq!(data["declared_touches"], json!(["README.md"]));
            Ok(advisor.await?)
        });
        tokio::pin!(second);
        assert!(poll!(second.as_mut()).is_pending());
        let second_id = pending(&app);
        assert_ne!(second_id, first_id);
        assert!(!app.delegates.resolve_approval(&first_id, "accept", false)?);
        assert!(release.send(r#"{"decision":"decline","reason":"Already checked."}"#.into()).is_ok());
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), second).await??, "decline");
        assert!(!app.delegates.resolve_approval(&second_id, "accept", false)?);
        assert!(app.hub.live_approval_threads().is_empty());

        let reopened = Hub::with_timeline(&app.state_dir);
        let (events, _) = reopened.subscribe(0);
        let audit: Vec<_> = events.iter().filter_map(|event| match &event.event {
            ServerEvent::ToolCall { name, item, input, .. } if name == "worker_approval" => Some((item, input)),
            _ => None,
        }).collect();
        assert_eq!(audit.len(), 1);
        let (item, input) = audit[0];
        assert_eq!(input["request_id"], second_id);
        assert_eq!(input["decision"], "decline");
        assert_eq!(input["automatic"], true);
        assert_eq!(input["decided_by"], "Front desk advisor");
        assert_eq!(input["reason"], "Already checked.");
        assert!(events.iter().any(|event| matches!(&event.event, ServerEvent::ToolResult { item: id, output, .. }
            if id == item && output.contains("Already checked."))));
        assert!(events.iter().any(|event| matches!(&event.event, ServerEvent::ApprovalResolved { request_id, decision, automatic }
            if request_id == &second_id && decision == "decline" && *automatic)));
        Ok(())
    }

    #[tokio::test]
    async fn changing_policy_during_review_leaves_the_approval_pending() -> Result<()> {
        let (_temp, app) = fixture(WorkerApprovalPolicy::Advisor)?;
        let read = json!({"file_path": "README.md"});
        let (release, advisor) = tokio::sync::oneshot::channel();
        let request = request_with(&app, Owner::new(SESSION, JOB, "read"), "Read", &read, false, |_| async {
            Ok(advisor.await?)
        });
        tokio::pin!(request);
        assert!(poll!(request.as_mut()).is_pending());
        let id = pending(&app);
        *app.worker_approval_policy.lock().unwrap() = WorkerApprovalPolicy::Ask;
        assert!(release.send(r#"{"decision":"accept","reason":"Relevant."}"#.into()).is_ok());
        assert!(poll!(request.as_mut()).is_pending());
        assert!(app.delegates.resolve_approval(&id, "decline", false)?);
        assert_eq!(request.await?, "decline");
        Ok(())
    }
}
