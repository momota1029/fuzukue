//! Shared storage for recoverable deletions, scoped to the selected workspace.

use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use fuzukue_i18n::Locale;
use fuzukue_proto::{
    CommentView, LedgerEntry, ServerEvent, TrashAction, TrashItem, TrashKind, TrashTarget,
};
use serde::{Deserialize, Serialize};

use crate::{comments, human_edits, Workspace};

const RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Serialize, Deserialize)]
pub(crate) enum Payload {
    File {
        path: PathBuf,
    },
    Ledger(LedgerEntry),
    Comment {
        deck: String,
        index: usize,
        draft: CommentView,
    },
    Report(crate::review_reports::DeletedReport),
    Extension(crate::extensions::DeletedExtension),
}

#[derive(Serialize, Deserialize)]
struct Entry {
    item: TrashItem,
    payload: Payload,
}

pub(crate) struct Store {
    root: PathBuf,
    /// Set where entries can be purged, so files kept beside a thread go too.
    project: Option<PathBuf>,
}

impl Store {
    pub(crate) fn new(state: &Path) -> Self {
        Self {
            root: state.join("trash"),
            project: None,
        }
    }

    fn purging(app: &Workspace) -> Self {
        Self {
            project: Some(app.project_dir.clone()),
            ..Self::new(&app.state_dir)
        }
    }

    /// Deletes an entry for good.
    fn discard(&self, entry_dir: &Path, entry: Option<&Entry>) -> Result<()> {
        if let (Some(project), Some(entry)) = (&self.project, entry) {
            if let (TrashKind::Thread, Some(id), Payload::File { path }) =
                (&entry.item.kind, &entry.item.source_id, &entry.payload)
            {
                // A restored thread keeps its list even if its entry lingered.
                if !project.join(path).exists() {
                    if let Err(error) = crate::todo::purge(project, id) {
                        tracing::warn!("could not remove the TODO list of {id}: {error:#}");
                    }
                }
            }
        }
        fs::remove_dir_all(entry_dir)?;
        Ok(())
    }

    pub(crate) fn put(
        &self,
        kind: TrashKind,
        source_id: String,
        title: String,
        payload: Payload,
        change: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.insert(kind, source_id, title, payload, None, change)
    }

    fn insert(
        &self,
        kind: TrashKind,
        source_id: String,
        title: String,
        payload: Payload,
        source: Option<&Path>,
        change: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let dir = tempfile::Builder::new()
            .prefix("item-")
            .tempdir_in(&self.root)?;
        let id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if let Some(source) = source {
            copy(source, &dir.path().join("data"))?;
        }
        let title = (!title.trim().is_empty()).then_some(title);
        let entry = Entry {
            item: TrashItem {
                id,
                kind,
                label: title.clone().unwrap_or_else(|| source_id.clone()),
                title,
                source_id: Some(source_id),
                expires_at: crate::usage::now() + RETENTION_SECONDS,
            },
            payload,
        };
        let metadata = tempfile::NamedTempFile::new_in(dir.path())?;
        fs::write(metadata.path(), serde_json::to_vec(&entry)?)?;
        metadata.persist(dir.path().join("entry.json"))?;
        change()?;
        let _ = dir.keep();
        Ok(())
    }

    pub(crate) fn file(
        &self,
        app: &Workspace,
        kind: TrashKind,
        source_id: String,
        title: String,
        path: &Path,
    ) -> Result<()> {
        app.history
            .write_trash(path, || self.file_unrecorded(app, kind, source_id, title, path))
    }

