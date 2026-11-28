//! Segmented threads, tested with synthetic records only.

use super::*;
use fuzukue_proto::{
    CommentApplication, CommentApplicationState, DelegateNameChange, Envelope, GitHubActivityKind,
    RunDetails, TrashAction, TrashTarget,
};
use std::io::BufWriter;
use std::time::Duration;
use tempfile::TempDir;

const ID: &str = "thread-segment-test";
const JOB: &str = "delegate:segment-job";

/// A workspace whose state directory sits below a private shared root.
pub(crate) struct Fixture {
    dir: TempDir,
    pub(crate) project: PathBuf,
    pub(crate) state: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().canonicalize()?.join("project");
        let state = dir.path().canonicalize()?.join("state/workspace");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&state)?;
        Ok(Self { dir, project, state })
    }

    fn hub(&self) -> Arc<crate::hub::Hub> {
        let hub = Arc::new(crate::hub::Hub::with_timeline(&self.state));
        hub.load_threads(&self.project, &self.state);
        hub
    }

    fn threads(&self) -> PathBuf {
        self.project.join("threads")
    }

    fn marker_root(&self) -> PathBuf {
        rollback_marker::root(&self.state)
    }
}

/// Cut a saved single-file thread into segments holding `counts` entries each,
/// keeping the stored bytes. This build offers no such operation.
pub(crate) fn split(path: &Path, counts: &[usize]) -> Result<Vec<PathBuf>> {
    let file = Arc::new(File::open(path)?);
    let mut input = BufReader::new(file.as_ref());
    let (front, layout, offset) = read_front(&mut input)?;
    assert!(layout.is_none());
    let mut raws = Vec::new();
    Transcript::read_entries(&mut input, &file, offset, |_, raw| raws.push(format!("{ENTRY}{raw}")))?;
    assert!(counts.iter().sum::<usize>() <= raws.len(), "only {} entries", raws.len());
    let dir = path.parent().unwrap().join(&front.id).join("segments");
    fs::create_dir_all(&dir)?;
    let mut rest = raws.as_slice();
    let mut infos = Vec::new();
    let mut paths = Vec::new();
    for (index, &count) in counts.iter().enumerate() {
        let (entries, tail) = rest.split_at(count);
        rest = tail;
        let content = format!("---\nthread_id: {}\nsegment: {}\n---\n{}", front.id, index + 1, entries.concat());
        let sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        let name = segments::file_name(index + 1, &sha256).unwrap();
        fs::write(dir.join(&name), &content)?;
        paths.push(dir.join(&name));
        infos.push(segments::SegmentInfo { file: name, entries: count, bytes: content.len() as u64, sha256 });
    }
    let infos: Vec<_> = infos.iter().collect();
    let latest = segments::front_text(&front, &infos, &[])? + &rest.concat();
    fs::write(path, latest)?;
    Ok(paths)
}

/// Journal positions differ between two workspaces. Everything else must match.
fn normalized(body: &str) -> String {
    body.split_inclusive('\n').map(|line| {
        let Some(header) = line.strip_prefix(ENTRY).and_then(|line| line.strip_suffix(" -->\n")) else {
            return line.to_owned();
        };
        let mut meta: serde_json::Value = serde_json::from_str(header).unwrap();
        if let Some(tool) = meta.get_mut("tool").and_then(serde_json::Value::as_object_mut) {
            for key in ["recovered_input", "recovered_output"] {
                if tool.contains_key(key) {
                    tool.insert(key.into(), true.into());
                }
            }
        }
        format!("{ENTRY}{meta} -->\n")
    }).collect()
}

/// The body the files on disk assemble to, read without the server's loader.
fn disk_body(path: &Path) -> Result<String> {
    let file = Arc::new(File::open(path)?);
    let mut input = BufReader::new(file.as_ref());
    let (front, layout, offset) = read_front(&mut input)?;
    let mut latest = Vec::new();
    Transcript::read_entries(&mut input, &file, offset, |_, raw| latest.push(format!("{ENTRY}{raw}")))?;
    let Some(layout) = layout else { return Ok(latest.concat()) };
    let mut sealed = Vec::new();
    for info in &layout.segments {
        let file = Arc::new(File::open(path.parent().unwrap().join(&front.id).join("segments").join(&info.file))?);
        let mut input = BufReader::new(file.as_ref());
        let (mut line, mut offset, mut fences) = (String::new(), 0, 0);
        while fences < 2 {
            line.clear();
            offset += input.read_line(&mut line)? as u64;
            fences += usize::from(line == "---\n");
        }
        Transcript::read_entries(&mut input, &file, offset, |_, raw| sealed.push(format!("{ENTRY}{raw}")))?;
    }
    let mut latest = latest.into_iter();
    for &position in &layout.overrides {
        sealed[position] = latest.next().unwrap();
    }
    sealed.extend(latest);
    Ok(sealed.concat())
}

fn overrides(path: &Path) -> Result<Vec<usize>> {
    let mut input = BufReader::new(File::open(path)?);
    Ok(read_front(&mut input)?.1.context("single-file thread")?.overrides)
}

fn front_fields(path: &Path) -> Result<serde_yaml::Value> {
    let mut input = BufReader::new(File::open(path)?);
    let mut front = serde_yaml::to_value(read_front(&mut input)?.0)?;
    let fields = front.as_mapping_mut().unwrap();
    fields.remove("created");
    fields.remove("updated");
    Ok(front)
}

fn memory_body(hub: &crate::hub::Hub, id: &str) -> Result<String> {
    hub.read_threads(|store| {
        let thread = &store.threads[id];
        thread.transcript.check()?;
        let mut body = Vec::new();
        thread.transcript.write_markdown(&mut body)?;
        Ok(String::from_utf8(body)?)
    })
}

fn prompt(item: &str, text: &str) -> ServerEvent {
    ServerEvent::UserPrompt { session: ID.into(), item: item.into(), text: text.into(), paths: Vec::new(), to_worker: false }
}

fn text(item: &str, text: &str) -> ServerEvent {
    ServerEvent::TextDelta { session: ID.into(), item: item.into(), actor: Actor::Claude, text: text.into() }
}

fn call(item: &str, input: serde_json::Value) -> ServerEvent {
    ServerEvent::ToolCall { session: ID.into(), item: item.into(), actor: Actor::Claude, name: "Read".into(), input }
}

fn result(item: &str, ok: bool, output: &str) -> ServerEvent {
    ServerEvent::ToolResult { session: ID.into(), item: item.into(), ok, output: output.into() }
}

fn turn_end() -> ServerEvent {
    ServerEvent::TurnEnded { session: ID.into(), actor: Actor::Claude }
}

