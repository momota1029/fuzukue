//! A disposable byte-offset index over the append-only workspace journals.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fuzukue_proto::{Envelope, ServerEvent};
use serde::{Deserialize, Serialize};

pub(crate) const BACKLOG: usize = 10_000;
pub(crate) const BACKLOG_BYTES: u64 = 16 * 1024 * 1024;
const CHECKPOINT_INTERVAL: u64 = 16 * 1024 * 1024;
const SAMPLE_BYTES: usize = 4096;
const INDEX_VERSION: u32 = 2;

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Record(u64, u64);

impl Record {
    fn len(self) -> u64 {
        self.1 - self.0
    }
}

#[derive(Deserialize)]
struct Header {
    seq: u64,
    event: EventHeader,
}

#[derive(Deserialize)]
struct EventHeader {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl EventHeader {
    fn history_session(&self) -> Option<&str> {
        match self.kind.as_str() {
            "local_notice" | "text_delta" | "reply_superseded" | "thread_restored"
            | "github_activity" | "user_prompt" | "comment_application" | "turn_activity"
            | "approval_requested"
            | "delegate_progress" | "delegate_renamed" | "delegate_dismissed"
            | "tool_call" | "tool_result" | "message_details" | "turn_ended" => self.session.as_deref(),
            _ => None,
        }
    }

    fn recovery(&self) -> bool {
        match self.kind.as_str() {
            "approval_requested" | "approval_resolved" | "question_asked"
            | "question_resolved" | "delegate_renamed" => true,
            "tool_call" => self.name.as_deref() == Some("delegate")
                && self.item.as_deref().is_some_and(is_worker),
            "tool_result" => self.item.as_deref().is_some_and(is_worker),
            _ => false,
        }
    }
}

fn is_worker(item: &str) -> bool {
    item.strip_prefix(fuzukue_proto::DELEGATE_JOB).is_some_and(|id| !id.contains(':'))
}

#[derive(Default, Deserialize, Serialize)]
struct Index {
    epoch: u64,
    end: u64,
    tail: VecDeque<Record>,
    tail_bytes: u64,
    recovery: Vec<Record>,
    threads: BTreeMap<String, Vec<Record>>,
    legacy_sequence: u64,
    snapshot_recovery: BTreeSet<String>,
}

impl Index {
    fn fresh() -> Self {
        Self { epoch: rand::random(), ..Self::default() }
    }

    fn record(&mut self, end: u64, header: Option<&Header>) {
        let record = Record(self.end, end);
        self.end = end;
        let Some(header) = header else { return };
        if let Some(session) = header.event.history_session() {
            self.history(session, record, header.seq);
            if header.event.kind == "comment_application"
                || header.event.kind == "tool_call" && header.event.name.as_deref().is_some_and(crate::thread_artifacts::may_record_path)
            {
                self.snapshot_recovery.insert(session.into());
            }
        }
        if header.event.recovery() {
            self.recovery.push(record);
        }
        self.push_tail(record);
    }

    fn history(&mut self, session: &str, record: Record, seq: u64) {
        self.threads.entry(session.into()).or_default().push(record);
        if crate::threads::is_legacy_session(session) {
            self.legacy_sequence = self.legacy_sequence.max(seq);
        }
    }

