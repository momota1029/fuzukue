//! Private, app-owned memory. Markdown items are authoritative.
mod desk;
mod import;

pub(crate) use desk::{
    Confirmations, Tool, WorkerItem, for_turn, guidance, user_confirmation, worker_attachment,
};

use anyhow::{Context, Result, anyhow, ensure};
use chrono::Utc;
use rustix::fs::{self, Dir, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;

const MAX_ITEMS: usize = 200;
const MAX_INDEX: usize = 32 * 1024;
const MAX_BODY: usize = 16 * 1024;
const MAX_FILE: usize = MAX_BODY + 4096;
const PRIVATE_DIR: Mode = Mode::RUSR.union(Mode::WUSR).union(Mode::XUSR);
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Global,
    Workspace,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Origin {
    User,
    Assistant,
    ClaudeImport,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u8,
    id: String,
    summary: String,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
    worker: bool,
    origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    imported_from: Option<import::SourceRecord>,
}

struct Item {
    metadata: Metadata,
    body: String,
    raw: String,
    revision: String,
}

impl Item {
    fn parse(raw: String, id: &str) -> Result<Self> {
        let (yaml, body) = raw
            .strip_prefix("---\n")
            .and_then(|text| text.split_once("\n---\n"))
            .context("missing Markdown frontmatter")?;
        // YAML errors may contain private text.
        let metadata: Metadata =
            serde_yaml::from_str(yaml).map_err(|_| anyhow!("invalid memory frontmatter"))?;
        ensure!(
            metadata.version == 1 && metadata.id == id,
            "invalid item version or ID"
        );
        ensure!(
            metadata.created_at <= metadata.updated_at,
            "invalid item timestamps"
        );
        validate_text(&metadata.summary, body)?;
        Ok(Self {
            metadata,
            body: body.to_owned(),
            revision: hash(raw.as_bytes()),
            raw,
        })
    }

    fn new(metadata: Metadata, body: &str) -> Result<Self> {
        let yaml = serde_yaml::to_string(&metadata)
            .map_err(|_| anyhow!("could not encode memory metadata"))?;
        let raw = format!("---\n{yaml}---\n{body}");
        ensure!(
            raw.len() <= MAX_FILE,
            "encoded memory exceeds its file size limit"
        );
        Self::parse(raw, &metadata.id)
    }

    fn index_entry(&self, scope: Scope) -> IndexEntry<'_> {
        IndexEntry {
            id: &self.metadata.id,
            scope,
            summary: &self.metadata.summary,
            worker: self.metadata.worker,
            updated_at: self.metadata.updated_at,
        }
    }
}

#[derive(Serialize)]
struct IndexEntry<'a> {
    id: &'a str,
    scope: Scope,
    summary: &'a str,
    worker: bool,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Default)]
struct Index {
    global: String,
    workspace: String,
    revision: String,
}

impl Index {
    fn build(items: &Items) -> Result<Self> {
        let mut index = Self::default();
        ensure!(
            items.len() <= MAX_ITEMS,
            "memory exceeds 200 items across the global and current workspace scopes"
        );
        let mut digest = Sha256::new();
        for ((scope, _), item) in items {
            let row = serde_json::to_string(&item.index_entry(*scope))?;
            let layer = match scope {
                Scope::Global => &mut index.global,
                Scope::Workspace => &mut index.workspace,
            };
            layer.push_str(&row);
            layer.push('\n');
            digest.update(row.as_bytes());
            digest.update(item.revision.as_bytes());
        }
        ensure!(
            index.global.len() + index.workspace.len() <= MAX_INDEX,
            "memory index exceeds 32 KiB across the global and current workspace scopes"
        );
        index.revision = format!("{:x}", digest.finalize());
        Ok(index)
    }

    fn jsonl(&self) -> String {
        format!("{}{}", self.global, self.workspace)
    }
}

type Items = BTreeMap<(Scope, String), Item>;

struct Store<'a> {
    home: &'a Path,
    workspace: &'a Path,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    scope: Scope,
    id: Option<String>,
    expected_revision: Option<String>,
    summary: String,
    body: String,
    worker: bool,
}