fn delegate_call(job: &str, large: bool) -> ServerEvent {
    let prompt = if large { "synthetic instructions ".repeat(300) } else { "short".into() };
    ServerEvent::ToolCall {
        session: ID.into(), item: job.into(), actor: Actor::System, name: "delegate".into(),
        input: serde_json::json!({"agent": "worker", "title": "Synthetic job", "label": "First label",
            "model": "", "prompt": prompt}),
    }
}

fn renamed(job: &str, at_ms: i64, label: &str) -> ServerEvent {
    ServerEvent::DelegateRenamed {
        session: ID.into(), job: job.into(),
        change: DelegateNameChange {
            at_ms, source: "user".into(), previous_label: None, label: label.into(), label_source: None,
        },
    }
}

fn application(state: CommentApplicationState) -> ServerEvent {
    ServerEvent::CommentApplication {
        session: ID.into(),
        application: CommentApplication { thread: "comment".into(), title: "Card".into(), count: 1, state },
    }
}

fn history() -> Vec<ServerEvent> {
    let child = Actor::Codex { thread_id: "worker".into(), parent_tool_use: Some(JOB.into()) };
    vec![
        prompt("u1", "First question"),
        text("r0", "Hello"),
        call("t1", serde_json::json!({"file_path": "one"})),
        result("t1", true, &"output ".repeat(400)),
        delegate_call(JOB, true),
        ServerEvent::ToolCall {
            session: ID.into(), item: "child".into(), actor: child.clone(), name: "Read".into(),
            input: serde_json::json!({}),
        },
        ServerEvent::TextDelta { session: ID.into(), item: "child-text".into(), actor: child, text: "Working".into() },
        call("t2", serde_json::json!({"file_path": "two"})),
        application(CommentApplicationState::Running),
        text("r1", "A reply"),
        turn_end(),
        prompt("u2", "Second question"),
        text("r2", "Second reply"),
        turn_end(),
    ]
}

/// Entries covered by the two test segments: everything through `u2`.
const SEALED: &[usize] = &[4, 6];

fn publish(hub: &crate::hub::Hub, events: impl IntoIterator<Item = ServerEvent>) {
    for event in events {
        hub.publish(event);
    }
}

/// Build the shared history, optionally segmented, and reopen it.
fn prepared(segmented: bool) -> Result<(Fixture, Arc<crate::hub::Hub>, PathBuf, Vec<PathBuf>)> {
    let fixture = Fixture::new()?;
    let hub = fixture.hub();
    hub.edit_threads(|store| store.create_with_id(ID, None, Some("Synthetic".into())))?;
    publish(&hub, history());
    let path = hub.thread_path(ID)?;
    drop(hub);
    let segments = if segmented { split(&path, SEALED)? } else { Vec::new() };
    let hub = fixture.hub();
    Ok((fixture, hub, path, segments))
}

fn stamps(segments: &[PathBuf]) -> Result<Vec<(FileVersion, Vec<u8>)>> {
    segments.iter().map(|path| Ok((FileVersion::read(path.metadata()?)?, fs::read(path)?))).collect()
}

struct Pair {
    single: (Fixture, Arc<crate::hub::Hub>, PathBuf),
    segmented: (Fixture, Arc<crate::hub::Hub>, PathBuf),
    segments: Vec<PathBuf>,
    stamps: Vec<(FileVersion, Vec<u8>)>,
}

impl Pair {
    fn new() -> Result<Self> {
        let (fixture, hub, path, _) = prepared(false)?;
        let single = (fixture, hub, path);
        let (fixture, hub, path, segments) = prepared(true)?;
        let stamps = stamps(&segments)?;
        Ok(Self { single, segmented: (fixture, hub, path), segments, stamps })
    }

    fn each(&self, mut apply: impl FnMut(&Fixture, &crate::hub::Hub, &Path) -> Result<()>) -> Result<()> {
        apply(&self.single.0, &self.single.1, &self.single.2)?;
        apply(&self.segmented.0, &self.segmented.1, &self.segmented.2)
    }

    #[track_caller]
    fn compare(&self, step: &str) {
        let check = || -> Result<()> {
            assert_eq!(normalized(&disk_body(&self.segmented.2)?), normalized(&disk_body(&self.single.2)?), "disk: {step}");
            assert_eq!(normalized(&memory_body(&self.segmented.1, ID)?), normalized(&memory_body(&self.single.1, ID)?),
                "memory: {step}");
            assert_eq!(front_fields(&self.segmented.2)?, front_fields(&self.single.2)?, "front: {step}");
            assert_eq!(stamps(&self.segments)?, self.stamps, "segments changed: {step}");
            Ok(())
        };
        check().unwrap();
    }

    fn publish(&self, step: &str, events: &[ServerEvent]) {
        for event in events {
            self.single.1.publish(event.clone());
            self.segmented.1.publish(event.clone());
            self.compare(step);
        }
    }

    fn restart(&mut self, step: &str) {
        for side in [&mut self.single, &mut self.segmented] {
            // Release the journal writer before reopening it.
            side.1 = Arc::new(crate::hub::Hub::new());
            side.1 = side.0.hub();
        }
        self.compare(step);
    }

    fn save(&self, step: &str) -> Result<()> {
        self.each(|_, hub, _| hub.edit_threads(|store| store.threads.get_mut(ID).unwrap().save("test")))?;
        self.compare(step);
        Ok(())
    }
}

