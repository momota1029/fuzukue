use super::*;
use crate::threads::split::{SplitStats, Step};
use std::process::Command;
use tempfile::NamedTempFile;

const SMALL_TURNS: usize = 440;

fn store(fixture: &Fixture) -> Store {
    Store::load(&fixture.project, &fixture.state, &BTreeMap::new())
}

fn append(thread: &mut Thread, start: usize, turns: usize, chars: usize) {
    let question = "q".repeat(chars);
    let answer = "a".repeat(chars);
    for turn in start..start + turns {
        thread.transcript.observe(&prompt(&format!("prompt-{turn}"), &question));
        thread.transcript.observe(&text(&format!("reply-{turn}"), &answer));
    }
}

fn seed(fixture: &Fixture, turns: usize, chars: usize) -> Result<PathBuf> {
    let mut store = store(fixture);
    store.create_with_id(ID, None, Some("Synthetic split".into()))?;
    let thread = store.threads.get_mut(ID).unwrap();
    thread.front.preview_payloads = true;
    thread.transcript.observe(&call("old-tool", serde_json::json!({"example": true})));
    append(thread, 0, turns, chars);
    thread.save("split_fixture")?;
    Ok(thread.path.clone())
}

fn run_split(store: &mut Store, checkpoint: &mut impl FnMut(Step)) -> Result<SplitStats> {
    let candidate = store.prepare_split(ID)?;
    let mut staged = candidate.stage(checkpoint)?;
    staged.mark(checkpoint)?;
    store.publish_split(staged, checkpoint)
}

fn layout(path: &Path) -> Result<segments::Layout> {
    read_front(&mut BufReader::new(File::open(path)?))?.1.context("expected segmented thread")
}

fn segment_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let directory = segments::directory(path, ID)?;
    Ok(layout(path)?.segments.into_iter().map(|info| directory.join(info.file)).collect())
}

fn original_path(path: &Path, original: &[u8]) -> PathBuf {
    path.parent().unwrap().join(fuzukue_proto::THREAD_SAVE_DIR).join("segmentation")
        .join(format!("{:x}.original", Sha256::digest(original)))
}

#[tokio::test]
async fn manual_split_then_join_is_byte_exact_and_reuses_segments() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let original = fs::read(&path)?;
    let hub = fixture.hub();
    let private = fixture.threads().join(fuzukue_proto::THREAD_SAVE_DIR);
    let paths = [path.clone(), private.join(format!("{}.previous", path.file_name().unwrap().to_string_lossy()))];
    let before = stamps(&paths)?;
    assert!(!private.join("segmentation").exists());
    hub.check_split_thread(ID, || Ok(())).await?;
    assert_eq!(stamps(&paths)?, before);
    assert!(!private.join("segmentation").exists());
    assert!(!segments::directory_path(&path, ID)?.exists());
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    let stats = hub.split_thread(ID, || Ok(())).await?;
    assert_eq!(stats.overrides, 0);
    assert_eq!(stats.segments, 1);
    let kept = original_path(&path, &original);
    assert!(fs::read(&kept)? == original, "the durable original differs");
    let files = segment_paths(&path)?;
    let file = Arc::new(File::open(&path)?);
    let mut input = BufReader::new(file.as_ref());
    let (_, _, offset) = read_front(&mut input)?;
    let mut count = 0;
    Transcript::read_entries(&mut input, &file, offset, |entry, _| {
        if count == 0 {
            assert_eq!(entry.meta.actor, Actor::User);
        }
        count += 1;
    })?;
    assert!(count >= 256);
    assert!(stats.latest_bytes - offset >= 4 * 1024 * 1024);
    let before = stamps(&files)?;
    hub.join_thread(ID).await?;
    assert!(fs::read(&path)? == original, "joining changed the original bytes");
    assert_eq!(stamps(&files)?, before);
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());

    hub.split_thread(ID, || Ok(())).await?;
    assert_eq!(segment_paths(&path)?, files);
    assert_eq!(stamps(&files)?, before);
    Ok(())
}

