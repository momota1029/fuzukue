use super::*;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    one: PathBuf,
    two: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let one = temp.path().join("workspace-a");
        let two = temp.path().join("workspace-b");
        for path in [&home, &one, &two] {
            fs::create_dir(path)?;
        }
        Ok(Self {
            _temp: temp,
            home,
            one,
            two,
        })
    }

    fn store<'a>(&'a self, workspace: &'a Path) -> Store<'a> {
        Store {
            home: &self.home,
            workspace,
        }
    }
}

fn save_args(scope: Scope, summary: &str, body: &str) -> Save {
    Save {
        scope,
        id: None,
        expected_revision: None,
        summary: summary.into(),
        body: body.into(),
        worker: false,
    }
}

fn save(store: &Store<'_>, scope: Scope, summary: &str, body: &str) -> Result<Saved> {
    store.save(save_args(scope, summary, body), Origin::User, || Ok(()))
}

fn item_path(root: &Path, id: &str) -> PathBuf {
    root.join(".fuzukue/memory/items").join(format!("{id}.md"))
}

#[test]
fn empty_save_cannot_erase_an_item() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    assert!(save(&store, Scope::Workspace, "summary", "").is_err());
    let saved = save(&store, Scope::Workspace, "summary", "original")?;
    let mut update = save_args(Scope::Workspace, "summary", " \n");
    update.id = Some(saved.id.clone());
    update.expected_revision = Some(saved.revision.clone());
    assert!(store.save(update, Origin::User, || Ok(())).is_err());
    assert_eq!(store.read(Scope::Workspace, saved.id)?.0.body, "original");
    Ok(())
}

#[test]
fn scopes_and_restart() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.store(&fixture.one);
    let global = save(&first, Scope::Global, "Synthetic preference", "Global only")?;
    let one = save(
        &first,
        Scope::Workspace,
        "Synthetic work A",
        "Workspace A only",
    )?;
    let other = fixture.store(&fixture.two);
    let two = save(
        &other,
        Scope::Workspace,
        "Synthetic work B",
        "Workspace B only",
    )?;

    // Store values retain no in-memory snapshots, indexes or revisions.
    let restarted = fixture.store(&fixture.one);
    let index = restarted.snapshot()?;
    let index2 = other.snapshot()?;
    assert_eq!(index.global, index2.global);
    assert!(index.global.contains(&global.id));
    assert!(index.workspace.contains(&one.id));
    assert!(!index.jsonl().contains(&two.id));
    assert!(index2.workspace.contains(&two.id));
    assert_ne!(index.revision, index2.revision);
    assert!(!index.jsonl().contains("Workspace A only"));
    assert_eq!(
        restarted.read(Scope::Workspace, one.id.clone())?.0.body,
        "Workspace A only"
    );
    assert!(restarted.read(Scope::Global, one.id.clone()).is_err());
    assert!(other.read(Scope::Workspace, one.id).is_err());
    Ok(())
}

fn seed(root: &Path, count: usize, summary: &str) -> Result<()> {
    let root = memory_dir(root, true)?.unwrap();
    let folder = directory(&root, "items", true, true)?.unwrap();
    for _ in 0..count {
        let now = "2026-11-28T00:00:00.123456789Z".parse()?;
        let metadata = Metadata {
            version: 1,
            id: new_id(),
            summary: summary.into(),
            worker: false,
            created_at: now,
            updated_at: now,
            origin: Origin::User,
            imported_from: None,
        };
        let file = format!("{}.md", metadata.id);
        let item = Item::new(metadata, "Synthetic seed")?;
        atomic_write(&folder, &file, item.raw.as_bytes(), || Ok(()), true)?;
    }
    Ok(())
}

#[test]
fn combined_item_limit_and_manual_overflow() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    seed(&fixture.home, 100, "G")?;
    seed(&fixture.one, 99, "W")?;
    save(&store, Scope::Workspace, "last", "synthetic")?;
    let at_limit = store.snapshot()?;
    assert_eq!(at_limit.jsonl().lines().count(), 200);
    assert!(
        save(&store, Scope::Global, "over", "synthetic")
            .err()
            .unwrap()
            .to_string()
            .contains("200 items")
    );
    assert_eq!(store.snapshot()?.revision, at_limit.revision);
    assert_eq!(
        fixture
            .store(&fixture.two)
            .snapshot()?
            .jsonl()
            .lines()
            .count(),
        100
    );
    seed(&fixture.one, 1, "Hand edit")?;
    assert!(
        store
            .snapshot()
            .unwrap_err()
            .to_string()
            .contains("200 items")
    );
    Ok(())
}

