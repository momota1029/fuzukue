//! Rejoin a segmented thread into the single-file format.
//!
//! Staging copies the stored bytes of every entry into one file, verifies that it
//! loads like the segmented thread, and keeps the replaced latest file. The
//! atomic exchange in `publish_join` is the only point that changes the thread.
//! Segment files are left in place.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use super::payload_migration::{exchange, hash, LiveVersion};
use super::{read_front, rollback_marker, save_files, segments, FileVersion, Front, Store, Transcript,
    TranscriptSource, ENTRY};
use crate::durable;

pub(crate) struct JoinCandidate {
    pub(crate) id: String,
    path: PathBuf,
    version: FileVersion,
    activity_revision: u64,
    front: Front,
    source: TranscriptSource,
    marker_root: PathBuf,
    project: PathBuf,
}

pub(crate) struct StagedJoin {
    pub(crate) candidate: JoinCandidate,
    live: File,
    verified: LiveVersion,
    replacement: NamedTempFile,
    scratch: File,
    segments: Vec<segments::Segment>,
    digest: [u8; 32],
    /// Digest of the latest file that publication replaces.
    replaced: [u8; 32],
}

/// The number of entries, their IDs in order, and the digest of the body the
/// single-file format would write.
fn summary(transcript: &Transcript) -> Result<(Vec<String>, [u8; 32])> {
    let mut digest = Sha256::new();
    transcript.write_markdown(&mut digest)?;
    Ok((transcript.items.iter().map(|entry| entry.meta.item.clone()).collect(), digest.finalize().into()))
}

