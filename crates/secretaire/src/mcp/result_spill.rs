use anyhow::{Context, Result, ensure};
use rustix::fs::{self, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DIRECTORY: &str = "mcp-results";
const INLINE_BYTES: usize = 8 * 1024;
const MAX_FILE_BYTES: usize = super::workspace_read::READ_SCAN_BYTES;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 64;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

fn root(project: &Path) -> PathBuf {
    fuzukue_proto::compat::workspace_data(project, DIRECTORY)
}

pub(crate) fn is_spill_path(path: &Path) -> bool {
    path.ancestors().any(|part| {
        part.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(DIRECTORY))
            && part.parent().and_then(Path::file_name).is_some_and(|name| {
                name.eq_ignore_ascii_case(".fuzukue") || name.eq_ignore_ascii_case(".labo")
            })
    })
}

#[derive(Clone)]
pub(super) struct Owner(String);

impl Owner {
    pub(super) fn thread(session: &str) -> Self {
        Self(format!("{:x}", Sha256::digest(session.as_bytes())))
    }

    pub(super) fn review(session: &str, job: &str) -> Self {
        Self::thread(&format!("review\0{session}\0{job}"))
    }

    pub(super) fn allows(&self, workspace: &Path, path: &Path) -> bool {
        let Some(name) = path
            .strip_prefix(root(workspace))
            .ok()
            .and_then(Path::to_str)
        else {
            return false;
        };
        let Some((session, _, _)) = file_parts(name) else {
            return false;
        };
        if session != self.0 {
            return false;
        }
        path.symlink_metadata().is_ok_and(|meta| {
            meta.is_file()
                && meta.nlink() == 1
                && meta.mode() & 0o077 == 0
                && meta
                    .modified()
                    .is_ok_and(|time| time.elapsed().unwrap_or_default() < RETENTION)
        })
    }
}

fn file_parts(name: &str) -> Option<(&str, &str, &str)> {
    let (stem, extension) = name.rsplit_once('.')?;
    let (session, nonce) = stem.split_once('-')?;
    (session.len() == 64
        && nonce.len() == 43
        && session.bytes().all(|byte| byte.is_ascii_hexdigit())
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && matches!(extension, "json" | "txt"))
    .then_some((session, nonce, extension))
}

fn directory(
    parent: impl std::os::fd::AsFd,
    name: &std::ffi::OsStr,
) -> Result<std::os::fd::OwnedFd> {
    match fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(fs::openat(parent, name, DIR_FLAGS, Mode::empty())?)
}

