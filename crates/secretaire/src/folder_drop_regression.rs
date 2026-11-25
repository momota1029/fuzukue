use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fuzukue_proto::{
    ATTACHMENT_CHUNK_BYTES, FolderAttachmentEntry as Entry, FolderAttachmentRequest as Request,
    LibraryKind, ServerEvent,
};
use std::{fs, path::Path, process::Command, sync::Arc};
use tokio::sync::Semaphore;

use crate::{folder_attachment::FolderUpload, test_support::TestDir};

fn received(event: ServerEvent) -> String {
    match event {
        ServerEvent::AttachmentUploaded { path: Some(path), error: None, .. } => path,
        event => panic!("expected an attachment, got {event:?}"),
    }
}

fn progress(event: ServerEvent, expected: u64) {
    assert!(matches!(event, ServerEvent::AttachmentProgress { offset, .. } if offset == expected),
        "expected progress to {expected}, got {event:?}");
}

fn synthetic_deck(dir: &Path, body: &str) -> Result<()> {
    fs::create_dir_all(dir.join("cards/demo"))?;
    fs::write(dir.join("deck.toml"), "kind = 'paper'")?;
    fs::write(dir.join("cards/demo/card.md"), format!("---\nid: demo\ntitle: Synthetic card\n---\n{body}"))?;
    Ok(())
}

#[tokio::test]
async fn folder_drop_synthetic_reference_stays_outside_workspace_and_prompt() -> Result<()> {
    let project = TestDir::new();
    let project = project.path().canonicalize()?;
    let state = TestDir::new();
    let state = state.path().to_path_buf();
    let source = tempfile::tempdir()?;
    let source = source.path().canonicalize()?;
    let repo = source.join("synthetic repository");
    synthetic_deck(&project.join("active"), "ActiveExample")?;
    synthetic_deck(&repo, "ExternalExample")?;
    fs::create_dir_all(repo.join(".git/objects"))?;
    fs::create_dir_all(repo.join("target"))?;
    fs::create_dir_all(repo.join("node_modules"))?;
    for index in 0..6000 {
        fs::File::create(repo.join(format!("sample-{index}.txt")))?;
    }
    fs::File::create(repo.join("synthetic.bin"))?.set_len(3 * 1024 * 1024 * 1024)?;
    fs::File::create(repo.join(".git/objects/synthetic.pack"))?.set_len(2 * 1024 * 1024 * 1024)?;
    std::os::unix::fs::symlink(&repo, repo.join("loop"))?;
    let git = |args: &[&str]| Command::new("git").current_dir(&project).args(args).output();
    assert!(git(&["init", "-q"])?.status.success());
    let before = git(&["status", "--porcelain", "--untracked-files=all"])?;
    ensure!(before.status.success(), "git status failed");

    let mut upload = FolderUpload::default();
    let slots = Arc::new(Semaphore::new(1));
    let path = received(upload.receive(&state, slots.clone(), 2, Request::Reference {
        path: repo.to_str().unwrap().into(), name: "synthetic repository".into(),
    }));
    assert_eq!(crate::folder_attachment::resolve(&state, &path)?, repo);
    let saved = fs::read_dir(state.join("inbox/folders"))?.next().unwrap()?.path();
    let files: Vec<_> = fs::read_dir(saved)?.collect::<std::io::Result<_>>()?;
    assert_eq!(files.len(), 1);
    assert!(files[0].metadata()?.len() < 4096);
    assert_eq!(git(&["status", "--porcelain", "--untracked-files=all"])?.stdout, before.stdout);

    let (prompt, images) = crate::with_attachments(&project, &state, "Continue the synthetic workspace", &[path])?;
    assert!(prompt.contains("read-only folder"));
    assert!(prompt.contains(repo.to_str().unwrap()));
    assert!(!prompt.contains("sample-5999.txt") && !prompt.contains("ExternalExample"));
    assert!(prompt.len() < 4096 && images.is_empty());

    let legacy = project.join("inbox/2026-11-25/old-repo");
    synthetic_deck(&legacy, "LegacyExample")?;
    assert!(crate::cards::is_ignored(&project, &legacy));
    assert!(crate::auto_paths::excluded(&project, &legacy));
    assert_eq!(fuzukue_cards::find_decks(&project), [project.join("active")]);
    assert!(crate::file_browser::list(&project, "inbox", false)?.entries.is_empty());
    assert!(crate::file_browser::find(&project, "old-repo", true, &[])?.entries.is_empty());
    let app = Arc::new(crate::Workspace::new(project.clone(), String::new(), state.clone(), Arc::new(crate::Hub::new())));
    let graph = &app.graph;
    assert!(graph.deck_for_file(&project, &state, &repo.join("cards/demo/card.md")).is_none());
    assert!(graph.deck_for_file(&project, &state, &legacy.join("cards/demo/card.md")).is_none());
    assert_eq!(graph.deck_for_file(&project, &state, &project.join("active/cards/demo/card.md")).unwrap().card.as_deref(), Some("demo"));
    for (query, expected) in [("LegacyExample", 0), ("ExternalExample", 0), ("ActiveExample", 1)] {
        let result = crate::library::search(app.clone(), crate::library::Search {
            query: query.into(), kinds: vec![LibraryKind::Card], deck: None,
            sort: Default::default(), offset: 0, limit: 10, protocol: fuzukue_proto::PROTOCOL_VERSION,
        }).await?;
        assert_eq!(result.hits.len(), expected, "{query}");
    }
    Ok(())
}

