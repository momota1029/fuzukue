//! The segmented thread format: older entries in immutable segment files, and a
//! latest file holding the front matter, changed older entries and new entries.
//!
//! Loading checks every segment before the journal is applied. Any mismatch is
//! an error, which keeps the thread unwritable until the files are repaired.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use sha2::{Digest as _, Sha256};

use super::{entries::Digest, Entry, FileVersion, Front, Transcript, ENTRY};

pub(super) const FORMAT: &str = "segmented-1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SegmentInfo {
    pub(super) file: String,
    pub(super) entries: usize,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

/// The segmentation fields of a latest file's front matter.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    pub(super) segments: Vec<SegmentInfo>,
    pub(super) overrides: Vec<usize>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SegmentFront {
    pub(super) thread_id: String,
    pub(super) segment: usize,
}

/// A verified segment of a loaded transcript.
pub(super) struct Segment {
    pub(super) info: SegmentInfo,
    path: PathBuf,
    version: RefCell<FileVersion>,
}

impl Segment {
    /// Saves refer to this file, so confirm it still holds the loaded bytes.
    pub(super) fn verify_unchanged(&self) -> Result<()> {
        let check = || -> Result<()> {
            let mut file = open_regular(&self.path)?;
            let version = FileVersion::read(file.metadata()?)?;
            if *self.version.borrow() != version {
                ensure!(version.length == self.info.bytes && file_digest(&mut file)? == self.info.sha256,
                    "segment content changed");
                self.version.replace(version);
            }
            Ok(())
        };
        check().with_context(|| format!("thread segment {}", self.path.display()))
    }
}

pub(super) fn parse_front(yaml: &str) -> Result<(Front, Option<Layout>)> {
    let value: Value = serde_yaml::from_str(yaml)?;
    if value.get("format").is_none() {
        return Ok((serde_yaml::from_str(yaml)?, None));
    }
    let Value::Mapping(mut fields) = value else { bail!("invalid thread front matter") };
    ensure!(fields.remove("format").as_ref().and_then(Value::as_str) == Some(FORMAT),
        "unsupported thread format");
    ensure!(!fields.contains_key("id"), "segmented thread has an unexpected id field");
    let id = fields.remove("thread_id").context("segmented thread is missing thread_id")?;
    let segments: Vec<SegmentInfo> = serde_yaml::from_value(
        fields.remove("segments").context("segmented thread is missing its segment list")?)?;
    let overrides: Vec<usize> = serde_yaml::from_value(
        fields.remove("overrides").context("segmented thread is missing its override list")?)?;
    ensure!(!segments.is_empty(), "segmented thread lists no segments");
    fields.insert("id".into(), id);
    Ok((serde_yaml::from_value(Value::Mapping(fields))?, Some(Layout { segments, overrides })))
}

pub(super) fn front_text(front: &Front, segments: &[&SegmentInfo], overrides: &[usize]) -> Result<String> {
    let Value::Mapping(fields) = serde_yaml::to_value(front)? else { bail!("invalid thread front matter") };
    let mut output = Mapping::new();
    output.insert("format".into(), FORMAT.into());
    output.insert("thread_id".into(), front.id.as_str().into());
    output.extend(fields.into_iter().filter(|(key, _)| key.as_str() != Some("id")));
    output.insert("segments".into(), serde_yaml::to_value(segments)?);
    output.insert("overrides".into(), serde_yaml::to_value(overrides)?);
    Ok(format!("---\n{}---\n", serde_yaml::to_string(&output)?))
}

pub(super) fn file_name(number: usize, sha256: &str) -> Option<String> {
    Some(format!("{number:06}-{}.md", sha256.get(..16)?))
}

/// `threads/<id>/segments`, refusing IDs and directories that lead elsewhere.
pub(super) fn directory_path(latest: &Path, id: &str) -> Result<PathBuf> {
    ensure!(!id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']),
        "invalid segmented thread ID");
    Ok(latest.parent().context("missing thread directory")?.join(id).join("segments"))
}

pub(super) fn directory(latest: &Path, id: &str) -> Result<PathBuf> {
    let segments = directory_path(latest, id)?;
    let thread = segments.parent().context("missing segment parent")?;
    for dir in [thread, &segments] {
        ensure!(fs::symlink_metadata(dir).with_context(|| format!("thread segments {}", dir.display()))?.is_dir(),
            "invalid thread segment directory {}", dir.display());
    }
    Ok(segments)
}

fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(), "not a regular file");
    Ok(file)
}

