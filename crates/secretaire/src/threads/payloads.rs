//! Tool payload storage shared by live events, recovery, and thread conversion.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{DelegateReport, ServerEvent, THREAD_SAVE_DIR};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::durable;

const PREVIEW_BYTES: usize = 1024;
const OUTPUT_PREVIEW_BYTES: usize = 256;
const INPUT_INLINE_BYTES: usize = 4096;
// Older writers preserve EntryMeta.attachments even when discarding tool fields.
const PAYLOAD_MARKER: &str = "fuzukue:tool-payload:v1:";

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub(crate) file: Arc<File>,
    pub(crate) offset: u64,
    pub(crate) len: usize,
}

pub(crate) type JournalPrefixes = [Option<(u64, u64, u64)>; 2];

impl Record {
    pub(super) fn in_prefixes(&self, prefixes: &JournalPrefixes) -> bool {
        self.file.metadata().is_ok_and(|meta| prefixes.iter().flatten().any(|&(device, inode, end)| {
            (device, inode) == (meta.dev(), meta.ino()) && self.offset + self.len as u64 <= end
        }))
    }

    fn bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = vec![0; self.len];
        self.file.read_exact_at(&mut bytes, self.offset)?;
        Ok(bytes)
    }

    pub(super) fn inline(&self) -> Result<super::ToolSummary> {
        let bytes = self.bytes()?;
        let header = std::str::from_utf8(&bytes)?.strip_prefix(super::ENTRY)
            .and_then(|line| line.strip_suffix(" -->\n"))
            .context("broken tool payload header")?;
        serde_json::from_str::<super::EntryMeta>(header)?.tool.context("missing tool payload")
    }

    fn journal(&self) -> Result<ServerEvent> {
        Ok(serde_json::from_slice::<fuzukue_proto::Envelope>(&self.bytes()?)?.event)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Refs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
}

impl Refs {
    pub(crate) fn is_empty(&self) -> bool {
        self.input.is_none() && self.output.is_none()
    }

    pub(super) fn restore_marker(&mut self, attachments: &mut Vec<String>) {
        attachments.retain(|attachment| {
            let Some(marker) = attachment.strip_prefix(PAYLOAD_MARKER) else { return true };
            let (input, output) = marker.split_once(':').unwrap_or(("", ""));
            if input != "-" {
                self.input.get_or_insert_with(|| input.to_owned());
            }
            if output != "-" {
                self.output.get_or_insert_with(|| output.to_owned());
            }
            false
        });
    }

    pub(super) fn write_marker(&self, attachments: &mut Vec<String>) {
        if !self.is_empty() {
            attachments.push(format!("{PAYLOAD_MARKER}{}:{}",
                self.input.as_deref().unwrap_or("-"), self.output.as_deref().unwrap_or("-")));
        }
    }
}

#[derive(Clone)]
pub(crate) enum Source {
    Inline(Record),
    Journal(Record),
    Memory(Arc<MemoryRecord>),
}

pub(crate) struct MemoryRecord {
    pub(crate) event: ServerEvent,
    pub(crate) journal_prefixes: JournalPrefixes,
}

impl Source {
    pub(super) fn input(&self) -> Result<Value> {
        match self {
            Self::Inline(record) => record.inline()?.input.context("missing tool input"),
            Self::Journal(record) => match record.journal()? {
                ServerEvent::ToolCall { input, .. } => Ok(input),
                _ => anyhow::bail!("missing journal tool input"),
            },
            Self::Memory(record) => match &record.event {
                ServerEvent::ToolCall { input, .. } => Ok(input.clone()),
                _ => anyhow::bail!("missing in-memory tool input"),
            },
        }
    }