    fn push_tail(&mut self, record: Record) {
        self.tail.push_back(record);
        self.tail_bytes += record.len();
        while self.tail.len() > BACKLOG
            || self.tail.len() > 1 && self.tail_bytes > BACKLOG_BYTES
        {
            self.tail_bytes -= self.tail.pop_front().unwrap().len();
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    version: u32,
    sample: [u8; 32],
    modified: Option<std::time::SystemTime>,
    index: Index,
}

struct Journal {
    path: PathBuf,
    index: Index,
    saved_end: u64,
}

impl Journal {
    fn open(path: PathBuf) -> io::Result<Self> {
        let mut journal = Self { path, index: Index::fresh(), saved_end: 0 };
        let mut file = match File::open(&journal.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(journal),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if let Ok(bytes) = std::fs::read(journal.checkpoint_path()) {
            if let Ok(saved) = serde_json::from_slice::<Checkpoint>(&bytes) {
                let end = saved.index.end;
                if saved.version == INDEX_VERSION && end <= metadata.len()
                    && (end != metadata.len() || metadata.modified().ok() == saved.modified)
                    && Self::sample(&mut file, end)? == saved.sample
                {
                    journal.saved_end = end;
                    journal.index = saved.index;
                }
            }
        }
        file.seek(SeekFrom::Start(journal.index.end))?;
        let mut file = BufReader::with_capacity(128 * 1024, file);
        let mut line = Vec::new();
        let mut terminated = true;
        loop {
            line.clear();
            if file.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            terminated = line.ends_with(b"\n");
            let end = journal.index.end + line.len() as u64;
            let header = serde_json::from_slice::<Header>(&line).ok();
            journal.index.record(end, header.as_ref());
        }
        // An older writer can leave an unterminated line. Re-read it after any
        // future append, since its bytes are not a stable prefix.
        if terminated && journal.saved_end != journal.index.end {
            journal.save_checkpoint();
        }
        Ok(journal)
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.path.with_extension("jsonl-index")
    }

    fn sample(file: &mut File, end: u64) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let sample_len = end.min(SAMPLE_BYTES as u64) as usize;
        let mut bytes = [0; SAMPLE_BYTES];
        for offset in [0, end - sample_len as u64] {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut bytes[..sample_len])?;
            hash.update(&bytes[..sample_len]);
        }
        Ok(hash.finalize().into())
    }

    fn save_checkpoint(&mut self) {
        if let Err(error) = self.write_checkpoint() {
            tracing::warn!(path=?self.path, %error, "could not cache timeline offsets");
        }
        // Avoid retrying on every event when the derived cache cannot be written.
        self.saved_end = self.index.end;
    }

    fn write_checkpoint(&self) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct BorrowedCheckpoint<'a> {
            version: u32,
            sample: [u8; 32],
            modified: Option<std::time::SystemTime>,
            index: &'a Index,
        }
        let mut file = File::open(&self.path)?;
        let sample = Self::sample(&mut file, self.index.end)?;
        let modified = file.metadata()?.modified().ok();
        crate::durable::write(&self.checkpoint_path(), |output| {
            serde_json::to_writer(output, &BorrowedCheckpoint {
                version: INDEX_VERSION, sample, modified, index: &self.index,
            })?;
            Ok(())
        })
    }

