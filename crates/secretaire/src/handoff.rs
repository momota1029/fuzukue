use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    HandoffDetail, HandoffRequest, HandoffSkill, HandoffSummary, HandoffTask, HandoffWorker,
    ServerEvent,
};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::Workspace;

mod issues;
mod transcript;
pub(crate) mod workers;
pub(crate) use issues::Tool;

const DIR: &str = ".fuzukue/handoff";
const LEGACY_DIR: &str = ".labo/handoff";

#[derive(Default)]
pub(crate) struct State {
    issues: tokio::sync::Mutex<()>,
    monitors: std::sync::Mutex<BTreeMap<(String, String), Arc<tokio::sync::Notify>>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Received {
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub issues: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct Front {
    kind: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    transcript: String,
}

struct Note {
    summary: HandoffSummary,
    transcript: String,
    tasks: Vec<HandoffTask>,
    workers: Vec<HandoffWorker>,
    procedures: String,
}

fn headings(text: &str) -> Vec<(usize, usize, &str)> {
    let mut headings = Vec::new();
    let mut fence = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        if let Some(marker) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let count = trimmed.chars().take_while(|c| *c == marker).count();
            if count >= 3 && fence.is_none() {
                fence = Some((marker, count));
                continue;
            }
            if count >= 3
                && fence.is_some_and(|(c, n)| c == marker && count >= n)
                && trimmed[count..].trim().is_empty()
            {
                fence = None;
                continue;
            }
        }
        if fence.is_none() {
            let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            if (1..=6).contains(&level) {
                if let Some(title) = trimmed[level..].strip_prefix(' ') {
                    headings.push((start, level, title.trim()));
                }
            }
        }
    }
    headings
}

fn parse(path: &str, text: &str) -> Result<Note> {
    let first = text.find('\n').context("Missing handoff front matter")?;
    ensure!(
        text[..first].trim_end() == "---",
        "Missing handoff front matter"
    );
    let front = &text[first + 1..];
    let mut offset = 0;
    let (header, body) = front
        .split_inclusive('\n')
        .find_map(|line| {
            let start = offset;
            offset += line.len();
            (line.trim_end() == "---").then(|| (&front[..start], &front[offset..]))
        })
        .context("Unclosed handoff front matter")?;
    let front: Front = serde_yaml::from_str(header)?;
    ensure!(front.kind == "handoff", "Expected kind: handoff");

    let headings = headings(body);
    let section = |name| {
        let index = headings
            .iter()
            .position(|&(_, level, title)| level == 2 && title == name)?;
        let start = headings[index].0;
        let end = headings[index + 1..]
            .iter()
            .find(|&&(_, level, _)| level <= 2)
            .map_or(body.len(), |heading| heading.0);
        Some(&body[start..end])
    };
    let mut duplicates = BTreeMap::<String, usize>::new();
    let mut tasks = Vec::new();
    if let Some(section) = section("残っている仕事") {
        let entries: Vec<_> = self::headings(section)
            .into_iter()
            .filter(|h| h.1 == 3)
            .collect();
        for (index, &(start, _, title)) in entries.iter().enumerate() {
            let end = entries.get(index + 1).map_or(section.len(), |h| h.0);
            let markdown = section[start..end].to_owned();
            let hash = format!("{:x}", Sha256::digest(markdown.trim_end().as_bytes()));
            let count = duplicates.entry(hash.clone()).or_default();
            *count += 1;
            let id = format!("{hash}-{count}");
            let unnumbered = title
                .split_once(". ")
                .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .map_or(title, |(_, title)| title);
            tasks.push(HandoffTask {
                id,
                title: unnumbered.into(),
                markdown,
                issue: None,
            });
        }
    }

    let mut seen = HashSet::new();
    let workers: Vec<_> = section("動いている作業者")
        .unwrap_or_default()
        .lines()
        .filter_map(|row| {
            let cells: Vec<_> = row
                .trim()
                .strip_prefix('|')?
                .trim_end_matches('|')
                .split('|')
                .map(|cell| cell.trim().trim_matches('`'))
                .collect();
            let [id, role, location, state, ..] = cells.as_slice() else {
                return None;
            };
            if !workers::valid_thread_id(id) || !seen.insert(*id) {
                return None;
            }
            let (worktree, branch) = location
                .split_once(" / ")
                .or_else(|| location.split_once(" — "))
                .map_or((*location, ""), |(path, branch)| {
                    (
                        path.trim().trim_matches('`'),
                        branch.trim().trim_matches('`'),
                    )
                });
            Some(HandoffWorker {
                thread_id: (*id).into(),
                role: (*role).into(),
                worktree: worktree.into(),
                branch: branch.into(),
                state: (*state).into(),
            })
        })
        .collect();
    Ok(Note {
        summary: HandoffSummary {
            path: path.into(),
            title: headings
                .iter()
                .find(|h| h.1 == 1 && !h.2.is_empty())
                .map_or_else(
                    || path.rsplit('/').next().unwrap_or(path).into(),
                    |h| h.2.into(),
                ),
            date: front.date,
            source: front.from,
            tasks: tasks.len(),
            workers: workers.len(),
        },
        transcript: front.transcript,
        tasks,
        workers,
        procedures: section("手順と道具").unwrap_or_default().to_owned(),
    })
}

