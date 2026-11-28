//! Stage, verify and publish one idle thread without transferring its payloads to the Hub.

use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, BufWriter, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::{payloads, Blobs, FileVersion, Front, Refs, Transcript, ENTRY};
use crate::durable;

type Object = BTreeMap<String, Box<RawValue>>;

pub(crate) struct Candidate {
    pub(crate) id: String,
    pub(super) path: PathBuf,
    pub(super) version: FileVersion,
    pub(super) activity_revision: u64,
}

pub(crate) struct Staged {
    pub(crate) candidate: Candidate,
    live: File,
    verified: LiveVersion,
    replacement: NamedTempFile,
    scratch: File,
    pub(super) refs: BTreeMap<String, Refs>,
}

#[derive(PartialEq, Eq)]
pub(super) struct LiveVersion {
    pub(super) file: FileVersion,
    changed: (i64, i64),
}

impl LiveVersion {
    pub(super) fn read(metadata: Metadata) -> std::io::Result<Self> {
        Ok(Self {
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            file: FileVersion::read(metadata)?,
        })
    }
}

impl Candidate {
    /// Copy the exact source and transform one record at a time. All expensive
    /// reads, writes and verification happen before taking either writer lock.
    pub(crate) fn stage(self) -> Result<Staged> {
        let mut input = BufReader::new(File::open(&self.path)?);
        ensure!(FileVersion::read(input.get_ref().metadata()?)? == self.version, "thread changed before conversion");
        let threads = self.path.parent().context("missing thread directory")?;
        let blobs = Blobs::new(threads);
        // Recover space left by an interrupted attempt before measuring free space.
        let scratch = blobs.dir().join("conversion");
        let lock = if scratch.exists() { Some(open_scratch(&scratch)?) } else { None };
        let stat = rustix::fs::statvfs(threads)?;
        // Source copy, replacement, blobs, and allocation overhead for small blobs.
        let block = stat.f_frsize.max(4096);
        let required = self.version.length.saturating_mul(3 + block / 1024).saturating_add(16 * 1024 * 1024);
        ensure!(stat.f_bavail.saturating_mul(stat.f_frsize) >= required, "insufficient free space for thread conversion");
        blobs.prepare()?;
        let lock = match lock {
            Some(lock) => lock,
            None => {
                durable::create_dir_all(&scratch)?;
                open_scratch(&scratch)?
            }
        };
        let mut original = NamedTempFile::new_in(&scratch)?;
        // Scratch cleanup must leave an exchanged file intact after a crash.
        let mut replacement = tempfile::Builder::new().prefix("displaced-").tempfile_in(&scratch)?;
        let mut copy = BufWriter::new(original.as_file_mut());
        let mut output = BufWriter::new(replacement.as_file_mut());
        let mut digest = Sha256::new();

        let mut front = String::new();
        input.read_line(&mut front)?;
        ensure!(matches!(front.as_str(), "---\n" | "---\r\n"), "missing thread front matter");
        let close = loop {
            let start = front.len();
            ensure!(input.read_line(&mut front)? != 0, "unterminated thread front matter");
            if matches!(&front[start..], "---" | "---\n" | "---\r\n") { break start; }
        };
        ensure!(!front.contains('\0'), "invalid thread front matter");
        let (yaml, _) = fuzukue_cards::split_frontmatter(&front).context("missing thread front matter")?;
        let parsed: Front = serde_yaml::from_str(yaml)?;
        ensure!(parsed.id == self.id && !parsed.preview_payloads, "thread has already been converted or changed");
        copy.write_all(front.as_bytes())?;
        digest.update(front.as_bytes());
        // Replace a previously explicit false marker, if present.
        for line in front[..close].split_inclusive('\n') {
            if !line.starts_with("preview_payloads:") {
                output.write_all(line.as_bytes())?;
            }
        }
        let newline = if front.starts_with("---\r\n") { "\r\n" } else { "\n" };
        write!(output, "preview_payloads: true{newline}{}", &front[close..])?;

        let mut line = String::new();
        let mut entry = String::new();
        let mut refs = BTreeMap::new();
        while input.read_line(&mut line)? != 0 {
            ensure!(!line.contains('\0'), "NUL bytes in transcript");
            copy.write_all(line.as_bytes())?;
            digest.update(line.as_bytes());
            if line.starts_with(ENTRY) && entry.ends_with("\n\n") {
                if let Some((item, payloads)) = convert_entry(&entry, &blobs, &mut output)? {
                    refs.insert(item, payloads);
                }
                entry.clear();
            }
            entry.push_str(&line);
            line.clear();
        }
        if !entry.is_empty() {
            if let Some((item, payloads)) = convert_entry(&entry, &blobs, &mut output)? {
                refs.insert(item, payloads);
            }
        }
        copy.flush()?;
        output.flush()?;
        drop((copy, output));
        original.as_file().sync_all()?;
        replacement.as_file().sync_all()?;
        ensure!(FileVersion::read(input.get_ref().metadata()?)? == self.version, "thread changed during conversion");

        let digest: [u8; 32] = digest.finalize().into();
        ensure!(hash(original.reopen()?)? == digest[..], "original thread backup failed verification");
        let original_path = blobs.dir().join(format!("{:x}.original", sha2::digest::Output::<Sha256>::from(digest)));
        if original_path.exists() {
            ensure!(hash(File::open(&original_path)?)? == digest[..], "existing original thread backup failed verification");
        } else {
            original.persist_noclobber(&original_path).map_err(|error| error.error)?;
        }
        // Includes the backup and all sidecar names. Each new file was
        // synced before linking it, and publication may now reference them.
        durable::sync_dir(blobs.dir())?;

        // Hash the live inode itself, after all staging and backup I/O. Check
        // ctime as well as mtime so a same-size write cannot restore the stamp
        // by setting mtime back. Keep the descriptor until after the exchange.
        let live = File::from(open(&self.path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK, Mode::empty())?);
        let verified = LiveVersion::read(live.metadata()?)?;
        ensure!(verified.file == self.version
            && hash(&live)? == digest
            && LiveVersion::read(live.metadata()?)? == verified
            && LiveVersion::read(fs::symlink_metadata(&self.path)?)? == verified,
            "thread changed before replacing it");
        Ok(Staged { candidate: self, live, verified, replacement, scratch: lock, refs })
    }
}

