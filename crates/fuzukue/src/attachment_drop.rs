use std::fs::{self, File, Metadata};
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use anyhow::{Context as _, Result, ensure};
use fuzukue_proto::{
    FolderAttachmentEntry as Entry, FolderAttachmentRequest as Request, MAX_FOLDER_ATTACHMENT_BATCH,
    MAX_FOLDER_ATTACHMENT_BYTES, MAX_FOLDER_ATTACHMENT_DEPTH, MAX_FOLDER_ATTACHMENT_ENTRIES,
    MAX_FOLDER_ATTACHMENT_PATH_BYTES, folder_attachment_excludes,
};

use super::*;
use super::attachment_image::attachment_image_format;
use crate::conn::LocalFiles;

const MAX_SCANNED_ENTRIES: usize = 50_000;
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

pub(super) enum DropState {
    Inspecting(Arc<AtomicBool>),
    Linking(PathBuf),
    Preview(Plan),
    Sending {
        transfer: Arc<Mutex<Transfer>>,
        acknowledged: u64,
        expected: Option<(u64, usize)>,
        total: u64,
        items: Option<usize>,
        completed: usize,
    },
}

impl DropState {
    pub(super) fn cancel(&self) {
        if let Self::Inspecting(cancel) = self {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn on_server(&self) -> bool {
        matches!(self, Self::Linking(_) | Self::Sending { .. })
    }

    fn copying_folder(&self) -> bool {
        matches!(self, Self::Sending { items: Some(_), .. })
    }

    pub(super) fn label(&self, name: &str) -> String {
        match self {
            Self::Inspecting(_) | Self::Linking(_) => tr!("chat-loading-attachment", name = name.to_owned()),
            Self::Preview(plan) => tr!("chat-folder-copy-preview", name = name.to_owned(),
                size = size_label(plan.bytes), count = plan.items.len().to_string()),
            Self::Sending { acknowledged, total, .. } if *total > 0 => {
                format!("{name}{STATUS_SEPARATOR}{}%", acknowledged * 100 / total)
            }
            Self::Sending { items: Some(items), completed, .. } => {
                tr!("chat-folder-copy-progress", name = name.to_owned(),
                    completed = completed.to_string(), count = items.to_string())
            }
            Self::Sending { .. } => tr!("chat-loading-attachment", name = name.to_owned()),
        }
    }
}

fn size_label(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(super) struct Plan {
    root: PathBuf,
    items: Vec<Item>,
    bytes: u64,
}

struct Item {
    path: String,
    size: Option<u64>,
}

enum Prepared {
    Link { local: PathBuf, server: String },
    Copy(Plan),
    File(FileTransfer, Option<Arc<AttachmentImage>>),
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn component(name: &str) -> bool {
    !name.is_empty() && name.len() <= 240 && !matches!(name, "." | "..")
        && !name.chars().any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':'))
}

fn inspect(path: &Path, local: Option<&LocalFiles>, folders: bool, cancel: &AtomicBool) -> Result<Prepared> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(!is_link(&meta), tr!("chat-choose-a-regular-file"));
    if meta.is_dir() {
        ensure!(folders, tr!("chat-folder-needs-server-update"));
        let name = path.file_name().and_then(|name| name.to_str())
            .context(tr!("chat-folder-invalid-name"))?;
        ensure!(component(name), tr!("chat-folder-invalid-name"));
        let root = path.canonicalize()?;
        if let Some(server) = server_path(local, path) {
            return Ok(Prepared::Link { local: root, server });
        }
        ensure!(!folder_attachment_excludes(name), tr!("chat-folder-excluded"));
        return plan(root, cancel).map(Prepared::Copy);
    }
    ensure!(meta.is_file(), tr!("chat-choose-a-regular-file"));
    ensure!(meta.len() <= MAX_ATTACHMENT_BYTES, tr!("chat-attachments-are-limited-to-50-mib-per-file"));
    let path = path.canonicalize()?;
    let mut transfer = FileTransfer::open(&path, meta.len())?;
    let preview = if meta.len() <= MAX_PREVIEW_BYTES && attachment_image_format(path.to_str().unwrap_or("")).is_some() {
        let mut bytes = vec![0; meta.len() as usize];
        transfer.file.read_exact(&mut bytes)?;
        use std::io::Seek as _;
        transfer.file.rewind()?;
        decode_attachment_image(path.to_str().unwrap_or(""), &bytes)
    } else {
        None
    };
    Ok(Prepared::File(transfer, preview))
}

fn server_path(local: Option<&LocalFiles>, path: &Path) -> Option<String> {
    let path_text = path.to_str()?;
    let candidate = match local? {
        LocalFiles::Native if path.is_absolute() => path_text.to_owned(),
        LocalFiles::Wsl(distro) => wsl_server_path(distro, path_text)?,
        _ => return None,
    };
    (candidate.len() <= MAX_FOLDER_ATTACHMENT_PATH_BYTES).then_some(candidate)
}

fn wsl_server_path(distro: &str, path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let unc = path.strip_prefix("//").or_else(|| path.strip_prefix("UNC/"));
    let (prefix, tail) = if let Some(unc) = unc {
        let mut parts = unc.splitn(3, '/');
        let host = parts.next()?;
        if !["wsl$", "wsl.localhost"].iter().any(|name| host.eq_ignore_ascii_case(name))
            || !parts.next()?.eq_ignore_ascii_case(distro)
        {
            return None;
        }
        (String::new(), parts.next().unwrap_or_default())
    } else {
        let drive = *path.as_bytes().first()?;
        if !drive.is_ascii_alphabetic() || path.as_bytes().get(1..3) != Some(b":/") {
            return None;
        }
        (format!("/mnt/{}", drive.to_ascii_lowercase() as char), &path[3..])
    };
    let tail = tail.trim_end_matches('/');
    if !tail.is_empty() && !tail.split('/').all(component) {
        return None;
    }
    if tail.is_empty() && !prefix.is_empty() { Some(prefix) } else { Some(format!("{prefix}/{tail}")) }
}

fn local_item(root: &Path, path: &str) -> PathBuf {
    path.split('/').filter(|part| !part.is_empty()).fold(root.to_path_buf(), |path, part| path.join(part))
}

fn plan(root: PathBuf, cancel: &AtomicBool) -> Result<Plan> {
    let mut items = Vec::new();
    let mut bytes = 0u64;
    let mut visited = 0;
    let mut dirs = vec![(String::new(), 0)];
    while let Some((parent, depth)) = dirs.pop() {
        let directory = local_item(&root, &parent);
        ensure!(directory.canonicalize()? == directory, tr!("chat-folder-changed"));
        for child in fs::read_dir(directory)? {
            ensure!(!cancel.load(Ordering::Relaxed), tr!("chat-remove-attachment"));
            visited += 1;
            ensure!(visited <= MAX_SCANNED_ENTRIES, tr!("chat-folder-copy-limit"));
            let child = child?;
            let meta = fs::symlink_metadata(child.path())?;
            if is_link(&meta) || (!meta.is_file() && !meta.is_dir()) {
                continue;
            }
            let name = child.file_name();
            let name = name.to_str().context(tr!("chat-folder-invalid-name"))?;
            if folder_attachment_excludes(name) {
                continue;
            }
            ensure!(component(name), tr!("chat-folder-invalid-name"));
            let path = if parent.is_empty() { name.to_owned() } else { format!("{parent}/{name}") };
            ensure!(path.len() <= MAX_FOLDER_ATTACHMENT_PATH_BYTES && depth < MAX_FOLDER_ATTACHMENT_DEPTH,
                tr!("chat-folder-copy-limit"));
            let size = meta.is_file().then(|| meta.len());
            bytes = bytes.saturating_add(size.unwrap_or(0));
            ensure!(bytes <= MAX_FOLDER_ATTACHMENT_BYTES && items.len() < MAX_FOLDER_ATTACHMENT_ENTRIES,
                tr!("chat-folder-copy-limit"));
            if meta.is_dir() {
                dirs.push((path.clone(), depth + 1));
            }
            items.push(Item { path, size });
        }
    }
    Ok(Plan { root, items, bytes })
}

pub(super) enum Transfer {
    File(FileTransfer),
    Folder(FolderTransfer),
}

pub(super) struct FileTransfer {
    file: File,
    size: u64,
    offset: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileTransfer {
    fn open(path: &Path, size: u64) -> Result<Self> {
        let file = open_regular(path)?;
        let meta = file.metadata()?;
        ensure!(meta.is_file() && !is_link(&meta) && meta.len() == size, tr!("chat-folder-changed"));
        Ok(Self { file, size, offset: 0, modified: meta.modified().ok() })
    }

    fn chunk(&mut self) -> Result<(u64, String)> {
        let offset = self.offset;
        let count = (self.size - offset).min(ATTACHMENT_CHUNK_BYTES as u64) as usize;
        let mut bytes = vec![0; count];
        self.file.read_exact(&mut bytes)?;
        self.offset += count as u64;
        if self.offset == self.size {
            let actual = self.file.metadata()?;
            ensure!(actual.len() == self.size && actual.modified().ok() == self.modified,
                tr!("chat-folder-changed"));
        }
        Ok((offset, STANDARD.encode(bytes)))
    }
}

#[cfg(unix)]
fn open_regular(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let mut directory = open("/", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let mut parts = path.strip_prefix("/")?.iter().peekable();
    while let Some(part) = parts.next() {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if parts.peek().is_none() {
            return Ok(openat(&directory, part, flags | OFlags::NONBLOCK, Mode::empty())?.into());
        }
        directory = openat(&directory, part, flags | OFlags::DIRECTORY, Mode::empty())?;
    }
    anyhow::bail!("Missing attachment file name")
}

#[cfg(windows)]
fn open_regular(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    ensure!(path.canonicalize()? == path, tr!("chat-folder-changed"));
    let file = File::options().read(true).custom_flags(0x0020_0000).open(path)?;
    ensure!(path.canonicalize()? == path, tr!("chat-folder-changed"));
    Ok(file)
}

pub(super) struct FolderTransfer {
    plan: Plan,
    index: usize,
    file: Option<FileTransfer>,
    sent: u64,
    begun: bool,
}

impl Transfer {
    fn next(&mut self, id: u64, name: String, session: String) -> Result<(ClientMsg, (u64, usize))> {
        match self {
            Self::File(file) => {
                let (offset, data) = file.chunk()?;
                let msg = ClientMsg::UploadAttachment { id, name, size: file.size, offset, data, session: Some(session) };
                Ok((msg, (file.offset, 0)))
            }
            Self::Folder(folder) => {
                let request = folder.next(name)?;
                Ok((ClientMsg::FolderAttachment { id, request }, (folder.sent, folder.index)))
            }
        }
    }
}

impl FolderTransfer {
    fn next(&mut self, name: String) -> Result<Request> {
        if !self.begun {
            self.begun = true;
            return Ok(Request::Begin { name, bytes: self.plan.bytes, entries: self.plan.items.len() });
        }
        if self.index == self.plan.items.len() {
            return Ok(Request::Finish);
        }
        let mut entries = Vec::new();
        let start = self.sent;
        while entries.len() < MAX_FOLDER_ATTACHMENT_BATCH {
            let Some(item) = self.plan.items.get(self.index) else { break };
            let path = item.path.clone();
            let Some(size) = item.size else {
                self.index += 1;
                entries.push(Entry::Directory { path });
                continue;
            };
            let offset = self.file.as_ref().map_or(0, |file| file.offset);
            let chunk = (size - offset).min(ATTACHMENT_CHUNK_BYTES as u64);
            if self.sent - start + chunk > ATTACHMENT_CHUNK_BYTES as u64 {
                break;
            }
            if self.file.is_none() {
                self.file = Some(FileTransfer::open(&local_item(&self.plan.root, &path), size)?);
            }
            let file = self.file.as_mut().unwrap();
            let (offset, data) = file.chunk()?;
            self.sent += chunk;
            if file.offset == size {
                self.file = None;
                self.index += 1;
            }
            entries.push(Entry::FileChunk { path, size, offset, data });
        }
        Ok(Request::Entries { entries })
    }
}

impl Workbench {
    pub(super) fn queue_dropped_path(&mut self, session: &str, path: PathBuf, cx: &mut Context<Self>) {
        let id = NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed);
        let session = session.to_owned();
        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
        self.chat(&session, cx).attachments.push(DraftAttachment {
            id, name, session: session.clone(), state: AttachmentState::Reading, preview: None,
        });
        self.inspect_drop(id, path, self.http.local_files().cloned(), cx);
    }

    fn inspect_drop(&mut self, id: u64, path: PathBuf, local: Option<LocalFiles>, cx: &mut Context<Self>) {
        let folders = self.chat_capabilities.as_ref().is_some_and(|capabilities| capabilities.folder_attachments);
        let Some(draft) = self.drop_draft(id) else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        draft.state = AttachmentState::Drop(DropState::Inspecting(cancel.clone()));
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { inspect(&path, local.as_ref(), folders, &cancel) }).await;
            let _ = this.update(cx, |this, cx| {
                let out = this.out.clone();
                let Some(draft) = this.drop_draft(id) else { return };
                if !matches!(draft.state, AttachmentState::Drop(DropState::Inspecting(_))) {
                    return;
                }
                match result {
                    Ok(Prepared::Link { local, server }) => {
                        let request = Request::Reference { path: server, name: draft.name.clone() };
                        draft.state = AttachmentState::Drop(DropState::Linking(local));
                        if out.send(ClientMsg::FolderAttachment { id, request }).is_err() {
                            draft.state = AttachmentState::Failed(tr!("chat-cannot-send-attachments").into());
                        }
                    }
                    Ok(Prepared::Copy(plan)) => draft.state = AttachmentState::Drop(DropState::Preview(plan)),
                    Ok(Prepared::File(file, preview)) => {
                        draft.preview = preview;
                        let size = file.size;
                        draft.start_transfer(Transfer::File(file), size, None);
                        this.advance_drop(id, 0, cx);
                    }
                    Err(error) => draft.state = AttachmentState::Failed(error.to_string()),
                }
                cx.notify();
            });
        }).detach();
    }

    pub(super) fn folder_reference_unavailable(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(draft) = self.drop_draft(id) else { return };
        let AttachmentState::Drop(DropState::Linking(path)) = &draft.state else { return };
        let path = path.clone();
        self.inspect_drop(id, path, None, cx);
        cx.notify();
    }

    pub(super) fn copying_folder(&self) -> bool {
        self.chats.values().flat_map(|chat| &chat.attachments)
            .any(|draft| matches!(&draft.state, AttachmentState::Drop(state) if state.copying_folder()))
    }

    pub(super) fn confirm_folder_copy(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.copying_folder() {
            return;
        }
        let Some(draft) = self.drop_draft(id) else { return };
        if !matches!(draft.state, AttachmentState::Drop(DropState::Preview(_))) {
            return;
        }
        let AttachmentState::Drop(DropState::Preview(plan)) = std::mem::replace(&mut draft.state, AttachmentState::Reading) else {
            unreachable!();
        };
        let size = plan.bytes;
        let items = plan.items.len();
        draft.start_transfer(
            Transfer::Folder(FolderTransfer { plan, index: 0, file: None, sent: 0, begun: false }),
            size, Some(items),
        );
        self.advance_drop(id, 0, cx);
        cx.notify();
    }

    pub(super) fn drop_draft(&mut self, id: u64) -> Option<&mut DraftAttachment> {
        self.chats.values_mut().find_map(|chat| chat.attachments.iter_mut().find(|draft| draft.id == id))
    }

    pub(super) fn advance_drop(&mut self, id: u64, offset: u64, cx: &mut Context<Self>) {
        let Some(draft) = self.drop_draft(id) else { return };
        let AttachmentState::Drop(DropState::Sending { transfer, acknowledged, expected, completed, .. }) = &mut draft.state else { return };
        let Some((next, count)) = *expected else { return };
        if offset != next {
            return;
        }
        *expected = None;
        *acknowledged = offset;
        *completed = count;
        let (transfer, name, session) = (transfer.clone(), draft.name.clone(), draft.session.clone());
        cx.spawn(async move |this, cx| {
            let step = transfer.clone();
            let result = cx.background_executor().spawn(async move { step.lock().unwrap().next(id, name, session) }).await;
            let _ = this.update(cx, |this, cx| {
                let out = this.out.clone();
                let Some(draft) = this.drop_draft(id) else { return };
                let AttachmentState::Drop(DropState::Sending { transfer: current, expected, .. }) = &mut draft.state else { return };
                if !Arc::ptr_eq(current, &transfer) {
                    return;
                }
                match result {
                    Ok((message, pending_offset)) => {
                        if out.send(message).is_ok() {
                            *expected = Some(pending_offset);
                        } else {
                            draft.state = AttachmentState::Failed(tr!("chat-cannot-send-attachments").into());
                        }
                    }
                    Err(error) => {
                        let _ = out.send(ClientMsg::CancelAttachment { id });
                        draft.state = AttachmentState::Failed(error.to_string());
                    }
                }
                cx.notify();
            });
        }).detach();
    }
}