#[derive(Serialize)]
struct Saved {
    scope: Scope,
    id: String,
    revision: String,
    backup: Option<String>,
}

impl Store<'_> {
    fn with_items<T>(
        &self,
        action: impl FnOnce(&File, Option<&File>, Items) -> Result<T>,
    ) -> Result<T> {
        let global =
            memory_dir(self.home, true)?.context("global memory directory is unavailable")?;
        // Both layers count towards the same limit. Every app reader and writer takes
        // this lock, including writers in a different workspace.
        let lock = File::from(fs::openat(
            &global,
            ".lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            PRIVATE_FILE,
        )?);
        check_file(&lock)?;
        lock.lock()?;

        let workspace = if self.home == self.workspace {
            None
        } else {
            memory_dir(self.workspace, false)?
        };
        let mut items = Items::new();
        load_items(&global, Scope::Global, &mut items)?;
        if let Some(workspace) = &workspace {
            load_items(workspace, Scope::Workspace, &mut items)?;
        }
        action(&global, workspace.as_ref(), items)
    }

    fn snapshot(&self) -> Result<Index> {
        self.with_items(|global, workspace, items| {
            let index = Index::build(&items)?;
            refresh_index(global, &index.global)?;
            if let Some(workspace) = workspace {
                refresh_index(workspace, &index.workspace)?;
            }
            Ok(index)
        })
    }

    fn read(&self, scope: Scope, id: String) -> Result<(Item, String)> {
        ensure!(valid_id(&id), "invalid memory ID");
        self.with_items(|_, _, mut items| {
            let index = Index::build(&items)?;
            let item = items
                .remove(&(scope, id))
                .context("memory item was removed or is outside the requested scope")?;
            Ok((item, index.revision))
        })
    }

    fn save(
        &self,
        args: Save,
        origin: Origin,
        check_turn: impl FnOnce() -> Result<()>,
    ) -> Result<Saved> {
        validate_text(&args.summary, &args.body)?;
        ensure!(!args.body.trim().is_empty(), "memory body must not be empty; use memory_delete to remove an item");
        ensure!(
            args.id.is_some() == args.expected_revision.is_some(),
            "updates require an ID and its expected revision"
        );
        let updating = args.id.is_some();
        let id = args.id.unwrap_or_else(new_id);
        ensure!(valid_id(&id), "invalid memory ID");
        ensure!(
            args.scope != Scope::Workspace || self.home != self.workspace,
            "the workspace and global memory roots coincide"
        );
        self.with_items(|global, workspace, mut items| {
            let key = (args.scope, id.clone());
            let previous = items.remove(&key);
            ensure!(
                !items.keys().any(|(_, existing)| existing == &id),
                "duplicate memory ID {id}"
            );
            let existing_revision = previous.as_ref().map(|item| item.revision.as_str());
            ensure!(
                existing_revision == args.expected_revision.as_deref(),
                "memory revision conflict; read the current item before retrying"
            );
            let now = Utc::now();
            let metadata = Metadata {
                version: 1,
                id: id.clone(),
                summary: args.summary,
                created_at: previous
                    .as_ref()
                    .map_or(now, |item| item.metadata.created_at),
                updated_at: previous
                    .as_ref()
                    .map_or(now, |item| now.max(item.metadata.updated_at)),
                worker: args.worker,
                origin: previous
                    .as_ref()
                    .map_or(origin, |item| item.metadata.origin),
                imported_from: previous
                    .as_ref()
                    .and_then(|item| item.metadata.imported_from.clone()),
            };
            items.insert(key.clone(), Item::new(metadata, &args.body)?);
            let index = Index::build(&items)?;
            let item = &items[&key];
            let workspace_owned;
            let root = match (args.scope, workspace) {
                (Scope::Global, _) => global,
                (Scope::Workspace, Some(workspace)) => workspace,
                (Scope::Workspace, None) => {
                    workspace_owned = memory_dir(self.workspace, true)?
                        .context("could not create workspace memory")?;
                    &workspace_owned
                }
            };
            let folder = directory(root, "items", true, true)?
                .context("could not create memory items directory")?;
            let filename = format!("{id}.md");
            let backup = previous
                .as_ref()
                .map(|item| self.backup(global, args.scope, item))
                .transpose()?;
            atomic_write(
                &folder,
                &filename,
                item.raw.as_bytes(),
                || {
                    let on_disk = read_file(&folder, &filename, MAX_FILE)?;
                    ensure!(
                        on_disk
                            .as_ref()
                            .map(|text| hash(text.as_bytes()))
                            .as_deref()
                            == existing_revision,
                        "memory was edited during save; read it again before retrying"
                    );
                    check_turn()
                },
                !updating,
            )?;

            // A failed derived-index update does not undo a committed item. The
            // next snapshot reads the source and retries both indexes.
            let _ = refresh_index(global, &index.global);
            if let Some(workspace) = workspace.or((args.scope == Scope::Workspace).then_some(root))
            {
                let _ = refresh_index(workspace, &index.workspace);
            }
            Ok(Saved {
                scope: args.scope,
                id,
                revision: item.revision.clone(),
                backup,
            })
        })
    }

    fn delete(&self, scope: Scope, id: String, revision: &str) -> Result<String> {
        ensure!(valid_id(&id), "invalid memory ID");
        self.with_items(|global, workspace, mut items| {
            let item = items
                .remove(&(scope, id.clone()))
                .context("memory item was already removed")?;
            ensure!(
                item.revision == revision,
                "memory changed; preview the deletion again"
            );
            let root = match scope {
                Scope::Global => global,
                Scope::Workspace => workspace.context("workspace memory is unavailable")?,
            };
            let folder =
                directory(root, "items", false, true)?.context("memory items are unavailable")?;
            let name = format!("{id}.md");
            let index = Index::build(&items)?;
            let backup = self.backup(global, scope, &item)?;
            ensure!(
                read_file(&folder, &name, MAX_FILE)?.as_deref() == Some(item.raw.as_str()),
                "memory changed; preview the deletion again"
            );
            fs::unlinkat(&folder, name.as_str(), fs::AtFlags::empty())?;
            folder.sync_all()?;
            let _ = refresh_index(global, &index.global);
            if let Some(workspace) = workspace {
                let _ = refresh_index(workspace, &index.workspace);
            }
            Ok(backup)
        })
    }

    fn backup(&self, global: &File, scope: Scope, item: &Item) -> Result<String> {
        let backups =
            directory(global, "backups", true, true)?.context("could not create memory backups")?;
        let layer = match scope {
            Scope::Global => "global".to_owned(),
            Scope::Workspace => {
                format!("workspace-{}", hash(self.workspace.as_os_str().as_bytes()))
            }
        };
        let folder = directory(&backups, &layer, true, true)?
            .context("could not create scoped memory backups")?;
        let date = Utc::now().format("%Y-%m-%dT%H%M%S%.fZ");
        let name = format!("{date}-{}-{}.md", item.metadata.id, new_id());
        atomic_write(&folder, &name, item.raw.as_bytes(), || Ok(()), true)?;
        Ok(format!("{layer}/{name}"))
    }
}