    pub(crate) fn file_unrecorded(
        &self,
        app: &Workspace,
        kind: TrashKind,
        source_id: String,
        title: String,
        path: &Path,
    ) -> Result<()> {
        let relative = path.strip_prefix(&app.project_dir)?.to_owned();
        let checked = destination(&app.project_dir, &relative)?;
        ensure!(checked == path, "Invalid trash path");
        self.insert(
            kind,
            source_id,
            title,
            Payload::File { path: relative },
            Some(path),
            || {
                // Detach the tree atomically on its own filesystem.
                let staging = tempfile::Builder::new()
                    .prefix(".fuzukue-trash-")
                    .tempdir_in(path.parent().unwrap())?;
                fs::rename(path, staging.path().join("data"))?;
                Ok(())
            },
        )
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.strip_prefix("item-")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())),
            "Invalid trash ID"
        );
        let path = self.root.join(id);
        ensure!(path.symlink_metadata()?.is_dir(), "Invalid trash entry");
        Ok(path)
    }

    fn read(path: &Path) -> Result<Entry> {
        Ok(serde_json::from_slice(&fs::read(path.join("entry.json"))?)?)
    }

    fn entries(&self) -> Result<Vec<(PathBuf, Entry)>> {
        let dirs = match fs::read_dir(&self.root) {
            Ok(dirs) => dirs,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        for dir in dirs {
            let dir = dir?;
            let Ok(path) = self.path(&dir.file_name().to_string_lossy()) else {
                continue;
            };
            match Self::read(&path) {
                Ok(entry) => entries.push((path, entry)),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == ErrorKind::NotFound) => {}
                Err(error) => {
                    tracing::warn!("could not read trash entry {}: {error:#}", path.display())
                }
            }
        }
        Ok(entries)
    }

    pub(crate) fn reserved_report(&self, deck: &str, submission: &str) -> Result<u32> {
        let now = crate::usage::now();
        Ok(self
            .entries()?
            .into_iter()
            .filter_map(|(_, entry)| match entry.payload {
                Payload::Report(report)
                    if entry.item.expires_at > now
                        && report.deck == deck
                        && report.submission == submission =>
                {
                    Some(report.number)
                }
                _ => None,
            })
            .max()
            .unwrap_or_default())
    }

    fn list(&self, now: i64) -> Result<Vec<TrashItem>> {
        let mut items = Vec::new();
        for (path, entry) in self.entries()? {
            if entry.item.expires_at > now {
                items.push(entry.item);
            } else {
                self.discard(&path, Some(&entry))?;
            }
        }
        items.sort_by(|a, b| b.expires_at.cmp(&a.expires_at).then(a.label.cmp(&b.label)));
        Ok(items)
    }
}

fn destination(project: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Invalid trash path"
    );
    let name = relative.file_name().context("Missing file name")?;
    let parent = project.join(relative.parent().unwrap());
    ensure!(
        parent.canonicalize()? == parent,
        "Trash parent path changed"
    );
    Ok(parent.join(name))
}

fn absent(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => anyhow::bail!("Destination already exists: {}", path.display()),
    }
}

fn copy(source: &Path, target: &Path) -> Result<()> {
    let metadata = source.symlink_metadata()?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            copy(&child.path(), &target.join(child.file_name()))?;
        }
        fs::set_permissions(target, metadata.permissions())?;
    } else if metadata.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    } else {
        ensure!(
            metadata.is_file(),
            "Unsupported trash file: {}",
            source.display()
        );
        fs::copy(source, target)?;
    }
    Ok(())
}