#[test]
fn folder_drop_copy_commits_only_complete_valid_entries_and_cleans_up() -> Result<()> {
    let state = TestDir::new();
    let state = state.path();
    let slots = Arc::new(Semaphore::new(1));
    let mut upload = FolderUpload::default();
    let binary = vec![0x97; ATTACHMENT_CHUNK_BYTES];
    let bytes = binary.len() as u64 + 1;
    let begin = |entries| Request::Begin { name: "portable".into(), bytes, entries };
    let chunk = |path: &str, offset, data| Request::Entries {
        entries: vec![Entry::FileChunk { path: path.into(), size: bytes, offset, data }],
    };
    let directory = |path: &str| Request::Entries {
        entries: vec![Entry::Directory { path: path.into() }],
    };
    let error = |event| {
        assert!(matches!(event, ServerEvent::AttachmentUploaded { path: None, error: Some(_), .. }),
            "expected an error, got {event:?}");
    };
    {
        let mut send = |id, request| upload.receive(state, slots.clone(), id, request);
        error(send(1, Request::Begin { name: "too-big".into(), bytes: 3 * 1024 * 1024 * 1024, entries: 1 }));
        error(send(1, Request::Begin { name: ".git".into(), bytes: 0, entries: 0 }));
        assert!(matches!(send(1, Request::Reference { name: "missing".into(), path: state.join("missing").to_str().unwrap().into() }),
            ServerEvent::FolderReferenceUnavailable { .. }));
        let file = state.join("regular-file");
        fs::write(&file, b"synthetic")?;
        assert!(matches!(send(1, Request::Reference { name: "wrong-kind".into(), path: file.to_str().unwrap().into() }),
            ServerEvent::FolderReferenceUnavailable { .. }));
        progress(send(2, begin(1)), 0);
        error(send(2, chunk("../escape", 0, STANDARD.encode(&binary))));
        progress(send(3, begin(1)), 0);
        error(send(3, directory(".git")));
        progress(send(4, begin(1)), 0);
        progress(send(4, chunk("partial.bin", 0, STANDARD.encode(&binary))), binary.len() as u64);
    }
    assert_eq!(slots.available_permits(), 0);
    upload.cancel(4);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(fs::read_dir(state.join("inbox/folders"))?.count(), 0);

    let mut send = |id, request| upload.receive(state, slots.clone(), id, request);
    progress(send(5, begin(1)), 0);
    error(send(5, Request::Finish));
    progress(send(6, begin(3)), 0);
    let batch = Request::Entries { entries: vec![
        Entry::Directory { path: "empty".into() },
        Entry::Directory { path: "src".into() },
        Entry::FileChunk { path: "src/binary.bin".into(), size: bytes, offset: 0, data: STANDARD.encode(&binary) },
    ] };
    let wire = serde_json::to_vec(&fuzukue_proto::ClientMsg::FolderAttachment { id: 6, request: batch.clone() })?;
    assert!(wire.len() < 750 * 1024);
    progress(send(6, batch), binary.len() as u64);
    progress(send(6, chunk("src/binary.bin", binary.len() as u64, STANDARD.encode(b"x"))), bytes);
    let path = received(send(6, Request::Finish));
    let dir = crate::folder_attachment::resolve(state, &path)?;
    assert!(dir.join("empty").is_dir());
    let bin = fs::read(dir.join("src/binary.bin"))?;
    assert_eq!(&bin[..binary.len()], &binary);
    assert_eq!(&bin[binary.len()..], b"x");
    progress(send(7, begin(1)), 0);
    drop(upload);
    assert_eq!(fs::read_dir(state.join("inbox/folders"))?.count(), 1);
    assert_eq!(slots.available_permits(), 1);
    Ok(())
}

#[test]
fn folder_drop_regular_android_wire_upload_keeps_legacy_paths_and_git_clean() -> Result<()> {
    let project = TestDir::new();
    let state = TestDir::new();
    let project = project.path();
    let state = state.path();
    let git = |args: &[&str]| Command::new("git").current_dir(project).args(args).output();
    assert!(git(&["init", "-q"])?.status.success());
    let mut uploads = crate::inbox::Uploads::default();
    let path = received(uploads.receive(project, state, 1, "synthetic.bin".into(), 3, 0, "YWJj"));
    assert!(path.starts_with("inbox/") && path.split('/').count() == 3);
    assert_eq!(fs::read(crate::inbox::attachment(project, state, &path)?)?, b"abc");
    assert!(!project.join("inbox").exists());
    assert!(git(&["status", "--porcelain", "--untracked-files=all"])?.stdout.is_empty());
    assert!(git(&["add", "-A"])?.status.success());
    assert!(git(&["diff", "--cached", "--name-only"])?.stdout.is_empty());
    fs::create_dir_all(project.join("inbox/2026-11-25"))?;
    fs::write(project.join("inbox/2026-11-25/legacy.txt"), b"legacy")?;
    assert_eq!(fs::read(crate::inbox::attachment(project, state, "inbox/2026-11-25/legacy.txt")?)?, b"legacy");
    Ok(())
}
