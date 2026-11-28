//! Durations and sizes of transcript loads and saves.
//!
//! Records carry fixed labels, counts, byte sizes and durations only. Thread
//! IDs, titles, paths, error texts and conversation content stay out.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use tracing::Level;

const TARGET: &str = "secretaire::transcript_timing";
pub(crate) const DIRECTIVE: &str = "secretaire::transcript_timing=info";
/// Work at or above either threshold is logged at info level.
const SLOW: Duration = Duration::from_millis(100);
const LARGE: u64 = 4 * 1024 * 1024;

thread_local! {
    /// Loads and saves finished on this thread, so a lock holder can tell
    /// whether any ran while it held the lock.
    static FINISHED: Cell<u64> = const { Cell::new(0) };
}

fn level(total: Duration, bytes: u64) -> Level {
    if total >= SLOW || bytes >= LARGE { Level::INFO } else { Level::DEBUG }
}

fn finished() {
    FINISHED.set(FINISHED.get().wrapping_add(1));
}

// A tracing callsite has a fixed level, so each record needs one per level.
macro_rules! record {
    ($level:expr, $($fields:tt)+) => {
        if $level == Level::INFO {
            tracing::info!(target: TARGET, $($fields)+)
        } else {
            tracing::debug!(target: TARGET, $($fields)+)
        }
    };
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

#[derive(Default)]
pub(super) struct Replay {
    records: u64,
    bytes: u64,
    time: Duration,
}

impl Replay {
    pub(super) fn add(&mut self, bytes: usize) {
        self.records += 1;
        self.bytes += bytes as u64;
    }
}

/// One cold read of a transcript. Dropping it writes the record, so a read
/// that returns early is reported with `ok=false`.
pub(super) struct Load {
    reason: &'static str,
    started: Instant,
    lap: Instant,
    ok: bool,
    entries: usize,
    markdown_bytes: u64,
    markdown: Duration,
    pub(super) journal: Replay,
    merge: Duration,
    pub(super) legacy: Replay,
    finish: Duration,
}

impl Load {
    pub(super) fn start(reason: &'static str) -> Self {
        let now = Instant::now();
        Self {
            reason, started: now, lap: now, ok: false, entries: 0, markdown_bytes: 0,
            markdown: Duration::ZERO, journal: Replay::default(), merge: Duration::ZERO,
            legacy: Replay::default(), finish: Duration::ZERO,
        }
    }

    fn lap(&mut self) -> Duration {
        let previous = std::mem::replace(&mut self.lap, Instant::now());
        self.lap - previous
    }

    pub(super) fn markdown_read(&mut self, bytes: u64) {
        self.markdown_bytes = bytes;
        self.markdown = self.lap();
    }

    pub(super) fn journal_replayed(&mut self) {
        self.journal.time = self.lap();
    }

    pub(super) fn merged(&mut self) {
        self.merge = self.lap();
    }

    pub(super) fn legacy_replayed(&mut self) {
        self.legacy.time = self.lap();
    }

    pub(super) fn succeeded(mut self, entries: usize) {
        self.finish = self.lap();
        self.entries = entries;
        self.ok = true;
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        let total = self.started.elapsed();
        let bytes = self.markdown_bytes + self.journal.bytes + self.legacy.bytes;
        record!(level(total, bytes),
            reason = self.reason, ok = self.ok, total_ms = millis(total), entries = self.entries,
            markdown_bytes = self.markdown_bytes, markdown_ms = millis(self.markdown),
            journal_records = self.journal.records, journal_bytes = self.journal.bytes,
            journal_ms = millis(self.journal.time), merge_ms = millis(self.merge),
            legacy_records = self.legacy.records, legacy_bytes = self.legacy.bytes,
            legacy_ms = millis(self.legacy.time), finish_ms = millis(self.finish),
            "transcript load");
        finished();
    }
}

/// Stage boundaries of one save. A stage the save did not reach reports zero,
/// and the stage it stopped in is covered by the total only.
pub(super) struct Save {
    trigger: &'static str,
    started: Instant,
    prepared: Cell<Option<Instant>>,
    written: Cell<Option<Instant>>,
    synced: Cell<Option<Instant>>,
    bytes: Cell<u64>,
}

impl Save {
    pub(super) fn start(trigger: &'static str) -> Self {
        Self {
            trigger, started: Instant::now(), prepared: Cell::new(None),
            written: Cell::new(None), synced: Cell::new(None), bytes: Cell::new(0),
        }
    }

    /// The save lock is held and payload sidecars are stored.
    pub(super) fn prepared(&self) {
        self.prepared.set(Some(Instant::now()));
    }

    /// The transcript is serialized into the staged file.
    pub(super) fn written(&self) {
        self.written.set(Some(Instant::now()));
    }

    /// The staged file is flushed and synced.
    pub(super) fn synced(&self) {
        self.synced.set(Some(Instant::now()));
    }

    /// The staged file of this size replaced the live one.
    pub(super) fn replaced(&self, bytes: u64) {
        self.bytes.set(bytes);
    }

    pub(super) fn log(self, ok: bool) {
        let end = Instant::now();
        let total = end - self.started;
        let between = |from: Option<Instant>, to: Option<Instant>| {
            from.zip(to).map_or(0, |(from, to)| millis(to - from))
        };
        let (prepared, written, synced) = (self.prepared.get(), self.written.get(), self.synced.get());
        let bytes = self.bytes.get();
        record!(level(total, bytes),
            trigger = self.trigger, ok, bytes, total_ms = millis(total),
            prepare_ms = between(Some(self.started), prepared),
            write_ms = between(prepared, written), sync_ms = between(written, synced),
            publish_ms = between(synced, Some(end)),
            "transcript save");
        finished();
    }
}

/// Declare after the Hub lock guard, so this drops while the lock is held.
pub(crate) struct LockHold {
    operation: &'static str,
    started: Instant,
    finished: u64,
}

impl LockHold {
    pub(crate) fn start(operation: &'static str) -> Self {
        Self { operation, started: Instant::now(), finished: FINISHED.get() }
    }
}

impl Drop for LockHold {
    fn drop(&mut self) {
        let held = self.started.elapsed();
        let transcripts = FINISHED.get().wrapping_sub(self.finished);
        // Most events neither load nor save, and are reported only when slow.
        if transcripts != 0 || held >= SLOW {
            record!(level(held, 0),
                operation = self.operation, held_ms = millis(held), transcripts,
                "transcript lock hold");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use fuzukue_proto::{Actor, ServerEvent};

    use super::*;

    #[test]
    fn slow_or_large_work_is_logged_at_info_level() {
        let short = SLOW - Duration::from_nanos(1);
        assert_eq!(level(short, LARGE - 1), Level::DEBUG);
        assert_eq!(level(SLOW, 0), Level::INFO);
        assert_eq!(level(Duration::ZERO, LARGE), Level::INFO);
    }

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn loads_saves_and_lock_holds_report_sizes_without_content() -> Result<()> {
        // Tracing callsite interest is process-wide. Capture with a global
        // subscriber in a dedicated test process.
        const CHILD: &str = "FUZUKUE_TEST_TRANSCRIPT_TIMING_CHILD";
        const NAME: &str =
            "threads::timing::tests::loads_saves_and_lock_holds_report_sizes_without_content";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, "1")
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
            return Ok(());
        }

        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(format!("off,{TARGET}=debug")))
            .with_writer(move || writer.clone()).finish();
        tracing::subscriber::set_global_default(subscriber)?;

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let threads = project.path().join("threads");
        std::fs::create_dir(&threads)?;
        let path = threads.join("private-name.md");
        std::fs::write(&path, concat!(
            "---\nid: thread-private-id\ntitle: private-title\ncreated: 1\nupdated: 1\n---\n",
        ))?;
        let session = "thread-private-id".to_owned();
        let mut journal = crate::state::TimelineWriter::open(state.path())?;
        journal.append(&fuzukue_proto::Envelope {
            workspace: None, seq: 1,
            event: ServerEvent::TextDelta {
                session: session.clone(), item: "reply".into(), actor: Actor::Claude,
                text: "private-journal-text".into(),
            },
        });
        drop(journal);

        let hub = crate::hub::Hub::with_timeline(state.path());
        hub.load_threads(project.path(), state.path());
        hub.publish(ServerEvent::UserPrompt {
            session: session.clone(), item: "prompt".into(), text: "private-prompt-text".into(),
            paths: Vec::new(), to_worker: false,
        });
        let saved = std::fs::read_to_string(&path)?;
        assert!(saved.contains("private-journal-text") && saved.contains("private-prompt-text"));

        let logs = String::from_utf8(logs.0.lock().unwrap().clone())?;
        let line = |message: &str| logs.lines().find(|line| line.contains(message))
            .unwrap_or_else(|| panic!("missing {message}: {logs}"));
        let load = line("transcript load");
        assert!(load.contains("reason=\"access\"") && load.contains("ok=true"), "{load}");
        assert!(load.contains("journal_records=1 "), "{load}");
        let save = line("transcript save");
        assert!(save.contains("trigger=\"user_prompt\"") && save.contains("ok=true"), "{save}");
        assert!(save.contains(&format!("bytes={} ", saved.len())), "{save}");
        let hold = line("transcript lock hold");
        assert!(hold.contains("operation=\"publish\"") && hold.contains("transcripts=2"), "{hold}");
        assert!(!logs.contains("private"), "{logs}");
        for dir in [project.path(), state.path()] {
            assert!(!logs.contains(dir.to_str().unwrap()), "{logs}");
        }
        Ok(())
    }
}
