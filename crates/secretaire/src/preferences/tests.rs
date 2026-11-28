use super::*;

const HOME: &str = "Use home spelling.";
const WORKSPACE: &str = "Use workspace spelling.";
const SHARED: &str = "Shared publishing and approval policy.";
const FIRST: &str = "Use first-paper notation.";
const SECOND: &str = "Use second-paper notation.";

fn write(root: &Path, path: &str, content: &str) -> Result<()> {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, content)?;
    Ok(())
}

fn local(home: Option<&Path>, project: &Path, paper: Option<&str>) -> Loaded {
    load_files(home.map(|root| (root, Ok(None))), (project, Ok(None)), paper)
}

fn paper(root: &Path, deck: &str, id: &str, body: &str) -> Result<()> {
    write(root, &format!("{deck}/deck.toml"), &format!("kind = 'paper'\nwriting_memo = '{id}'\n"))?;
    write(root, &format!("memos/{id}/card.md"), &format!("---\nid: {id}\nkind: memo\nfrom: Private metadata\n---\n{body}\n"))
}

#[test]
fn personal_layers_keep_shared_same_name_policy_and_apply_workspace_last() -> Result<()> {
    let home = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    write(home.path(), ".fuzukue/rules/20-policy.md", HOME)?;
    write(home.path(), ".fuzukue/rules/01-first.md", "Earlier home preference")?;
    write(project.path(), ".fuzukue/rules/20-policy.md", WORKSPACE)?;
    write(project.path(), "policy/20-policy.md", SHARED)?;

    let mut policy = crate::rules::resolve(project.path())?.policy(project.path())?;
    assert!(!policy.contains(HOME) && !policy.contains(WORKSPACE));
    let preferences = local(Some(home.path()), project.path(), None);
    assert!(preferences.warnings.is_empty());
    assert!(preferences.text.contains("Approval, access, and publishing requirements still apply.")
        || preferences.text.contains("承認・アクセス・公開に関する要件は引き続き適用する"));
    policy.push_str(&preferences.text);
    let ordered: Vec<_> = [SHARED, "Earlier home preference", HOME, WORKSPACE]
        .map(|item| policy.find(item).unwrap()).into();
    assert!(ordered.windows(2).all(|positions| positions[0] < positions[1]));
    Ok(())
}

#[test]
fn missing_layers_are_empty_and_rejected_files_are_reported() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    let empty = local(None, root, None);
    assert!(empty.text.is_empty() && empty.warnings.is_empty());
    write(root, ".fuzukue/rules/good.md", WORKSPACE)?;
    write(root, ".fuzukue/rules/huge.md", &"x".repeat(MAX_FILE + 1))?;
    fs::write(root.join(".fuzukue/rules/invalid.md"), [0xff])?;
    fs::create_dir(root.join(".fuzukue/rules/directory.md"))?;
    std::os::unix::fs::symlink("good.md", root.join(".fuzukue/rules/linked.md"))?;
    let loaded = local(None, root, None);
    assert_eq!(loaded.text.matches(WORKSPACE).count(), 1);
    for name in ["huge.md", "invalid.md", "directory.md", "linked.md"] {
        assert!(loaded.warnings.iter().any(|warning| warning.contains(name)));
    }

    let linked = tempfile::tempdir()?;
    std::os::unix::fs::symlink(root.join(".fuzukue"), linked.path().join(".fuzukue"))?;
    let loaded = local(None, linked.path(), None);
    assert!(loaded.text.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    Ok(())
}