#[test]
fn segmented_and_single_threads_write_the_same_body() -> Result<()> {
    let mut pair = Pair::new()?;
    pair.compare("loaded");
    pair.save("unchanged save")?;
    // Reopening reports the unfinished job as interrupted, a late change to a
    // sealed entry. Nothing else differs from its segment.
    let job = pair.segmented.1.read_threads(|store| store.threads[ID].transcript.index[JOB]);
    assert_eq!(overrides(&pair.segmented.2)?, [job]);

    // A reply on a sealed entry is replaced while its user message is latest.
    let rewrite = |hub: &crate::hub::Hub| hub.edit_threads(|store| store.rewrite(ID, "r2", |_, _| Ok(())));
    pair.each(|_, hub, _| rewrite(hub).map(|_| ()))?;
    pair.compare("reply superseded");

    pair.publish("streamed reply", &[text("r0", " again"), text("r0", " and again"), turn_end()]);
    pair.publish("details", &[ServerEvent::MessageDetails {
        session: ID.into(), item: "r1".into(),
        details: RunDetails {
            selection: AgentSelection { backend: AgentBackend::Claude, model: None, effort: None },
            escalated: false, tokens: Some(5), cost_usd: None, duration_ms: Some(10),
        },
    }]);
    pair.publish("late result", &[result("t2", true, &"late ".repeat(600))]);
    pair.save("late result saved")?;
    pair.publish("redelivery", &[
        prompt("u1", "First question"), call("t1", serde_json::json!({"file_path": "one"})), turn_end(),
    ]);
    pair.publish("progress", &[ServerEvent::DelegateProgress { session: ID.into(), job: JOB.into(), summary: "Halfway".into() }]);
    pair.publish("rename", &[renamed(JOB, 10, "Second label")]);
    pair.publish("dismiss", &[ServerEvent::DelegateDismissed { session: ID.into(), job: JOB.into() }]);
    pair.publish("job result", &[result(JOB, true, "done")]);
    pair.save("job saved")?;
    pair.publish("comments", &[application(CommentApplicationState::Applied { cards: vec!["card".into()] })]);
    pair.restart("restart after sealed changes");
    pair.save("saved after restart")?;

    let link = GitHubLink { repo: "example/synthetic".into(), number: 1 };
    let activity = GitHubActivity {
        id: "activity".into(), link: link.clone(), actor: "someone".into(), author: GitHubAuthor::default(),
        kind: GitHubActivityKind::Comment, excerpt: "Synthetic".into(), body: None, url: String::new(),
        at: String::new(), review_comment: None, intake: false,
    };
    pair.each(|_, hub, _| {
        hub.edit_threads(|store| store.github_edit(ID, |state| state.links.push(link.clone())).map(|_| ()))?;
        hub.deliver_github(ID, &link, std::slice::from_ref(&activity), false)
    })?;
    pair.compare("github");

    pair.each(|fixture, hub, _| {
        // An obstructed staging path makes the save fail and withdraws the note.
        let staged = fixture.threads().join(fuzukue_proto::THREAD_SAVE_DIR).join("staged-thread");
        fs::create_dir(&staged)?;
        assert!(hub.deliver_system_note(ID, "note", "Synthetic note".into()).is_err());
        assert!(!hub.read_threads(|store| store.has_item(ID, "note")));
        fs::remove_dir(&staged)?;
        Ok(())
    })?;
    pair.compare("system note withdrawn");
    pair.each(|_, hub, _| hub.deliver_system_note(ID, "note", "Synthetic note".into()))?;
    pair.compare("system note");

    // Sidecar writes fail: the journal keeps the full payload.
    pair.each(|fixture, _, _| {
        let payloads = Blobs::new(&fixture.threads()).dir().to_owned();
        fs::rename(&payloads, payloads.with_extension("aside"))?;
        fs::write(payloads, "obstruct sidecars")?;
        Ok(())
    })?;
    // A converted checkpoint refuses to drop a journal-only payload, in both formats.
    pair.publish("sidecar failure", &[result("t1", false, &"journal only ".repeat(500))]);
    pair.each(|_, hub, _| {
        assert!(hub.edit_threads(|store| store.threads.get_mut(ID).unwrap().save("test")).is_err());
        Ok(())
    })?;
    pair.compare("save refused without sidecars");
    pair.each(|fixture, _, _| {
        let payloads = Blobs::new(&fixture.threads()).dir().to_owned();
        fs::remove_file(&payloads)?;
        fs::rename(payloads.with_extension("aside"), payloads)?;
        Ok(())
    })?;
    pair.save("sidecars restored")?;
    pair.restart("restart after sidecar failure");
    pair.save("saved")?;

    // Journal appends and sidecars both fail: only memory holds the payload.
    pair.each(|fixture, hub, _| {
        let payloads = Blobs::new(&fixture.threads()).dir().to_owned();
        fs::rename(&payloads, payloads.with_extension("aside"))?;
        fs::write(payloads, "obstruct sidecars")?;
        hub.disable_journal_writes(&fixture.state)?;
        Ok(())
    })?;
    let volatile = format!("memory only {}", "payload ".repeat(700));
    pair.publish("append failure", &[result("t2", false, &volatile)]);
    pair.save("memory payload written through")?;
    assert!(fs::read_to_string(&pair.segmented.2)?.contains(&volatile), "the override carries the whole payload");
    pair.restart("restart after append failure");
    pair.save("final")?;
    let sealed = overrides(&pair.segmented.2)?;
    assert!(!sealed.is_empty() && sealed.windows(2).all(|pair| pair[0] < pair[1]), "{sealed:?}");
    assert_eq!(pair.segmented.1.tool_detail(ID, "t2").unwrap().output()?, Some(volatile));
    Ok(())
}

#[test]
fn overrides_appear_only_for_changed_sealed_entries_and_disappear_when_reverted() -> Result<()> {
    let (fixture, hub, path, segments) = prepared(true)?;
    let before = stamps(&segments)?;
    // Reopening has already reported the unfinished job as interrupted.
    let (job, applied) = hub.read_threads(|store| {
        let index = &store.threads[ID].transcript.index;
        (index[JOB], index["comments-comment"])
    });
    publish(&hub, [application(CommentApplicationState::Applied { cards: Vec::new() })]);
    assert_eq!(overrides(&path)?, [job, applied]);
    // Putting the stored value back removes the override on the next save.
    publish(&hub, [application(CommentApplicationState::Running)]);
    assert_eq!(overrides(&path)?, [job]);
    drop(hub);

    // Only a sealed entry changes, through events that do not save by themselves.
    let hub = fixture.hub();
    let latest = fs::read(&path)?;
    publish(&hub, [result("t2", true, "late sealed result"),
        ServerEvent::DelegateProgress { session: ID.into(), job: JOB.into(), summary: "Sealed progress".into() }]);
    assert_eq!(fs::read(&path)?, latest);
    drop(hub);
    let hub = fixture.hub();
    assert!(memory_body(&hub, ID)?.contains("Sealed progress"));
    hub.edit_threads(|store| store.rename(ID, "Renamed"))?;
    assert_eq!(overrides(&path)?.len(), 2);
    assert!(fs::read_to_string(&path)?.contains("late sealed result"));
    assert_eq!(stamps(&segments)?, before);
    assert!(fs::read_to_string(&path)?.starts_with("---\nformat: segmented-1\nthread_id: thread-segment-test\n"));
    Ok(())
}

/// The delegate input a portable checkpoint holds, without any journal.
fn saved_delegate(path: &Path, job: &str) -> Result<serde_json::Value> {
    let text = fs::read_to_string(path)?;
    let header = text.lines().filter_map(|line| line.strip_prefix(ENTRY)?.strip_suffix(" -->"))
        .map(serde_json::from_str::<serde_json::Value>).collect::<Result<Vec<_>, _>>()?
        .into_iter().find(|meta| meta["item"] == job).context("missing delegate entry")?;
    Ok(header["tool"]["input"].clone())
}

fn write_unconverted(fixture: &Fixture, migrated_from: Option<&str>, input: &serde_json::Value) -> Result<PathBuf> {
    fs::create_dir_all(fixture.threads())?;
    let path = fixture.threads().join("synthetic.md");
    let legacy = migrated_from.map_or_else(String::new, |id| format!("migrated_from: {id}\n"));
    let meta = serde_json::json!({"item": JOB, "actor": {"kind": "system"},
        "tool": {"name": "delegate", "ok": true, "input": input, "output": "done"}});
    fs::write(&path, format!("---\nid: {ID}\ntitle: Test\ncreated: 1\nupdated: 1\n{legacy}---\n\
        {ENTRY}{meta} -->\n- System called `delegate` (success)\n\n"))?;
    Ok(path)
}