#[test]
fn repeat_sealing_keeps_old_segments_and_late_overrides() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let mut store = store(&fixture);
    run_split(&mut store, &mut |_| {})?;
    let old = segment_paths(&path)?;
    let before = stamps(&old)?;
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.observe(&result("old-tool", true, "late result for sealed tool"));
    thread.transcript.observe(&call("new-tool", serde_json::json!({})));
    thread.transcript.observe(&result("new-tool", true, "result for the next segment"));
    append(thread, SMALL_TURNS, SMALL_TURNS, 8 * 1024);
    thread.save("split_growth")?;
    assert_eq!(layout(&path)?.overrides, [0]);
    assert_eq!(stamps(&old)?, before);

    let mut expected = Vec::new();
    thread.transcript.write_markdown(&mut expected)?;
    let stats = run_split(&mut store, &mut |_| {})?;
    assert!(stats.segments > old.len());
    assert_eq!(stats.overrides, 1);
    let all = segment_paths(&path)?;
    assert_eq!(all[..old.len()], old);
    assert_eq!(stamps(&old)?, before);
    let mut next = String::new();
    for segment in &all[old.len()..] {
        next.push_str(&fs::read_to_string(segment)?);
    }
    assert!(next.contains("result for the next segment"));
    let mut restarted = self::store(&fixture);
    let mut actual = Vec::new();
    restarted.threads[ID].transcript.write_markdown(&mut actual)?;
    assert!(actual == expected, "resealing changed the loaded body");
    restarted.join_now(ID)?;
    let joined = fs::read_to_string(&path)?;
    assert!(fuzukue_cards::split_frontmatter(&joined).unwrap().1.as_bytes() == expected,
        "joining changed the resealed body");
    Ok(())
}

#[test]
fn split_refuses_changed_thread_without_exchanging_latest() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let mut store = store(&fixture);
    let mut staged = store.prepare_split(ID)?.stage(&mut |_| {})?;
    staged.mark(&mut |_| {})?;
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.observe(&text("new-reply", "new event while staging"));
    thread.save("split_concurrent")?;
    let changed = fs::read(&path)?;
    assert!(store.publish_split(staged, &mut |_| {}).is_err());
    assert_eq!(fs::read(&path)?, changed);
    assert!(String::from_utf8(changed)?.contains("new event while staging"));
    assert!(!self::store(&fixture).is_segmented(ID));
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    Ok(())
}

#[test]
fn split_refuses_external_replacement_with_preserved_size_and_mtime() -> Result<()> {
    use std::fs::FileTimes;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let mut store = store(&fixture);
    let mut staged = store.prepare_split(ID)?.stage(&mut |_| {})?;
    staged.mark(&mut |_| {})?;
    let metadata = path.metadata()?;
    let original_version = FileVersion::read(metadata.clone())?;
    let original = fs::read_to_string(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(metadata.permissions().mode() ^ 0o100))?;
    assert_eq!(FileVersion::read(path.metadata()?)?, original_version);
    assert!(store.publish_split(staged, &mut |_| {}).is_err(), "ctime changed");
    assert!(fs::read_to_string(&path)? == original);

    let mut staged = store.prepare_split(ID)?.stage(&mut |_| {})?;
    staged.mark(&mut |_| {})?;
    let modified = path.metadata()?.modified()?;
    // Replace the path with a same-length valid file rather than modify the
    // inode of the hard-linked backup kept for the staged split.
    let mut external = NamedTempFile::new_in(path.parent().unwrap())?;
    let changed = original.replacen("Synthetic split", "Synthetic edit!", 1);
    assert_eq!(original.len(), changed.len());
    external.write_all(changed.as_bytes())?;
    external.as_file().set_times(FileTimes::new().set_modified(modified))?;
    external.persist(&path)?;
    assert!(store.publish_split(staged, &mut |_| {}).is_err());
    assert_eq!(fs::read_to_string(&path)?, changed);
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    Ok(())
}

#[test]
fn refused_split_keeps_the_mark_for_an_external_segmented_replacement() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let mut store = store(&fixture);
    let mut staged = store.prepare_split(ID)?.stage(&mut |_| {})?;
    staged.mark(&mut |_| {})?;

    let mut external = NamedTempFile::new_in(path.parent().unwrap())?;
    std::io::copy(&mut File::open(&path)?, &mut external)?;
    super::split(external.path(), &[1])?;
    external.persist(&path)?;
    assert!(store.publish_split(staged, &mut |_| {}).is_err());
    assert!(layout(&path).is_ok());
    assert_eq!(rollback_marker::listed(&fixture.marker_root())?.len(), 1);
    Ok(())
}