fn validate_text(summary: &str, body: &str) -> Result<()> {
    ensure!(
        !summary.trim().is_empty()
            && summary.chars().count() <= 160
            && !summary
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')),
        "memory summary must be one nonempty line of at most 160 Unicode characters"
    );
    ensure!(body.len() <= MAX_BODY, "memory body exceeds 16 KiB");
    Ok(())
}

fn new_id() -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = bytes;
    format!(
        "{a:02x}{b:02x}{c:02x}{d:02x}-{e:02x}{f:02x}-{g:02x}{h:02x}-{i:02x}{j:02x}-{k:02x}{l:02x}{m:02x}{n:02x}{o:02x}{p:02x}"
    )
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Give sandbox mounts a stable target even before the first memory save.
pub(crate) fn prepare_sandbox_mount(path: &Path) -> Result<()> {
    let base = path.ancestors().nth(2).context("invalid memory mount path")?;
    memory_dir(base, true)
        .with_context(|| format!("prepare memory mount at {}", path.display()))?
        .context("missing memory mount directory")?;
    Ok(())
}

fn memory_dir(root: &Path, create: bool) -> Result<Option<File>> {
    let root = crate::mcp::workspace_read::open_no_symlinks(root)?;
    ensure!(
        root.metadata()?.is_dir(),
        "memory parent is not a directory"
    );
    let Some(parent) = directory(&root, ".fuzukue", create, false)? else {
        return Ok(None);
    };
    directory(&parent, "memory", create, true)
}