async fn delete(
    app: &Arc<Workspace>,
    store: &Store,
    target: TrashTarget,
    locale: Locale,
) -> Result<()> {
    match target {
        TrashTarget::Thread { id } => {
            let _start = app.prompt_start.lock().await;
            ensure!(!app.delegates.is_running(&id), "Thread is running");
            let path = app.hub.thread_path(&id)?;
            app.history.write_trash(&path, || {
                app.hub.trash_thread(app, store, &id)
            })?;
        }
        TrashTarget::Memo { id } => {
            let _guard = app.file_save.lock().await;
            let _write = crate::cards::CARD_WRITE_LOCK.lock().unwrap();
            let path = crate::memos::memo_folder(&app.project_dir, &id)?;
            let title = fuzukue_cards::read_card(
                &app.project_dir.join(crate::memos::DECK),
                &path.join("card.md"),
            )
            .ok()
            .flatten()
            .map(|card| {
                card.title
                    .lines()
                    .chain(card.body.lines())
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default();
            store.file(app, TrashKind::Memo, id, title, &path)?;
            app.graph.invalidate();
        }
        TrashTarget::Ledger { key } => crate::references::ledger::trash_entry(app, key).await?,
        TrashTarget::Submission { deck, id } => {
            crate::review_reports::trash(app, store, &deck, &id, None).await?;
        }
        TrashTarget::Report {
            deck,
            submission,
            number,
        } => {
            crate::review_reports::trash(app, store, &deck, &submission, Some(number)).await?;
        }
        TrashTarget::Comment { deck, draft } => {
            let root = crate::safe_join(&app.project_dir, &deck)?;
            let _edit = app.comment_edit.lock().await;
            let mut items = comments::read(&root)?;
            let index = items
                .iter()
                .position(|item| *item == draft)
                .context("Comment draft not found")?;
            remove_comment(app, store, &deck, &root, &mut items, index)?;
        }
        TrashTarget::Extension { id } => crate::extensions::trash(app, store, &id, locale)?,
    }
    Ok(())
}

fn remove_comment(
    app: &Workspace,
    store: &Store,
    deck: &str,
    root: &Path,
    items: &mut Vec<CommentView>,
    index: usize,
) -> Result<()> {
    ensure!(index < items.len(), "Comment draft not found");
    let draft = items.remove(index);
    let payload = Payload::Comment {
        deck: deck.into(),
        index,
        draft: draft.clone(),
    };
    let mut words = draft.text.split_whitespace();
    let opening = words.by_ref().take(8).collect::<Vec<_>>().join(" ");
    let mut chars = opening.chars();
    let snippet: String = chars.by_ref().take(60).collect();
    let suffix = if chars.next().is_some() || words.next().is_some() {
        "…"
    } else {
        ""
    };
    let title = if snippet.is_empty() {
        draft.card.clone()
    } else {
        format!("{} · {snippet}{suffix}", draft.card)
    };
    store.put(TrashKind::Comment, draft.card.clone(), title, payload, || {
        app.history
            .write_trash(&comments::file(root), || comments::save(root, items))
    })?;
    if let Err(error) = human_edits::remember_rejected(root, &draft) {
        tracing::warn!("could not remember rejected suggestion: {error:#}");
    }
    app.hub.publish_snapshot(ServerEvent::Comments {
        deck: deck.into(),
        items: std::mem::take(items),
    });
    Ok(())
}

// Legacy clients still use index-based removal.
pub(crate) async fn legacy_comment(app: &Workspace, deck: &str, index: usize) -> Result<()> {
    let _trash = app.trash_lock.lock().await;
    let root = crate::safe_join(&app.project_dir, deck)?;
    let _edit = app.comment_edit.lock().await;
    let mut items = comments::read(&root)?;
    remove_comment(
        app,
        &Store::new(&app.state_dir),
        deck,
        &root,
        &mut items,
        index,
    )?;
    app.hub.publish_snapshot(ServerEvent::TrashChanged);
    Ok(())
}

fn restore_file(app: &Workspace, path: &Path, data: &Path) -> Result<PathBuf> {
    let path = destination(&app.project_dir, path)?;
    app.history.write_trash(&path, || {
        absent(&path)?;
        let staging = tempfile::tempdir_in(path.parent().unwrap())?;
        let staged = staging.path().join("data");
        copy(data, &staged)?;
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            staged,
            rustix::fs::CWD,
            &path,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(path)
}

async fn restore(app: &Arc<Workspace>, entry: Entry, data: &Path) -> Result<()> {
    match entry.payload {
        Payload::File { path } => match entry.item.kind {
            TrashKind::Thread => {
                let _start = app.prompt_start.lock().await;
                let path = restore_file(app, &path, data)?;
                app.hub.restore_thread(path)?;
                if let Some(id) = &entry.item.source_id {
                    // The list stayed with the thread. Show it again.
                    crate::todo::republish(app, id);
                }
            }
            TrashKind::Submission => {
                crate::review_reports::restore_submission(app, &path, || {
                    restore_file(app, &path, data)
                })
                .await?
            }
            TrashKind::Memo => {
                let _guard = app.file_save.lock().await;
                let _write = crate::cards::CARD_WRITE_LOCK.lock().unwrap();
                restore_file(app, &path, data)?;
                app.graph.invalidate();
            }
            _ => anyhow::bail!("Invalid trash file kind"),
        },
        Payload::Ledger(entry) => crate::references::ledger::restore_entry(app, entry).await?,
        Payload::Report(report) => crate::review_reports::restore_report(app, report).await?,
        Payload::Extension(extension) => crate::extensions::restore(app, extension)?,
        Payload::Comment { deck, index, draft } => {
            let root = crate::safe_join(&app.project_dir, &deck)?;
            let _edit = app.comment_edit.lock().await;
            let mut items = comments::read(&root)?;
            items.insert(index.min(items.len()), draft);
            app.history
                .write_trash(&comments::file(&root), || comments::save(&root, &items))?;
            app.hub
                .publish_snapshot(ServerEvent::Comments { deck, items });
        }
    }
    Ok(())
}

pub(crate) async fn request(
    app: &Arc<Workspace>,
    action: TrashAction,
    locale: Locale,
) -> Result<ServerEvent> {
    let _trash = app.trash_lock.lock().await;
    let store = Store::purging(app);
    let now = crate::usage::now();
    store.list(now)?;
    let changed = !matches!(action, TrashAction::List);
    match action {
        TrashAction::List => {}
        TrashAction::Delete { target } => delete(app, &store, target, locale).await?,
        TrashAction::Restore { id } => {
            let path = store.path(&id)?;
            let entry = Store::read(&path)?;
            ensure!(entry.item.expires_at > now, "Trash entry expired");
            restore(app, entry, &path.join("data")).await?;
            fs::remove_dir_all(path)?;
        }
        TrashAction::Purge { id } => {
            let path = store.path(&id)?;
            store.discard(&path, Store::read(&path).ok().as_ref())?;
        }
    }
    if changed {
        app.hub.publish_snapshot(ServerEvent::TrashChanged);
    }
    Ok(ServerEvent::TrashItems {
        items: store.list(now)?,
    })
}

pub(crate) fn sweep(app: &Arc<Workspace>) {
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            tick.tick().await;
            let Some(app) = app.upgrade() else { break };
            let _trash = app.trash_lock.lock().await;
            if let Err(error) = Store::purging(&app).list(crate::usage::now()) {
                tracing::warn!("could not expire trash: {error:#}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_changes_leave_no_entry_and_expiry_removes_the_payload() -> Result<()> {
        let state = tempfile::tempdir()?;
        let store = Store::new(state.path());
        let source = state.path().join("source");
        fs::write(&source, "original")?;
        let insert = |apply: fn() -> Result<()>| {
            store.insert(
                TrashKind::Memo,
                "memo".into(),
                "Memo title".into(),
                Payload::File {
                    path: "memo".into(),
                },
                Some(&source),
                apply,
            )
        };
        assert!(insert(|| anyhow::bail!("write failed")).is_err());
        assert!(store.list(crate::usage::now())?.is_empty());
        insert(|| Ok(()))?;
        let items = store.list(crate::usage::now())?;
        let [item] = &items[..] else {
            anyhow::bail!("expected one item")
        };
        let path = store.path(&item.id)?;
        assert_eq!(fs::read_to_string(path.join("data"))?, "original");
        assert_eq!(item.title.as_deref(), Some("Memo title"));
        assert_eq!(item.source_id.as_deref(), Some("memo"));
        assert!(store.list(item.expires_at - 1)?.len() == 1);
        assert!(store.list(item.expires_at)?.is_empty());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn reads_trash_records_written_before_titles_were_added() -> Result<()> {
        let state = tempfile::tempdir()?;
        fs::write(
            state.path().join("entry.json"),
            r#"{
            "item": {"id":"item-1","kind":"memo","label":"2026-11-21-e899399e","expires_at":200},
            "payload": {"File":{"path":"memos/2026-11-21-e899399e"}}
        }"#,
        )?;
        let entry = Store::read(state.path())?;
        assert_eq!(entry.item.label, "2026-11-21-e899399e");
        assert!(entry.item.title.is_none() && entry.item.source_id.is_none());
        Ok(())
    }
}