fn full_delegate_input() -> serde_json::Value {
    let ServerEvent::ToolCall { input, .. } = delegate_call(JOB, true) else { unreachable!() };
    input
}

fn rename_and_dismiss(fixture: &Fixture, path: &Path) -> Result<()> {
    let hub = fixture.hub();
    hub.rename_delegate(ID, JOB, "Kept label".into(), None, "user")?;
    hub.dismiss_delegate(ID, JOB)?;
    // Another save writes the same checkpoint again.
    hub.edit_threads(|store| store.rename(ID, "Again"))?;
    let input = saved_delegate(path, JOB)?;
    assert_eq!(input["label"], "Kept label", "{input}");
    assert_eq!(input["dismissed_from_panel"], true);
    assert_eq!(input["rename_history"].as_array().unwrap().len(), 1);
    drop(hub);
    let hub = fixture.hub();
    hub.edit_threads(|store| store.rename(ID, "After restart"))?;
    let input = saved_delegate(path, JOB)?;
    assert_eq!(input["label"], "Kept label");
    assert_eq!(input["dismissed_from_panel"], true);
    assert_eq!(input["rename_history"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn delegate_edits_survive_writing_through_an_inline_input() -> Result<()> {
    let fixture = Fixture::new()?;
    let input = full_delegate_input();
    let path = write_unconverted(&fixture, None, &input)?;
    rename_and_dismiss(&fixture, &path)?;
    assert_eq!(saved_delegate(&path, JOB)?["prompt"], input["prompt"], "the full input stays in the checkpoint");
    Ok(())
}

#[test]
fn delegate_edits_survive_writing_through_journal_inputs() -> Result<()> {
    for legacy in [false, true] {
        let fixture = Fixture::new()?;
        let session = if legacy { "tab-synthetic" } else { ID };
        let input = full_delegate_input();
        let path = write_unconverted(&fixture, legacy.then_some(session), &input)?;
        let mut writer = state::TimelineWriter::open(&fixture.state)?;
        let mut event = delegate_call(JOB, true);
        if let ServerEvent::ToolCall { session: owner, .. } = &mut event {
            *owner = session.into();
        }
        writer.append(&Envelope { workspace: None, seq: 1, event });
        drop(writer);
        rename_and_dismiss(&fixture, &path).with_context(|| format!("legacy={legacy}"))?;
        assert_eq!(saved_delegate(&path, JOB)?["prompt"], input["prompt"]);
    }
    Ok(())
}

#[test]
fn sidecar_conversion_at_save_keeps_delegate_edits() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = write_unconverted(&fixture, None, &full_delegate_input())?;
    let text = fs::read_to_string(&path)?.replace("title: Test\n", "title: Test\npreview_payloads: true\n");
    fs::write(&path, text)?;
    let hub = fixture.hub();
    hub.rename_delegate(ID, JOB, "Kept label".into(), None, "user")?;
    let stored = hub.tool_detail(ID, JOB).unwrap().input()?.unwrap();
    assert!(saved_delegate(&path, JOB)?["preview"].is_string());
    assert_eq!(stored["label"], "Kept label", "{stored}");
    Ok(())
}

#[test]
fn rename_history_too_large_for_the_preview_is_not_truncated() {
    let change = |at: i64| serde_json::json!({"at_ms": at, "label": format!("label {at}"), "label_source": null});
    let edited = serde_json::json!({"label": "label 3", "label_source": null, "rename_history": [change(3)]});
    let mut full = serde_json::json!({"label": "label 2", "rename_history": [change(1), change(2)]});
    carry_delegate_edits(&edited, &mut full);
    assert_eq!(full["rename_history"], serde_json::json!([change(1), change(2), change(3)]));
    assert_eq!(full["label"], "label 3");
}

#[tokio::test]
async fn trash_saves_and_joins_before_moving_and_restores_everything() -> Result<()> {
    for segmented in [false, true] {
        for loaded in [false, true] {
            let (fixture, hub, path, segments) = prepared(segmented)?;
            let kept = stamps(&segments)?;
            // These updates reach memory and the journal, but not the Markdown.
            publish(&hub, [result("t2", true, "unsaved sealed result"), text("late", "unsaved late reply")]);
            let stale = fs::read_to_string(&path)?;
            assert!(!stale.contains("unsaved"));
            drop(hub);
            let hub = fixture.hub();
            if loaded {
                memory_body(&hub, ID)?;
            }
            let app = Arc::new(crate::Workspace::new(
                fixture.project.clone(), String::new(), fixture.state.clone(), Arc::new(crate::Hub::new())));
            let expected = memory_body(&app.hub, ID)?;
            let entries = app.hub.history_cursor(ID);
            let locale = fuzukue_i18n::Locale::JAPANESE;
            crate::trash::request(&app, TrashAction::Delete { target: TrashTarget::Thread { id: ID.into() } }, locale).await?;
            assert!(!path.exists() && !app.hub.has_thread(ID));
            assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
            assert_eq!(stamps(&segments)?, kept);
            let ServerEvent::TrashItems { items, .. } = crate::trash::request(&app, TrashAction::List, locale).await? else {
                anyhow::bail!("no trash list");
            };
            let (_, mut events) = app.hub.subscribe(u64::MAX);
            crate::trash::request(&app, TrashAction::Restore { id: items[0].id.clone() }, locale).await?;
            assert!(read_front(&mut BufReader::new(File::open(&path)?))?.1.is_none(), "single-file format");
            assert_eq!(app.hub.history_cursor(ID), entries);
            assert_eq!(normalized(&memory_body(&app.hub, ID)?), normalized(&expected));
            assert!(expected.contains("unsaved late reply") && disk_body(&path)?.contains("unsaved sealed result"));
            let restored = loop {
                if let ServerEvent::ThreadRestored { events, .. } = events.try_recv()?.event {
                    break events;
                }
            };
            for wanted in ["First question", "unsaved sealed result", "unsaved late reply"] {
                assert!(serde_json::to_string(&restored)?.contains(wanted), "{wanted}");
            }
        }
    }
    Ok(())
}

#[test]
fn a_thread_that_cannot_be_saved_or_joined_stays_out_of_the_trash() -> Result<()> {
    let (fixture, hub, path, segments) = prepared(true)?;
    let app = crate::Workspace::new(
        fixture.project.clone(), String::new(), fixture.state.clone(), Arc::new(crate::Hub::new()));
    drop(hub);
    let trash = crate::trash::Store::new(&fixture.state);
    let grant_dir = fixture.dir.path().join("grant");
    fs::create_dir_all(&grant_dir)?;
    app.hub.add_folder_grant(ID, &grant_dir, fuzukue_proto::FolderGrantExpiry::NoExpiry, "desktop")?;
    let latest = fs::read(&path)?;
    let bytes = fs::read(&segments[0])?;
    fs::remove_file(&segments[0])?;
    let error = app.hub.trash_thread(&app, &trash, ID).unwrap_err();
    assert!(format!("{error:#}").contains("retry"));
    assert_eq!(app.hub.list_folder_grants(ID, None)?.0.len(), 1);
    assert_eq!(fs::read(&path)?, latest);
    assert!(app.hub.has_thread(ID));
    fs::write(&segments[0], bytes)?;
    app.hub.trim_idle_transcripts(Duration::ZERO);
    // A save that works followed by a join that cannot keep the old generation.
    let kept = fixture.threads().join(fuzukue_proto::THREAD_SAVE_DIR).join("segmentation");
    fs::write(&kept, "obstruct the join")?;
    let error = app.hub.trash_thread(&app, &trash, ID).unwrap_err();
    assert_eq!(app.hub.list_folder_grants(ID, None)?.0.len(), 1);
    assert!(format!("{error:#}").contains("could not join"), "{error:#}");
    assert!(path.exists() && app.hub.has_thread(ID));
    assert!(app.hub.read_threads(|store| store.is_segmented(ID)));
    fs::remove_file(kept)?;
    app.hub.trash_thread(&app, &trash, ID)?;
    assert!(!path.exists());
    Ok(())
}

fn replace_once(path: &Path, from: &str, to: &str) -> Result<()> {
    let text = fs::read_to_string(path)?;
    assert!(text.contains(from), "{from}");
    Ok(fs::write(path, text.replacen(from, to, 1))?)
}

/// Rewrite one segment and update the list so only the intended check fails.
fn reseal(latest: &Path, segment: &Path, from: &str, to: &str) -> Result<()> {
    let old = fs::read_to_string(segment)?;
    replace_once(segment, from, to)?;
    let new = fs::read_to_string(segment)?;
    let (old_sha, new_sha) = (format!("{:x}", Sha256::digest(&old)), format!("{:x}", Sha256::digest(&new)));
    let number = segment.file_name().unwrap().to_str().unwrap()[..6].parse()?;
    let renamed = segment.with_file_name(segments::file_name(number, &new_sha).unwrap());
    fs::rename(segment, &renamed)?;
    replace_once(latest, &old_sha[..16], &new_sha[..16])?;
    replace_once(latest, &old_sha, &new_sha)?;
    replace_once(latest, &format!("bytes: {}", old.len()), &format!("bytes: {}", new.len()))
}

#[test]
fn damaged_segments_stop_writes_and_repair_recovers_from_the_journal() -> Result<()> {
    type Damage = fn(&Path, &[PathBuf]) -> Result<()>;
    let cases: [(&str, Damage, &str); 14] = [
        ("missing segment", |_, segments| Ok(fs::remove_file(&segments[1])?), "No such file"),
        ("short segment", |_, segments| {
            let bytes = fs::read(&segments[0])?;
            Ok(fs::write(&segments[0], &bytes[..bytes.len() - 1])?)
        }, "length"),
        ("changed content", |_, segments| replace_once(&segments[0], "First question", "First questioN"), "digest"),
        ("entry count", |latest, _| replace_once(latest, "entries: 4", "entries: 5"), "entry count"),
        ("another thread", |latest, segments| reseal(latest, &segments[0], ID, "thread-segment-else"), "another thread"),
        ("segment number", |latest, segments| reseal(latest, &segments[1], "segment: 2", "segment: 3"), "segment number"),
        ("list order", |latest, segments| {
            let name = |path: &Path| path.file_name().unwrap().to_str().unwrap().to_owned();
            replace_once(latest, &name(&segments[0]), &name(&segments[0]).replacen("000001", "000002", 1))
        }, "out of order"),
        ("override range", |latest, _| replace_once(latest, "overrides: []", "overrides:\n- 99"), "override"),
        ("override order", |latest, _| replace_once(latest, "overrides: []", "overrides:\n- 2\n- 1"), "override positions"),
        ("override identity", |latest, _| replace_once(latest, "overrides: []", "overrides:\n- 0"), "does not match"),
        ("duplicate across files", |latest, _| replace_once(latest, "\"item\":\"r2\"", "\"item\":\"u1\""), "both a segment"),
        ("duplicate across segments", |latest, segments| {
            reseal(latest, &segments[1], "\"item\":\"u2\"", "\"item\":\"u1\"")
        }, "more than one segment"),
        ("empty list", |latest, _| {
            let text = fs::read_to_string(latest)?;
            let (start, end) = (text.find("segments:\n").unwrap(), text.find("overrides:").unwrap());
            Ok(fs::write(latest, format!("{}segments: []\n{}", &text[..start], &text[end..]))?)
        }, "no segments"),
        ("linked directory", |_, segments| {
            let dir = segments[0].parent().unwrap();
            fs::rename(dir, dir.with_extension("aside"))?;
            Ok(std::os::unix::fs::symlink(dir.with_extension("aside"), dir)?)
        }, "invalid thread segment directory"),
    ];
    for (name, damage, expected) in cases {
        let (fixture, hub, path, segments) = prepared(true)?;
        drop(hub);
        let latest = fs::read(&path)?;
        let originals = stamps(&segments)?;
        damage(&path, &segments).with_context(|| name)?;
        let damaged = fs::read(&path)?;
        let hub = fixture.hub();
        if name == "empty list" {
            // The front matter itself is refused, so the file is reported at startup.
            assert!(!hub.has_thread(ID), "{name}");
            assert!(hub.read_threads(|store| store.damaged_files.iter().any(|file| file.contains(expected))), "{name}");
        } else {
            publish(&hub, [prompt("after", "Sent while damaged"), result("t2", true, "result while damaged"), turn_end()]);
            let damage = hub.read_threads(|store| store.thread_damage(ID));
            assert!(matches!(&damage, Some(ServerEvent::Error { message }) if message.contains(expected)),
                "{name}: {damage:?}");
            assert!(hub.edit_threads(|store| store.rename(ID, "Refused")).is_err(), "{name}");
            assert!(hub.edit_threads(|store| store.set_pinned(ID, true)).is_err(), "{name}");
            assert!(hub.deliver_system_note(ID, "note", "refused".into()).is_err(), "{name}");
            assert!(hub.rename_delegate(ID, JOB, "refused".into(), None, "user").is_err(), "{name}");
            hub.recover_thread_metadata(None);
            // Neither shutdown nor a self-update waits on the unwritable thread.
            assert!(!hub.read_threads(|store| store.threads[ID].transcript.dirty.get()), "{name}");
            hub.flush_dirty_transcripts(false)?;
        }
        assert_eq!(fs::read(&path)?, damaged, "{name}: the latest file was written");
        drop(hub);

        // Repair the files.
        let dir = segments[0].parent().unwrap();
        if fs::symlink_metadata(dir)?.is_symlink() {
            fs::remove_file(dir)?;
            fs::rename(dir.with_extension("aside"), dir)?;
        }
        for (path, (_, bytes)) in segments.iter().zip(&originals) {
            fs::write(path, bytes)?;
        }
        fs::write(&path, &latest)?;
        let hub = fixture.hub();
        if name != "empty list" {
            let body = memory_body(&hub, ID).with_context(|| name)?;
            assert!(body.contains("Sent while damaged") && body.contains("result while damaged"), "{name}");
            hub.edit_threads(|store| store.rename(ID, "Repaired"))?;
            assert!(disk_body(&path)?.contains("result while damaged"), "{name}");
        }
    }
    Ok(())
}

#[test]
fn a_segment_repaired_while_running_is_read_again() -> Result<()> {
    let (_fixture, hub, path, segments) = prepared(true)?;
    let bytes = fs::read(&segments[0])?;
    fs::remove_file(&segments[0])?;
    publish(&hub, [prompt("after", "Sent while missing")]);
    assert!(hub.read_threads(|store| store.thread_damage(ID)).is_some());
    fs::write(&segments[0], bytes)?;
    publish(&hub, [turn_end()]);
    hub.edit_threads(|store| {
        store.threads.get_mut(ID).unwrap().active = false;
        Ok(())
    })?;
    hub.trim_idle_transcripts(Duration::ZERO);
    assert!(memory_body(&hub, ID)?.contains("Sent while missing"));
    hub.edit_threads(|store| store.rename(ID, "Repaired"))?;
    assert!(disk_body(&path)?.contains("Sent while missing"));
    Ok(())
}

#[test]
fn a_segment_changed_after_loading_blocks_saving() -> Result<()> {
    let (_fixture, hub, path, segments) = prepared(true)?;
    memory_body(&hub, ID)?;
    let latest = fs::read(&path)?;
    let bytes = fs::read(&segments[0])?;
    replace_once(&segments[0], "First question", "First questioN")?;
    assert!(hub.edit_threads(|store| store.rename(ID, "Refused")).is_err());
    assert_eq!(fs::read(&path)?, latest);
    // The same bytes in a new file are accepted again.
    fs::remove_file(&segments[0])?;
    fs::write(&segments[0], bytes)?;
    hub.edit_threads(|store| store.rename(ID, "Accepted"))?;
    Ok(())
}

#[test]
fn two_files_with_one_id_are_reported_and_neither_is_written() -> Result<()> {
    for segmented in [false, true] {
        let (fixture, hub, path, _) = prepared(segmented)?;
        drop(hub);
        let copy = fixture.threads().join("zz-copy.md");
        fs::copy(&path, &copy)?;
        let (first, second) = (fs::read(&path)?, fs::read(&copy)?);
        let hub = fixture.hub();
        assert!(hub.read_threads(|store| store.damage_notices().any(|event|
            matches!(event, ServerEvent::Error { message } if message.contains("two thread files")))));
        publish(&hub, [prompt("after", "Sent while ambiguous"), turn_end()]);
        assert!(hub.edit_threads(|store| store.rename(ID, "Refused")).is_err());
        assert!(hub.edit_threads(|store| store.create_with_id(ID, None, None)).is_err());
        hub.flush_dirty_transcripts(false)?;
        assert_eq!((fs::read(&path)?, fs::read(&copy)?), (first, second));
        assert_eq!(fs::read_dir(fixture.threads())?.filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "md")).count(), 2);
        drop(hub);
        fs::remove_file(copy)?;
        let hub = fixture.hub();
        assert!(memory_body(&hub, ID)?.contains("Sent while ambiguous"));
    }
    Ok(())
}