#[test]
fn split_check_reports_the_first_item_changed_by_journal_replay() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let before = fs::read(&path)?;
    {
        let hub = fixture.hub();
        hub.publish(result("old-tool", false, "private payload"));
        hub.publish(text("another-item", "private message"));
    }
    let store = store(&fixture);
    let candidate = store.prepare_split(ID)?;
    let error = candidate.check().unwrap_err().to_string();
    assert_eq!(error, "split replay mismatch at item ID \"old-tool\" (tool)");
    let error = candidate.stage(&mut |_| {}).err().unwrap().to_string();
    assert_eq!(error, "split replay mismatch at item ID \"old-tool\" (tool)");
    assert_eq!(fs::read(&path)?, before);
    assert!(!segments::directory_path(&path, ID)?.exists());
    Ok(())
}

#[test]
fn split_check_rejects_duplicate_item_ids_within_the_same_file() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let original = fs::read_to_string(&path)?;
    let duplicate = original.replacen("\"reply-1\"", "\"reply-0\"", 1);
    assert_ne!(duplicate, original);
    fs::write(&path, &duplicate)?;
    let store = store(&fixture);
    let candidate = store.prepare_split(ID)?;
    let error = candidate.check().unwrap_err().to_string();
    assert_eq!(error, "duplicate thread item ID \"reply-0\"");
    let error = candidate.stage(&mut |_| {}).err().unwrap().to_string();
    assert_eq!(error, "duplicate thread item ID \"reply-0\"");
    assert_eq!(fs::read_to_string(&path)?, duplicate);
    Ok(())
}

#[test]
fn split_eligibility_and_segment_collision() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let mut store = store(&fixture);
    let issue = store.issue_thread(42)?;
    assert!(store.prepare_split(&issue).err().unwrap().to_string().contains("issue-number"));
    let thread = store.threads.get_mut(ID).unwrap();
    thread.front.migrated_from = Some("legacy".into());
    assert!(store.prepare_split(ID).err().unwrap().to_string().contains("legacy-session"));
    let thread = store.threads.get_mut(ID).unwrap();
    thread.front.migrated_from = None;
    thread.front.preview_payloads = false;
    assert!(store.prepare_split(ID).err().unwrap().to_string().contains("payload conversion"));
    let thread = store.threads.get_mut(ID).unwrap();
    thread.front.preview_payloads = true;
    thread.transcript.volatile_payloads = true;
    assert!(store.prepare_split(ID).err().unwrap().to_string().contains("only in memory"));
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.volatile_payloads = false;
    thread.active = true;
    assert!(store.prepare_split(ID).err().unwrap().to_string().contains("running"));
    store.threads.get_mut(ID).unwrap().active = false;

    // Stage without publishing, then leave the unlisted segment behind.
    let staged = store.prepare_split(ID)?.stage(&mut |_| {})?;
    let busy = store.prepare_split(ID)?.check().unwrap_err().to_string();
    assert!(busy.contains("busy: another thread split or join is running"));
    drop(staged);
    let dir = segments::directory(&path, ID)?;
    let orphan = fs::read_dir(&dir)?.next().unwrap()?.path();
    fs::write(&orphan, b"conflicting orphan")?;
    assert!(run_split(&mut store, &mut |_| {}).is_err());
    assert_eq!(fs::read(&orphan)?, b"conflicting orphan");
    assert!(!store.is_segmented(ID));
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    Ok(())
}

#[test]
fn split_keeps_published_state_when_hash_check_fails() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let moved = path.with_extension("injected-failure");
    let mut store = store(&fixture);
    let result = run_split(&mut store, &mut |step| {
        if matches!(step, Step::Exchanged) {
            fs::rename(&path, &moved).unwrap();
        }
    });
    assert!(result.err().unwrap().to_string().contains("split was published but the live file could not be verified"));
    fs::rename(&moved, &path)?;
    assert!(store.is_segmented(ID));
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.observe(&text("after-failure", "saved"));
    thread.save("after_split_failure")?;
    Ok(())
}

