//! Folder attachments live in the server's inbox, outside the workspace tree.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fuzukue_proto::{
    ATTACHMENT_CHUNK_BYTES, FolderAttachmentEntry as Entry, FolderAttachmentRequest as Request,
    MAX_FOLDER_ATTACHMENT_BATCH, MAX_FOLDER_ATTACHMENT_BYTES, MAX_FOLDER_ATTACHMENT_DEPTH,
    MAX_FOLDER_ATTACHMENT_ENTRIES, MAX_FOLDER_ATTACHMENT_PATH_BYTES, ServerEvent,
    folder_attachment_excludes,
};
use tempfile::TempDir;

const ROOT: &str = "inbox/folders";
const REFERENCE: &str = "reference";

#[derive(Default)]
pub(crate) struct FolderUpload {
    pending: Option<Copy>,
}

struct Copy {
    id: u64,
    stage: TempDir,
    name: String,
    bytes: u64,
    entries: usize,
    received: u64,
    created: usize,
    file: Option<PartialFile>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

struct PartialFile {
    path: String,
    size: u64,
    offset: u64,
    file: File,
}

pub(crate) fn failed(id: u64, error: impl ToString) -> ServerEvent {
    ServerEvent::AttachmentUploaded { id, path: None, error: Some(error.to_string()), proposal: None }
}

impl FolderUpload {
    pub(crate) fn cancel(&mut self, id: u64) {
        if self.pending.as_ref().is_some_and(|copy| copy.id == id) {
            self.pending = None;
        }
    }

    pub(crate) fn receive(
        &mut self,
        state: &Path,
        slots: std::sync::Arc<tokio::sync::Semaphore>,
        id: u64,
        request: Request,
    ) -> ServerEvent {
        match self.apply(state, slots, id, request) {
            Ok(event) => event,
            Err(error) => {
                self.cancel(id);
                failed(id, error)
            }
        }
    }

    fn apply(
        &mut self,
        state: &Path,
        slots: std::sync::Arc<tokio::sync::Semaphore>,
        id: u64,
        request: Request,
    ) -> Result<ServerEvent> {
        if let Request::Reference { path, name } = request {
            ensure!(path.len() <= MAX_FOLDER_ATTACHMENT_PATH_BYTES, "Folder path is too long");
            component(&name)?;
            let path = Path::new(&path);
            crate::folder_grants::absolute(path)?;
            // Opening the directory checks read access without listing its contents.
            match fs::read_dir(path) {
                Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotADirectory) =>
                {
                    return Ok(ServerEvent::FolderReferenceUnavailable { id });
                }
                result => drop(result?),
            }
            let path = crate::folder_grants::canonical_folder(path, &crate::worker_scope::protected_roots(state))?;
            let stage = stage(state)?;
            fs::write(stage.path().join(REFERENCE), serde_json::to_vec(&(name.as_str(), path))?)?;
            return publish(state, id, stage, "ref", &name);
        }
        if let Request::Begin { name, bytes, entries } = request {
            ensure!(self.pending.is_none(), "Copy one folder at a time");
            component(&name)?;
            ensure!(!folder_attachment_excludes(&name), "Excluded folder name");
            ensure!(bytes <= MAX_FOLDER_ATTACHMENT_BYTES && entries <= MAX_FOLDER_ATTACHMENT_ENTRIES,
                "Folder copy exceeds the size or entry limit");
            let permit = slots.try_acquire_owned().context("Too many folder copies are in progress")?;
            let stage = stage(state)?;
            fs::create_dir(stage.path().join(&name))?;
            self.pending = Some(Copy {
                id, stage, name, bytes, entries, received: 0, created: 0, file: None, _permit: permit,
            });
            return Ok(ServerEvent::AttachmentProgress { id, offset: 0 });
        }
        let copy = self.pending.as_mut().filter(|copy| copy.id == id).context("Folder copy has not started")?;
        match request {
            Request::Entries { entries } => copy.write_entries(entries)?,
            Request::Finish => {
                ensure!(copy.file.is_none() && copy.received == copy.bytes && copy.created == copy.entries,
                    "Folder copy did not match the preview");
                let copy = self.pending.take().unwrap();
                return publish(state, id, copy.stage, "copy", &copy.name);
            }
            Request::Reference { .. } | Request::Begin { .. } => unreachable!(),
        }
        Ok(ServerEvent::AttachmentProgress { id, offset: copy.received })
    }
}

impl Copy {
    fn path(&self, path: &str) -> Result<PathBuf> {
        ensure!(path.len() <= MAX_FOLDER_ATTACHMENT_PATH_BYTES, "Folder item path is too long");
        let parts = path.split('/');
        ensure!(parts.clone().count() <= MAX_FOLDER_ATTACHMENT_DEPTH, "Folder item is too deeply nested");
        for part in parts {
            component(part)?;
            ensure!(!folder_attachment_excludes(part), "Excluded folder item");
        }
        Ok(self.stage.path().join(&self.name).join(path))
    }

    fn can_create(&self) -> Result<()> {
        ensure!(self.file.is_none() && self.created < self.entries, "Unexpected folder item");
        Ok(())
    }