#[test]
fn combined_index_byte_limit_and_field_validation() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let summary = "界".repeat(160);
    seed(&fixture.home, 14, &summary)?;
    seed(&fixture.one, 38, &summary)?;
    let before = store.snapshot()?;
    assert!(before.global.len() < MAX_INDEX && before.workspace.len() < MAX_INDEX);
    assert!(before.jsonl().len() <= MAX_INDEX);
    let error = save(&store, Scope::Workspace, &summary, "synthetic")
        .err()
        .unwrap();
    assert!(error.to_string().contains("32 KiB"));
    assert_eq!(store.snapshot()?.revision, before.revision);
    assert!(save(&store, Scope::Global, "invalid\nsummary", "synthetic").is_err());
    assert!(
        save(
            &store,
            Scope::Global,
            "valid",
            &"界".repeat(MAX_BODY / 3 + 1)
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn dated_backup_recovery_and_index_regeneration() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let first = save(
        &store,
        Scope::Workspace,
        "Old synthetic summary",
        "Old synthetic body",
    )?;
    let path = item_path(&fixture.one, &first.id);
    let before = fs::read(&path)?;
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);

    let mut args = save_args(
        Scope::Workspace,
        "New synthetic summary",
        "New synthetic body",
    );
    args.id = Some(first.id.clone());
    args.expected_revision = Some("stale".into());
    assert!(store.save(args, Origin::Assistant, || Ok(())).is_err());
    assert_eq!(fs::read(&path)?, before);

    let mut args = save_args(
        Scope::Workspace,
        "New synthetic summary",
        "New synthetic body",
    );
    args.id = Some(first.id.clone());
    args.expected_revision = Some(first.revision.clone());
    let updated = store.save(args, Origin::Assistant, || Ok(()))?;
    let backup = updated.backup.unwrap();
    let (layer, filename) = backup.split_once('/').unwrap();
    assert!(layer.starts_with("workspace-"));
    assert!(filename.starts_with(&Utc::now().format("%Y-").to_string()));
    let backup = fixture.home.join(".fuzukue/memory/backups").join(backup);
    assert_eq!(
        fs::metadata(backup.parent().unwrap())?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::read(&backup)?, before);

    let restarted = fixture.store(&fixture.one);
    assert!(
        restarted
            .snapshot()?
            .workspace
            .contains("New synthetic summary")
    );
    // Simulate local recovery using the authoritative backup while the index is stale.
    fs::copy(backup, &path)?;
    assert!(
        restarted
            .snapshot()?
            .workspace
            .contains("Old synthetic summary")
    );
    assert_eq!(
        restarted.read(Scope::Workspace, first.id)?.0.revision,
        first.revision
    );
    assert!(
        fs::read_to_string(fixture.one.join(".fuzukue/memory/index.jsonl"))?
            .contains("Old synthetic summary")
    );
    Ok(())
}

#[test]
fn invalid_edits_and_link_escape_fail_closed() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let item = save(&store, Scope::Workspace, "Synthetic", "Synthetic body")?;
    let path = item_path(&fixture.one, &item.id);
    let original = fs::read_to_string(&path)?;
    let outside = fixture._temp.path().join("outside");
    fs::rename(&path, &outside)?;
    symlink(&outside, &path)?;
    assert!(store.snapshot().is_err());
    fs::remove_file(&path)?;
    fs::hard_link(&outside, &path)?;
    assert!(store.snapshot().is_err());
    fs::remove_file(&path)?;
    fs::rename(&outside, &path)?;
    fs::write(
        &path,
        "---\ninvalid: synthetic secret\n---\nsynthetic secret",
    )?;
    let error = format!("{:#}", store.snapshot().err().unwrap());
    assert!(error.contains(&item.id) && !error.contains("synthetic secret"));

    fs::write(&path, original)?;
    let global = memory_dir(&fixture.home, false)?.unwrap();
    let folder = directory(&global, "items", true, true)?.unwrap();
    let filename = format!("{}.md", item.id);
    atomic_write(&folder, &filename, &fs::read(&path)?, || Ok(()), true)?;
    assert!(format!("{:#}", store.snapshot().err().unwrap()).contains("duplicate memory ID"));
    Ok(())
}