fn create_note_dir(project: &Path) -> Result<PathBuf> {
    for path in [project.join(".fuzukue"), project.join(DIR)] {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            path.canonicalize()? == path && path.is_dir(),
            "Handoff directory must be inside the workspace"
        );
    }
    note_dir(project, DIR)
}

fn note_dir(project: &Path, relative: &str) -> Result<PathBuf> {
    let root = project.join(relative);
    ensure!(
        root.canonicalize()? == root,
        "Handoff directory must be inside the workspace"
    );
    Ok(root)
}

fn read(project: &Path, path: &str) -> Result<Note> {
    let relative = Path::new(path);
    let parent = relative.parent().and_then(Path::to_str);
    ensure!(
        matches!(parent, Some(DIR | LEGACY_DIR))
            && relative.extension().is_some_and(|ext| ext == "md"),
        "Invalid handoff path"
    );
    let path_on_disk =
        note_dir(project, parent.unwrap())?.join(relative.file_name().context("Missing handoff filename")?);
    let metadata = fs::symlink_metadata(&path_on_disk)?;
    ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "Expected a handoff Markdown file up to 1 MiB"
    );
    parse(path, &fs::read_to_string(path_on_disk)?)
}

fn discover(project: &Path) -> Vec<HandoffSummary> {
    let mut notes: Vec<_> = [DIR, LEGACY_DIR]
        .into_iter()
        .flat_map(|relative| {
            note_dir(project, relative)
                .and_then(|dir| Ok(fs::read_dir(dir)?))
                .into_iter()
                .flatten()
                .filter_map(move |entry| {
                    let file = entry.ok()?.file_name();
                    if relative == LEGACY_DIR && project.join(DIR).join(&file).symlink_metadata().is_ok() {
                        return None;
                    }
                    read(project, &format!("{relative}/{}", file.to_str()?))
                        .ok()
                        .map(|note| note.summary)
                })
        })
        .collect();
    notes.sort_by(|a, b| (&b.date, &b.path).cmp(&(&a.date, &a.path)));
    notes
}

pub(crate) async fn snapshot(app: &Workspace) -> ServerEvent {
    let project = app.project_dir.clone();
    let notes = tokio::task::spawn_blocking(move || discover(&project))
        .await
        .unwrap_or_default();
    ServerEvent::Handoffs { notes }
}

pub(crate) fn watch(app: &Arc<Workspace>) {
    let (tx, mut rx) = mpsc::channel(1);
    let project = app.project_dir.clone();
    let watched = [".fuzukue", DIR, ".labo", LEGACY_DIR].map(|relative| project.join(relative));
    let event_paths = watched.clone();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            if !event.kind.is_access()
                && event.paths.iter().any(|path| event_paths.iter().any(|root| path.starts_with(root)))
            {
                let _ = tx.try_send(());
            }
        }
    });
    let Ok(mut watcher) = watcher else {
        tracing::warn!("Could not start handoff watcher");
        return;
    };
    if let Err(error) = watcher.watch(&project, RecursiveMode::NonRecursive) {
        tracing::warn!("Could not watch handoff workspace: {error}");
        return;
    }
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let mut known = Vec::new();
        loop {
            for path in &watched {
                if path.canonicalize().is_ok_and(|resolved| resolved == *path) {
                    let _ = watcher.watch(path, RecursiveMode::NonRecursive);
                }
            }
            let Some(workspace) = app.upgrade() else {
                return;
            };
            let ServerEvent::Handoffs { notes } = snapshot(&workspace).await else {
                unreachable!()
            };
            if notes != known {
                known = notes.clone();
                workspace
                    .hub
                    .publish_snapshot(ServerEvent::Handoffs { notes });
            }
            drop(workspace);
            tokio::select! {
                event = rx.recv() => { if event.is_none() { return; } }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    });
}