#[tokio::test]
async fn join_restores_the_single_file_bytes_and_keeps_the_old_generation() -> Result<()> {
    let (fixture, hub, path, _) = prepared(false)?;
    memory_body(&hub, ID)?;
    hub.edit_threads(|store| store.threads.get_mut(ID).unwrap().save("test"))?;
    drop(hub);
    let original = fs::read(&path)?;
    let segments = split(&path, SEALED)?;
    let kept = stamps(&segments)?;
    let segmented = fs::read(&path)?;
    let hub = fixture.hub();
    let marked = rollback_marker::listed(&fixture.marker_root())?;
    assert_eq!(marked.iter().filter_map(|entry| entry.thread.as_deref()).collect::<Vec<_>>(), [ID]);
    assert_eq!(hub.read_threads(|store| store.segmented_threads().count()), 1);
    hub.join_thread(ID).await?;
    assert_eq!(fs::read(&path)?, original);
    assert!(rollback_marker::listed(&fixture.marker_root())?.is_empty());
    assert_eq!(hub.read_threads(|store| store.segmented_threads().count()), 0);
    assert_eq!(stamps(&segments)?, kept);
    let generation = fixture.threads().join(fuzukue_proto::THREAD_SAVE_DIR).join("segmentation")
        .join(format!("{:x}.original", Sha256::digest(&segmented)));
    assert_eq!(fs::read(generation)?, segmented);
    assert!(hub.join_thread(ID).await.is_err(), "already a single file");
    hub.edit_threads(|store| store.rename(ID, "After join"))?;
    assert!(disk_body(&path)?.contains("First question"));
    Ok(())
}