impl Staged {
    pub(super) fn exchange(&mut self) -> Result<FileVersion> {
        self.exchange_locked(super::save_files::try_private_dir)
    }

    fn exchange_locked(&mut self, lock: impl FnOnce(&Path) -> Result<File>) -> Result<FileVersion> {
        let path = &self.candidate.path;
        let threads = path.parent().context("missing thread directory")?;
        let private = threads.join(fuzukue_proto::THREAD_SAVE_DIR);
        let _lock = lock(&private)?;
        let version = FileVersion::read(self.replacement.as_file().metadata()?)?;
        ensure!(LiveVersion::read(self.live.metadata()?)? == self.verified
            && LiveVersion::read(fs::symlink_metadata(path)?)? == self.verified,
            "thread changed before replacing it");

        // The exchange retains the inode displaced in the interval between
        // the last check and rename, even for writers ignoring our lock.
        exchange(self.replacement.path(), path)?;
        let displaced = fs::symlink_metadata(self.replacement.path())
            .and_then(FileVersion::read);
        if !displaced.is_ok_and(|old| old == self.candidate.version) {
            self.replacement.disable_cleanup(true);
            tracing::warn!(?path, preserved=?self.replacement.path(), "preserved a concurrent external thread replacement");
        }
        Ok(version)
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        let path = &self.candidate.path;
        let threads = path.parent().expect("thread directory");
        if let Err(error) = durable::sync_dir(threads).and_then(|()| self.scratch.sync_all()) {
            self.replacement.disable_cleanup(true);
            return Err(error).context("could not sync converted thread directory after exchange");
        }
        Ok(())
    }