    fn write_entries(&mut self, entries: Vec<Entry>) -> Result<()> {
        ensure!(!entries.is_empty() && entries.len() <= MAX_FOLDER_ATTACHMENT_BATCH,
            "Invalid folder batch size");
        let encoded: usize = entries.iter().map(|entry| match entry {
            Entry::Directory { .. } => 0,
            Entry::FileChunk { data, .. } => data.len(),
        }).sum();
        ensure!(encoded <= ATTACHMENT_CHUNK_BYTES.div_ceil(3) * 4 + 4 * entries.len(),
            "Folder batch is too large");
        let before = self.received;
        for entry in entries {
            match entry {
                Entry::Directory { path } => {
                    self.can_create()?;
                    fs::create_dir(self.path(&path)?)?;
                    self.created += 1;
                }
                Entry::FileChunk { path, size, offset, data } => self.chunk(path, size, offset, &data)?,
            }
        }
        ensure!(self.received - before <= ATTACHMENT_CHUNK_BYTES as u64, "Folder batch is too large");
        Ok(())
    }

    fn chunk(&mut self, path: String, size: u64, offset: u64, data: &str) -> Result<()> {
        ensure!(data.len() <= ATTACHMENT_CHUNK_BYTES.div_ceil(3) * 4, "Folder chunk is too large");
        let data = STANDARD.decode(data)?;
        ensure!(data.len() <= ATTACHMENT_CHUNK_BYTES && (!data.is_empty() || size == 0),
            "Invalid folder chunk size");
        let next = offset.checked_add(data.len() as u64).context("Invalid folder chunk offset")?;
        ensure!(next <= size, "File exceeds its declared size");
        if offset == 0 {
            self.can_create()?;
            ensure!(size <= self.bytes - self.received, "Folder exceeds its declared size");
            let file = File::create_new(self.path(&path)?)?;
            self.file = Some(PartialFile { path: path.clone(), size, offset, file });
            self.created += 1;
        }
        let file = self.file.as_mut().context("File transfer has not started")?;
        ensure!(file.path == path && file.size == size && file.offset == offset, "Folder chunks are out of order");
        file.file.write_all(&data)?;
        file.offset = next;
        self.received += data.len() as u64;
        if next == size {
            self.file = None;
        }
        Ok(())
    }
}

fn component(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 240 && !matches!(name, "." | "..")
            && !name.chars().any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':')),
        "Invalid folder attachment name"
    );
    Ok(())
}

fn root(state: &Path, create: bool) -> Result<PathBuf> {
    let inbox = crate::inbox::directory(state, "inbox", create)?;
    crate::inbox::directory(&inbox, "folders", create)
}

fn stage(state: &Path) -> Result<TempDir> {
    fs::create_dir_all(state)?;
    Ok(tempfile::Builder::new().prefix(".incoming-").tempdir_in(root(state, true)?)?)
}

fn publish(state: &Path, id: u64, stage: TempDir, kind: &str, name: &str) -> Result<ServerEvent> {
    let suffix = stage.path().file_name().and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".incoming-")).context("Invalid folder staging path")?;
    let key = format!("{kind}-{suffix}");
    let target = root(state, false)?.join(&key);
    rustix::fs::renameat_with(
        rustix::fs::CWD, stage.path(), rustix::fs::CWD, &target, rustix::fs::RenameFlags::NOREPLACE,
    )?;
    let _ = stage.keep();
    Ok(ServerEvent::AttachmentUploaded {
        id, path: Some(format!("{ROOT}/{key}/{name}")), error: None, proposal: None,
    })
}

pub(crate) fn resolve(state: &Path, path: &str) -> Result<PathBuf> {
    let mut parts = path.strip_prefix("inbox/folders/").context("Invalid folder attachment")?.split('/');
    let key = parts.next().context("Missing folder attachment ID")?;
    let name = parts.next().context("Missing folder name")?;
    ensure!(parts.next().is_none(), "Invalid folder attachment path");
    component(name)?;
    let (kind, suffix) = key.split_once('-').context("Invalid folder attachment ID")?;
    ensure!(matches!(kind, "ref" | "copy") && suffix.len() == 6
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()), "Invalid folder attachment ID");
    let entry = crate::inbox::directory(&root(state, false)?, key, false)?;
    if kind == "copy" {
        return crate::inbox::directory(&entry, name, false)?.canonicalize().map_err(Into::into);
    }
    let descriptor = entry.join(REFERENCE);
    let meta = fs::symlink_metadata(&descriptor)?;
    ensure!(meta.is_file() && meta.len() <= 2 * MAX_FOLDER_ATTACHMENT_PATH_BYTES as u64, "Invalid folder reference");
    let (saved_name, path): (String, String) = serde_json::from_slice(&fs::read(descriptor)?)?;
    ensure!(saved_name == name, "Invalid folder reference name");
    let canonical = crate::folder_grants::canonical_folder(Path::new(&path), &crate::worker_scope::protected_roots(state))?;
    ensure!(canonical == path, "Folder reference has moved");
    Ok(PathBuf::from(canonical))
}