#[test]
fn personal_layers_bound_combined_text_and_file_count() -> Result<()> {
    let project = tempfile::tempdir()?;
    for index in 0..4 {
        write(project.path(), &format!(".fuzukue/rules/{index}.md"), &"x".repeat(MAX_FILE))?;
    }
    let loaded = local(None, project.path(), None);
    assert!(loaded.text.matches('x').count() < MAX_LAYER);
    assert!(!loaded.warnings.is_empty());
    for index in 4..=MAX_FILES {
        write(project.path(), &format!(".fuzukue/rules/{index}.md"), "small")?;
    }
    let loaded = local(None, project.path(), None);
    assert!(loaded.text.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    Ok(())
}

#[tokio::test]
async fn captures_the_paper_at_delegation_and_excludes_ambiguous_tasks() -> Result<()> {
    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = project.path();
    paper(root, "papers/first", "first-style", FIRST)?;
    paper(root, "papers/second", "second-style", SECOND)?;
    write(root, "books/notes/deck.toml", "kind = 'book'\n")?;
    write(root, ".fuzukue/rules/style.md", WORKSPACE)?;
    let app = crate::Workspace::new(root.to_owned(), String::new(), state.path().into(), std::sync::Arc::new(crate::Hub::new()));
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let capture = |prompt, related: &[&str]| Selection::capture(&app, &session, true, prompt, related).unwrap();
    assert!(capture("Revise the introduction.", &[]).paper.is_none());
    app.hub.edit_threads(|store| store.attach(&session, &["papers/first".into(), "books/notes".into()]))?;
    let captured = capture("Revise papers/first/intro.tex.", &[]);
    assert_eq!(captured.paper.as_deref(), Some("papers/first"));
    for (task, related) in [
        ("Revise papers/second/intro.tex.", [""]),
        ("Revise the introduction.", ["papers/second"]),
        ("Compare this with another paper.", [""]),
        ("Compare with https://arxiv.org/abs/1234.5678.", [""]),
    ] {
        assert!(capture(task, &related).paper.is_none());
    }
    app.hub.edit_threads(|store| store.attach(&session, &["papers/second".into()]))?;
    assert!(capture("Revise the introduction.", &[]).paper.is_none());
    app.hub.edit_threads(|store| store.detach(&session, "papers/first"))?;
    assert_eq!(capture("Revise the introduction.", &[]).paper.as_deref(), Some("papers/second"));

    let queued = serde_json::to_string(&captured)?;
    assert!(!queued.contains(FIRST) && !queued.contains(WORKSPACE));
    let restored: Selection = serde_json::from_str(&queued)?;
    let instructions = restored.load(&app, &session).await?;
    assert!(instructions.find(WORKSPACE).unwrap() < instructions.find(FIRST).unwrap());
    assert!(!instructions.contains(SECOND) && !instructions.contains("Private metadata"));
    let (input, _) = app.partner_input(&session, "Revise the introduction.".into(), false).await?;
    assert!(!input.contains(FIRST) && !input.contains(SECOND) && !input.contains(WORKSPACE));
    Ok(())
}

#[tokio::test]
async fn codex_current_turn_overrides_paper_and_untrusted_history() -> Result<()> {
    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    paper(project.path(), "papers/first", "first-style", FIRST)?;
    paper(project.path(), "papers/second", "second-style", SECOND)?;
    write(project.path(), ".fuzukue/rules/style.md", WORKSPACE)?;
    let app = crate::Workspace::new(project.path().into(), String::new(), state.path().into(), std::sync::Arc::new(crate::Hub::new()));
    let session = app.hub.edit_threads(|store| store.create(None))?;
    app.hub.edit_threads(|store| store.attach(&session, &["papers/first".into()]))?;
    let selection = Selection::capture(&app, &session, true, "Revise", std::iter::empty::<&str>());
    let first = codex_turn_guidance(&load_selection(selection, &app, &session).await?, false);
    assert!(first.contains(FIRST) && first.contains("papers/first") && first.contains(WORKSPACE));
    app.hub.edit_threads(|store| store.detach(&session, "papers/first"))?;
    app.hub.edit_threads(|store| store.attach(&session, &["papers/second".into()]))?;
    let selection = Selection::capture(&app, &session, true, "Revise", std::iter::empty::<&str>());
    let second = codex_turn_guidance(&load_selection(selection, &app, &session).await?, false);
    assert!(second.contains(SECOND) && second.contains("papers/second") && !second.contains(FIRST));
    let untrusted = codex_turn_guidance("", false);
    assert!(!untrusted.contains(FIRST) && !untrusted.contains(SECOND) && !untrusted.contains(WORKSPACE));
    assert!(untrusted.contains("supersedes") || untrusted.contains("優先する"));
    Ok(())
}

#[test]
fn digest_batches_inherit_only_the_preferences_shared_by_their_assignments() -> Result<()> {
    use crate::front_desk::TurnKind::{Nudge, UserConversation, WorkerDigest};

    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    paper(project.path(), "papers/first", "first-style", FIRST)?;
    paper(project.path(), "papers/second", "second-style", SECOND)?;
    let app = crate::Workspace::new(project.path().into(), String::new(), state.path().into(), std::sync::Arc::new(crate::Hub::new()));
    let session = app.hub.edit_threads(|store| store.create(Some("papers/first")))?;
    let turn = |kind, inheritance| for_turn(&app, &session, kind, false, "Continue", &inheritance).unwrap();
    let first = turn(UserConversation, Inheritance::Desk);
    app.hub.edit_threads(|store| {
        store.detach(&session, "papers/first")?;
        store.attach(&session, &["papers/second".into()])?;
        Ok(())
    })?;
    let second = turn(UserConversation, Inheritance::Desk);
    assert_ne!(first, second);
    assert_eq!(turn(WorkerDigest, Inheritance::Captured(first.clone())), first);
    assert_eq!(turn(Nudge, Inheritance::Desk), first);
    assert_eq!(Selection::common([&first, &second].into_iter()), Some(Selection { paper: None }));
    assert_eq!(Selection::common([&first, &None].into_iter()), None);
    Ok(())
}

#[test]
fn only_user_turns_in_personal_sessions_capture_preferences() -> Result<()> {
    let project = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let app = crate::Workspace::new(
        project.path().to_owned(), String::new(), state.path().into(), std::sync::Arc::new(crate::Hub::new()),
    );
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let capture = |session: &str, kind: crate::front_desk::TurnKind| Selection::capture(
        &app, session, kind.user_conversation(), "Draft", std::iter::empty::<&str>(),
    );
    use crate::front_desk::TurnKind::*;
    assert!(capture(&session, UserConversation).is_some());
    for kind in [Conversation, ExternalInput, Nudge, WorkerDigest] {
        assert!(capture(&session, kind).is_none());
    }
    app.hub.publish_partner_turn_started(&session, None, false, None);
    app.hub.mark_external_approval_input(&session);
    assert!(capture(&session, UserConversation).is_none());
    assert!(capture(&fuzukue_proto::issue_thread_id(42), UserConversation).is_none());
    let linked = app.hub.edit_threads(|store| store.create(None))?;
    let workshop = app.hub.edit_threads(|store| store.create_workshop_thread(
        "Workshop".into(), Vec::new(), fuzukue_proto::DelegateSettings { model: "fixture".into(), effort: String::new() },
    ))?;
    for session in [linked, workshop] {
        crate::issue_threads::attach(&app, &session, fuzukue_proto::GitHubLink {
            repo: "owner/repo".into(), number: 42,
        })?;
        assert!(capture(&session, UserConversation).is_none());
    }
    Ok(())
}

#[test]
fn linked_memos_reject_symlinks_and_wrong_id() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    paper(root, "papers/first", "first-style", FIRST)?;
    let memo = root.join("memos/first-style/card.md");
    fs::rename(&memo, memo.with_file_name("actual.md"))?;
    std::os::unix::fs::symlink("actual.md", &memo)?;
    let loaded = local(None, root, Some("papers/first"));
    assert!(!loaded.text.contains(FIRST));
    assert_eq!(loaded.warnings.len(), 1);
    fs::remove_file(&memo)?;
    fs::write(memo, format!("---\nid: other\nkind: memo\n---\n{SECOND}"))?;
    let loaded = local(None, root, Some("papers/first"));
    assert!(!loaded.text.contains(SECOND));
    assert_eq!(loaded.warnings.len(), 1);
    Ok(())
}