impl DraftAttachment {
    fn start_transfer(&mut self, transfer: Transfer, total: u64, items: Option<usize>) {
        self.state = AttachmentState::Drop(DropState::Sending {
            transfer: Arc::new(Mutex::new(transfer)), acknowledged: 0, expected: Some((0, 0)),
            total, items, completed: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_paths_match_only_the_connected_distro_and_windows_drives() {
        assert_eq!(wsl_server_path("Ubuntu", r"C:\Users\研究 100%\repo").as_deref(), Some("/mnt/c/Users/研究 100%/repo"));
        assert_eq!(wsl_server_path("Ubuntu", r"\\wsl.localhost\ubuntu\home\repo").as_deref(), Some("/home/repo"));
        assert_eq!(wsl_server_path("Ubuntu", r"\\?\UNC\wsl$\ubuntu\home\repo").as_deref(), Some("/home/repo"));
        assert_eq!(wsl_server_path("Ubuntu", r"\\wsl$\Debian\home\repo"), None);
        assert_eq!(wsl_server_path("Ubuntu", r"\\server\share\repo"), None);
        assert_eq!(wsl_server_path("Ubuntu", r"C:\a\..\repo"), None);
    }

    #[test]
    fn synthetic_preflight_bounds_sparse_binaries_and_skips_repo_metadata() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().canonicalize()?;
        for ignored in [".git", "target", "node_modules"] {
            fs::create_dir(path.join(ignored))?;
            File::create(path.join(ignored).join("sparse.bin"))?.set_len(2 * 1024 * 1024 * 1024)?;
        }
        fs::create_dir(path.join("empty"))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, path.join("loop"))?;
        for index in 0..6000 {
            File::create(path.join(format!("synthetic-{index}.txt")))?;
        }
        let cancel = AtomicBool::new(false);
        let preview = plan(path.clone(), &cancel)?;
        assert_eq!(preview.items.len(), 6001);
        assert_eq!(preview.bytes, 0);
        let mut transfer = FolderTransfer { plan: preview, index: 0, file: None, sent: 0, begun: false };
        assert!(matches!(transfer.next("synthetic".into())?, Request::Begin { entries: 6001, .. }));
        let mut batches = 0;
        while let Request::Entries { entries } = transfer.next("synthetic".into())? {
            assert!(!entries.is_empty() && entries.len() <= MAX_FOLDER_ATTACHMENT_BATCH);
            batches += 1;
        }
        assert_eq!(batches, 6001usize.div_ceil(MAX_FOLDER_ATTACHMENT_BATCH));
        assert_eq!(transfer.index, 6001);
        File::create(path.join("sparse.bin"))?.set_len(3 * 1024 * 1024 * 1024)?;
        assert!(plan(path.clone(), &cancel).is_err());
        assert!(matches!(inspect(&path, Some(&LocalFiles::Native), true, &cancel)?, Prepared::Link { .. }));
        cancel.store(true, Ordering::Relaxed);
        assert!(plan(path, &cancel).is_err());
        Ok(())
    }
}
