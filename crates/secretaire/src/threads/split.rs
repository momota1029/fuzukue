//! Stage a manual split of an idle thread and publish it by atomic exchange.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::join::{open_regular, retain_original, segmentation_scratch};
use super::payload_migration::{exchange, hash, LiveVersion};
use super::{entry_digest, read_front, rollback_marker, save_files, segments, timing, Entry,
    Front, Store, Transcript, TranscriptSource, ENTRY};
use crate::durable;

const MIB: usize = 1024 * 1024;
const MIN_LATEST: usize = 4 * MIB;
const MIN_ENTRIES: usize = 256;
const MAX_SEGMENT: usize = 16 * MIB;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    Saved,
    SegmentWritten,
    SegmentsSynced,
    LatestWritten,
    LatestSynced,
    Verified,
    OriginalKept,
    Marked,
    Exchanged,
    Published,
}

pub(crate) struct SplitCandidate {
    pub(crate) id: String,
    path: PathBuf,
    version: LiveVersion,
    revision: u64,
    front: Front,
    source: TranscriptSource,
    segmented: bool,
    marker_root: PathBuf,
    project: PathBuf,
}

pub(crate) struct StagedSplit {
    pub(crate) candidate: SplitCandidate,
    live: File,
    verified: LiveVersion,
    replacement: NamedTempFile,
    scratch: File,
    segments: Vec<segments::Segment>,
    source_digest: [u8; 32],
    latest_digest: [u8; 32],
    stats: SplitStats,
    release_mark: Option<rollback_marker::Listed>,
}

