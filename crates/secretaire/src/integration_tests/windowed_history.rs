use std::collections::BTreeSet;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use axum::{extract::ConnectInfo, Extension};
use futures_util::{SinkExt, StreamExt};
use fuzukue_proto::{CachedSession, ThreadHistoryFailure, PROTOCOL_VERSION, THREAD_HISTORY_PROTOCOL};
use tokio::{io::duplex, sync::mpsc};
use tokio_tungstenite::{client_async, tungstenite::Message};

use super::*;

const THREAD: &str = "thread-windowed-history";
const TURNS: u64 = 20_000;
const EVENTS: u64 = TURNS * 5;

fn write_history(project: &Path, state: &Path) -> Result<u64> {
    let threads = project.join("threads");
    fs::create_dir(&threads)?;
    let path = threads.join(format!("{THREAD}.md"));
    let mut transcript = BufWriter::new(fs::File::create(&path)?);
    let mut journal = BufWriter::new(fs::File::create(state.join("thread-timeline.jsonl"))?);
    write!(
        transcript,
        "---\nid: {THREAD}\ntitle: Synthetic history\ncreated: 0\nupdated: 0\n---\n"
    )?;
    let reply = "**Synthetic Markdown** with `inline code`.\n\n".repeat(23);
    let prompt = "How should we investigate this fixture? ".repeat(3);
    let output = "Synthetic tool output with a workspace path.\n".repeat(11);
    for turn in 0..TURNS {
        let user = format!("user:{turn}");
        let assistant = format!("reply:{turn}");
        let tool = format!("tool:{turn}");
        let user_meta = json!({"item": user, "actor": Actor::User});
        let assistant_meta = json!({"item": assistant, "actor": Actor::Claude});
        let tool_meta = json!({
            "item": tool, "actor": Actor::Claude, "tool": {"name": "Read", "ok": true},
            "turn_ends": [Actor::Claude],
        });
        write!(
            transcript,
            "<!-- labo-thread {user_meta} -->\n## User\n\n{prompt}\n\n"
        )?;
        write!(
            transcript,
            "<!-- labo-thread {assistant_meta} -->\n## Claude\n\n{reply}\n\n"
        )?;
        write!(
            transcript,
            "<!-- labo-thread {tool_meta} -->\n- Claude called `Read` (success)\n\n"
        )?;
        let session = || THREAD.to_owned();
        for (offset, event) in [
            ServerEvent::UserPrompt {
                session: session(),
                item: user,
                text: prompt.clone(),
                paths: vec![],
                to_worker: false,
            },
            ServerEvent::TextDelta {
                session: session(),
                item: assistant,
                actor: Actor::Claude,
                text: reply.clone(),
            },
            ServerEvent::ToolCall {
                session: session(),
                item: tool.clone(),
                actor: Actor::Claude,
                name: "Read".into(),
                input: json!({"file_path": "notes/synthetic.md"}),
            },
            ServerEvent::ToolResult {
                session: session(),
                item: tool,
                ok: true,
                output: output.clone(),
            },
            ServerEvent::TurnEnded {
                session: session(),
                actor: Actor::Claude,
            },
        ]
        .into_iter()
        .enumerate()
        {
            serde_json::to_writer(
                &mut journal,
                &Envelope {
                    workspace: None,
                    seq: turn * 5 + offset as u64 + 1,
                    event,
                },
            )?;
            writeln!(journal)?;
        }
    }
    transcript.flush()?;
    journal.flush()?;
    Ok(fs::metadata(path)?.len())
}