    #[cfg(test)]
    fn publish(self) -> Result<FileVersion> {
        self.publish_locked(super::save_files::try_private_dir)
    }

    #[cfg(test)]
    fn publish_locked(mut self, lock: impl FnOnce(&Path) -> Result<File>) -> Result<FileVersion> {
        let version = self.exchange_locked(lock)?;
        self.finish()?;
        Ok(version)
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub(super) fn exchange(staged: &Path, live: &Path) -> Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    #[cfg(target_os = "linux")]
    let flag = RenameFlags::EXCHANGE;
    #[cfg(target_vendor = "apple")]
    let flag = RenameFlags::EXCHANGE;
    renameat_with(CWD, staged, CWD, live, flag)?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub(super) fn exchange(_staged: &Path, _live: &Path) -> Result<()> {
    anyhow::bail!("atomic thread exchange is unavailable on this platform")
}

fn open_scratch(path: &Path) -> Result<File> {
    let blobs = path.parent().context("missing blob directory")?;
    let private = blobs.parent().context("missing private directory")?;
    ensure!(fs::symlink_metadata(blobs)?.is_dir() && fs::symlink_metadata(private)?.is_dir(),
        "invalid conversion directory");
    let lock = File::from(open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?);
    flock(&lock, FlockOperation::LockExclusive)?;
    // This private subdirectory contains only scratch files from conversion.
    // Other writers place immutable payloads and originals one level above.
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().starts_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(lock)
}

pub(super) fn hash(mut file: impl std::io::Read) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(digest.finalize().into())
}

fn convert_entry(entry: &str, blobs: &Blobs, output: &mut impl Write) -> Result<Option<(String, Refs)>> {
    let part = entry.strip_prefix(ENTRY).context("missing transcript entry")?
        .strip_suffix("\n\n").context("missing transcript separator")?;
    let decoded = Transcript::decode_entry(part)?;
    let Some(tool) = decoded.meta.tool else {
        output.write_all(entry.as_bytes())?;
        return Ok(None);
    };
    let (header, body) = entry[ENTRY.len()..].split_once(" -->\n").context("broken transcript header")?;
    let mut refs: Refs = tool.payloads;
    let mut meta: Object = serde_json::from_str(header)?;
    let mut fields: Object = serde_json::from_str(meta["tool"].get())?;
    if let Some(digest) = &refs.input {
        // Old writers drop ordinary previews. Rebuild them only from the
        // original bytes identified by the same entry's surviving marker.
        if tool.input.is_none() {
            if let Ok(input) = blobs.input(digest) {
                let encoded = serde_json::to_vec(&input)?;
                fields.insert("input".into(), serde_json::value::to_raw_value(
                    &payloads::input_preview(&input, &encoded, tool.name == "delegate"))?);
            }
        }
    } else if let Some(input) = tool.input {
        let raw = fields["input"].get().as_bytes();
        if payloads::oversized_input(&input).is_some() || raw.len() > 4096 {
            refs.input = Some(blobs.stage(raw)?);
            fields.insert("input".into(), serde_json::value::to_raw_value(&payloads::input_preview(&input, raw, tool.name == "delegate"))?);
        }
    }
    if let Some(digest) = &refs.output {
        if tool.output.is_none() {
            if let Ok(result) = blobs.output(digest) {
                fields.insert("output".into(), serde_json::value::to_raw_value(
                    &payloads::output_preview(&result, tool.name == "delegate"))?);
            }
        }
    } else if let Some(result) = tool.output {
        if payloads::needs_output_preview(&result) {
            refs.output = Some(blobs.stage(result.as_bytes())?);
            fields.insert("output".into(), serde_json::value::to_raw_value(&payloads::output_preview(&result, tool.name == "delegate"))?);
        }
    }
    if !refs.is_empty() {
        let mut attachments = decoded.meta.attachments;
        refs.write_marker(&mut attachments);
        meta.insert("attachments".into(), serde_json::value::to_raw_value(&attachments)?);
        fields.insert("payloads".into(), serde_json::value::to_raw_value(&refs)?);
        meta.insert("tool".into(), serde_json::value::to_raw_value(&fields)?);
        let preview = serde_json::to_string(&meta)?.replace('>', "\\u003e");
        write!(output, "{ENTRY}{preview} -->\n{body}")?;
    } else {
        output.write_all(entry.as_bytes())?;
    }
    Ok((!refs.is_empty()).then_some((decoded.meta.item, refs)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::{Actor, ServerEvent};
    use std::{sync::Arc, time::{Duration, Instant}};

    const SESSION: &str = "thread-test";
    const IDLE: Duration = Duration::from_secs(120);

    fn fixture(path: &Path) -> Result<(String, String)> {
        let input = format!(r#"{{"z":"{}", "a":"\u3042"}}"#, "synthetic".repeat(520));
        let output = format!("{}\nlast line\0", "試験".repeat(600));
        let result = serde_json::to_string(&output)?;
        let markdown = format!(
            "---\nid: thread-test\ntitle: Test\ncreated: 1\nupdated: 1\n---\n\
             {ENTRY}{{\"item\":\"user\",\"actor\":{{\"kind\":\"user\"}}}} -->\n## User\n\nConversation  \n\n\
             {ENTRY}{{\"item\":\"tool\",\"actor\":{{\"kind\":\"claude\"}},\"tool\":{{\"name\":\"Read\",\"ok\":true,\
             \"input\":{input},\"output\":{result}}}}} -->\n- Claude called Read\n\n"
        );
        fs::write(path, markdown)?;
        Ok((input, output))
    }

    fn candidate(path: &Path) -> Result<Candidate> {
        Ok(Candidate { id: "thread-test".into(), path: path.into(),
            version: FileVersion::read(path.metadata()?)?, activity_revision: 0 })
    }

    fn load_fixture(project: &Path, state: &Path) -> Result<(Arc<crate::hub::Hub>, PathBuf)> {
        let threads = project.join("threads");
        fs::create_dir(&threads)?;
        let path = threads.join("test.md");
        fixture(&path)?;
        let hub = Arc::new(crate::hub::Hub::with_timeline(state));
        hub.load_threads(project, state);
        Ok((hub, path))
    }

    fn pass_idle_window(hub: &crate::hub::Hub) -> Result<()> {
        hub.edit_threads(|store| {
            store.threads.get_mut(SESSION).unwrap().transcript.last_activity = Instant::now() - IDLE;
            Ok(())
        })
    }

    fn is_converted(path: &Path) -> Result<bool> {
        Ok(fs::read_to_string(path)?.contains("preview_payloads: true"))
    }

    #[tokio::test]
    async fn maintenance_and_history_reads_do_not_reset_write_idleness() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let (hub, path) = load_fixture(project.path(), state.path())?;
        pass_idle_window(&hub)?;
        for _ in 0..3 {
            hub.trim_idle_transcripts(IDLE);
            hub.flush_dirty_transcripts(true)?;
            hub.recover_thread_metadata(None);
            assert!(hub.library_thread(SESSION, |_| true).is_some());
            assert_eq!(hub.history_cursor(SESSION), 2);
            assert!(hub.read_threads(|store| store.history_page(SESSION, 2).is_some()));
        }
        assert!(hub.read_threads(|store| store.threads[SESSION].transcript.accessed.get().elapsed() < IDLE));
        hub.migrate_idle_payloads(IDLE).await;
        assert!(is_converted(&path)?);
        Ok(())
    }

    #[tokio::test]
    async fn late_events_and_failed_flushes_do_not_pin_dirty_transcript() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let (hub, path) = load_fixture(project.path(), state.path())?;
        let session = SESSION.to_owned();
        hub.publish(ServerEvent::TurnEnded { session: session.clone(), actor: Actor::Claude });
        hub.publish(ServerEvent::UserPrompt {
            session: session.clone(), item: "held".into(), text: "queued prompt".into(),
            paths: Vec::new(), to_worker: false,
        });
        hub.publish(ServerEvent::TextDelta {
            session: session.clone(), item: "late".into(), actor: Actor::Claude, text: "late reply".into(),
        });
        hub.publish(ServerEvent::ToolCall {
            session, item: "late-tool".into(), actor: Actor::Claude, name: "Read".into(),
            input: serde_json::json!({}),
        });
        assert!(hub.thread_active(SESSION));
        assert!(!hub.thread_running(SESSION));
        hub.migrate_idle_payloads(IDLE).await;
        assert!(!is_converted(&path)?);

        pass_idle_window(&hub)?;
        hub.edit_threads(|_| {
            assert_eq!(hub.flush_dirty_transcripts(true).unwrap_err().to_string(), "transcript store is busy");
            Ok(())
        })?;
        let private = path.parent().unwrap().join(fuzukue_proto::THREAD_SAVE_DIR);
        let save_lock = super::super::save_files::private_dir(&private)?;
        assert!(hub.flush_dirty_transcripts(true).is_err());
        assert!(hub.read_threads(|store| store.threads[SESSION].transcript.dirty.get()));
        drop(save_lock);
        hub.migrate_idle_payloads(IDLE).await;
        assert!(is_converted(&path)?);
        hub.flush_dirty_transcripts(true)?;
        let saved = fs::read_to_string(&path)?;
        assert!(saved.contains("queued prompt") && saved.contains("late reply") && saved.contains("late-tool"));
        Ok(())
    }

    #[tokio::test]
    async fn live_turns_and_workers_block_but_orphaned_worker_events_do_not() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let (hub, path) = load_fixture(project.path(), state.path())?;
        let session = SESSION.to_owned();
        hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });
        pass_idle_window(&hub)?;
        hub.migrate_idle_payloads(IDLE).await;
        assert!(!is_converted(&path)?);
        hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: false });

        let worker = hub.hold_payload_conversion(SESSION);
        let job = format!("{}interrupted", fuzukue_proto::DELEGATE_JOB);
        hub.publish(ServerEvent::ToolCall {
            session, item: job.clone(), name: "delegate".into(), actor: Actor::System,
            input: serde_json::json!({ "started_at_ms": 1 }),
        });
        pass_idle_window(&hub)?;
        hub.migrate_idle_payloads(IDLE).await;
        assert!(!is_converted(&path)?);
        drop(worker);
        assert!(hub.drain_blockers("synthetic").iter().any(|blocker| blocker.id == job));
        pass_idle_window(&hub)?;
        hub.migrate_idle_payloads(IDLE).await;
        assert!(is_converted(&path)?);
        Ok(())
    }

    #[test]
    fn a_turn_during_staging_cancels_publication_and_the_next_stage_reuses_sidecars() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let (hub, path) = load_fixture(project.path(), state.path())?;
        let stage = || {
            hub.read_threads(|store| store.payload_candidate(Duration::ZERO, |_| None).0)
                .unwrap().stage()
        };
        let mut staged = stage()?;
        let sidecar = Blobs::new(path.parent().unwrap()).dir().join(
            staged.refs["tool"].output.as_ref().unwrap());
        let original = fs::read(&path)?;
        let inode = sidecar.metadata()?.ino();
        for running in [true, false] {
            hub.publish(ServerEvent::TurnActivity { session: SESSION.into(), running });
        }
        assert!(!hub.edit_threads(|store| store.publish_payloads(&mut staged))?);
        assert_eq!(fs::read(&path)?, original);
        drop(staged);

        hub.publish(ServerEvent::TextDelta {
            session: SESSION.into(), item: "next".into(), actor: Actor::Claude, text: "new version".into(),
        });
        hub.publish(ServerEvent::TurnEnded { session: SESSION.into(), actor: Actor::Claude });
        assert_ne!(fs::read(&path)?, original);
        let mut retry = stage()?;
        assert!(hub.edit_threads(|store| store.publish_payloads(&mut retry))?);
        retry.finish()?;
        assert!(is_converted(&path)?);
        assert_eq!(sidecar.metadata()?.ino(), inode);
        assert!(fs::read_to_string(&path)?.contains("new version"));
        Ok(())
    }

    #[test]
    fn conversion_preserves_the_original_and_exact_payload_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.md");
        let (input, output) = fixture(&path)?;
        let original = fs::read(&path)?;
        let staging = candidate(&path)?.stage()?;
        assert_eq!(fs::read(&path)?, original);
        let backup = Blobs::new(dir.path()).dir().join(format!("{:x}.original", Sha256::digest(&original)));
        staging.publish_locked(super::super::save_files::private_dir)?;
        assert_eq!(fs::read(backup)?, original);

        let saved = fs::read_to_string(&path)?;
        assert!(saved.contains("Conversation  \n\n"));
        assert!(saved.contains("last line"));
        let mut thread = super::super::Thread::read(path.clone())?;
        let blobs = thread.blobs();
        let tool = thread.transcript.items[1].meta.tool.as_ref().unwrap();
        blobs.verify(tool.payloads.input.as_deref().unwrap(), input.as_bytes())?;
        assert_eq!(tool.full_output(&blobs)?.as_deref(), Some(output.as_str()));
        assert_eq!(tool.full_input(&blobs)?, Some(serde_json::from_str(&input)?));
        assert!(candidate(&path)?.stage().is_err());
        thread.save("test")?;
        let tool = thread.transcript.items[1].meta.tool.as_ref().unwrap();
        assert_eq!(tool.full_output(&blobs)?.as_deref(), Some(output.as_str()));
        Ok(())
    }

    #[tokio::test]
    async fn dirty_checkpoint_keeps_newer_journal_payload_out_of_converted_markdown() -> Result<()> {
        use fuzukue_proto::ServerEvent;

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let threads = project.path().join("threads");
        fs::create_dir(&threads)?;
        let path = threads.join("test.md");
        fixture(&path)?;
        let hub = std::sync::Arc::new(crate::hub::Hub::with_timeline(state.path()));
        hub.load_threads(project.path(), state.path());

        let blobs = Blobs::new(&threads);
        super::super::save_files::private_dir(blobs.dir().parent().unwrap())?;
        fs::write(blobs.dir(), "temporarily obstruct sidecar writes")?;
        let session = "thread-test".to_owned();
        let item = "tool".to_owned();
        let output = format!("head {} tail", "new journal payload ".repeat(500));
        hub.publish(ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: output.clone(),
        });
        fs::remove_file(blobs.dir())?;

        hub.migrate_idle_payloads(std::time::Duration::ZERO).await;
        assert!(fs::read_to_string(&path)?.contains("preview_payloads: true"));
        hub.flush_dirty_transcripts(false)?;
        let saved = fs::read_to_string(&path)?;
        assert!(!saved.contains(&output));
        drop(hub);

        let no_journal = tempfile::tempdir()?;
        let reopened = super::super::Store::load(project.path(), no_journal.path(), &BTreeMap::new());
        assert_eq!(reopened.tool_detail(&session, &item).unwrap().output()?, Some(output));
        Ok(())
    }

    #[test]
    fn checkpoint_keeps_sidecars_after_replaying_an_old_journal() -> Result<()> {
        use fuzukue_proto::{Actor, Envelope, ServerEvent};

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let threads = project.path().join("threads");
        fs::create_dir(&threads)?;
        let path = threads.join("test.md");
        let (input, output) = fixture(&path)?;
        let expected_input: serde_json::Value = serde_json::from_str(&input)?;
        let staged = candidate(&path)?.stage()?;
        staged.publish()?;

        let session = "thread-test".to_owned();
        let item = "tool".to_owned();
        let mut journal = crate::state::TimelineWriter::open(state.path())?;
        journal.append(&Envelope { workspace: None, seq: 1, event: ServerEvent::ToolCall {
            session: session.clone(), item: item.clone(), name: "Read".into(),
            actor: Actor::Claude, input: expected_input.clone(),
        } });
        journal.append(&Envelope { workspace: None, seq: 2, event: ServerEvent::ToolResult {
            session: session.clone(), item: item.clone(), ok: true, output: output.clone(),
        } });
        drop(journal);

        let mut saved = super::super::Store::load(project.path(), state.path(), &BTreeMap::new());
        let tool = saved.tool_detail(&session, &item).unwrap();
        assert_eq!(tool.input()?, Some(expected_input.clone()));
        assert_eq!(tool.output()?.as_deref(), Some(output.as_str()));
        saved.threads.get_mut(&session).unwrap().save("test")?;
        assert!(fs::read_to_string(&path)?.contains("fuzukue:tool-payload:v1:"));
        drop(saved);

        let no_journal = tempfile::tempdir()?;
        let reopened = super::super::Store::load(project.path(), no_journal.path(), &BTreeMap::new());
        let tool = reopened.tool_detail(&session, &item).unwrap();
        assert_eq!(tool.input()?, Some(expected_input));
        assert_eq!(tool.output()?.unwrap().as_bytes(), output.as_bytes());
        let thread = &reopened.threads[&session];
        let tool = thread.transcript.items[1].meta.tool.as_ref().unwrap();
        thread.blobs().verify(tool.payloads.input.as_deref().unwrap(), input.as_bytes())?;
        Ok(())
    }

    #[test]
    fn preserved_marker_never_treats_a_preview_as_the_original() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.md");
        fixture(&path)?;
        candidate(&path)?.stage()?.publish()?;
        let saved = fs::read_to_string(&path)?;

        // Only the old-writer-compatible attachment marker survives here.
        let copy = tempfile::tempdir()?;
        let copied = copy.path().join("test.md");
        let saved = saved.replace("preview_payloads: true\n", "");
        let mut restored = String::new();
        for line in saved.lines() {
            if line.starts_with(ENTRY) && line.contains("\"payloads\"") {
                let mut meta: serde_json::Value = serde_json::from_str(
                    line.strip_prefix(ENTRY).unwrap().strip_suffix(" -->").unwrap())?;
                meta["tool"].as_object_mut().unwrap().remove("payloads");
                restored.push_str(&format!("{ENTRY}{} -->\n", meta.to_string().replace('>', "\\u003e")));
            } else {
                restored.push_str(line);
                restored.push('\n');
            }
        }
        fs::write(&copied, restored)?;
        candidate(&copied)?.stage()?.publish()?;
        let thread = super::super::Thread::read(copied)?;
        let tool = thread.transcript.items[1].meta.tool.as_ref().unwrap();
        assert!(tool.input.is_some() && tool.output.is_some());
        assert!(tool.full_input(&thread.blobs()).is_err());
        assert!(tool.full_output(&thread.blobs()).is_err());
        assert_eq!(fs::read_dir(thread.blobs().dir())?.count(), 2); // conversion directory and original
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    #[ignore = "writes a 320 MB synthetic thread and 33,000 sidecars"]
    fn measure_large_thread_publication_and_cache() -> Result<()> {
        use std::time::{Duration, Instant};

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let threads = project.path().join("threads");
        fs::create_dir(&threads)?;
        let path = threads.join("test.md");
        let session = "thread-test";
        let mut writer = BufWriter::new(File::create(&path)?);
        writeln!(writer, "---\nid: {session}\ntitle: Test\ncreated: 1\nupdated: 1\n---")?;
        let body = "x".repeat(9550);
        for id in 0..33_000 {
            writeln!(writer, r#"{ENTRY}{{"item":"tool-{id}","actor":{{"kind":"claude"}},"tool":{{"name":"Read","ok":true,"output":"head {id} {body} tail {id}"}}}} -->
- Claude called Read
"#)?;
        }
        writer.flush()?;
        drop(writer);
        let original_size = path.metadata()?.len();

        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        let candidate = hub.read_threads(|store|
            store.payload_candidate(Duration::ZERO, |_| None).0).unwrap();
        let stage_start = Instant::now();
        let mut staged = candidate.stage()?;
        let stage_time = stage_start.elapsed();

        let hash_start = Instant::now();
        hash(File::open(&path)?)?;
        let old_locked_hash = hash_start.elapsed();

        let hub_hold = hub.edit_threads(|store| {
            let started = Instant::now();
            assert!(store.publish_payloads(&mut staged)?);
            Ok(started.elapsed())
        })?;
        let sync_start = Instant::now();
        staged.finish()?;
        let sync_time = sync_start.elapsed();
        drop(staged);
        let converted_size = path.metadata()?.len();

        let mut loads = 0;
        let mut first_access = Duration::ZERO;
        let mut warm_total = Duration::ZERO;
        for index in 0..20 {
            loads += usize::from(hub.try_cache_size().unwrap().1.loaded_transcripts == 0);
            let started = Instant::now();
            assert!(hub.tool_detail(session, "tool-32999").is_some());
            if index == 0 {
                first_access = started.elapsed();
            } else {
                warm_total += started.elapsed();
            }
        }
        let cache = hub.try_cache_size().unwrap().1;
        assert_eq!(cache.transcript_entries, 33_000);
        assert_eq!(loads, 1);
        eprintln!(
            "source={original_size} B converted={converted_size} B cache={} B loads={loads}/20 cold={first_access:?} warm_mean={:?} stage={stage_time:?} old_locked_hash_component={old_locked_hash:?} hub_hold_including_save_lock={hub_hold:?} deferred_sync={sync_time:?}",
            cache.transcript_source_bytes, warm_total / 19,
        );
        Ok(())
    }

    #[test]
    fn preflight_rejects_a_source_larger_than_the_available_space() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.md");
        let stat = rustix::fs::statvfs(dir.path())?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        File::create(&path)?.set_len(free / 3 + 1)?;
        assert!(candidate(&path)?.stage().err().unwrap().to_string().contains("insufficient free space"));
        assert!(!dir.path().join(fuzukue_proto::THREAD_SAVE_DIR).exists());
        Ok(())
    }

    #[test]
    fn verification_failure_and_a_competing_save_leave_the_live_file_intact() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.md");
        let (input, _) = fixture(&path)?;
        let original = fs::read(&path)?;
        let blobs = Blobs::new(dir.path());
        blobs.prepare()?;
        let scratch = blobs.dir().join("conversion");
        fs::create_dir(&scratch)?;
        let interrupted = scratch.join(".tmp-interrupted");
        fs::write(&interrupted, b"old staging file")?;
        let digest = format!("{:x}", Sha256::digest(input.as_bytes()));
        let corrupt = blobs.dir().join(&digest);
        fs::write(&corrupt, b"wrong bytes")?;
        assert!(candidate(&path)?.stage().is_err());
        assert_eq!(fs::read(&path)?, original);
        assert_eq!(fs::read(&corrupt)?, b"wrong bytes");
        assert!(!interrupted.exists());
        fs::remove_file(corrupt)?;

        let staged = candidate(&path)?.stage()?;
        let changed = original.repeat(2);
        fs::write(&path, &changed)?;
        let error = staged.publish_locked(super::super::save_files::private_dir).err().unwrap();
        assert!(error.to_string().contains("thread changed before replacing it"));
        assert_eq!(fs::read(&path)?, changed);
        fs::write(&path, &original)?;
        let staged = candidate(&path)?.stage()?;
        let modified = path.metadata()?.modified()?;
        let mut same_size = original.clone();
        let i = same_size.windows(b"Conversation".len()).position(|bytes| bytes == b"Conversation").unwrap();
        same_size[i] = b'c';
        fs::write(&path, &same_size)?;
        File::options().write(true).open(&path)?.set_times(std::fs::FileTimes::new().set_modified(modified))?;
        assert!(staged.publish().unwrap_err().to_string().contains("thread changed before replacing it"));
        assert_eq!(fs::read(&path)?, same_size);
        fs::write(&path, &original)?;
        candidate(&path)?.stage()?.publish_locked(super::super::save_files::private_dir)?;
        Ok(())
    }
}