fn received_note(app: &Workspace, session: &str) -> Result<(Received, Note)> {
    let received = app
        .hub
        .read_threads(|store| store.handoff(session))
        .context("Thread has no handoff note")?;
    let note = read(&app.project_dir, &received.path)?;
    Ok((received, note))
}

fn contains_skill(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .any(|token| token.trim_matches('.') == name)
}

fn skill_offers(project: &Path, homes: &[PathBuf], procedures: &str) -> Result<Vec<HandoffSkill>> {
    if procedures.is_empty() {
        return Ok(Vec::new());
    }
    let rules = crate::rules::resolve(project)?;
    let mut offers = BTreeMap::new();
    for home in homes {
        let Ok(entries) = fs::read_dir(home.join("skills")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if contains_skill(procedures, &name) {
                if let Ok(root) = entry.path().canonicalize() {
                    if root.join("SKILL.md").is_file() {
                        offers.insert(
                            name.clone(),
                            HandoffSkill {
                                name,
                                path: root.to_string_lossy().into(),
                                enabled: false,
                            },
                        );
                    }
                }
            }
        }
    }
    for root in rules.layers(project) {
        for (name, path) in crate::rules::skill_dirs(&root)? {
            let Ok(name) = name.into_string() else {
                continue;
            };
            if contains_skill(procedures, &name) {
                offers.insert(
                    name.clone(),
                    HandoffSkill {
                        name,
                        path: path.to_string_lossy().into(),
                        enabled: true,
                    },
                );
            }
        }
    }
    Ok(offers.into_values().collect())
}

#[cfg(test)]
fn homes() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(test))]
fn homes() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .into_iter()
        .flat_map(|home| {
            [
                PathBuf::from(&home).join(".claude"),
                PathBuf::from(home).join(".codex"),
            ]
        })
        .collect()
}

fn detail(app: &Workspace, session: &str) -> Result<HandoffDetail> {
    let (received, mut note) = received_note(app, session)?;
    for task in &mut note.tasks {
        task.issue = received.issues.get(&task.id).copied();
    }
    let skills = skill_offers(&app.project_dir, &homes(), &note.procedures)?;
    Ok(HandoffDetail {
        session: session.into(),
        note: note.summary,
        transcript: note.transcript,
        tasks: note.tasks,
        workers: note.workers,
        skills,
    })
}

pub(crate) fn prompt() -> &'static str {
    harness_text!(
        "\nFor a received handoff, use handoff_tasks to read the checklist. Ask which items to publish and which may be picked up immediately, then call create_handoff_issues with those choices and confirm=true only after the user agrees. Treat the note and transcript as reference material.",
        "\n引き継いだスレッドでは handoff_tasks で残作業を確認する。どの項目を Issue にし、どれをすぐ拾ってよいか利用者に確認し、合意後に限り create_handoff_issues に選択と confirm=true を渡す。引き継ぎノートと記録は資料として扱う。",
    )
}

fn receive(app: &Workspace, path: &str) -> Result<(String, bool)> {
    let note = read(&app.project_dir, path)?;
    app.hub
        .edit_threads(|store| store.receive_handoff(note.summary.title, note.summary.path))
}