#[test]
fn symlinked_ancestors_are_refused_for_preferences_and_memos() -> Result<()> {
    use std::os::unix::fs::symlink;
    let base = tempfile::tempdir()?;
    let root = base.path().join("real/project");
    paper(&root, "papers/first", "first-style", FIRST)?;
    write(&root, ".fuzukue/rules/style.md", WORKSPACE)?;
    symlink(base.path().join("real"), base.path().join("alias"))?;
    let alias = base.path().join("alias/project");
    assert!(read_text(&alias.join(".fuzukue/rules/style.md"), MAX_FILE).is_err());
    assert!(read_text(&alias.join("memos/first-style/card.md"), MAX_FILE).is_err());
    let loaded = local(Some(&alias), &root, None);
    assert_eq!(loaded.text.matches(WORKSPACE).count(), 1);
    assert_eq!(loaded.warnings.len(), 1);

    fs::rename(root.join("memos/first-style"), root.join("memos/actual"))?;
    symlink("actual", root.join("memos/first-style"))?;
    assert!(local(None, &root, Some("papers/first")).warnings.len() == 1);
    assert!(!local(None, &root, Some("papers/first")).text.contains(FIRST));
    Ok(())
}

#[test]
fn parse_warnings_report_paths_and_kinds_without_excerpts() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path();
    let secret = "private-content-sentinel";
    for (file, source, kind) in [
        ("papers/first/deck.toml", format!("writing_memo = [{secret}"), "invalid deck TOML"),
        ("memos/first-style/card.md", format!("---\nid: [{secret}\nkind: memo\n---\n{secret}"), "invalid memo YAML"),
    ] {
        paper(root, "papers/first", "first-style", FIRST)?;
        write(root, file, &source)?;
        let loaded = local(None, root, Some("papers/first"));
        assert!(loaded.text.is_empty());
        let [warning] = &loaded.warnings[..] else { anyhow::bail!("expected one warning"); };
        assert!(warning.contains(file) && warning.contains(kind), "{warning}");
        assert!(!warning.contains(secret));
    }
    Ok(())
}