fn directory(parent: &File, name: &str, create: bool, private: bool) -> Result<Option<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match fs::openat(parent, name, flags, Mode::empty()) {
        Err(Errno::NOENT) if create => {
            match fs::mkdirat(parent, name, PRIVATE_DIR) {
                Ok(()) => parent.sync_all()?,
                Err(Errno::EXIST) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("create memory directory {name}"));
                }
            }
            fs::openat(parent, name, flags, Mode::empty())
        }
        Err(Errno::NOENT) => return Ok(None),
        result => result,
    }
    .with_context(|| format!("open memory directory {name}"))?;
    let file = File::from(file);
    let metadata = file.metadata()?;
    ensure!(
        !private
            || (metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o077 == 0),
        "memory directory {name} must be owned by the app user and private"
    );
    Ok(Some(file))
}

fn check_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "memory file must be a private regular file with one link"
    );
    Ok(())
}

fn read_file(parent: &File, name: &str, limit: usize) -> Result<Option<String>> {
    let file = match fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    check_file(&file)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "memory file exceeds its size limit");
    Ok(Some(
        String::from_utf8(bytes).map_err(|_| anyhow!("memory file is not UTF-8"))?,
    ))
}

fn load_items(root: &File, scope: Scope, items: &mut Items) -> Result<()> {
    let Some(dir) = directory(root, "items", false, true)? else {
        return Ok(());
    };
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| anyhow!("memory item name is not UTF-8"))?;
        if matches!(name, "." | "..") || name.strip_prefix(".tmp-").is_some_and(valid_id) {
            continue;
        }
        let id = name
            .strip_suffix(".md")
            .filter(|id| valid_id(id))
            .with_context(|| format!("invalid memory item filename {name:?} in {scope:?}"))?;
        let raw = read_file(&dir, name, MAX_FILE)
            .with_context(|| format!("read {scope:?} memory item {name}"))?
            .context("memory item changed while reading; retry the turn")?;
        let item = Item::parse(raw, id)
            .with_context(|| format!("invalid {scope:?} memory item {name}"))?;
        ensure!(
            !items.keys().any(|(_, existing)| existing == id),
            "duplicate memory ID {id}"
        );
        items.insert((scope, id.to_owned()), item);
        ensure!(
            items.len() <= MAX_ITEMS,
            "memory exceeds 200 items across the global and current workspace scopes"
        );
    }
    Ok(())
}

fn atomic_write(
    parent: &File,
    name: &str,
    data: &[u8],
    check: impl FnOnce() -> Result<()>,
    new: bool,
) -> Result<()> {
    let temporary = format!(".tmp-{}", new_id());
    let mut file = File::from(fs::openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        PRIVATE_FILE,
    )?);
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        check()?;
        if new {
            fs::renameat_with(
                parent,
                temporary.as_str(),
                parent,
                name,
                fs::RenameFlags::NOREPLACE,
            )?;
        } else {
            fs::renameat(parent, temporary.as_str(), parent, name)?;
        }
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::unlinkat(parent, temporary.as_str(), fs::AtFlags::empty());
    }
    result
}

fn refresh_index(root: &File, text: &str) -> Result<()> {
    if !matches!(read_file(root, "index.jsonl", MAX_INDEX), Ok(Some(old)) if old == text) {
        atomic_write(root, "index.jsonl", text.as_bytes(), || Ok(()), false)
            .context("could not rebuild memory index")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