#[test]
fn split_requires_both_latest_size_limits() -> Result<()> {
    for (turns, chars) in [(96, 32 * 1024), (SMALL_TURNS, 1024)] {
        let fixture = Fixture::new()?;
        let path = seed(&fixture, turns, chars)?;
        let original = fs::read(&path)?;
        let mut store = store(&fixture);
        let error = run_split(&mut store, &mut |_| {}).err().unwrap();
        assert!(error.to_string().contains("no user input boundary"));
        assert!(fs::read(&path)? == original);
        assert!(!segments::directory_path(&path, ID)?.exists());
        assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn split_checks_previous_before_saving_and_again_before_exchange() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let hub = fixture.hub();
    let before = stamps(std::slice::from_ref(&path))?;
    let error = hub.split_thread(ID, || anyhow::bail!("no compatible previous build")).await.err().unwrap();
    assert!(error.to_string().contains("no compatible"));
    assert_eq!(stamps(std::slice::from_ref(&path))?, before);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let error = hub.split_thread(ID, move || {
        anyhow::ensure!(calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0,
            "previous build changed");
        Ok(())
    }).await.err().unwrap();
    assert_eq!(error.to_string(), "previous build changed");
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    assert_eq!(fs::read(&path)?, before[0].1);
    Ok(())
}

/// The child exits without unwinding so the parent opens the actual on-disk
/// state left at each point in the publication protocol.
#[test]
#[ignore = "child of the publication crash test"]
fn split_crash_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("FUZUKUE_SPLIT_CRASH_ROOT").context("missing crash fixture")?);
    let point = std::env::var("FUZUKUE_SPLIT_CRASH_POINT")?;
    let mut store = Store::load(&root.join("project"), &root.join("state/workspace"), &BTreeMap::new());
    run_split(&mut store, &mut |step| {
        if format!("{step:?}") == point {
            std::process::exit(86);
        }
    })?;
    anyhow::bail!("unreached crash point {point}")
}

#[test]
fn split_publication_survives_process_exit_at_every_step() -> Result<()> {
    let exe = std::env::current_exe()?;
    for step in [Step::Saved, Step::SegmentWritten, Step::SegmentsSynced, Step::LatestWritten,
        Step::LatestSynced, Step::Verified, Step::OriginalKept, Step::Marked, Step::Exchanged,
        Step::Published] {
        let fixture = Fixture::new()?;
        let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
        let before = fs::read(&path)?;
        let output = Command::new(&exe)
            .args(["--ignored", "--exact", "threads::segment_tests::split_tests::split_crash_child"])
            .env("FUZUKUE_SPLIT_CRASH_ROOT", fixture.dir.path())
            .env("FUZUKUE_SPLIT_CRASH_POINT", format!("{step:?}"))
            .output()?;
        assert_eq!(output.status.code(), Some(86), "{step:?}: {} {}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if matches!(step, Step::OriginalKept | Step::Marked | Step::Exchanged | Step::Published) {
            assert!(fs::read(original_path(&path, &before))? == before, "original at {step:?}");
        }
        let exchanged = matches!(step, Step::Exchanged | Step::Published);
        assert_eq!(rollback_marker::listed(&fixture.marker_root())?.len(),
            usize::from(exchanged || matches!(step, Step::Marked)));
        let mut reopened = store(&fixture);
        assert_eq!(reopened.is_segmented(ID), exchanged, "{step:?}");
        assert_eq!(rollback_marker::listed(&fixture.marker_root())?.len(), usize::from(exchanged));
        if exchanged {
            reopened.join_now(ID)?;
        }
        assert!(fs::read(&path)? == before, "different checkpoint after {step:?}");
    }
    Ok(())
}

#[test]
#[ignore = "large synthetic save measurement"]
fn split_large_save_bytes() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = seed(&fixture, 34_000, 960)?;
    let mut store = store(&fixture);
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.observe(&result("old-tool", true, "first update"));
    thread.save("split_measure_single")?;
    let before = path.metadata()?.len();
    let stats = run_split(&mut store, &mut |_| {})?;
    assert!(stats.segments >= 4);
    assert_eq!(stats.overrides, 0);
    let files = segment_paths(&path)?;
    let sealed = stamps(&files)?;
    let thread = store.threads.get_mut(ID).unwrap();
    thread.transcript.observe(&result("old-tool", true, "later update"));
    thread.save("split_measure_segmented")?;
    let after = path.metadata()?.len();
    assert_eq!(layout(&path)?.overrides, [0]);
    assert_eq!(stamps(&files)?, sealed);
    assert!(after < before / 10);
    println!("save checkpoint bytes: {before} before, {after} after, {} sealed segments, 1 override, {:.2}% reduction",
        stats.segments, 100.0 * (1.0 - after as f64 / before as f64));
    Ok(())
}