#[tokio::test]
async fn join_includes_overrides_and_unsaved_changes() -> Result<()> {
    let (fixture, hub, path, _) = prepared(true)?;
    publish(&hub, [result("t2", true, "sealed result before join"), turn_end(), text("late", "unsaved before join")]);
    hub.edit_threads(|store| {
        store.threads.get_mut(ID).unwrap().active = false;
        Ok(())
    })?;
    let expected = memory_body(&hub, ID)?;
    hub.join_thread(ID).await?;
    assert_eq!(disk_body(&path)?, expected);
    drop(hub);
    assert_eq!(memory_body(&fixture.hub(), ID)?, expected);
    Ok(())
}

#[test]
fn join_refuses_to_publish_over_a_later_change() -> Result<()> {
    for external in [false, true] {
        let (fixture, hub, path, _) = prepared(true)?;
        let candidate = hub.edit_threads(|store| store.prepare_join(ID))?;
        let mut staged = candidate.stage()?;
        if external {
            let mut file = File::options().append(true).open(&path)?;
            file.write_all(format!("{ENTRY}{{\"item\":\"external\",\"actor\":{{\"kind\":\"user\"}}}} -->\n## User\n\nx\n\n").as_bytes())?;
        } else {
            publish(&hub, [text("late", "arrived during staging")]);
        }
        let current = fs::read(&path)?;
        assert!(hub.edit_threads(|store| store.publish_join(&mut staged)).is_err());
        assert_eq!(fs::read(&path)?, current);
        assert!(!rollback_marker::listed(&fixture.marker_root())?.is_empty());
        assert!(hub.read_threads(|store| store.is_segmented(ID)));
    }
    Ok(())
}