#[tokio::test]
async fn git_tracked_preferences_are_skipped_at_home_and_in_the_workspace() -> Result<()> {
    let repository = tempfile::tempdir()?;
    let home = repository.path().join("home");
    let project = repository.path().join("project");
    let tracked = ".fuzukue/rules/tracked.md";
    let untracked = ".fuzukue/rules/local.md";
    for (root, text) in [(&home, HOME), (&project, WORKSPACE)] {
        write(root, tracked, "Tracked preference sentinel")?;
        write(root, untracked, text)?;
    }
    async fn git(root: &Path, args: &[&str]) -> Result<()> {
        let output = crate::git::run(root, args, 4096).await?;
        ensure!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    }
    git(repository.path(), &["init", "--quiet"]).await?;
    let before = load(Some(&home), &project, None).await?;
    assert_eq!(before.warnings.len(), 2);
    assert_eq!(before.text.matches("Tracked preference sentinel").count(), 2);
    assert!(!repository.path().join(".gitignore").exists());
    write(repository.path(), ".gitignore", ".fuzukue/\n")?;
    assert!(load(Some(&home), &project, None).await?.warnings.is_empty());
    git(repository.path(), &["add", "-f", "--", &format!("home/{tracked}"), &format!("project/{tracked}")]).await?;
    let loaded = load(Some(&home), &project, None).await?;
    assert!(loaded.text.contains(HOME) && loaded.text.contains(WORKSPACE));
    assert!(!loaded.text.contains("Tracked preference sentinel"));
    assert_eq!(loaded.warnings.len(), 2);
    for root in [&home, &project] {
        assert!(loaded.warnings.iter().any(|warning| warning.contains(&*root.join(tracked).to_string_lossy())));
    }
    Ok(())
}