#[tokio::test]
#[ignore = "requires a release 1 server in FUZUKUE_PREVIOUS_SERVER_BINARY"]
async fn previous_server_reads_split_and_replays_late_update() -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{ClientMsg, PROTOCOL_VERSION};
    use std::process::Stdio;
    use tokio::net::TcpStream;
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let binary = PathBuf::from(std::env::var_os("FUZUKUE_PREVIOUS_SERVER_BINARY")
        .context("set FUZUKUE_PREVIOUS_SERVER_BINARY")?);
    assert!(crate::self_update::drain::contains_marker(&binary, rollback_marker::FORMAT_MARKER)?);
    let mut fixture = Fixture::new()?;
    let home = fixture.dir.path().canonicalize()?;
    let flat = fixture.project.to_string_lossy().trim_start_matches('/').replace('/', "%");
    let state_root = home.join("xdg-state");
    fixture.state = state_root.join("fuzukue").join(flat);
    fs::create_dir_all(&fixture.state)?;
    let path = seed(&fixture, SMALL_TURNS, 8 * 1024)?;
    let hub = fixture.hub();
    hub.split_thread(ID, || Ok(())).await?;
    let sealed = segment_paths(&path)?;
    let before = stamps(&sealed)?;
    let latest = fs::read(&path)?;
    hub.publish(result("old-tool", true, "late journal update after manual split"));
    assert_eq!(fs::read(&path)?, latest, "the late result is only in the journal");
    drop(hub);

    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let log = home.join("previous-server.log");
    let mut child = tokio::process::Command::new(binary)
        .arg(&fixture.project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", state_root)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("FUZUKUE_ADDR", address.to_string())
        .stdin(Stdio::null())
        .stdout(File::create(&log)?)
        .stderr(File::options().append(true).open(&log)?)
        .kill_on_drop(true)
        .spawn()?;

    let read = tokio::time::timeout(Duration::from_secs(20), async {
        let connection = loop {
            if let Ok(connection) = TcpStream::connect(address).await {
                break connection;
            }
            anyhow::ensure!(child.try_wait()?.is_none(), "previous server stopped during startup");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let (mut socket, _) = client_async(format!("ws://{address}/ws"), connection).await?;
        let hello = ClientMsg::Hello {
            protocol: PROTOCOL_VERSION, markdown_components: Vec::new(), thread_history: true,
            selection_scoped_transcripts: false, epoch: None, cached_sessions: Vec::new(),
            locale: Some("en".into()), resume_from: None, workspace: None, platform: None, build_id: None,
        };
        socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        let request = ClientMsg::GetThreadHistory {
            session: ID.into(), request: 2, before: 4, generation: None,
        };
        socket.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
        while let Some(message) = socket.next().await {
            let message = message?;
            let envelope: Envelope = serde_json::from_str(message.to_text()?)?;
            match envelope.event {
                ServerEvent::ThreadHistoryPage { request: 2, events, .. } => return Ok(events),
                ServerEvent::ThreadHistoryFailed { request: 2, .. } => anyhow::bail!("previous server refused split history"),
                _ => {}
            }
        }
        anyhow::bail!("previous server closed the connection")
    }).await;
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    child.wait().await?;
    let events = read.context("timed out reading split history")
        .and_then(|result| result)
        .with_context(|| fs::read_to_string(&log).unwrap_or_default())?;
    assert!(events.iter().any(|event| matches!(event,
        ServerEvent::TextDelta { item, actor: Actor::User, .. } if item == "prompt-0")));
    assert!(events.iter().any(|event| matches!(event,
        ServerEvent::ToolResult { item, output, .. }
            if item == "old-tool" && output == "late journal update after manual split")));
    assert_eq!(stamps(&sealed)?, before);
    Ok(())
}
