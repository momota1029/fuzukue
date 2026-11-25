use super::{mcp_app, MemoryListener, TempProject};

use std::fs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::{body::{to_bytes, Body}, extract::ConnectInfo, http::{Request, StatusCode}, Extension, Router};
use futures_util::{SinkExt, StreamExt};
use fuzukue_proto::{
    CardContent, DeckCardPage, DeckDiagnosticPage, Envelope, ServerEvent, CARD_CONTENT_BYTES,
    DECK_PAGE_BYTES, LARGE_LEGACY_FRAME_PROTOCOL, LAZY_DECK_PROTOCOL,
};
use tokio::io::DuplexStream;
use tokio_tungstenite::{client_async_with_config, tungstenite::{protocol::{Role, WebSocketConfig}, Message}, WebSocketStream};
use tower::ServiceExt;

const LARGE_CARD: usize = 4 * 1024 * 1024;
const DUPLICATES: usize = 12_300;
const DECK: &str = "synthetic";

async fn get(router: &Router, uri: &str, budget: usize) -> Result<Vec<u8>> {
    let response = router.clone().oneshot(Request::builder().uri(uri).body(Body::empty())?).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), budget).await?;
    anyhow::ensure!(status == StatusCode::OK, "HTTP {status}: {}", String::from_utf8_lossy(&bytes));
    Ok(bytes.to_vec())
}

async fn send(socket: &mut WebSocketStream<DuplexStream>, value: serde_json::Value) -> Result<()> {
    socket.send(Message::Text(value.to_string().into())).await?;
    Ok(())
}

async fn receive(socket: &mut WebSocketStream<DuplexStream>) -> Result<(ServerEvent, usize)> {
    let frame = socket.next().await.context("WebSocket closed")??;
    let text = frame.to_text()?;
    let env: Envelope = serde_json::from_str(text)?;
    Ok((env.event, text.len()))
}

#[tokio::test]
async fn large_deck_opens_without_bodies_and_loads_one_large_editable_card() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(180), large_deck()).await?
}

