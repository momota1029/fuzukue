use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    HandoffWorker, HandoffWorkerMode as Mode, HandoffWorkerStatus as Status, ServerEvent,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use crate::{hub::Hub, Workspace};

static ADOPTIONS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

struct Adoption(String);

impl Adoption {
    fn reserve(id: &str) -> Result<Self> {
        ensure!(
            ADOPTIONS.lock().unwrap().insert(id.into()),
            "This Codex thread is already being adopted"
        );
        Ok(Self(id.into()))
    }
}

impl Drop for Adoption {
    fn drop(&mut self) {
        ADOPTIONS.lock().unwrap().remove(&self.0);
    }
}

pub(super) fn valid_thread_id(id: &str) -> bool {
    let lengths = [8, 4, 4, 4, 12];
    let parts = id.split('-').collect::<Vec<_>>();
    parts.len() == lengths.len()
        && parts
            .iter()
            .zip(lengths)
            .all(|(part, len)| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

pub(super) fn rollout(home: &Path, id: &str) -> Option<PathBuf> {
    ["sessions", "archived_sessions"]
        .into_iter()
        .find_map(|root| {
            walkdir::WalkDir::new(home.join(root))
                .max_depth(5)
                .into_iter()
                .filter_map(Result::ok)
                .find(|entry| {
                    entry.file_type().is_file()
                        && entry.file_name().to_str().is_some_and(|name| {
                            name.starts_with("rollout-")
                                && name
                                    .strip_suffix(".jsonl")
                                    .is_some_and(|name| name.ends_with(id))
                        })
                })
                .map(|entry| entry.into_path())
        })
}

#[derive(Default, PartialEq, Eq)]
struct Tail {
    running: Option<bool>,
    text: String,
}

fn tail(path: &Path) -> Result<Tail> {
    let bytes = super::transcript::last_lines(path)?;
    let mut tail = Tail::default();
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "event_msg" {
            match payload["type"].as_str() {
                Some("task_started" | "turn_started") => {
                    tail.running = Some(true);
                    tail.text.clear();
                }
                Some("task_complete" | "turn_complete" | "turn_aborted") => {
                    tail.running = Some(false);
                    if let Some(text) = payload["last_agent_message"].as_str() {
                        tail.text = text.into();
                    }
                }
                Some("agent_message") => {
                    if let Some(text) = payload["message"].as_str() {
                        tail.text = text.into();
                    }
                }
                _ => {}
            }
        }
    }
    if tail.text.chars().count() > 12_000 {
        tail.text = tail.text.chars().take(12_000).collect();
    }
    Ok(tail)
}

pub(crate) fn update(hub: &Hub, session: &str, thread_id: &str, status: Status) {
    hub.publish(ServerEvent::HandoffWorkerUpdated {
        session: session.into(),
        thread_id: thread_id.into(),
        status,
    });
}

pub(super) async fn start(
    app: &Arc<Workspace>,
    session: String,
    worker: HandoffWorker,
    mode: Mode,
) -> Result<()> {
    let thread_id = worker.thread_id.clone();
    let key = (session.clone(), thread_id.clone());
    if matches!(mode, Mode::Stop) {
        if let Some(cancel) = app.handoff.monitors.lock().unwrap().get(&key) {
            cancel.notify_one();
        }
        return Ok(());
    }
    if matches!(mode, Mode::Adopt) {
        let app = app.clone();
        tokio::spawn(async move {
            let result = async {
                let _adoption = Adoption::reserve(&thread_id)?;
                update(&app.hub, &session, &thread_id, Status::Adopting);
                // The app-server owns the writer lock. An unfinished marker can
                // remain in a rollout whose originating process has already exited.
                let cwd = if let Some(tail) = worker.worktree.strip_prefix("~/") {
                    PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?).join(tail)
                } else {
                    app.project_dir.join(&worker.worktree)
                }
                .canonicalize()
                .context("Worker worktree is unavailable")?;
                ensure!(!worker.worktree.is_empty(), "Missing worker worktree");
                let repo = crate::git::Repo::open(&app.project_dir).await?;
                let entries = repo.git(&["worktree", "list", "--porcelain", "-z"]).await?;
                ensure!(
                    entries
                        .split('\0')
                        .filter_map(|field| field.strip_prefix("worktree "))
                        .any(|path| Path::new(path)
                            .canonicalize()
                            .is_ok_and(|registered| registered == cwd)),
                    "Worker path is not a worktree of this workspace repository"
                );
                let job = crate::delegate::adopt(&app, &session, worker, cwd).await?;
                // The delegate publishes its own result before dropping the reservation.
                let _ = job.await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                update(
                    &app.hub,
                    &session,
                    &thread_id,
                    Status::Failed(format!("{error:#}")),
                );
            }
        });
        return Ok(());
    }

    let home = codex_home().context("Codex home directory is unavailable")?;
    let cancel = Arc::new(tokio::sync::Notify::new());
    {
        let mut monitors = app.handoff.monitors.lock().unwrap();
        ensure!(
            !monitors.contains_key(&key),
            "A rollout monitor is already open for this worker"
        );
        monitors.insert(key.clone(), cancel.clone());
    }
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let waiting = matches!(mode, Mode::Wait);
        let Some(workspace) = app.upgrade() else {
            return;
        };
        update(
            &workspace.hub,
            &session,
            &thread_id,
            if waiting {
                Status::Waiting
            } else {
                Status::Viewing
            },
        );
        drop(workspace);
        let result = tokio::select! {
            biased;
            _ = cancel.notified() => Ok(None),
            result = monitor(&app, &session, &thread_id, home, waiting) => result,
        };
        if let Some(app) = app.upgrade() {
            app.handoff.monitors.lock().unwrap().remove(&key);
            let status = match result {
                Ok(Some(text)) => {
                    crate::delegate::report_handoff(
                        &app,
                        &session,
                        thread_id.clone(),
                        text.clone(),
                    );
                    Status::Finished(text)
                }
                Ok(None) => Status::Stopped,
                Err(error) => Status::Failed(format!("{error:#}")),
            };
            update(&app.hub, &session, &thread_id, status);
        }
    });
    Ok(())
}