    fn visit(&self, records: impl IntoIterator<Item = Record>, mut visit: impl FnMut(Envelope, u64)) {
        let Ok(file) = File::open(&self.path) else { return };
        let mut input = BufReader::with_capacity(128 * 1024, file);
        let mut line = Vec::new();
        let mut offset = 0;
        for record in records {
            if record.0 > record.1 || record.1 > self.index.end {
                tracing::warn!(path=?self.path, "invalid timeline record offsets");
                break;
            }
            let result = (|| -> io::Result<()> {
                if let Ok(delta) = i64::try_from(record.0 as i128 - offset as i128) {
                    input.seek_relative(delta)?;
                } else {
                    input.seek(SeekFrom::Start(record.0))?;
                }
                let len = usize::try_from(record.len()).map_err(io::Error::other)?;
                line.resize(len, 0);
                input.read_exact(&mut line)?;
                offset = record.1;
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(path=?self.path, %error, "could not read indexed timeline record");
                break;
            }
            if let Ok(envelope) = serde_json::from_slice(&line) {
                visit(envelope, record.len());
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct Timeline(Arc<Mutex<[Journal; 2]>>);

impl Timeline {
    pub(crate) fn open(dir: &Path) -> Self {
        Self(Arc::new(Mutex::new([super::LEGACY_TIMELINE, super::TIMELINE].map(|name| {
            let path = dir.join(name);
            Journal::open(path.clone()).unwrap_or_else(|error| {
                tracing::warn!(%error, ?path, "could not index timeline");
                Journal { path, index: Index::fresh(), saved_end: 0 }
            })
        }))))
    }

    pub(crate) fn restore(&self, mut visit: impl FnMut(Envelope, Option<u64>)) {
        for journal in self.0.lock().unwrap().iter() {
            let tail_start = journal.index.tail.front().map_or(u64::MAX, |record| record.0);
            journal.visit(journal.index.recovery.iter().copied().take_while(|record| record.0 < tail_start),
                |env, _| visit(env, None));
            journal.visit(journal.index.tail.iter().copied(), |env, len| visit(env, Some(len)));
        }
    }

    pub(crate) fn sessions(&self) -> Vec<String> {
        let journals = self.0.lock().unwrap();
        let mut sessions: Vec<_> = journals.iter().flat_map(|journal| journal.index.threads.keys().cloned()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        sessions
    }

    pub(crate) fn session_stamp(&self, session: &str) -> [Option<(u64, u64)>; 2] {
        let journals = self.0.lock().unwrap();
        std::array::from_fn(|index| {
            let journal = &journals[index];
            let record = journal.index.threads.get(session)?.last()?;
            Some((journal.index.epoch, record.1))
        })
    }

    pub(crate) fn latest_legacy_sequence(&self) -> u64 {
        self.0.lock().unwrap().iter().map(|journal| journal.index.legacy_sequence).max().unwrap_or(0)
    }

    pub(crate) fn snapshot_recovery_sessions(&self) -> BTreeSet<String> {
        self.0.lock().unwrap().iter()
            .flat_map(|journal| journal.index.snapshot_recovery.iter().cloned()).collect()
    }

    pub(crate) fn visit_session(&self, session: &str, mut visit: impl FnMut(Envelope, u64)) {
        for journal in self.0.lock().unwrap().iter() {
            if let Some(records) = journal.index.threads.get(session) {
                journal.visit(records.iter().copied(), &mut visit);
            }
        }
    }

    pub(crate) fn record_append(&self, start: u64, end: u64, env: &Envelope) {
        let mut journals = self.0.lock().unwrap();
        let journal = &mut journals[1];
        if journal.index.end != start {
            // A partial write or an older process appended past our indexed
            // boundary. Rebuild the affected suffix before publishing offsets.
            if let Ok(reopened) = Journal::open(journal.path.clone()) {
                *journal = reopened;
            }
            return;
        }
        let history = crate::threads::history_session(&env.event);
        let header = match &env.event {
            ServerEvent::ApprovalRequested { .. } => Some(("approval_requested", None, None)),
            ServerEvent::ApprovalResolved { .. } => Some(("approval_resolved", None, None)),
            ServerEvent::QuestionAsked { .. } => Some(("question_asked", None, None)),
            ServerEvent::QuestionResolved { .. } => Some(("question_resolved", None, None)),
            ServerEvent::DelegateRenamed { .. } => Some(("delegate_renamed", None, None)),
            ServerEvent::ToolCall { item, name, .. } => Some(("tool_call", Some(item), Some(name))),
            ServerEvent::ToolResult { item, .. } => Some(("tool_result", Some(item), None)),
            _ => None,
        };
        let record = Record(start, end);
        if let Some(session) = history {
            journal.index.history(session, record, env.seq);
            if matches!(&env.event, ServerEvent::CommentApplication { .. })
                || matches!(&env.event, ServerEvent::ToolCall { name, .. } if crate::thread_artifacts::may_record_path(name))
            {
                journal.index.snapshot_recovery.insert(session.into());
            }
        }
        if let Some((kind, item, name)) = header {
            let header = EventHeader {
                kind: kind.into(), session: None, item: item.cloned(), name: name.cloned(),
            };
            if header.recovery() {
                journal.index.recovery.push(record);
            }
        }
        // The thread and recovery projections above were classified from typed
        // events. The tail needs only their byte ranges.
        journal.index.end = end;
        journal.index.push_tail(record);
        if journal.saved_end == 0 || end - journal.saved_end >= CHECKPOINT_INTERVAL {
            journal.save_checkpoint();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use super::*;

    fn event(seq: u64) -> Envelope {
        Envelope { seq, workspace: None, event: ServerEvent::FileChanged { path: format!("synthetic-{seq}") } }
    }

    #[test]
    fn checkpoint_limits_recovery_and_accepts_appends_after_a_partial_record() -> anyhow::Result<()> {
        let state = tempfile::tempdir()?;
        let path = state.path().join(super::super::TIMELINE);
        {
            let mut output = io::BufWriter::new(File::create(&path)?);
            for seq in 1..=(BACKLOG as u64 * 2) {
                serde_json::to_writer(&mut output, &event(seq))?;
                writeln!(output)?;
            }
            output.flush()?;
        }
        let timeline = Timeline::open(state.path());
        let saved_len = path.metadata()?.len();
        let epoch = timeline.0.lock().unwrap()[1].index.epoch;
        assert_eq!(timeline.0.lock().unwrap()[1].saved_end, saved_len);
        assert!(path.with_extension("jsonl-index").is_file());

        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"{\"seq\":")?;
        let next = BACKLOG as u64 * 2 + 1;
        let mut writer = super::super::TimelineWriter::open(state.path())?;
        writer.append(&event(next));
        drop(writer);

        let restored = Timeline::open(state.path());
        assert_eq!(restored.0.lock().unwrap()[1].index.epoch, epoch);
        let mut replayed = Vec::new();
        restored.restore(|env, size| {
            assert!(size.is_some());
            replayed.push(env.seq);
        });
        assert_eq!(replayed.len(), BACKLOG);
        assert_eq!(replayed.last(), Some(&next));
        assert!(std::fs::read(&path)?[saved_len as usize..].starts_with(b"{\"seq\":\n"));
        Ok(())
    }
}