fn install_gitignore(dir: &std::os::fd::OwnedFd) -> Result<()> {
    // The rule also ignores the .gitignore itself in workspaces without a root rule.
    match fs::openat(
        dir,
        ".gitignore",
        FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(file) => File::from(file).write_all(b"*\n")?,
        Err(rustix::io::Errno::EXIST) => {
            let file = fs::openat(
                dir,
                ".gitignore",
                FILE_FLAGS | OFlags::RDONLY,
                Mode::empty(),
            )?;
            let file = File::from(file);
            ensure!(
                file.metadata()?.is_file(),
                "Invalid MCP result directory ignore file"
            );
            let mut contents = Vec::new();
            file.take(3).read_to_end(&mut contents)?;
            ensure!(
                contents == b"*\n",
                "Invalid MCP result directory ignore rule"
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn prune(dir: &std::os::fd::OwnedFd, incoming: usize) -> Result<()> {
    let mut files = Vec::new();
    let now = SystemTime::now();
    let mut bytes = incoming;
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Ok(name_str) = name.to_str() else {
            continue;
        };
        if file_parts(name_str).is_none() {
            continue;
        }
        let stat = fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            continue;
        }
        let modified = Duration::from_secs(stat.st_mtime.max(0) as u64);
        if now
            .duration_since(SystemTime::UNIX_EPOCH + modified)
            .unwrap_or_default()
            >= RETENTION
        {
            fs::unlinkat(dir, name, AtFlags::empty())?;
            continue;
        }
        let len = stat.st_size.max(0) as usize;
        files.push((modified, name.to_owned(), len));
        bytes = bytes.saturating_add(len);
    }
    files.sort_unstable();
    let mut count = files.len();
    for (_, name, len) in files {
        if count < MAX_FILES && bytes <= MAX_TOTAL_BYTES {
            break;
        }
        fs::unlinkat(dir, name.as_c_str(), AtFlags::empty())?;
        count -= 1;
        bytes -= len;
    }
    Ok(())
}

pub(super) fn save(
    project: &Path,
    owner: &Owner,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "MCP result exceeds the 16 MiB storage limit"
    );
    let root = root(project);
    let relative = root.strip_prefix(project)?;
    let area = relative
        .parent()
        .and_then(Path::file_name)
        .context("Missing workspace data directory")?;
    let dir = super::workspace_read::open_exec_directory(project)?;
    let area = directory(dir, area)?;
    let dir = directory(
        area,
        root.file_name().context("Missing MCP result directory")?,
    )?;
    ensure!(
        fs::fstat(&dir)?.st_mode & 0o077 == 0,
        "MCP result directory must be private"
    );
    fs::flock(&dir, FlockOperation::LockExclusive)?;
    install_gitignore(&dir)?;
    prune(&dir, bytes.len())?;
    let nonce = crate::mail::session_key();
    let name = format!("{}-{nonce}.{extension}", owner.0);
    ensure!(file_parts(&name).is_some(), "Invalid MCP result file name");
    let file = fs::openat(
        &dir,
        name.as_str(),
        FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    )?;
    if let Err(error) = File::from(file).write_all(bytes) {
        let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
        return Err(error.into());
    }
    Ok(relative.join(name))
}

pub(super) fn head(text: &str, bytes: usize) -> &str {
    &text[..text.floor_char_boundary(bytes.min(text.len()))]
}

fn shape(data: &Value) -> Value {
    match data {
        Value::Object(fields) => json!({
            "type": "object",
            "key_count": fields.len(),
            "keys": fields.keys().take(24).map(|key| head(key, 64)).collect::<Vec<_>>()
        }),
        Value::Array(items) => json!({"type": "array", "items": items.len()}),
        _ => json!({"type": match data {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            _ => unreachable!(),
        }}),
    }
}

pub(super) async fn limit(project: &Path, owner: Owner, mut result: Value) -> Value {
    if result["isError"] == true
        || result["content"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item["type"] != "text"))
        || serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= INLINE_BYTES)
    {
        return result;
    }

    let content = result["content"].as_array().expect("text content");
    let text = content
        .first()
        .and_then(|item| item["text"].as_str())
        .unwrap_or("");
    let data = result.get("structuredContent");
    let continuation: Map<String, Value> = data
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(key, value)| {
            (*key == "next"
                || key.starts_with("next_")
                || [
                    "truncated",
                    "total_lines",
                    "audit_before",
                    "has_more",
                    "has_next",
                ]
                .contains(&key.as_str()))
                && value.to_string().len() <= 256
        })
        .take(8)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let preview = head(text, 768).to_owned();
    let text_only = data.is_none() && content.len() == 1;
    let payload = match data {
        Some(data) if content.len() == 1 && text == data.to_string() => data,
        _ => &result,
    };
    let (extension, format, summary, bytes) = if text_only {
        (
            "txt",
            "text",
            json!({"type": "text", "lines": text.lines().count()}),
            text.as_bytes().to_vec(),
        )
    } else {
        let format = if std::ptr::eq(payload, &result) {
            "mcp_json"
        } else {
            "json"
        };
        let mut bytes = serde_json::to_vec_pretty(payload).expect("JSON value serializes");
        if bytes.len() > MAX_FILE_BYTES {
            bytes = serde_json::to_vec(payload).expect("JSON value serializes");
        }
        ("json", format, shape(payload), bytes)
    };
    let size = bytes.len();
    let project = project.to_path_buf();
    let spill = tokio::task::spawn_blocking(move || {
        save(&project.canonicalize()?, &owner, extension, &bytes)
    })
    .await;
    let mut reply = Value::Object(continuation);
    reply["preview"] = json!(preview);
    reply["bytes"] = json!(size);
    reply["shape"] = summary;
    match spill {
        Ok(Ok(path)) => {
            reply["spilled"] = json!(true);
            reply["path"] = json!(path);
            reply["format"] = json!(format);
            reply["hint"] = json!(if text_only {
                "Use this path with fs_read (start_line, end_line, max_bytes) or fs_search."
            } else {
                "Use this path with jq (choose fields and set max_bytes) or fs_read/fs_search. For mcp_json, the tool data is under .structuredContent when present."
            });
        }
        other => {
            tracing::warn!(?other, "could not store oversized MCP result");
            reply["truncated"] = json!(true);
            reply["spill_error"] =
                json!("Result storage was unavailable; only a preview is returned.");
        }
    }
    // Keep MCP metadata, such as annotations on an extension result.
    let mut short = super::data_result(reply, false);
    if let Some(meta) = result
        .as_object_mut()
        .and_then(|object| object.remove("_meta"))
        .filter(|meta| meta.to_string().len() <= 512)
    {
        short["_meta"] = meta;
    }
    short
}