#[test]
fn shared_operations_and_turn_attachments() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let args = json!({
        "scope": "workspace", "summary": "Synthetic index row", "body": "Synthetic body", "worker": true
    });
    assert!(store.dispatch(Tool::Save, args.clone(), None).is_err());

    let cancel = crate::cancellation::Cancellation::default();
    let saved = store.dispatch(Tool::Save, args, Some(&cancel))?;
    let list = store.dispatch(Tool::List, json!({}), None)?;
    assert!(
        list["index_jsonl"]
            .as_str()
            .unwrap()
            .contains("Synthetic index row")
    );
    let read = store.dispatch(
        Tool::Read,
        json!({
            "scope": "workspace", "id": saved["id"], "known_revision": "outdated", "known_snapshot": list["snapshot"]
        }),
        None,
    )?;
    assert_eq!(read["body"], "Synthetic body");
    assert_eq!(read["changed"], true);
    assert_eq!(read["snapshot_changed"], false);
    assert_eq!(read["metadata"]["worker"], true);
    let index = store.snapshot()?;
    let attached = desk::attachment(&index);
    let (snapshot, rows) = attached
        .split_once('\n')
        .unwrap()
        .1
        .split_once('\n')
        .unwrap();
    assert_eq!(
        snapshot,
        format!("Snapshot {}", list["snapshot"].as_str().unwrap())
    );
    assert_eq!(rows, list["index_jsonl"].as_str().unwrap());
    assert!(!attached.contains("Synthetic body"));

    cancel.cancel();
    assert!(store
        .dispatch(
            Tool::Save,
            json!({
                "scope": "global", "summary": "Cancelled", "body": "Synthetic body", "worker": false
            }),
            Some(&cancel)
        )
        .is_err());
    assert_eq!(store.snapshot()?.revision, index.revision);
    Ok(())
}