fn file_digest(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    std::io::copy(file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

/// The digest of an entry as it is stored: its marker and the following text.
pub(super) fn stored_digest(raw: &str) -> Digest {
    let mut digest = Sha256::new();
    digest.update(ENTRY.as_bytes());
    digest.update(raw.as_bytes());
    digest.finalize().into()
}

/// Verify one listed segment and visit its entries with their stored text,
/// which excludes the leading entry marker.
pub(super) fn read_segment(
    directory: &Path, id: &str, number: usize, info: &SegmentInfo, mut visit: impl FnMut(Entry, &str),
) -> Result<Segment> {
    ensure!(info.sha256.len() == 64 && info.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid segment digest in the thread list");
    ensure!(file_name(number, &info.sha256).as_deref() == Some(info.file.as_str()),
        "thread segment list is out of order or misnamed: {}", info.file);
    let path = directory.join(&info.file);
    let mut read = || -> Result<Segment> {
        let mut file = open_regular(&path)?;
        let version = FileVersion::read(file.metadata()?)?;
        ensure!(version.length == info.bytes, "segment length differs from the thread list");
        ensure!(file_digest(&mut file)? == info.sha256, "segment digest differs from the thread list");
        file.seek(SeekFrom::Start(0))?;
        let file = Arc::new(file);
        let mut input = BufReader::new(file.as_ref());
        let mut text = String::new();
        input.read_line(&mut text)?;
        ensure!(text == "---\n", "missing segment front matter");
        loop {
            let start = text.len();
            ensure!(input.read_line(&mut text)? != 0, "unterminated segment front matter");
            if &text[start..] == "---\n" { break; }
        }
        let (yaml, _) = fuzukue_cards::split_frontmatter(&text).context("missing segment front matter")?;
        let front: SegmentFront = serde_yaml::from_str(yaml)?;
        ensure!(front.thread_id == id, "segment belongs to another thread");
        ensure!(front.segment == number, "segment number differs from the thread list");
        let mut count = 0;
        Transcript::read_entries(&mut input, &file, text.len() as u64, |entry, raw| {
            count += 1;
            visit(entry, raw);
        })?;
        ensure!(count == info.entries, "segment entry count differs from the thread list");
        Ok(Segment { info: info.clone(), path: path.clone(), version: RefCell::new(version) })
    };
    read().with_context(|| format!("thread segment {}", path.display()))
}

/// How many threads a load may use for segment work.
pub(super) fn parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, |count| count.get().min(8))
}

/// Check the whole layout and assemble the entries the single-file format would
/// hold. `latest` contains the latest file's entries, overrides first.
pub(super) fn compose(path: &Path, id: &str, layout: &Layout, latest: Vec<Entry>) -> Result<Transcript> {
    let directory = directory(path, id)?;
    let mut segments = Vec::new();
    let mut sealed = Vec::new();
    let mut originals = Vec::new();
    // Item IDs must stay within one file. A repeat across files means two
    // copies of an entry, or a segment that does not belong in this list.
    let mut files = HashMap::<String, usize>::new();
    let mut unique = |entry: &Entry, file: usize| -> bool {
        files.insert(entry.meta.item.clone(), file).is_none_or(|previous| previous == file)
    };
    let read = |number: usize, info: &SegmentInfo| -> Result<(Segment, Vec<(Entry, Digest)>)> {
        let mut entries = Vec::new();
        let segment = read_segment(&directory, id, number, info,
            |entry, raw| entries.push((entry, stored_digest(raw))))?;
        Ok((segment, entries))
    };
    // Segments are independent files, so verify and parse several at once.
    for (group, infos) in layout.segments.chunks(parallelism()).enumerate() {
        let first = group * parallelism();
        let results: Vec<_> = std::thread::scope(|scope| {
            let read = &read;
            let workers: Vec<_> = infos.iter().enumerate()
                .map(|(offset, info)| scope.spawn(move || read(first + offset + 1, info))).collect();
            workers.into_iter().map(|worker| worker.join().expect("segment reader")).collect()
        });
        for (offset, result) in results.into_iter().enumerate() {
            let (segment, entries) = result?;
            segments.push(segment);
            for (entry, original) in entries {
                if !unique(&entry, first + offset) {
                    bail!("duplicate thread item ID {} appears in more than one segment", entry.meta.item);
                }
                originals.push(original);
                sealed.push(entry);
            }
        }
    }
    ensure!(layout.overrides.windows(2).all(|pair| pair[0] < pair[1])
        && layout.overrides.last().is_none_or(|&last| last < sealed.len()),
        "invalid thread override positions");
    ensure!(latest.len() >= layout.overrides.len(), "thread overrides are missing from the latest file");
    let mut latest = latest.into_iter();
    for &position in &layout.overrides {
        let replacement = latest.next().expect("checked override count");
        let original = &mut sealed[position];
        ensure!(original.meta.item == replacement.meta.item && original.meta.actor == replacement.meta.actor,
            "thread override {position} does not match its segment entry");
        *original = replacement;
    }
    let latest: Vec<_> = latest.collect();
    for entry in &latest {
        ensure!(unique(entry, layout.segments.len()),
            "duplicate thread item ID {} appears in both a segment and the latest file", entry.meta.item);
    }
    let mut transcript = Transcript::default();
    for entry in sealed.into_iter().chain(latest) {
        transcript.push(entry);
    }
    let replaced: BTreeSet<_> = layout.overrides.iter().copied().collect();
    transcript.items.seal(segments, originals, replaced);
    Ok(transcript)
}