    pub(super) fn output(&self) -> Result<String> {
        match self {
            Self::Inline(record) => record.inline()?.output.context("missing tool output"),
            Self::Journal(record) => match record.journal()? {
                ServerEvent::ToolResult { output, .. } => Ok(output),
                _ => anyhow::bail!("missing journal tool output"),
            },
            Self::Memory(record) => match &record.event {
                ServerEvent::ToolResult { output, .. } => Ok(output.clone()),
                _ => anyhow::bail!("missing in-memory tool output"),
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct Blobs(PathBuf);

impl Blobs {
    pub(crate) fn new(threads: &Path) -> Self {
        Self(threads.join(THREAD_SAVE_DIR).join("payloads"))
    }

    pub(super) fn dir(&self) -> &Path {
        &self.0
    }

    pub(super) fn prepare(&self) -> Result<()> {
        super::save_files::private_dir(self.0.parent().expect("private directory"))?;
        self.prepare_dir()
    }

    fn prepare_dir(&self) -> Result<()> {
        match DirBuilder::new().mode(0o700).create(&self.0) {
            Ok(()) => durable::sync_dir(self.0.parent().unwrap())?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(&self.0)?.is_dir() => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn read(&self, digest: &str) -> Result<Vec<u8>> {
        ensure!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid tool payload digest");
        let parent = self.0.parent().expect("private directory");
        ensure!(fs::symlink_metadata(parent)?.is_dir() && fs::symlink_metadata(&self.0)?.is_dir(),
            "invalid payload directory");
        let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.0.join(digest))?;
        ensure!(file.metadata()?.is_file(), "invalid tool payload file");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(format!("{:x}", Sha256::digest(&bytes)) == digest, "damaged tool payload {digest}");
        Ok(bytes)
    }

    pub(super) fn input(&self, digest: &str) -> Result<Value> {
        Ok(serde_json::from_slice(&self.read(digest)?)?)
    }

    pub(super) fn output(&self, digest: &str) -> Result<String> {
        Ok(String::from_utf8(self.read(digest)?)?)
    }

    pub(super) fn put(&self, bytes: &[u8]) -> Result<String> {
        self.put_with(bytes, Self::prepare)
    }

    // The caller already holds the thread save directory lock.
    pub(super) fn put_locked(&self, bytes: &[u8]) -> Result<String> {
        self.put_with(bytes, Self::prepare_dir)
    }

    fn put_with(&self, bytes: &[u8], prepare: impl FnOnce(&Self) -> Result<()>) -> Result<String> {
        let (digest, _) = self.put_file(bytes, prepare)?;
        // An earlier writer may have reached rename just before a crash.
        durable::sync_dir(&self.0)?;
        Ok(digest)
    }

    // The conversion prepared the directory and syncs it once before
    // publication. Reuse verified files from earlier, aborted attempts.
    pub(super) fn stage(&self, bytes: &[u8]) -> Result<String> {
        let (digest, created) = self.put_file(bytes, |_| Ok(()))?;
        if created {
            self.verify(&digest, bytes)?;
        }
        Ok(digest)
    }

    fn put_file(&self, bytes: &[u8], prepare: impl FnOnce(&Self) -> Result<()>) -> Result<(String, bool)> {
        let digest = format!("{:x}", Sha256::digest(bytes));
        let path = self.0.join(&digest);
        if !path.exists() {
            prepare(self)?;
            let mut staged = tempfile::NamedTempFile::new_in(&self.0)?;
            staged.write_all(bytes)?;
            staged.as_file().sync_all()?;
            match staged.persist_noclobber(&path) {
                Ok(_) => return Ok((digest, true)),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => self.verify(&digest, bytes)?,
                Err(error) => return Err(error.error.into()),
            }
        } else {
            self.verify(&digest, bytes)?;
        }
        Ok((digest, false))
    }

    pub(super) fn verify(&self, digest: &str, original: &[u8]) -> Result<()> {
        ensure!(self.read(digest)? == original, "tool payload verification failed");
        Ok(())
    }

    /// The original event remains available to the caller until every blob is durable.
    pub(crate) fn compact(&self, event: &mut ServerEvent) -> Result<Refs> {
        let mut refs = Refs::default();
        match event {
            ServerEvent::ToolCall { item, name, input, .. } => {
                let delegate = name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB);
                if let Some(encoded) = oversized_input(input) {
                    refs.input = Some(self.put(&encoded)?);
                    *input = input_preview(input, &encoded, delegate);
                }
            }
            ServerEvent::ToolResult { item, output, .. } if output.len() > PREVIEW_BYTES => {
                refs.output = Some(self.put(output.as_bytes())?);
                *output = output_preview(output, item.starts_with(fuzukue_proto::DELEGATE_JOB));
            }
            _ => {}
        }
        Ok(refs)
    }
}

pub(super) fn oversized_input(input: &Value) -> Option<Vec<u8>> {
    // Measure without allocating a second copy for the common, small input.
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| std::io::Error::other("oversized tool input"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(Budget(INPUT_INLINE_BYTES), input).is_err()
        .then(|| serde_json::to_vec(input).expect("serializable tool input"))
}

fn head(text: &str) -> &str {
    &text[..text.floor_char_boundary(text.len().min(PREVIEW_BYTES))]
}

fn head_and_tail(text: &str) -> String {
    const GAP: &str = "\n…\n";
    if text.len() <= OUTPUT_PREVIEW_BYTES {
        return text.to_owned();
    }
    let half = (OUTPUT_PREVIEW_BYTES - GAP.len()) / 2;
    let head = text.floor_char_boundary(half);
    let tail = text.ceil_char_boundary(text.len() - half);
    let mut preview = String::with_capacity(head + GAP.len() + text.len() - tail);
    preview.push_str(&text[..head]);
    preview.push_str(GAP);
    preview.push_str(&text[tail..]);
    preview
}

pub(super) fn input_preview(input: &Value, encoded: &[u8], delegate: bool) -> Value {
    let text = std::str::from_utf8(encoded).expect("UTF-8 JSON");
    if delegate {
        if let Some(fields) = input.as_object() {
            let mut preview = serde_json::Map::new();
            for (key, value) in fields {
                if matches!(key.as_str(), "agent" | "title" | "label" | "label_source" | "card_id" | "queued_at_ms"
                    | "model" | "started_at_ms" | "dismissed_from_panel" | "selection" | "routing" | "waiting"
                    | "rename_history") && oversized_input(value).is_none()
                {
                    preview.insert(key.clone(), value.clone());
                }
            }
            preview.entry("model").or_insert(json!(""));
            preview.insert("preview".into(), json!(head(text)));
            return Value::Object(preview);
        }
    }
    let mut preview = json!({"preview": head(text)});
    // Keep useful labels and test classification when a large argument is present.
    for key in ["command", "file_path", "path", "file", "notebook_path"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            preview[key] = json!(head(value));
        }
    }
    preview
}

pub(super) fn needs_output_preview(output: &str) -> bool {
    output.len() > PREVIEW_BYTES
}

pub(super) fn output_preview(output: &str, delegate: bool) -> String {
    if delegate {
        if !needs_output_preview(output) {
            return output.to_owned();
        }
        if let Ok(mut report) = serde_json::from_str::<DelegateReport>(output) {
            if report.text.len() > PREVIEW_BYTES {
                report.text = head_and_tail(&report.text);
            }
            if report.changes.len() > PREVIEW_BYTES {
                report.changes = head_and_tail(&report.changes);
            }
            let changed = report.files.as_mut().and_then(|observation| {
                let count = observation.changed_files.as_ref().map(Vec::len);
                for paths in [&mut observation.touches, &mut observation.changed_files].into_iter().flatten() {
                    let mut remaining = PREVIEW_BYTES;
                    let count = paths.iter().take_while(|path| {
                        if let Some(rest) = remaining.checked_sub(path.len() + 3) {
                            remaining = rest;
                            true
                        } else {
                            false
                        }
                    }).count();
                    paths.truncate(count);
                }
                count
            });
            for attempt in &mut report.attempts {
                if let Some(reason) = &mut attempt.escalation_reason {
                    if reason.len() > PREVIEW_BYTES {
                        *reason = format!("{}…", head(reason));
                    }
                }
            }
            let mut preview = serde_json::to_value(report).expect("serializable job report");
            if let Some(changed) = changed {
                preview["preview_changed_file_count"] = changed.into();
            }
            return preview.to_string();
        }
    }
    head_and_tail(output)
}

pub(crate) fn bound_replay(event: &mut ServerEvent) -> bool {
    match event {
        ServerEvent::ToolCall { item, name, input, .. } => {
            if let Some(encoded) = oversized_input(input) {
                *input = input_preview(input, &encoded, name == "delegate" && item.starts_with(fuzukue_proto::DELEGATE_JOB));
                return true;
            }
        }
        ServerEvent::ToolResult { item, output, .. } if needs_output_preview(output) => {
            *output = output_preview(output, item.starts_with(fuzukue_proto::DELEGATE_JOB));
            return true;
        }
        _ => {}
    }
    false
}