#[test]
fn worker_selection_only_delivers_marked_items_in_the_current_scopes() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let mut selected = save_args(Scope::Global, "Selected summary", "Selected worker body");
    selected.worker = true;
    let selected = store.save(selected, Origin::User, || Ok(()))?;
    let mut omitted = save_args(Scope::Workspace, "Omitted summary", "Omitted worker body");
    omitted.worker = true;
    store.save(omitted, Origin::User, || Ok(()))?;
    let private = save(&store, Scope::Workspace, "Desk only", "Private body")?;
    let other = save(
        &fixture.store(&fixture.two),
        Scope::Workspace,
        "Other",
        "Other body",
    )?;
    let reference = |scope: &str, id: &str| {
        serde_json::from_value::<WorkerItem>(json!({"scope": scope, "id": id}))
    };

    let attachment = store.worker_attachment(&[
        reference("global", &selected.id)?,
        reference("global", &selected.id)?,
    ])?;
    assert_eq!(attachment.matches("Selected worker body").count(), 1);
    assert!(!attachment.contains("Selected summary") && !attachment.contains(&selected.id));
    assert!(!attachment.contains("Omitted") && !attachment.contains("Private"));
    assert!(
        store
            .worker_attachment(&[reference("workspace", &private.id)?])
            .is_err()
    );
    assert!(
        store
            .worker_attachment(&[reference("workspace", &other.id)?])
            .is_err()
    );
    assert!(
        store
            .worker_attachment(&[reference("workspace", &selected.id)?])
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn fresh_index_is_attached_only_to_trusted_user_turns() -> Result<()> {
    use crate::front_desk::TurnKind;
    let fixture = Fixture::new()?;
    let app = crate::Workspace::new(
        fixture.one.clone(),
        String::new(),
        fixture.one.join("state"),
        std::sync::Arc::new(crate::Hub::new()),
    );
    let id = app.hub.edit_threads(|store| store.create(None))?;
    let (home, workspace) = desk::roots(&app)?;
    let store = Store {
        home: &home,
        workspace: &workspace,
    };
    let item = save(
        &store,
        Scope::Global,
        "Original synthetic summary",
        "Synthetic body",
    )?;
    let first = for_turn(&app, &id, TurnKind::UserConversation)
        .await?
        .unwrap();
    assert!(first.contains("Original synthetic summary") && !first.contains("Synthetic body"));

    let mut changed = save_args(
        Scope::Global,
        "Updated synthetic summary",
        "Edited synthetic body",
    );
    changed.id = Some(item.id);
    changed.expected_revision = Some(item.revision);
    store.save(changed, Origin::User, || Ok(()))?;
    let next = for_turn(&app, &id, TurnKind::Escalated).await?.unwrap();
    assert!(
        next.contains("Updated synthetic summary") && !next.contains("Original synthetic summary")
    );

    for kind in [
        TurnKind::ExternalInput,
        TurnKind::WorkerDigest,
        TurnKind::Nudge,
    ] {
        assert!(for_turn(&app, &id, kind).await?.is_none());
    }
    app.hub.publish_partner_turn_started(&id, None, false, None);
    app.hub.mark_external_approval_input(&id);
    assert!(
        for_turn(&app, &id, TurnKind::UserConversation)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn only_chat_confirmation_deletes_the_previewed_revision() -> Result<()> {
    use std::sync::Arc;

    let fixture = Fixture::new()?;
    let app = Arc::new(crate::Workspace::new(
        fixture.one.clone(),
        String::new(),
        fixture.one.join("state"),
        Arc::new(crate::Hub::new()),
    ));
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let other = app.hub.edit_threads(|store| store.create(None))?;
    let (home, workspace) = desk::roots(&app)?;
    let store = Store {
        home: &home,
        workspace: &workspace,
    };
    let item = save(&store, Scope::Workspace, "Synthetic note", "Before")?;
    let path = item_path(&workspace, &item.id);
    let cancel = Arc::new(crate::cancellation::Cancellation::default());
    let args = json!({"scope": "workspace", "id": item.id});
    let preview = Tool::DeletePreview
        .call(&app, &session, args.clone(), Some(cancel.clone()))
        .await?;
    let confirm = |preview: serde_json::Value| preview["confirmation"].as_str().unwrap().to_owned();
    let send = |session: String, text: String| {
        crate::handle(&app, fuzukue_proto::ClientMsg::Prompt { session, text })
    };
    let stale = confirm(preview);
    assert!(path.exists());
    send(other, stale.clone()).await?;
    assert!(path.exists());

    let mut update = save_args(Scope::Workspace, "Updated synthetic note", "After");
    update.id = Some(item.id.clone());
    update.expected_revision = Some(item.revision);
    store.save(update, Origin::User, || Ok(()))?;
    let before_delete = fs::read(&path)?;
    send(session.clone(), stale).await?;
    assert_eq!(fs::read(&path)?, before_delete);

    let new = Tool::DeletePreview
        .call(&app, &session, args, Some(cancel))
        .await?;
    let events = app.hub.subscribe(u64::MAX).1;
    send(session.clone(), confirm(new.clone())).await?;
    assert!(!path.exists());
    let mut events = events;
    let backup = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event.event {
            fuzukue_proto::ServerEvent::TextDelta {
                text,
                actor: fuzukue_proto::Actor::System,
                ..
            } => text
                .strip_prefix("Deleted one memory. Private backup: ")
                .map(str::to_owned),
            _ => None,
        })
        .context("missing deletion receipt")?;
    assert_eq!(
        fs::read(home.join(".fuzukue/memory").join(backup))?,
        before_delete
    );
    assert!(store.snapshot()?.workspace.is_empty());
    send(session, confirm(new)).await?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn import_preview_scopes_backup_staleness_and_retry() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(&fixture.one);
    let source = fixture._temp.path().join("claude-source");
    fs::create_dir(&source)?;
    let notes = "---\nname: Sample\n---\nFirst synthetic fact\nSecond synthetic fact\n";
    fs::write(source.join("notes.md"), notes)?;
    fs::write(source.join("MEMORY.md"), "[Notes](notes.md)\n")?;
    symlink(source.join("notes.md"), source.join("unavailable.md"))?;
    let existing = save(&store, Scope::Workspace, "Existing", "Existing memory body")?;
    let preview = import::preview(serde_json::from_value(json!({"source": source}))?)?;
    assert_eq!(preview["candidates"][0]["index_file"], true);
    assert!(preview["candidates"][1]["scope"].is_null());
    assert!(preview["candidates"][2]["unavailable"].is_string());
    let revision = &preview["candidates"][1]["revision"];
    let choices = json!([
        {"file": "notes.md", "revision": revision, "scope": "global", "first_line": 4, "last_line": 4},
        {"file": "notes.md", "revision": revision, "scope": "workspace", "first_line": 5, "last_line": 5,
         "body": "Edited synthetic fact", "worker": true}
    ]);
    let prepare = || {
        store.prepare_import(serde_json::from_value(
            json!({"source": source, "choices": choices}),
        )?)
    };
    let (stale, final_preview) = prepare()?;
    assert_eq!(final_preview["items"].as_array().unwrap().len(), 2);
    assert!(!fixture.home.join(".fuzukue/memory/backups").exists());
    fs::write(source.join("notes.md"), "Changed synthetic source")?;
    assert!(store.apply_import(stale.unwrap()).is_err());
    assert!(!fixture.home.join(".fuzukue/memory/backups").exists());

    fs::write(source.join("notes.md"), notes)?;
    let (plan, _) = prepare()?;
    let result = store.apply_import(plan.unwrap())?;
    assert_eq!(result.count, 2);
    let backup = fixture.home.join(".fuzukue/memory").join(result.backup);
    assert_eq!(fs::metadata(&backup)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(
        fs::read(backup.join("workspace").join(format!("{}.md", existing.id)))?,
        fs::read(item_path(&fixture.one, &existing.id))?
    );
    assert!(backup.join("manifest.json").is_file());
    let index = store.snapshot()?;
    assert!(index.global.contains("First synthetic fact"));
    assert!(index.workspace.contains("Edited synthetic fact"));
    assert!(!index.jsonl().contains("MEMORY.md"));
    assert_eq!(fs::read_to_string(source.join("notes.md"))?, notes);
    let (nothing_to_apply, repeated) = prepare()?;
    assert!(nothing_to_apply.is_none());
    assert_eq!(repeated["duplicates"].as_array().unwrap().len(), 2);
    Ok(())
}

async fn mcp(
    app: &std::sync::Arc<crate::Workspace>,
    session: &str,
    backend: fuzukue_proto::AgentBackend,
    token: &str,
    request: serde_json::Value,
) -> Result<serde_json::Value> {
    use axum::{
        Json,
        body::to_bytes,
        extract::{Query, State},
        http::{HeaderMap, header},
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    let response = crate::mcp::handler(
        State(app.clone()),
        Query(crate::mcp::McpQuery {
            session: session.into(),
            backend: Some(backend),
            workspace: None,
            job: None,
        }),
        headers,
        Json(request),
    )
    .await;
    Ok(serde_json::from_slice(
        &to_bytes(response.into_body(), 512 * 1024).await?,
    )?)
}

async fn assert_mcp_gate(
    app: &std::sync::Arc<crate::Workspace>,
    session: &str,
    backend: fuzukue_proto::AgentBackend,
    token: &str,
    trusted: bool,
) -> Result<()> {
    let list = mcp(
        app,
        session,
        backend,
        token,
        json!({"id": 1, "method": "tools/list"}),
    )
    .await?;
    assert_eq!(
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "memory_list"),
        trusted
    );
    let result = mcp(
        app,
        session,
        backend,
        token,
        json!({
            "id": 2, "method": "tools/call", "params": {"name": "memory_list", "arguments": {}}
        }),
    )
    .await?;
    assert_eq!(result["result"]["isError"], !trusted);
    assert_eq!(
        result.to_string().contains("Synthetic wire summary"),
        trusted
    );
    let result = mcp(app, session, backend, token, json!({
        "id": 3, "method": "tools/call", "params": {"name": "memory_save", "arguments": {
            "scope": "workspace", "summary": "MCP synthetic save", "body": "MCP fixture body", "worker": false,
        }}
    })).await?;
    assert_eq!(result["result"]["isError"], !trusted);
    let result = mcp(app, session, backend, token, json!({
        "id": 4, "method": "tools/call", "params": {"name": "fs_read", "arguments": {
            "path": ".fuzukue/memory/index.jsonl",
        }}
    })).await?;
    assert_eq!(result["result"]["isError"], true);
    assert!(result.to_string().contains("Access denied to app-owned memory"));
    Ok(())
}

async fn read_wire(path: &Path, marker: &str) -> Result<String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if let Some(line) = text.lines().find(|line| {
                    line.contains(marker) && serde_json::from_str::<serde_json::Value>(line).is_ok()
                }) {
                    return line.to_owned();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("missing synthetic backend input")
}

#[tokio::test]
async fn both_desk_backends_gate_memory_attachments_and_mcp_calls() -> Result<()> {
    use crate::{
        claude::ClaudeSession,
        codex::{Codex, PartnerOptions},
        front_desk::TurnKind,
    };
    use fuzukue_proto::{AgentBackend, ChatSettings};
    use std::sync::Arc;
    use tokio::process::Command;

    let fixture = Fixture::new()?;
    let root = &fixture.one;
    let fake = root.join("claude-fixture");
    let wire = root.join("claude-wire.jsonl");
    fs::write(
        &fake,
        "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> claude-wire.jsonl; done\n",
    )?;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700))?;
    let mut app = crate::Workspace::new(
        root.clone(),
        String::new(),
        root.join("state"),
        Arc::new(crate::Hub::new()),
    );
    app.claude_program = Some(fake);
    let app = Arc::new(app);
    let (home, workspace) = desk::roots(&app)?;
    let store = Store {
        home: &home,
        workspace: &workspace,
    };
    save(
        &store,
        Scope::Global,
        "Synthetic wire summary",
        "Synthetic wire body",
    )?;
    save(&store, Scope::Workspace, "Private workspace item", "Private fixture body")?;

    for (kind, trusted, marker) in [
        (TurnKind::UserConversation, true, "trusted Claude fixture"),
        (TurnKind::ExternalInput, false, "external Claude fixture"),
    ] {
        let id = app.hub.edit_threads(|store| store.create(None))?;
        let session = ClaudeSession::spawn(
            id.clone(),
            root.clone(),
            "http://127.0.0.1:1/mcp",
            None,
            None,
            ChatSettings::default(),
            Box::new(|_| {}),
            &app,
        )
        .await?;
        app.sessions
            .lock()
            .await
            .insert(id.clone(), session.clone());
        session
            .prompt_with_auth(
                marker,
                Vec::new(),
                kind,
                None,
                None,
                Vec::new(),
                Default::default(),
            )
            .await?;
        let input = read_wire(&wire, marker).await?;
        assert_eq!(input.contains("Synthetic wire summary"), trusted);
        assert!(!input.contains("Synthetic wire body"));
        assert_mcp_gate(
            &app,
            &id,
            AgentBackend::Claude,
            session.mcp_key().unwrap(),
            trusted,
        )
        .await?;
        session.interrupt().await?;
    }

    let wire = root.join("codex-wire.jsonl");
    let release = root.join("release-private-memory-turn");
    let mut command =
        Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/codex"));
    command.env("FUZUKUE_CODEX_FIXTURE_LOG", &wire);
    let codex = Codex::spawn_command(
        command,
        root,
        app.hub.clone(),
        app.usage.clone(),
        app.session_stats.clone(),
    )
    .await?;
    app.codex.lock().await.replace(codex.clone());
    let id = app.hub.edit_threads(|store| store.create(None))?;
    let (thread, _) = codex
        .open_partner(
            &id,
            None,
            PartnerOptions {
                settings: ChatSettings::default(),
                context: String::new(),
                mcp_url: "http://127.0.0.1:1/mcp".into(),
                mcp_key: "memory-fixture-key".into(),
                agent_settings: Default::default(),
                mail_enabled: false,
            },
        )
        .await?;

    for (kind, trusted, marker) in [
        (TurnKind::UserConversation, true, "trusted Codex fixture"),
        (TurnKind::ExternalInput, false, "external Codex fixture"),
    ] {
        let attachment = for_turn(&app, &id, kind).await?;
        let run = tokio::spawn({
            let codex = codex.clone();
            let thread = thread.clone();
            async move {
                codex
                    .run_turn_with_images(
                        &thread,
                        &format!("private-memory turn fixture {marker}"),
                        &[],
                        kind,
                        None,
                        attachment.as_deref(),
                        || Ok(()),
                        || {},
                    )
                    .await
            }
        });
        let input = read_wire(&wire, marker).await?;
        assert_eq!(input.contains("Synthetic wire summary"), trusted);
        assert!(!input.contains("Synthetic wire body"));
        assert_mcp_gate(
            &app,
            &id,
            AgentBackend::Codex,
            "memory-fixture-key",
            trusted,
        )
        .await?;
        fs::write(&release, "")?;
        tokio::time::timeout(std::time::Duration::from_secs(5), run).await???;
        fs::remove_file(&release)?;
    }
    Ok(())
}