#[cfg(not(target_os = "linux"))]
fn rss_kib() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[tokio::test]
async fn large_thread_history_wire() -> Result<()> {
    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let markdown_bytes = write_history(project.path(), state.path())?;
    let journal_bytes = fs::metadata(state.path().join("thread-timeline.jsonl"))?.len();

    let before_rss = rss_kib();
    let startup = Instant::now();
    let hub = Arc::new(Hub::with_timeline(state.path()));
    let hub_ms = startup.elapsed().as_millis();
    let hub_rss = rss_kib();
    let app = Arc::new(Workspace::new(
        project.path().canonicalize()?,
        "127.0.0.1:1234".into(),
        state.path().into(),
        hub,
    ));
    eprintln!(
        "{EVENTS} events, journal={journal_bytes} B, Markdown={markdown_bytes} B, \
         hub={hub_ms} ms, workspace={} ms total, RSS KiB before={before_rss:?} \
         after_hub={hub_rss:?} after_workspace={:?}",
        startup.elapsed().as_millis(),
        rss_kib(),
    );

    let (incoming, connections) = mpsc::channel(1);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let loopback = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access))
        .layer(Extension(ConnectInfo(loopback)));
    let server =
        tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        for (protocol, after, paging) in [
            (61, 0, true), // Even an opt-in is ignored below protocol 62.
            (62, 0, false), // Android receives the existing tail.
            (61, EVENTS - 10, false),
            (PROTOCOL_VERSION, EVENTS - 10, true),
            (PROTOCOL_VERSION, 1, true),
            (THREAD_HISTORY_PROTOCOL, 0, true),
        ] {
            let (client, peer) = duplex(64 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            let start = Instant::now();
            let hello = ClientMsg::Hello {
                protocol, thread_history: paging, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(), locale: Some("en".into()), resume_from: Some(after),
                workspace: None, platform: (!paging).then(|| "android".into()), build_id: None,
            };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            let welcome = socket.next().await.unwrap()?.into_text()?;
            let welcome_ms = start.elapsed().as_millis();
            let mut total_bytes = welcome.len();
            let mut total_frames = 1;
            let Envelope { event, .. } = serde_json::from_str(&welcome)?;
            assert!(matches!(event, ServerEvent::Welcome { protocol: actual, .. } if actual == protocol));
            let marker = format!("windowed-history-{protocol}-{after}");
            app.hub.publish_snapshot(ServerEvent::FileChanged { path: marker.clone() });
            let mut chat_bytes = 0;
            let mut chat_events = 0;
            let mut prompts = Vec::new();
            let mut replies = Vec::new();
            let mut open_tool = None;
            let mut last_seq = 0;
            let mut threads = None;
            let mut before = None;
            loop {
                let frame = socket.next().await.unwrap()?.into_text()?;
                let Envelope { event, seq, .. } = serde_json::from_str(&frame)?;
                if matches!(&event, ServerEvent::FileChanged { path } if path == &marker) {
                    break;
                }
                if let ServerEvent::ThreadHistoryStart { session, before: cursor, workers } = &event {
                    assert_eq!(session, THREAD);
                    assert!(workers.is_empty());
                    assert_eq!(seq, 0);
                    assert!(before.replace(*cursor).is_none());
                }
                total_frames += 1;
                total_bytes += frame.len();
                last_seq = last_seq.max(seq);
                if super::threads::history_session(&event) == Some(THREAD) {
                    chat_events += 1;
                    chat_bytes += frame.len();
                    match &event {
                        ServerEvent::UserPrompt { item, .. } => {
                            prompts.push(item.strip_prefix("user:").unwrap().parse::<u64>()?);
                        }
                        ServerEvent::TextDelta { item, actor: Actor::User, .. } => {
                            prompts.push(item.strip_prefix("user:").unwrap().parse::<u64>()?);
                        }
                        ServerEvent::TextDelta { item, actor: Actor::Claude, .. } => {
                            replies.push(item.strip_prefix("reply:").unwrap().parse::<u64>()?);
                        }
                        ServerEvent::ToolCall { item, .. } => {
                            assert!(open_tool.replace(item.clone()).is_none());
                        }
                        ServerEvent::ToolResult { item, output, .. } => {
                            assert_eq!(open_tool.take().as_deref(), Some(item.as_str()));
                            assert!(!output.is_empty());
                        }
                        _ => {}
                    }
                }
                if matches!(event, ServerEvent::Threads { .. }) {
                    threads = Some((frame.len(), start.elapsed().as_millis()));
                }
            }
            eprintln!(
                "protocol={protocol} after={after}: Welcome={} B at {welcome_ms} ms, \
                 Threads={threads:?} (B, ms), all={total_frames} frames/{total_bytes} B, \
                 transcript={chat_events} events/{chat_bytes} B, handshake={} ms, RSS KiB={:?}",
                welcome.len(), start.elapsed().as_millis(), rss_kib(),
            );
            assert_eq!(replies.last(), Some(&(TURNS - 1)));
            assert!(prompts.iter().copied().eq(prompts[0]..TURNS));
            assert!(replies.iter().copied().eq(replies[0]..TURNS));
            assert!(open_tool.is_none());
            if after == 0 {
                assert!(chat_events <= 430, "{chat_events} events");
                assert!(chat_bytes <= 600 * 1024, "{chat_bytes} bytes");
                assert!(total_frames <= 500, "{total_frames} frames");
                assert!(total_bytes <= 700 * 1024, "{total_bytes} bytes");
            } else {
                assert_eq!(chat_events as u64, (EVENTS - after).min(10_000));
            }
            assert_eq!(last_seq, EVENTS);
            assert!(threads.is_some());

            let request = ClientMsg::GetThreadArtifacts { session: THREAD.into() };
            socket.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
            let mut artifacts_bytes = 0;
            loop {
                let frame = socket.next().await.unwrap()?.into_text()?;
                let Envelope { event, .. } = serde_json::from_str(&frame)?;
                assert_ne!(super::threads::history_session(&event), Some(THREAD));
                if matches!(event, ServerEvent::ThreadArtifacts { session, .. } if session == THREAD) {
                    artifacts_bytes += frame.len();
                    break;
                }
            }
            eprintln!("protocol={protocol}: artifact request response={artifacts_bytes} B, switch history=0 B");
            assert_eq!(before.is_some(), paging && protocol >= THREAD_HISTORY_PROTOCOL && after == 0);
            if let Some(before) = before {
                assert_eq!(before, TURNS * 3 - 256);
                let next = before - 256;
                let live_item = "live:append";
                // Append during paging and send two requests with IDs out of numeric order.
                let scope = |request| ClientMsg::InWorkspace { id: app.id.clone(), request: Box::new(request) };
                for (request, before) in [(71, next), (13, before)] {
                    let request = scope(ClientMsg::GetThreadHistory { session: THREAD.into(), request, before, generation: None });
                    socket.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
                }
                app.hub.publish(ServerEvent::TextDelta {
                    session: THREAD.into(), item: live_item.into(), actor: Actor::User, text: "Live fixture".into(),
                });
                let mut pages = BTreeSet::new();
                let mut live = false;
                while pages.len() != 2 || !live {
                    let env: Envelope = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                    match env.event {
                        ServerEvent::TextDelta { item, .. } if item == live_item => {
                            assert_eq!(env.seq, EVENTS + 1);
                            live = true;
                        }
                        ServerEvent::ThreadHistoryPage { session, request, events, next_before, .. } => {
                            assert_eq!(env.seq, 0);
                            assert_eq!(env.workspace.as_deref(), Some(app.id.as_str()));
                            assert_eq!(session, THREAD);
                            let (range_end, expected_next) = match request {
                                71 => (next, next - 256),
                                13 => (before, next),
                                other => panic!("unexpected request {other}"),
                            };
                            assert_eq!(next_before, Some(expected_next));
                            let ordinals = events.into_iter().filter_map(|event| {
                                let (item, kind) = match event {
                                    ServerEvent::TextDelta { item, actor: Actor::User, .. } => (item, 0),
                                    ServerEvent::TextDelta { item, actor: Actor::Claude, .. } => (item, 1),
                                    ServerEvent::ToolCall { item, .. } => (item, 2),
                                    _ => return None,
                                };
                                let turn = item.split_once(':').unwrap().1.parse::<u64>().unwrap();
                                Some(turn * 3 + kind)
                            });
                            assert!(ordinals.eq(expected_next..range_end));
                            assert!(pages.insert(request));
                        }
                        _ => {}
                    }
                }

                app.hub.edit_threads(|store| store.set_archived(THREAD, true))?;
                for session in ["thread-missing", THREAD] {
                    let request = scope(ClientMsg::GetThreadHistory { session: session.into(), request: 91, before, generation: None });
                    socket.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
                    loop {
                        let env: Envelope = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                        if let ServerEvent::ThreadHistoryFailed { session: failed, request, .. } = env.event {
                            assert_eq!((failed.as_str(), request, env.seq), (session, 91, 0));
                            break;
                        }
                    }
                }
            }
            socket.close(None).await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}


#[tokio::test]
async fn selection_scoped_startup_and_reconnect() -> Result<()> {
    const THREADS: usize = 48;
    const ITEMS: usize = 384;
    const SELECTED: &str = "thread-startup-47";
    const OTHER: &str = "thread-startup-00";
    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let dir = project.path().join("threads");
    fs::create_dir(&dir)?;
    let text = "**Synthetic thread history** with `code` and several paragraphs.\n\n".repeat(12);
    for thread in 0..THREADS {
        let id = format!("thread-startup-{thread:02}");
        let mut file = BufWriter::new(fs::File::create(dir.join(format!("{id}.md")))?);
        writeln!(file, "---\nid: {id}\ntitle: Fixture {thread}\ncreated: 0\nupdated: {thread}\n---")?;
        for item in 0..ITEMS {
            let meta = json!({"item": format!("reply:{item}"), "actor": Actor::Claude});
            write!(file, "<!-- labo-thread {meta} -->\n## Claude\n\n{text}\n\n")?;
        }
        file.flush()?;
    }
    let hub = Arc::new(Hub::with_timeline(state.path()));
    let app = Arc::new(Workspace::new(project.path().canonicalize()?, "127.0.0.1:1234".into(), state.path().into(), hub));
    let (incoming, connections) = mpsc::channel(1);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access))
        .layer(Extension(ConnectInfo("127.0.0.1:1234".parse::<std::net::SocketAddr>()?)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });

    type Socket = tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>;
    async fn receive(socket: &mut Socket) -> Result<(Envelope, usize)> {
        let frame = socket.next().await.context("missing WebSocket frame")??.into_text()?;
        Ok((serde_json::from_str(&frame)?, frame.len()))
    }
    let connect = || async {
        let (client, peer) = duplex(64 * 1024);
        incoming.send(peer).await?;
        Ok::<_, anyhow::Error>(client_async("ws://localhost/ws", client).await?.0)
    };
    let start = |protocol: u32, scoped: bool, after: Option<u64>, epoch: Option<String>, cached: Vec<CachedSession>| {
        let workspace = app.id.clone();
        let connect = &connect;
        async move {
            let mut socket = connect().await?;
            let hello = json!({
                "type": "hello", "protocol": protocol, "thread_history": true,
                "selection_scoped_transcripts": scoped, "workspace": workspace,
                "resume_from": after, "epoch": epoch, "cached_sessions": cached,
            });
            let start = Instant::now();
            socket.send(Message::Text(hello.to_string().into())).await?;
            let (welcome, size) = receive(&mut socket).await?;
            assert!(matches!(welcome.event, ServerEvent::Welcome {
                protocol: agreed, selection_scoped_transcripts, ..
            } if agreed == protocol.min(PROTOCOL_VERSION) && selection_scoped_transcripts == scoped));
            let mut bytes = size;
            let mut frames = 1;
            let mut rows = 0;
            let mut sessions = BTreeSet::new();
            let mut activity = false;
            let mut waiting = false;
            let mut threads_seen = false;
            let marker = loop {
                let (env, size) = receive(&mut socket).await?;
                bytes += size;
                frames += 1;
                if let Some(session) = env.event.transcript_session() {
                    rows += 1;
                    sessions.insert(session.to_owned());
                }
                match env.event {
                    ServerEvent::ChatActivity { .. } => activity = true,
                    ServerEvent::ChatWaiting { .. } => waiting = true,
                    ServerEvent::Threads { threads } => {
                        assert_eq!(threads.len(), THREADS);
                        threads_seen = true;
                        if !scoped {
                            break None;
                        }
                    }
                    ServerEvent::ThreadHistoryStart { .. } => assert!(!scoped),
                    ServerEvent::ThreadSync { epoch, through, reset_sessions } => {
                        assert!(scoped && threads_seen && activity && waiting);
                        assert_eq!(env.workspace.as_deref(), Some(workspace.as_str()));
                        assert_eq!(env.seq, 0);
                        break Some((epoch, through, reset_sessions));
                    }
                    _ => {}
                }
            };
            Ok::<_, anyhow::Error>((socket, marker, bytes, frames, rows, sessions, start.elapsed()))
        }
    };
    let scope = |request| ClientMsg::InWorkspace { id: app.id.clone(), request: Box::new(request) };
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // Same workspace and server, using the pre-R-17 desktop subscription as the baseline.
        for sample in 1..=3 {
            let (mut legacy, _, bytes, frames, rows, sessions, elapsed) =
                start(63_u32, false, None::<u64>, None, vec![]).await?;
            assert_eq!(rows, THREADS * 257);
            assert_eq!(sessions.len(), THREADS);
            eprintln!("sample {sample}, legacy: {bytes} bytes, {frames} messages, {rows} transcript events, {:.2} ms", elapsed.as_secs_f64() * 1000.);

            let (mut scoped, marker, bytes, frames, rows, sessions, elapsed) =
                start(PROTOCOL_VERSION + 10, true, None, None, vec![]).await?;
            let (epoch, through, reset) = marker.unwrap();
            assert!(sessions.is_empty() && reset.is_empty());
            assert_eq!(rows, 0);
            eprintln!("sample {sample}, scoped: {bytes} bytes, {frames} messages, {rows} transcript events, {:.2} ms", elapsed.as_secs_f64() * 1000.);

            let requested = Instant::now();
            let request = scope(ClientMsg::GetThreadTail { session: SELECTED.into(), request: 7 });
            scoped.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
            let (generation, tail_through, before) = loop {
                let (env, size) = receive(&mut scoped).await?;
                assert!(env.event.transcript_session().is_none());
                if let ServerEvent::ThreadTail { session, request, generation, through, events, workers, next_before } = env.event {
                    assert_eq!((session.as_str(), request, env.seq), (SELECTED, 7, 0));
                    assert_eq!(env.workspace.as_deref(), Some(app.id.as_str()));
                    assert!(workers.is_empty());
                    assert_eq!(events.len(), 256);
                    assert!(events.iter().all(|event| event.transcript_session() == Some(SELECTED)));
                    let before = (ITEMS - 256) as u64;
                    assert_eq!(next_before, Some(before));
                    eprintln!("sample {sample}, selected tail: {size} bytes, 1 message, {} transcript events, {:.2} ms",
                        events.len(), requested.elapsed().as_secs_f64() * 1000.);
                    break (generation, through, before);
                }
            };
            assert_eq!(through, tail_through);
            legacy.send(Message::Text(r#"{"type":"list_workspaces"}"#.into())).await?;
            loop {
                let (env, _) = receive(&mut legacy).await?;
                assert!(!matches!(env.event, ServerEvent::ThreadTail { .. } | ServerEvent::ThreadSync { .. }));
                if matches!(env.event, ServerEvent::Workspaces { .. }) {
                    break;
                }
            }
            legacy.close(None).await?;
            if sample != 3 {
                scoped.close(None).await?;
                continue;
            }

            for (request, generation) in [(8, generation), (9, generation + 1)] {
                let request = scope(ClientMsg::GetThreadHistory { session: SELECTED.into(), request, before, generation: Some(generation) });
                scoped.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
            }
            loop {
                let (env, _) = receive(&mut scoped).await?;
                match env.event {
                    ServerEvent::ThreadHistoryPage { request: 8, generation: page_generation, through: page_through, events, next_before, .. } => {
                        assert_eq!(page_generation, Some(generation));
                        assert_eq!(page_through, Some(through));
                        assert_eq!(events.len(), before as usize);
                        assert_eq!(next_before, None);
                    }
                    ServerEvent::ThreadHistoryFailed { request: 9, reason: Some(ThreadHistoryFailure::Stale), .. } => break,
                    _ => {}
                }
            }
            scoped.close(None).await?;

            for session in [SELECTED, OTHER] {
                app.hub.publish(ServerEvent::TextDelta {
                    session: session.into(), item: format!("reply:{}", ITEMS - 1), actor: Actor::Claude,
                    text: "live segment".into(),
                });
            }
            let cache = || vec![CachedSession { session: SELECTED.into(), generation }];
            let (mut resumed, marker, _, _, rows, sessions, _) =
                start(THREAD_HISTORY_PROTOCOL, true, Some(through), Some(epoch.clone()), cache()).await?;
            assert_eq!(rows, 1);
            assert_eq!(sessions, BTreeSet::from([SELECTED.into()]));
            assert_eq!(marker, Some((epoch.clone(), through + 2, vec![])));
            resumed.close(None).await?;

            let (mut reset, marker, _, _, rows, _, _) =
                start(PROTOCOL_VERSION, true, Some(through + 2), Some(format!("{epoch}-old")), cache()).await?;
            assert_eq!(rows, 0);
            assert_eq!(marker, Some((epoch, through + 2, vec![SELECTED.into()])));
            reset.close(None).await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}