impl Drop for StagedSplit {
    fn drop(&mut self) {
        let Some(entry) = self.release_mark.take() else { return };
        let c = &self.candidate;
        let is_single = || {
            let inspect = || -> Result<bool> {
                let file = open_regular(&c.path)?;
                let version = LiveVersion::read(file.metadata()?)?;
                let mut input = BufReader::new(file);
                let (front, layout, _) = read_front(&mut input)?;
                Ok(front.id == c.id && layout.is_none()
                    && LiveVersion::read(input.get_ref().metadata()?)? == version
                    && LiveVersion::read(fs::symlink_metadata(&c.path)?)? == version)
            };
            inspect().unwrap_or(false)
        };
        if let Err(error) = rollback_marker::release_split(&c.marker_root, entry, is_single) {
            tracing::error!(%error, thread = %c.id, "failed to release refused split rollback mark");
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SplitStats {
    pub(crate) segments: usize,
    pub(crate) latest_bytes: u64,
    pub(crate) overrides: usize,
}

struct RawEntry {
    text: String,
    prompt: bool,
}

#[derive(PartialEq, Eq)]
struct ReplayItem {
    id: String,
    kind: &'static str,
    digest: super::entries::Digest,
}

impl ReplayItem {
    fn stored(entry: &Entry, raw: &str) -> Self {
        Self::new(entry, segments::stored_digest(raw))
    }

    fn new(entry: &Entry, digest: super::entries::Digest) -> Self {
        Self { id: entry.meta.item.clone(), kind: item_kind(entry), digest }
    }

    fn label(&self) -> String {
        format!("item ID {:?} ({})", self.id, self.kind)
    }
}

fn item_kind(entry: &Entry) -> &'static str {
    if entry.meta.tool.is_some() {
        "tool"
    } else if entry.meta.notice.is_some() {
        "notice"
    } else if !entry.meta.github.is_empty() {
        "GitHub activity"
    } else {
        match entry.meta.actor {
            fuzukue_proto::Actor::User => "user message",
            fuzukue_proto::Actor::Claude => "Claude message",
            fuzukue_proto::Actor::Codex { .. } => "Codex message",
            fuzukue_proto::Actor::System => "system message",
        }
    }
}

fn replay_items(transcript: &Transcript) -> Result<Vec<ReplayItem>> {
    transcript.items.iter().map(|entry| {
        let digest = entry_digest(entry).map_err(|_| anyhow::anyhow!(
            "split replay could not serialize item ID {:?} ({})", entry.meta.item, item_kind(entry)
        ))?;
        Ok(ReplayItem::new(entry, digest))
    }).collect()
}

fn unique_ids(transcript: &Transcript) -> Result<()> {
    let mut seen = HashSet::new();
    for entry in transcript.items.iter() {
        ensure!(seen.insert(&entry.meta.item), "duplicate thread item ID {:?}", entry.meta.item);
    }
    Ok(())
}

fn compare_replay(expected: &[ReplayItem], actual: &[ReplayItem]) -> Result<()> {
    for (saved, replayed) in expected.iter().zip(actual) {
        if saved != replayed {
            if saved.id == replayed.id && saved.kind == replayed.kind {
                anyhow::bail!("split replay mismatch at {}", saved.label());
            }
            anyhow::bail!("split replay mismatch at {}; replay found {}", saved.label(), replayed.label());
        }
    }
    if let Some(item) = expected.get(actual.len()) {
        anyhow::bail!("split replay mismatch at {}; missing from replay", item.label());
    }
    if let Some(item) = actual.get(expected.len()) {
        anyhow::bail!("split replay mismatch at {}; added by replay", item.label());
    }
    Ok(())
}

fn verify_replay(before: &[ReplayItem], after: &Transcript, overrides: &[usize]) -> Result<()> {
    unique_ids(after)?;
    compare_replay(before, &replay_items(after)?)?;
    let actual = after.items.overrides();
    if actual != overrides {
        let position = overrides.iter().zip(&actual).find(|(a, b)| a != b)
            .map(|(&a, &b)| a.min(b))
            .or_else(|| overrides.get(actual.len()).or_else(|| actual.get(overrides.len())).copied())
            .expect("different override lists");
        anyhow::bail!("split replay mismatch at {}; override state changed", before[position].label());
    }
    Ok(())
}

struct SplitPlan {
    front: Front,
    infos: Vec<segments::SegmentInfo>,
    overrides: Vec<usize>,
    raws: Vec<RawEntry>,
    cuts: Vec<usize>,
    before: Vec<ReplayItem>,
    source_digest: [u8; 32],
    source_body: [u8; 32],
}

/// Length of the first entries (including their entry markers), at each cut.
fn offsets(entries: &[RawEntry]) -> Vec<usize> {
    let mut offsets = vec![0];
    for entry in entries {
        offsets.push(offsets.last().unwrap() + ENTRY.len() + entry.text.len());
    }
    offsets
}

fn boundaries(entries: &[RawEntry]) -> Result<Vec<usize>> {
    let offsets = offsets(entries);
    let total = offsets[entries.len()];
    let last = (1..entries.len()).rev().find(|&pos| {
        entries[pos].prompt && entries.len() - pos >= MIN_ENTRIES && total - offsets[pos] >= MIN_LATEST
    }).context("no user input boundary leaves at least 4 MiB and 256 entries in the latest file")?;

    let mut cuts = vec![0];
    let mut available = (1..last).filter(|&pos| entries[pos].prompt).peekable();
    while offsets[last] - offsets[*cuts.last().unwrap()] > MAX_SEGMENT {
        let start = *cuts.last().unwrap();
        // Prefer the last cut within 16 MiB that leaves at least 4 MiB.
        // A turn larger than the target stays intact.
        let mut preferred = None;
        while let Some(pos) = available.next_if(|&pos| {
            offsets[pos] - offsets[start] <= MAX_SEGMENT
                && offsets[last] - offsets[pos] >= MIN_LATEST
        }) {
            preferred = Some(pos);
        }
        let Some(next) = preferred.or_else(|| available.next()) else { break };
        cuts.push(next);
    }
    cuts.push(last);
    Ok(cuts)
}

fn write_entries<'a>(out: &mut impl Write, raws: impl IntoIterator<Item = &'a RawEntry>) -> Result<()> {
    for raw in raws {
        out.write_all(ENTRY.as_bytes())?;
        out.write_all(raw.text.as_bytes())?;
    }
    Ok(())
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    ensure!(fs::symlink_metadata(path)?.is_dir(), "invalid segment directory {}", path.display());
    Ok(())
}

fn segment_header(id: &str, number: usize) -> Result<String> {
    Ok(format!("---\n{}---\n", serde_yaml::to_string(&segments::SegmentFront {
        thread_id: id.into(), segment: number,
    })?))
}

fn segment_info(id: &str, number: usize, entries: &[RawEntry]) -> Result<segments::SegmentInfo> {
    let header = segment_header(id, number)?;
    let mut digest = Sha256::new();
    digest.update(header.as_bytes());
    write_entries(&mut digest, entries)?;
    let sha256 = format!("{:x}", digest.finalize());
    let file = segments::file_name(number, &sha256).context("invalid segment digest")?;
    let bytes = header.len() + entries.iter().map(|entry| ENTRY.len() + entry.text.len()).sum::<usize>();
    Ok(segments::SegmentInfo { file, entries: entries.len(), bytes: bytes as u64, sha256 })
}

fn stage_segment(
    dir: &Path, id: &str, number: usize, info: &segments::SegmentInfo, entries: &[RawEntry],
) -> Result<()> {
    let mut file = NamedTempFile::new_in(dir)?;
    {
        let mut output = BufWriter::new(file.as_file_mut());
        output.write_all(segment_header(id, number)?.as_bytes())?;
        write_entries(&mut output, entries)?;
        output.flush()?;
    }
    file.as_file().sync_all()?;
    match file.persist_noclobber(dir.join(&info.file)) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_regular(&dir.join(&info.file))?.sync_all()?;
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}
fn metadata_if_present(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn check_space(threads: &Path, source_bytes: u64) -> Result<()> {
    let stat = rustix::fs::statvfs(threads).context("could not check free space for the split")?;
    let required = source_bytes.saturating_mul(3).saturating_add(16 * MIB as u64);
    ensure!(stat.f_bavail.saturating_mul(stat.f_frsize) >= required,
        "insufficient free space to split the thread (need {required} bytes)");
    Ok(())
}

impl SplitCandidate {
    pub(crate) fn ensure_same(&self, current: &Self) -> Result<()> {
        ensure!(self.path == current.path && self.version == current.version
            && self.revision == current.revision && self.segmented == current.segmented,
            "thread changed during split verification");
        Ok(())
    }

    /// Check the published generation and model the split without writing it.
    fn plan(&self) -> Result<SplitPlan> {
        let threads = self.path.parent().context("missing thread directory")?;
        check_space(threads, self.version.file.length)?;
        let file = Arc::new(open_regular(&self.path)?);
        ensure!(LiveVersion::read(file.metadata()?)? == self.version, "thread changed before split verification");
        let source_digest = hash(&*file)?;
        file.as_ref().rewind()?;
        let mut input = BufReader::new(file.as_ref());
        let (front, layout, offset) = read_front(&mut input)?;
        ensure!(front.id == self.id && serde_yaml::to_value(&front)? == serde_yaml::to_value(&self.front)?
            && layout.is_some() == self.segmented, "thread settings differ from the saved file");

        let mut raws = Vec::new();
        let mut stored = Vec::new();
        let mut parsed = Vec::new();
        let mut source_body = Sha256::new();
        Transcript::read_entries(&mut input, &file, offset, |entry, raw| {
            source_body.update(ENTRY.as_bytes());
            source_body.update(raw.as_bytes());
            stored.push(ReplayItem::stored(&entry, raw));
            raws.push(RawEntry {
                text: raw.into(),
                prompt: matches!(entry.meta.actor, fuzukue_proto::Actor::User)
                    && !entry.meta.to_worker && !entry.meta.item.starts_with(fuzukue_proto::slash::ECHO),
            });
            parsed.push(entry);
        })?;
        let mut prospective = if let Some(layout) = &layout {
            segments::compose(&self.path, &self.id, layout, parsed)?
        } else {
            let mut transcript = Transcript::default();
            for entry in parsed {
                transcript.push(entry);
            }
            transcript
        };
        unique_ids(&prospective)?;
        let (mut infos, overrides) = layout.map_or_else(Default::default, |layout| (layout.segments, layout.overrides));
        let latest = &raws[overrides.len()..];
        let cuts = boundaries(latest)?;
        let before = {
            let transcript = self.source.read("split")?;
            unique_ids(&transcript)?;
            replay_items(&transcript)?
        };
        if !self.segmented {
            compare_replay(&stored, &before)?;
        }

        // Only entries that round-trip byte for byte may move into a segment.
        let first = prospective.items.sealed_len();
        let moved = *cuts.last().unwrap();
        let moved_items = &stored[overrides.len()..overrides.len() + moved];
        for (item, entry) in moved_items.iter().zip(&prospective.items[first..first + moved]) {
            let encoded = entry_digest(entry).ok();
            ensure!(encoded == Some(item.digest), "split replay mismatch at {}; stored entry does not round-trip", item.label());
        }
        prospective.items.extend_sealed_prefix(moved_items.iter().map(|item| item.digest).collect());
        let replayed = self.source.replay(prospective, timing::Load::start("split_check"));
        verify_replay(&before, &replayed, &overrides)?;

        let directory = segments::directory_path(&self.path, &self.id)?;
        let parent = directory.parent().context("missing segment parent")?;
        for dir in [parent, &directory] {
            if let Some(metadata) = metadata_if_present(dir)? {
                ensure!(metadata.is_dir(), "invalid segment directory {}", dir.display());
            }
        }
        for cut in cuts.windows(2) {
            let number = infos.len() + 1;
            let info = segment_info(&self.id, number, &latest[cut[0]..cut[1]])?;
            if metadata_if_present(&directory.join(&info.file))?.is_some() {
                segments::read_segment(&directory, &self.id, number, &info, |_, _| {})?;
            }
            infos.push(info);
        }
        ensure!(LiveVersion::read(file.metadata()?)? == self.version
            && LiveVersion::read(fs::symlink_metadata(&self.path)?)? == self.version,
            "thread changed during split verification");
        Ok(SplitPlan { front, infos, overrides, raws, cuts, before, source_digest, source_body: source_body.finalize().into() })
    }

    pub(crate) fn check(&self) -> Result<()> {
        let threads = self.path.parent().context("missing thread directory")?;
        let _scratch = super::join::check_segmentation_scratch(threads)?;
        rollback_marker::listed(&self.marker_root).context("cannot read the split rollback marks")?;
        self.plan()?;
        Ok(())
    }

    /// Construct and verify the new generation outside the Hub lock.
    pub(crate) fn stage(self, checkpoint: &mut impl FnMut(Step)) -> Result<StagedSplit> {
        checkpoint(Step::Saved);
        let threads = self.path.parent().context("missing thread directory")?;
        let SplitPlan { front, infos, overrides, raws, cuts, before, source_digest, source_body } = self.plan()?;
        let (scratch_path, scratch) = segmentation_scratch(threads)?;
        let (overridden, latest) = raws.split_at(overrides.len());

        // An existing segment is immutable. Only the latest portion moves.
        let directory = segments::directory_path(&self.path, &self.id)?;
        let parent = directory.parent().context("missing segment parent")?;
        create_directory(parent)?;
        create_directory(&directory)?;
        let first_new = infos.len() - (cuts.len() - 1);
        for (index, cut) in cuts.windows(2).enumerate() {
            let ordinal = first_new + index;
            stage_segment(&directory, &self.id, ordinal + 1, &infos[ordinal], &latest[cut[0]..cut[1]])?;
            checkpoint(Step::SegmentWritten);
        }
        let tail = &latest[*cuts.last().unwrap()..];
        let mut replacement = tempfile::Builder::new().prefix("displaced-").tempfile_in(&scratch_path)?;
        {
            let mut output = BufWriter::new(replacement.as_file_mut());
            let references: Vec<_> = infos.iter().collect();
            output.write_all(segments::front_text(&front, &references, &overrides)?.as_bytes())?;
            write_entries(&mut output, overridden.iter().chain(tail))?;
            output.flush()?;
        }
        checkpoint(Step::LatestWritten);
        replacement.as_file().sync_all()?;
        scratch.sync_all()?;
        checkpoint(Step::LatestSynced);

        // Read the actual staged files. Existing overrides include the full
        // replacement of old entries. The new segments must round-trip verbatim.
        let reopened = Arc::new(open_regular(replacement.path())?);
        let mut input = BufReader::new(reopened.as_ref());
        let (staged_front, staged_layout, staged_offset) = read_front(&mut input)?;
        let staged_layout = staged_layout.context("staged thread lost its segment layout")?;
        ensure!(serde_yaml::to_value(&staged_front)? == serde_yaml::to_value(&front)?
            && infos == staged_layout.segments && overrides == staged_layout.overrides,
            "staged thread settings differ");
        let mut body = Sha256::new();
        let mut latest_index = 0;
        let mut verified = Vec::with_capacity(infos.len());
        let mut staged_latest = Vec::new();
        Transcript::read_entries(&mut input, &reopened, staged_offset, |_, raw| {
            staged_latest.push(RawEntry { text: raw.into(), prompt: false });
        })?;
        ensure!(staged_latest.len() == overridden.len() + tail.len(), "staged latest entry count differs");
        let (saved_overrides, saved_tail) = staged_latest.split_at(overridden.len());
        write_entries(&mut body, saved_overrides)?;
        let mut encoded = Vec::new();
        for (index, info) in infos.iter().enumerate() {
            let mut checked: Result<()> = Ok(());
            let segment = segments::read_segment(&directory, &self.id, index + 1, info, |entry, raw| {
                if index < first_new || checked.is_err() {
                    return;
                }
                checked = (|| {
                    encoded.clear();
                    Transcript::write_entry(&entry, &mut encoded)?;
                    ensure!(encoded.strip_prefix(ENTRY.as_bytes()) == Some(raw.as_bytes()),
                        "new segment entry {} does not round-trip", entry.meta.item);
                    body.update(&encoded);
                    latest_index += 1;
                    Ok(())
                })();
            })?;
            checked?;
            verified.push(segment);
        }
        write_entries(&mut body, saved_tail)?;
        ensure!(latest_index == *cuts.last().unwrap() && <[u8; 32]>::from(body.finalize()) == source_body,
            "staged entries do not match the original body");
        durable::sync_dir(&directory)?;
        durable::sync_dir(parent)?;
        durable::sync_dir(threads)?;
        checkpoint(Step::SegmentsSynced);

        drop((raws, staged_latest));
        let replayed = self.source.read_from(replacement.path(), "split_verify")?;
        verify_replay(&before, &replayed, &overrides)?;
        reopened.as_ref().rewind()?;
        let latest_digest = hash(&*reopened)?;
        checkpoint(Step::Verified);

        ensure!(LiveVersion::read(fs::symlink_metadata(&self.path)?)? == self.version,
            "thread changed before the original could be kept");
        retain_original(&self.path, &scratch_path, source_digest)?;
        let live = open_regular(&self.path)?;
        let verified_live = LiveVersion::read(live.metadata()?)?;
        // Our hard link changes the inode's ctime. Establish the new ctime
        // only after checking the original bytes and the same file version.
        ensure!(verified_live.file == self.version.file && hash(&live)? == source_digest
            && LiveVersion::read(live.metadata()?)? == verified_live
            && LiveVersion::read(fs::symlink_metadata(&self.path)?)? == verified_live,
            "thread changed before replacing it");
        checkpoint(Step::OriginalKept);
        let stats = SplitStats {
            segments: infos.len(), latest_bytes: replacement.as_file().metadata()?.len(), overrides: overrides.len(),
        };
        Ok(StagedSplit {
            candidate: self, live, verified: verified_live, replacement, scratch,
            segments: verified, source_digest, latest_digest, stats, release_mark: None,
        })
    }
}

impl StagedSplit {
    pub(crate) fn mark(&mut self, checkpoint: &mut impl FnMut(Step)) -> Result<()> {
        let c = &self.candidate;
        let file = c.path.file_name().and_then(|name| name.to_str()).context("invalid thread filename")?;
        rollback_marker::mark_split(&c.marker_root, &c.project, &c.id, file, c.segmented, &mut self.release_mark)?;
        // A retry may encounter a mark that already exists.
        durable::sync_dir(&c.marker_root)?;
        checkpoint(Step::Marked);
        Ok(())
    }
}

impl Store {
    pub(crate) fn prepare_split(&self, id: &str) -> Result<SplitCandidate> {
        let thread = self.threads.get(id).context("thread not found")?;
        ensure!(!thread.active, "busy: thread is running");
        ensure!(!id.starts_with("thread-issue-"), "issue-number threads cannot be split");
        ensure!(thread.front.migrated_from.is_none(), "migrated legacy-session threads cannot be split");
        ensure!(thread.front.preview_payloads, "wait for payload conversion before splitting this thread");
        ensure!(!thread.transcript.volatile_payloads, "thread has payloads held only in memory");
        ensure!(!thread.transcript.dirty.get(), "thread has unsaved changes");
        let version = LiveVersion::read(fs::symlink_metadata(&thread.path)?)?;
        ensure!(thread.file_version.borrow().as_ref() == Some(&version.file), "saved thread changed on disk");
        Ok(SplitCandidate {
            id: id.into(), path: thread.path.clone(), version, revision: thread.transcript.activity_revision,
            front: thread.front.clone(), source: thread.transcript.source.clone().context("thread has no source")?,
            segmented: thread.segmented, marker_root: self.marker_root.clone(), project: self.project.clone(),
        })
    }

    pub(crate) fn publish_split(&mut self, mut staged: StagedSplit, checkpoint: &mut impl FnMut(Step)) -> Result<SplitStats> {
        const CHANGED: &str = "thread changed since the split started";
        let c = &staged.candidate;
        let thread = self.threads.get_mut(&c.id).context(CHANGED)?;
        ensure!(!thread.active && !thread.transcript.dirty.get() && !thread.transcript.volatile_payloads
            && thread.path == c.path && thread.segmented == c.segmented
            && thread.transcript.activity_revision == c.revision
            && thread.file_version.borrow().as_ref() == Some(&c.version.file), CHANGED);

        let threads = c.path.parent().context("missing thread directory")?;
        let _lock = save_files::try_private_dir(&threads.join(fuzukue_proto::THREAD_SAVE_DIR))
            .context("could not lock the thread save directory for split publication")?;
        for segment in &staged.segments {
            segment.verify_unchanged()?;
        }
        let replacement = LiveVersion::read(staged.replacement.as_file().metadata()?)?;
        ensure!(hash(open_regular(staged.replacement.path())?)? == staged.latest_digest
            && LiveVersion::read(fs::symlink_metadata(staged.replacement.path())?)? == replacement,
            "staged latest file changed before publication");
        ensure!(hash(open_regular(&c.path)?)? == staged.source_digest
            && LiveVersion::read(staged.live.metadata()?)? == staged.verified
            && LiveVersion::read(fs::symlink_metadata(&c.path)?)? == staged.verified, CHANGED);
        exchange(staged.replacement.path(), &c.path)?;
        staged.release_mark = None;
        // Keep the displaced file whenever publication cannot be confirmed.
        staged.replacement.disable_cleanup(true);
        thread.file_version.replace(Some(replacement.file));
        thread.segmented = true;
        thread.transcript.loaded.take();
        checkpoint(Step::Exchanged);
        let displaced = fs::symlink_metadata(staged.replacement.path()).and_then(super::FileVersion::read);
        let displaced_matches = displaced.is_ok_and(|old| old == c.version.file)
            && open_regular(staged.replacement.path()).and_then(hash).is_ok_and(|digest| digest == staged.source_digest);
        if !displaced_matches {
            tracing::warn!(path = ?c.path, preserved = ?staged.replacement.path(),
                "preserved a concurrent external thread replacement");
        }
        durable::sync_dir(threads).context("split was published but the thread directory could not be synced")?;
        staged.scratch.sync_all().context("split was published but the scratch directory could not be synced")?;
        let published = open_regular(&c.path).and_then(hash)
            .context("split was published but the live file could not be verified")?;
        ensure!(published == staged.latest_digest, "published thread differs from the verified file");
        checkpoint(Step::Published);
        if displaced_matches {
            staged.replacement.disable_cleanup(false);
        }
        ensure!(displaced_matches, "split was published, but the displaced thread differed and was preserved at {}",
            staged.replacement.path().display());
        Ok(staged.stats)
    }
}