fn open_regular(path: &Path) -> Result<File> {
    let file = File::from(open(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?);
    ensure!(file.metadata()?.is_file(), "not a regular file: {}", path.display());
    Ok(file)
}

impl JoinCandidate {
    /// All expensive reads, writes and verification. Takes no writer lock on
    /// the thread, so `publish_join` has to confirm that nothing changed.
    pub(crate) fn stage(self) -> Result<StagedJoin> {
        let threads = self.path.parent().context("missing thread directory")?;
        let private = threads.join(fuzukue_proto::THREAD_SAVE_DIR);
        // Create the private directory and its ignore rule, then release it.
        drop(save_files::private_dir(&private)?);
        let scratch_path = private.join("segmentation");
        durable::create_dir_all(&scratch_path)?;
        let scratch = File::from(open(&scratch_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?);
        // Held until publication, which needs the store. Waiting here while the
        // caller holds the store would deadlock, so a second join fails instead.
        flock(&scratch, FlockOperation::NonBlockingLockExclusive).context("another thread join is running")?;

        let file = Arc::new(open_regular(&self.path)?);
        ensure!(FileVersion::read(file.metadata()?)? == self.version, "thread changed before joining");
        let latest_digest = hash(file.as_ref())?;
        std::io::Seek::rewind(&mut file.as_ref())?;
        let mut input = BufReader::new(file.as_ref());
        let (front, layout, offset) = read_front(&mut input)?;
        let layout = layout.context("thread is not segmented")?;
        ensure!(front.id == self.id && serde_yaml::to_value(&front)? == serde_yaml::to_value(&self.front)?,
            "thread settings differ from the saved file");

        // This applies every layout check before any stored bytes are copied.
        let before = summary(&self.source.read("join")?)?;

        let total = layout.segments.iter().try_fold(self.version.length, |total, info| total.checked_add(info.bytes))
            .context("invalid thread size")?;
        let stat = rustix::fs::statvfs(threads)?;
        let required = total.saturating_mul(2).saturating_add(16 * 1024 * 1024);
        ensure!(stat.f_bavail.saturating_mul(stat.f_frsize) >= required, "insufficient free space to join the thread");

        let mut latest = Vec::new();
        Transcript::read_entries(&mut input, &file, offset, |_, raw| latest.push(raw.to_owned()))?;
        ensure!(latest.len() >= layout.overrides.len(), "thread overrides are missing from the latest file");
        let directory = segments::directory(&self.path, &self.id)?;
        let mut sealed = Vec::new();
        let mut verified_segments = Vec::new();
        for (index, info) in layout.segments.iter().enumerate() {
            verified_segments.push(segments::read_segment(&directory, &self.id, index + 1, info,
                |_, raw| sealed.push(raw.to_owned()))?);
        }
        let mut latest = latest.into_iter();
        for &position in &layout.overrides {
            *sealed.get_mut(position).context("invalid thread override position")? =
                latest.next().context("missing thread override")?;
        }

        // Scratch files are never removed by pattern. After an exchange, this
        // path holds the replaced thread.
        let mut replacement = tempfile::Builder::new().prefix("displaced-").tempfile_in(&scratch_path)?;
        let mut digest = Sha256::new();
        {
            let mut output = BufWriter::new(replacement.as_file_mut());
            let front = format!("---\n{}---\n", serde_yaml::to_string(&self.front)?);
            output.write_all(front.as_bytes())?;
            digest.update(front.as_bytes());
            for raw in sealed.into_iter().chain(latest) {
                for part in [ENTRY, raw.as_str()] {
                    output.write_all(part.as_bytes())?;
                    digest.update(part.as_bytes());
                }
            }
            output.flush()?;
        }
        replacement.as_file().sync_all()?;
        let digest: [u8; 32] = digest.finalize().into();
        ensure!(hash(open_regular(replacement.path())?)? == digest, "joined thread failed read-back verification");

        let mut joined = self.source.clone();
        joined.path = replacement.path().to_owned();
        let after = joined.read("join_verify")?;
        ensure!(!after.items.is_sealed() && summary(&after)? == before,
            "joined thread differs from the segmented thread");

        // Keep the replaced generation beyond the next save's `.previous`.
        let original = scratch_path.join(format!("{:x}.original", sha2::digest::Output::<Sha256>::from(latest_digest)));
        if fs::symlink_metadata(&original).is_err() && fs::hard_link(&self.path, &original).is_err() {
            let mut copy = NamedTempFile::new_in(&scratch_path)?;
            std::io::copy(&mut open_regular(&self.path)?, &mut copy)?;
            copy.as_file().sync_all()?;
            copy.persist_noclobber(&original).map_err(|error| error.error)?;
        }
        let kept = open_regular(&original)?;
        ensure!(hash(&kept)? == latest_digest, "kept thread generation failed verification");
        kept.sync_all()?;
        durable::sync_dir(&scratch_path)?;
        durable::sync_dir(&private)?;

        // Hash the live inode after all other I/O, including the hard link.
        let live = open_regular(&self.path)?;
        let verified = LiveVersion::read(live.metadata()?)?;
        ensure!(verified.file == self.version && hash(&live)? == latest_digest
            && LiveVersion::read(live.metadata()?)? == verified
            && LiveVersion::read(fs::symlink_metadata(&self.path)?)? == verified,
            "thread changed before replacing it");
        Ok(StagedJoin {
            candidate: self, live, verified, replacement, scratch, segments: verified_segments, digest,
            replaced: latest_digest,
        })
    }
}

impl StagedJoin {
    /// Make the exchange durable, confirm the published bytes, and only then
    /// stop requiring a build that reads segmented threads for this one.
    pub(crate) fn finish(&mut self) -> Result<()> {
        let path = &self.candidate.path;
        let threads = path.parent().context("missing thread directory")?;
        let published = (|| -> Result<()> {
            durable::sync_dir(threads)?;
            self.scratch.sync_all()?;
            ensure!(hash(open_regular(path)?)? == self.digest, "published thread differs from the verified file");
            Ok(())
        })();
        if let Err(error) = published {
            self.replacement.disable_cleanup(true);
            return Err(error).context("could not confirm the joined thread");
        }
        let candidate = &self.candidate;
        if let Err(error) = rollback_marker::remove(&candidate.marker_root, &candidate.project, &candidate.id) {
            // The mark stays, which only blocks switching to older builds.
            tracing::warn!("could not clear the segmented thread mark: {error:#}");
        }
        Ok(())
    }
}

impl Store {
    /// Save the idle thread and capture what the publication has to find unchanged.
    pub(crate) fn prepare_join(&mut self, id: &str) -> Result<JoinCandidate> {
        let thread = self.threads.get_mut(id).context("Thread not found")?;
        ensure!(thread.segmented, "thread is not segmented");
        ensure!(!thread.active, "Thread is running");
        thread.save("join_prepare")?;
        ensure!(!thread.transcript.dirty.get(), "thread has unsaved changes");
        let version = thread.file_version.borrow().clone().context("thread has no saved version")?;
        Ok(JoinCandidate {
            id: id.into(), path: thread.path.clone(), version,
            activity_revision: thread.transcript.activity_revision,
            front: thread.front.clone(),
            source: thread.transcript.source.clone().context("thread has no source")?,
            marker_root: self.marker_root.clone(), project: self.project.clone(),
        })
    }

    pub(crate) fn publish_join(&mut self, staged: &mut StagedJoin) -> Result<()> {
        const CHANGED: &str = "thread changed since the join started";
        let candidate = &staged.candidate;
        let thread = self.threads.get_mut(&candidate.id).context(CHANGED)?;
        ensure!(thread.segmented && !thread.active && !thread.transcript.dirty.get()
            && thread.path == candidate.path
            && thread.transcript.activity_revision == candidate.activity_revision
            && thread.file_version.borrow().as_ref() == Some(&candidate.version), CHANGED);
        let path = &candidate.path;
        let private = path.parent().context("missing thread directory")?.join(fuzukue_proto::THREAD_SAVE_DIR);
        let _lock = save_files::try_private_dir(&private)?;
        for segment in &staged.segments {
            segment.verify_unchanged()?;
        }
        let version = FileVersion::read(staged.replacement.as_file().metadata()?)?;
        // The latest file of a segmented thread is small enough to hash again here.
        ensure!(hash(open_regular(path)?)? == staged.replaced
            && LiveVersion::read(staged.live.metadata()?)? == staged.verified
            && LiveVersion::read(fs::symlink_metadata(path)?)? == staged.verified, CHANGED);
        exchange(staged.replacement.path(), path)?;
        let displaced = fs::symlink_metadata(staged.replacement.path()).and_then(FileVersion::read);
        if !displaced.is_ok_and(|old| old == candidate.version) {
            staged.replacement.disable_cleanup(true);
            tracing::warn!(?path, preserved=?staged.replacement.path(), "preserved a concurrent external thread replacement");
        }
        thread.file_version.replace(Some(version));
        thread.segmented = false;
        thread.transcript.loaded.take();
        Ok(())
    }

    /// Join while the caller holds the store, as trashing does.
    pub(crate) fn join_now(&mut self, id: &str) -> Result<()> {
        let mut staged = self.prepare_join(id)?.stage()?;
        self.publish_join(&mut staged)?;
        staged.finish()
    }
}