#[test]
fn marks_are_released_only_for_confirmed_single_files() -> Result<()> {
    let (fixture, hub, path, _) = prepared(false)?;
    drop(hub);
    let root = fixture.marker_root();
    let file = path.file_name().unwrap().to_str().unwrap();
    rollback_marker::add(&root, &fixture.project, Some(ID), file)?;
    rollback_marker::add(&fixture.marker_root(), &fixture.project, Some("thread-missing"), "missing.md")?;
    rollback_marker::add(&fixture.marker_root(), Path::new("/elsewhere"), Some(ID), file)?;
    drop(fixture.hub());
    let left = rollback_marker::listed(&root)?;
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().all(|entry| entry.thread.as_deref() == Some("thread-missing") || entry.workspace == "/elsewhere"));

    // A segmented file that cannot be read is listed by its name.
    fs::write(fixture.threads().join("broken.md"), "---\nformat: segmented-9\nthread_id: thread-broken\n---\n")?;
    let hub = fixture.hub();
    assert!(rollback_marker::listed(&root)?.iter().any(|entry| entry.thread.is_none() && entry.file == "broken.md"));
    assert!(hub.read_threads(|store| store.damaged_files.iter().any(|file| file.contains("unsupported thread format"))));

    // An unreadable list is an error, and it is never overwritten.
    fs::write(root.join("segmented-threads.json"), "not json")?;
    assert!(rollback_marker::listed(&root).is_err());
    assert!(rollback_marker::add(&root, &fixture.project, Some(ID), file).is_err());
    drop(fixture.hub());
    assert_eq!(fs::read_to_string(root.join("segmented-threads.json"))?, "not json");
    Ok(())
}

#[test]
fn a_build_without_this_format_cannot_parse_the_latest_front_matter() -> Result<()> {
    let (_fixture, _hub, path, _) = prepared(true)?;
    let text = fs::read_to_string(&path)?;
    let (yaml, _) = fuzukue_cards::split_frontmatter(&text).unwrap();
    // Builds before this format deserialize the whole front matter, which requires `id`.
    let Err(error) = serde_yaml::from_str::<Front>(yaml) else { anyhow::bail!("parsed as a single file") };
    assert!(error.to_string().contains("missing field `id`"), "{error}");
    assert!(Thread::read(path).is_err(), "a restore never opens segments");
    Ok(())
}

/// A workspace with one segmented thread, for tests outside this module.
pub(crate) fn segmented_workspace(project: &Path, hub: &crate::hub::Hub, state: &Path) -> Result<String> {
    segmented_workspace_with_root(project, hub, state, &rollback_marker::root(state))
}

pub(crate) fn segmented_workspace_with_root(project: &Path, hub: &crate::hub::Hub, state: &Path, root: &Path) -> Result<String> {
    let id = hub.edit_threads(|store| store.create(None))?;
    hub.publish(ServerEvent::UserPrompt {
        session: id.clone(), item: "synthetic-first".into(), text: "Synthetic".into(),
        paths: Vec::new(), to_worker: false,
    });
    hub.publish(ServerEvent::TurnEnded { session: id.clone(), actor: Actor::Claude });
    let path = hub.thread_path(&id)?;
    split(&path, &[1])?;
    hub.load_threads_with_root(project, state, root);
    Ok(id)
}