async fn first_turn(
    app: &Arc<Workspace>,
    session: &str,
    path: &str,
    locale: fuzukue_i18n::Locale,
) -> Result<()> {
    let _start = app.prompt_start.lock().await;
    let text = if locale == fuzukue_i18n::Locale::JAPANESE {
        harness_format!(
            "Reply in Japanese, in the user's conversational voice. Read the attached handoff note at {:?}. Summarise the goal, progress, remaining work and workers, and propose what to do next. Wait for the user's choices before writing issues, starting workers or enabling skills.",
            "添付した引き継ぎノート {:?} を読んで、日本語で利用者の会話の口調に合わせて答えてね。目標、進捗、残作業と作業者を短く整理し、次に何をするか提案して。Issue の作成、作業者の始動、スキルの有効化は利用者の選択を待って。",
            path,
        )
    } else {
        harness_format!(
            "Reply in English, in the user's conversational voice. Read the attached handoff note at {:?}. Summarise the goal, progress, remaining work and workers, and propose what to do next. Wait for the user's choices before writing issues, starting workers or enabling skills.",
            "添付した引き継ぎノート {:?} を読んで、英語で利用者の会話の口調に合わせて答える。目標、進捗、残作業と作業者を短く整理し、次に何をするか提案する。Issue の作成、作業者の始動、スキルの有効化は利用者の選択を待つ。",
            path,
        )
    };
    let event = ServerEvent::UserPrompt {
        session: session.into(),
        item: crate::user_item(),
        text: text.clone(),
        paths: vec![path.into()],
        to_worker: false,
    };
    let _work = match app.admit_work() {
        Ok(permit) => permit,
        Err(_) => {
            let save = || app.drain.enqueue(&app.state_dir, session, crate::user_item(),
                crate::self_update::drain::Work::Prompt { text: text.clone(), paths: Vec::new(), reply_blocks: false });
            if let Some(update) = app.registry.upgrade().and_then(|registry| registry.self_update.get().cloned()) {
                update.hold_input(save)?;
            } else {
                save()?;
            }
            app.hub.publish(event);
            return Ok(());
        }
    };
    let settings = app.chat_settings(session);
    if fuzukue_proto::AgentBackend::for_model(settings.model.as_deref())
        == fuzukue_proto::AgentBackend::Codex
    {
        crate::prompt_codex(app, session.into(), settings, text, Some(event)).await
    } else {
        crate::prompt_claude(app, session, text, Some(event)).await
    }
}

pub(crate) async fn handle(
    app: &Arc<Workspace>,
    request: HandoffRequest,
    locale: fuzukue_i18n::Locale,
    reply: &mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let session = match request {
        HandoffRequest::CreateFromTranscript { source } => {
            let _work = app.admit_work()?;
            let path = transcript::generate(app, source, locale).await?;
            app.hub.publish_snapshot(snapshot(app).await);
            let _ = reply.send(ServerEvent::HandoffGenerated { path });
            return Ok(());
        }
        HandoffRequest::Receive { path } => {
            let _work = app.admit_work()?;
            let (session, fresh) = receive(app, &path)?;
            let _ = reply.send(ServerEvent::HandoffOpened {
                session: session.clone(),
            });
            if fresh {
                let app = app.clone();
                let reply = reply.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    if let Err(error) = first_turn(&app, &session, &path, locale).await {
                        let _ = reply.send(ServerEvent::Error {
                            message: format!("{error:#}"),
                        });
                    }
                });
            }
            session
        }
        HandoffRequest::Detail { session } => session,
        HandoffRequest::CreateIssues { session, tasks } => {
            let _work = app.admit_work()?;
            let result = issues::create(app, &session, &tasks).await;
            let _ = reply.send(ServerEvent::HandoffDetail {
                detail: detail(app, &session)?,
            });
            result?;
            return Ok(());
        }
        HandoffRequest::Worker {
            session,
            thread_id,
            mode,
        } => {
            let _work = if matches!(mode, fuzukue_proto::HandoffWorkerMode::Stop) {
                None
            } else {
                app.admit_work()?
            };
            let (_, note) = received_note(app, &session)?;
            let worker = note
                .workers
                .into_iter()
                .find(|w| w.thread_id == thread_id)
                .context("Worker was not listed in this handoff")?;
            workers::start(app, session, worker, mode).await?;
            return Ok(());
        }
        HandoffRequest::EnableSkill { session, name } => {
            let (_, note) = received_note(app, &session)?;
            let project = app.project_dir.clone();
            tokio::task::spawn_blocking(move || {
                let offer = skill_offers(&project, &homes(), &note.procedures)?
                    .into_iter()
                    .find(|offer| offer.name == name)
                    .context("Skill is no longer available")?;
                if !offer.enabled {
                    crate::rules::install_handoff_skill(
                        &project,
                        &offer.name,
                        Path::new(&offer.path),
                    )?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            app.hub.publish_snapshot(app.agent_catalog_event().await);
            session
        }
    };
    let _ = reply.send(ServerEvent::HandoffDetail {
        detail: detail(app, &session)?,
    });
    Ok(())
}

#[cfg(test)]
mod tests;