async fn large_deck() -> Result<()> {
    let project = TempProject::new()?;
    let root = project.path().join(DECK);
    fs::create_dir_all(root.join("cards/notes"))?;
    fs::write(root.join("deck.toml"), "kind = 'paper'\n[build]\nauto = false\n")?;
    fs::write(root.join("structure.yaml"), "title: Synthetic paper\nsections:\n  - title: Checks\n    items: [c001, c002]\n")?;
    let large_body = format!("First [[c001]]. needleonlyinbody\n\n<!--{}-->\n", "x".repeat(LARGE_CARD));
    fs::write(root.join("source.txt"), "Fixture source.")?;
    fs::write(root.join("cards/c000.md"), format!("---\nid: c000\ntitle: Large card\nfiles: [source.txt]\n---\n{large_body}"))?;
    let ordinary = format!("<!--{}-->\n", "a".repeat(21_000));
    for index in 1..499 {
        let markdown = format!("---\nid: c{index:03}\nkind: lemma\n---\nSynthetic lemma {index}.\n{ordinary}");
        if index < 3 {
            let folder = root.join(format!("cards/c{index:03}"));
            fs::create_dir(&folder)?;
            fs::write(folder.join("card.md"), markdown)?;
            fs::write(folder.join("statement.tex"), format!("Synthetic lemma {index}.\n"))?;
        } else {
            fs::write(root.join(format!("cards/c{index:03}.md")), markdown)?;
        }
    }
    let duplicate = format!("duplicate-{}", "i".repeat(880));
    let note = format!("---\nid: {duplicate}\n---\nSynthetic note.\n");
    for index in 0..=DUPLICATES {
        fs::write(root.join(format!("cards/notes/n{index:05}.md")), &note)?;
    }

    // Reproduce the original full snapshot representation, including all diagnostics.
    let before = Instant::now();
    let baseline = crate::cards::snapshot_for_deck_with(project.path(), &root, |prefix, diagnostics| {
        diagnostics.iter().map(|diag| crate::cards::diagnostic_view(prefix, diag)).collect()
    });
    let ServerEvent::Cards { cards, diagnostics, .. } = &baseline else { unreachable!() };
    assert_eq!(cards.len(), 500);
    assert_eq!(diagnostics.len(), DUPLICATES);
    let baseline_bytes = serde_json::to_string(&crate::snapshot(baseline))?;
    let baseline_size = baseline_bytes.len();
    assert!((38_000_000..45_000_000).contains(&baseline_size), "{baseline_size}");
    let baseline_time = before.elapsed();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let write = tokio::spawn(async move { server.send(Message::Text(baseline_bytes.into())).await });
    assert!(matches!(client.next().await, Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_)))));
    write.abort();

    let app = mcp_app(&project);
    *app.deck.write().unwrap() = DECK.into();
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let router = crate::router(crate::App::fixture(app.clone()), std::sync::Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let (incoming, connections) = tokio::sync::mpsc::channel(2);
    let server = tokio::spawn({
        let router = router.clone();
        async move { axum::serve(MemoryListener(connections), router).await }
    });
    let connect = |protocol, platform, config| {
        let incoming = incoming.clone();
        async move {
            let (client, peer) = tokio::io::duplex(64 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async_with_config("ws://localhost/ws", client, config).await?;
            send(&mut socket, serde_json::json!({"type":"hello","protocol":protocol,"platform":platform})).await?;
            Ok::<_, anyhow::Error>(socket)
        }
    };

    let start = Instant::now();
    let mut socket = connect(LAZY_DECK_PROTOCOL, "linux-x86_64", None).await?;
    let (mut bytes, mut announced, mut document) = (0, false, false);
    while !announced || !document {
        let (event, size) = receive(&mut socket).await?;
        bytes += size;
        match event {
            ServerEvent::DeckCardsChanged { deck } if deck == DECK => announced = true,
            ServerEvent::Document { title, sections, .. } => {
                assert_eq!(title, "Synthetic paper");
                assert!(matches!(&sections[0].items[0], fuzukue_proto::DocItem::Card { id } if id == "c001"));
                document = true;
            }
            ServerEvent::Cards { .. } => anyhow::bail!("lazy peer received the legacy card bodies"),
            _ => {}
        }
    }

    let mut summaries = Vec::new();
    let mut query = format!("/deck/cards?deck={DECK}");
    let revision = loop {
        let body = get(&router, &query, DECK_PAGE_BYTES).await?;
        bytes += body.len();
        let page: DeckCardPage = serde_json::from_slice(&body)?;
        summaries.extend(page.cards);
        match page.next {
            Some(next) => query = format!("/deck/cards?deck={DECK}&revision={}&offset={next}", page.revision),
            None => break page.revision,
        }
    };
    let open_time = start.elapsed();
    assert_eq!(summaries.len(), 500);
    assert!(summaries.iter().all(|card| card.body.is_empty() && card.edit_body.is_none() && card.edit_proof.is_none()));
    let first = summaries.iter().find(|card| card.id == "c000").unwrap();
    assert!(first.links.iter().any(|(_, id)| id == "c001"));
    assert_eq!(first.files, ["source.txt"]);
    let second = summaries.iter().find(|card| card.id == "c001").unwrap();
    assert!(second.backlinks.iter().any(|(_, id)| id == "c000"));
    assert!(bytes < 250_000, "lazy deck open sent {bytes} bytes");

    let body = get(&router, &format!("/deck/card?deck={DECK}&revision={revision}&card=c000"), CARD_CONTENT_BYTES).await?;
    let card_bytes = body.len();
    let content: CardContent = serde_json::from_slice(&body)?;
    assert_eq!(content.body, large_body);
    let edit = content.edit_body.context("large card is editable")?;
    assert_eq!(edit.body, large_body);

    let tail = get(&router, &format!("/deck/diagnostics?deck={DECK}&revision={revision}&offset={}", DUPLICATES - 1), DECK_PAGE_BYTES).await?;
    let tail: DeckDiagnosticPage = serde_json::from_slice(&tail)?;
    assert_eq!(tail.total, DUPLICATES);
    assert_eq!(tail.errors, DUPLICATES);
    assert_eq!(tail.diagnostics.len(), 1);
    assert_eq!(tail.next, None);
    let first_page = get(&router, &format!("/deck/diagnostics?deck={DECK}&revision={revision}"), DECK_PAGE_BYTES).await?;
    let page: DeckDiagnosticPage = serde_json::from_slice(&first_page)?;
    assert_eq!(page.diagnostics.len(), 64);
    assert_eq!(page.next, Some(64));
    let per_card = get(&router, &format!("/deck/diagnostics?deck={DECK}&revision={revision}&card=c000"), DECK_PAGE_BYTES).await?;
    assert_eq!(serde_json::from_slice::<DeckDiagnosticPage>(&per_card)?.total, 0);

    send(&mut socket, serde_json::json!({"type":"search_library","request":19,"query":"needleonlyinbody","deck":DECK,"kinds":["card"],"limit":5})).await?;
    loop {
        if let ServerEvent::LibraryResults { request: 19, hits, .. } = receive(&mut socket).await?.0 {
            assert!(hits.iter().any(|hit| matches!(&hit.location, fuzukue_proto::LibraryLocation::Card { card } if card == "c000")), "{hits:?}");
            break;
        }
    }

    // A pre-fix peer gets a readable deck error; the Step 1 peer gets the bounded legacy snapshot.
    let mut v69 = connect(LARGE_LEGACY_FRAME_PROTOCOL - 1, "linux-x86_64", None).await?;
    loop {
        if let ServerEvent::Cards { cards, diagnostics, .. } = receive(&mut v69).await?.0 {
            assert!(cards.is_empty() && diagnostics[0].message.contains("size limit"));
            break;
        }
    }
    drop(v69);
    let config = WebSocketConfig::default().max_frame_size(Some(CARD_CONTENT_BYTES)).max_message_size(Some(CARD_CONTENT_BYTES));
    let step_start = Instant::now();
    let mut step = connect(LARGE_LEGACY_FRAME_PROTOCOL, "linux-x86_64", Some(config)).await?;
    let step1_size = loop {
        if let (ServerEvent::Cards { cards, diagnostics, .. }, size) = receive(&mut step).await? {
            assert_eq!(cards.len(), 500);
            assert_eq!(diagnostics.len(), 257);
            break size;
        }
    };
    let step1_time = step_start.elapsed();
    drop(step);
    let mut android = connect(67, "android", None).await?;
    send(&mut android, serde_json::json!({"type":"list_workspaces"})).await?;
    let mut lists = 0;
    while lists < 2 {
        match receive(&mut android).await?.0 {
            ServerEvent::Workspaces { .. } => lists += 1,
            ServerEvent::Cards { .. } | ServerEvent::Document { .. } | ServerEvent::DeckCardsChanged { .. } =>
                anyhow::bail!("Android received desktop-only deck data"),
            _ => {}
        }
    }

    let updated = format!("{large_body}\nEdited via the versioned card API.\n");
    send(&mut socket, serde_json::json!({"type":"save_card","request":20,"deck":DECK,
        "card":"c000","part":"body","base_hash":edit.base_hash,"body":updated})).await?;
    loop {
        if let ServerEvent::CardSaveResult { request: 20, outcome } = receive(&mut socket).await?.0 {
            assert!(matches!(outcome, fuzukue_proto::CardSaveOutcome::Saved), "{outcome:?}");
            break;
        }
    }
    assert!(fs::read_to_string(root.join("cards/c000.md"))?.ends_with(&updated));

    let source = get(&router, "/cards-for-file/synthetic/source.txt", DECK_PAGE_BYTES).await?;
    let source: serde_json::Value = serde_json::from_slice(&source)?;
    assert_eq!(source[0]["id"], "c000");
    let (_, tex) = crate::build::deck_tex(project.path(), DECK)?;
    assert!(tex.contains(r"\label{card:c001}") && tex.contains("cards/c001/statement.tex"));
    if crate::test_support::tex::ready("uplatex", &["latexmk", "dvipdfmx"])? {
        let built = crate::build::build(project.path(), DECK).await?;
        assert!(built.ok, "{}", built.log);
        assert!(fs::read(project.path().join(built.pdf))?.starts_with(b"%PDF-"));
        eprintln!("large deck: built the synthetic paper PDF");
    }

    eprintln!("large deck: baseline {baseline_size} bytes / {baseline_time:?} generation (default peer rejects); Step 1 {step1_size} bytes / {step1_time:?} to receive; lazy open {bytes} bytes / {open_time:?} including summaries and document; one 4 MiB card {card_bytes} HTTP bytes");
    server.abort();
    Ok(())
}

#[test]
fn overlapping_card_directories_do_not_report_the_same_file_as_a_duplicate() -> Result<()> {
    let project = TempProject::new()?;
    let root = project.path();
    fs::create_dir_all(root.join("cards/notes"))?;
    let card = "---\nid: repeated\n---\nSynthetic.\n";
    fs::write(root.join("cards/first.md"), card)?;
    fs::write(root.join("cards/notes/second.md"), card)?;
    fs::write(root.join("cards/notes/third.md"), card)?;
    let graph = fuzukue_cards::Graph::load(root, &["cards".into(), "cards/notes".into(), "cards".into()])?;
    assert_eq!(graph.cards.len(), 1);
    assert_eq!(graph.diagnostics.len(), 2);
    Ok(())
}

#[test]
fn oversized_snapshots_and_replies_degrade_inside_the_frame_budget() -> Result<()> {
    use fuzukue_proto::{HistoryOutcome, SymbolLocation, SymbolReference, SymbolResults};

    // Control characters expand in JSON. The budget must count encoded bytes.
    fn large() -> String { "\0".repeat(2 * 1024 * 1024 + 1) }
    let frame = |event, protocol| -> Result<(Envelope, usize)> {
        let mut env = crate::snapshot(event);
        env.seq = 10;
        let axum::extract::ws::Message::Text(text) =
            crate::wire_event(&env, protocol, Some("fixture"), fuzukue_i18n::Locale::JAPANESE)
        else { unreachable!() };
        let decoded: Envelope = serde_json::from_str(&text)?;
        assert_eq!(decoded.seq, 10);
        assert_eq!(decoded.workspace.as_deref(), Some("fixture"));
        Ok((decoded, text.len()))
    };
    let cases: [(&str, fn() -> ServerEvent); 7] = [
        ("Deck structure", || ServerEvent::Document { title: large(), sections: vec![], macros: vec![], references: None }),
        ("Thread history", || ServerEvent::ThreadTail { session: "session".into(), request: 1, generation: 1,
            through: 2, workers: vec![large()], events: vec![], next_before: None }),
        ("Thread history", || ServerEvent::ThreadHistoryPage { session: "session".into(), request: 2, next_before: Some(1),
            generation: Some(1), through: Some(2), events: vec![ServerEvent::Error { message: large() }] }),
        ("Thread history", || ServerEvent::ThreadRestored { session: "session".into(), events: vec![ServerEvent::Error { message: large() }] }),
        ("Graph lookup", || ServerEvent::SymbolResults { request: 3, results: SymbolResults {
            references: vec![SymbolReference { location: SymbolLocation { deck: "deck".into(), file: "file".into(),
                card: None, start: 0, end: 1, line: 1, end_line: None, snippet: large() }, alternates: vec![] }],
            ..Default::default()
        } }),
        ("Search response", || ServerEvent::LibraryResults { request: 4, decks: vec![large()], hits: vec![], more: false }),
        ("Search response", || ServerEvent::DeckSearchResults { query: large(), decks: vec![] }),
    ];
    for (label, event) in cases {
        let (env, size) = frame(event(), LAZY_DECK_PROTOCOL)?;
        assert!(size < 1024);
        assert!(matches!(env.event, ServerEvent::ResponseTooLarge { message } if message.starts_with(label)));
    }
    let (env, size) = frame(ServerEvent::HistoryResult { id: 5, outcome: HistoryOutcome::Diff { text: large(), truncated: false } }, LAZY_DECK_PROTOCOL)?;
    assert!(size < 1024);
    assert!(matches!(env.event, ServerEvent::HistoryResult { id: 5, outcome: HistoryOutcome::Error { message } } if message.contains("size limit")));

    let (env, size) = frame(ServerEvent::LspDocument {
        document: 6,
        update: fuzukue_proto::LspUpdate::Diagnostics { version: 1, diagnostics: vec![serde_json::Value::String(large())] },
    }, LAZY_DECK_PROTOCOL)?;
    assert!(size < 1024);
    assert!(matches!(env.event, ServerEvent::ResponseTooLarge { message } if message.starts_with("Diagnostics")));

    let (env, size) = frame(ServerEvent::Comments { deck: large(), items: vec![] }, LARGE_LEGACY_FRAME_PROTOCOL)?;
    assert!(size < 1024);
    assert!(matches!(env.event, ServerEvent::Error { message } if message.contains("size limit")));
    let exact = serde_json::to_vec("aβ\0")?;
    assert_eq!(crate::bounded_json(&"aβ\0", exact.len()), Some(exact.clone()));
    assert!(crate::bounded_json(&"aβ\0", exact.len() - 1).is_none());
    Ok(())
}