/// Start an older server on a synthetic segmented thread. It has to report the
/// file as unreadable and leave every thread file as it was. Run with
/// `FUZUKUE_OLD_SERVER_BINARY=<binary> cargo test -p secretaire old_server -- --ignored`
#[test]
#[ignore = "needs an older server binary in FUZUKUE_OLD_SERVER_BINARY"]
fn old_server_leaves_a_segmented_thread_alone() -> Result<()> {
    use std::process::{Command, Stdio};
    let binary = std::env::var_os("FUZUKUE_OLD_SERVER_BINARY").context("set FUZUKUE_OLD_SERVER_BINARY")?;
    assert!(!crate::self_update::drain::contains_marker(Path::new(&binary), rollback_marker::FORMAT_MARKER)?);
    let home = tempfile::tempdir()?;
    let home_dir = home.path().canonicalize()?;
    let project = home_dir.join("project");
    fs::create_dir_all(&project)?;
    let state = home_dir.join("fixture-state/workspace");
    let hub = Arc::new(crate::hub::Hub::with_timeline(&state));
    hub.load_threads(&project, &state);
    hub.edit_threads(|store| store.create_with_id(ID, None, None))?;
    publish(&hub, history());
    let path = hub.thread_path(ID)?;
    drop(hub);
    let mut files = split(&path, SEALED)?;
    files.push(path);
    let before = stamps(&files)?;
    // An empty held-work journal makes startup open the project without a client.
    let flat = project.to_string_lossy().trim_start_matches('/').replace('/', "%");
    let server_state = home_dir.join("xdg-state/fuzukue").join(flat);
    fs::create_dir_all(&server_state)?;
    fs::write(server_state.join("held-work.json"), "[]")?;
    let log = home_dir.join("server.log");
    let mut server = Command::new(binary)
        .arg(&project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home_dir)
        .env("XDG_STATE_HOME", home_dir.join("xdg-state"))
        .env("XDG_CONFIG_HOME", home_dir.join("xdg-config"))
        .env("FUZUKUE_ADDR", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(File::create(&log)?)
        .stderr(File::options().append(true).open(&log)?)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let reported = loop {
        let text = fs::read_to_string(&log)?;
        if text.contains("could not restore thread") || server.try_wait()?.is_some() || Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Let startup work finish, then request an ordinary shutdown with its flush.
    std::thread::sleep(Duration::from_secs(3));
    let running = server.try_wait()?.is_none();
    if running {
        // SAFETY: the child is unreaped, so its process ID still names it.
        unsafe { libc::kill(server.id() as libc::pid_t, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.try_wait()?.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if server.try_wait()?.is_none() {
        server.kill()?;
    }
    server.wait()?;
    println!("{}", fs::read_to_string(&log)?);
    assert!(running, "the server stopped early: {reported}");
    assert!(reported.contains("missing field `id`"), "{reported}");
    assert_eq!(stamps(&files)?, before);
    assert_eq!(fs::read_dir(project.join("threads"))?.filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "md")).count(), 1);
    Ok(())
}

#[test]
fn search_history_and_job_lists_match_the_single_file() -> Result<()> {
    let mut pair = Pair::new()?;
    pair.publish("job", &[result(JOB, true, "done"), turn_end()]);
    pair.restart("restart");
    let read = |hub: &crate::hub::Hub| -> Result<serde_json::Value> {
        let messages = hub.library_thread(ID, |_| true).unwrap().messages.unwrap().into_iter()
            .map(|message| (message.index, message.item, message.text)).collect::<Vec<_>>();
        hub.read_threads(|store| {
            let generation = store.history_generation(ID).unwrap();
            let end = store.history_cursor(ID) as u64;
            let workers = BTreeSet::from([JOB.to_owned()]);
            Ok(serde_json::json!({
                "messages": messages,
                "tail": store.history_tail(ID, &workers, false, |event| event),
                "compact": store.history_tail(ID, &workers, true, |event| event),
                "pages": [store.history_page(ID, end), store.history_page(ID, 3)],
                "from": store.history_from(ID, 2, generation).ok(),
                "jobs": store.job_summaries(ID),
                "details": store.job_details(ID, JOB, None),
                "targets": store.delegate_target("Synthetic", None).ok(),
                "requests": store.recent_requests(ID),
                "recovery": store.recovery_summary(ID),
            }))
        })
    };
    assert_eq!(read(&pair.segmented.1)?, read(&pair.single.1)?);
    Ok(())
}

#[tokio::test]
async fn joined_thread_can_save_a_newer_generation() -> Result<()> {
    let (_fixture, hub, path, _) = prepared(true)?;
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer_hub = hub.clone();
    let writer = std::thread::spawn(move || {
        start_rx.recv().unwrap();
        attempt_tx.send(()).unwrap();
        let result = writer_hub.edit_threads(|store| store.rename(ID, "New generation"));
        let _ = done_tx.send(());
        result
    });
    hub.join_thread_with(ID, move || {
        start_tx.send(()).unwrap();
        attempt_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }).await?;
    writer.join().unwrap()?;
    assert!(fs::read_to_string(path)?.contains("New generation"));
    assert!(!hub.read_threads(|store| store.is_segmented(ID)));
    Ok(())
}

#[test]
fn segment_declaration_requires_front_matter_and_segmented_format() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("candidate.md");
    for (text, expected) in [
        ("body\nformat: segmented-1\n---\n", false),
        ("---\nformat: single\n---\n", false),
        ("---\nformat: segmented-9\n---\n", true),
        ("---\ntitle: x\n---\nformat: segmented-9\n", false),
    ] {
        fs::write(&path, text)?;
        assert_eq!(declares_segments(&path), expected, "{text}");
    }
    Ok(())
}

#[test]
fn payload_conversion_skips_duplicate_thread_ids() -> Result<()> {
    let (fixture, hub, path, _) = prepared(false)?;
    drop(hub);
    replace_once(&path, "preview_payloads: true\n", "")?;
    let duplicate = fixture.threads().join("duplicate.md");
    fs::copy(&path, &duplicate)?;
    let store = Store::load(&fixture.project, &fixture.state, &Default::default());
    assert!(store.payload_candidate(Duration::ZERO, |_| None).0.is_none());
    Ok(())
}

#[test]
fn payload_conversion_leaves_segmented_threads_alone() -> Result<()> {
    let (_fixture, hub, path, _) = prepared(true)?;
    drop(hub);
    replace_once(&path, "preview_payloads: true\n", "")?;
    let store = Store::load(&_fixture.project, &_fixture.state, &Default::default());
    let (candidate, _) = store.payload_candidate(Duration::ZERO, |_| None);
    assert!(candidate.is_none());
    Ok(())
}

/// Cold-load time of a large segmented thread against the single file. The
/// synthetic record has the measured shape of a large thread: about 70 MB of
/// Markdown, 69,000 entries and a journal of 460,000 records and 430 MB.
/// Run with an optimized test profile:
/// `cargo test --release -p secretaire segmented_load_time -- --ignored --nocapture`
#[test]
#[ignore = "measurement: writes about 600 MB of synthetic data"]
fn segmented_load_time() -> Result<()> {
    const TURNS: usize = 23_000;
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.threads())?;
    let path = fixture.threads().join("large.md");
    let mut markdown = BufWriter::new(File::create(&path)?);
    write!(markdown, "---\nid: {ID}\ntitle: Large\ncreated: 1\nupdated: 1\npreview_payloads: true\n---\n")?;
    drop(state::TimelineWriter::open(&fixture.state)?);
    let mut lines = BufWriter::new(File::options().append(true).open(fixture.state.join("thread-timeline.jsonl"))?);
    let mut transcript = Transcript::default();
    let mut seq = 0;
    for turn in 0..TURNS {
        let mut events = vec![prompt(&format!("u{turn}"), &"synthetic question ".repeat(10))];
        // A reply arrives in several streaming records.
        events.extend((0..5).map(|_| text(&format!("r{turn}"), &"synthetic reply text ".repeat(11))));
        events.push(call(&format!("c{turn}"), serde_json::json!({"command": "x".repeat(500)})));
        events.push(result(&format!("c{turn}"), true, &"y".repeat(700)));
        // Most journal records leave no entry of their own.
        events.extend((0..11).map(|_| ServerEvent::DelegateProgress {
            session: ID.into(), job: "delegate:absent".into(), summary: "z".repeat(1200),
        }));
        events.push(turn_end());
        for event in events {
            seq += 1;
            transcript.observe(&event);
            serde_json::to_writer(&mut lines, &Envelope { workspace: None, seq, event })?;
            lines.write_all(b"\n")?;
        }
    }
    lines.flush()?;
    transcript.write_markdown(&mut markdown)?;
    markdown.flush()?;
    drop((markdown, transcript));
    let source = TranscriptSource {
        path: path.clone(), session: ID.into(), legacy: None, after: 0,
        timeline: state::Timeline::open(&fixture.state), conflict: None,
    };
    let measure = || -> Result<Duration> {
        let mut times = (0..5).map(|_| {
            let started = Instant::now();
            assert_eq!(source.read("measure")?.items.len(), TURNS * 3);
            Ok(started.elapsed())
        }).collect::<Result<Vec<_>>>()?;
        times.sort();
        Ok(times[2])
    };
    let bytes = path.metadata()?.len();
    measure()?;
    let single = measure()?;
    let sealed = TURNS * 3 * 94 / 100;
    split(&path, &[sealed / 6; 6])?;
    let segmented = measure()?;
    let journal = fixture.state.join("thread-timeline.jsonl").metadata()?.len();
    println!("markdown {bytes} bytes; journal {journal} bytes, {seq} records; single {single:?}; \
        segmented {segmented:?}; ratio {:.3}", segmented.as_secs_f64() / single.as_secs_f64());
    drop(fixture.dir);
    assert!(segmented.as_secs_f64() <= single.as_secs_f64() * 1.2);
    Ok(())
}