async fn monitor(
    app: &std::sync::Weak<Workspace>,
    session: &str,
    id: &str,
    home: PathBuf,
    waiting: bool,
) -> Result<Option<String>> {
    let mut path = None;
    let mut previous = Tail::default();
    loop {
        let Some(app) = app.upgrade() else {
            return Ok(None);
        };
        let (root, id_to_find, cached) = (home.clone(), id.to_owned(), path.take());
        let (found, current) = tokio::task::spawn_blocking(move || {
            let found = cached
                .filter(|path: &PathBuf| path.is_file())
                .or_else(|| rollout(&root, &id_to_find));
            let current = found.as_deref().map(tail).transpose();
            (found, current)
        })
        .await?;
        path = found;
        if let Some(current) = current? {
            if waiting && current.running == Some(false) {
                let text = harness_format!(
                    "The source Codex turn for {} has ended. Last report:\n\n{}",
                    "{} の元の Codex ターンが終了した。最後の報告:\n\n{}",
                    id,
                    current.text,
                );
                return Ok(Some(text));
            }
            if !current.text.is_empty() && current.text != previous.text {
                update(&app.hub, session, id, Status::Output(current.text.clone()));
            }
            previous = current;
        }
        drop(app);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tails_latest_turn_and_ignores_incomplete_rows() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let id = "00000000-0000-4000-8000-000000000001";
        let name = format!("sessions/2026/11/21/rollout-2026-11-21T00-00-00-{id}.jsonl");
        dir.write(&name, concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"old\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"working\"}}\n",
        ));
        let path = rollout(dir.path(), id).context("Missing fixture rollout")?;
        let current = tail(&path)?;
        assert_eq!(current.running, Some(true));
        assert_eq!(current.text, "working");
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&path)?,
            "{}",
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}"#
        )?;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"{")?;
        let complete = tail(&path)?;
        assert_eq!(complete.running, Some(false));
        assert_eq!(complete.text, "done");
        Ok(())
    }
}
