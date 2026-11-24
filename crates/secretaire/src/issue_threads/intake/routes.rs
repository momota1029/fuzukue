use super::*;

static CHANGES: AsyncMutex<()> = AsyncMutex::const_new(());

#[derive(Clone, Deserialize, Serialize)]
struct Binding {
    workspace: String,
    session: String,
}

fn index_path(root: &Path) -> PathBuf {
    root.join("github-intake").join("routes.json")
}

fn bindings(root: &Path) -> Result<BTreeMap<String, Binding>> {
    Ok(load_json(&index_path(root))?.unwrap_or_default())
}

pub(in crate::issue_threads) fn restored_workspaces(root: &Path) -> BTreeSet<String> {
    match bindings(root) {
        Ok(routes) => routes.into_values().map(|route| route.workspace).collect(),
        Err(error) => {
            tracing::warn!("Read GitHub intake designations: {error:#}");
            BTreeSet::new()
        }
    }
}

pub(super) fn root(app: &Workspace) -> PathBuf {
    app.registry.upgrade().map_or_else(
        || app.state_dir.clone(),
        |registry| registry.intake_state_root().to_owned(),
    )
}

fn local(app: &Workspace, repo: &str) -> Option<String> {
    app.hub
        .read_threads(|store| store.github_intake(repo).map(str::to_owned))
}

pub(in crate::issue_threads) fn designated(app: &Workspace, repo: &str) -> bool {
    let Ok(routes) = bindings(&root(app)) else {
        return true;
    };
    let Some(route) = routes.get(repo) else {
        return false;
    };
    if route.workspace == app.id {
        return local(app, repo).as_ref() == Some(&route.session);
    }
    // Reloaded intake workspaces are restored on startup. A missing thread (for
    // example, after trashing it) must release the route for legacy discovery.
    let owner = app
        .registry
        .upgrade()
        .and_then(|registry| registry.loaded(Some(&route.workspace)).ok());
    owner.is_none_or(|owner| local(&owner, repo).as_ref() == Some(&route.session))
}

pub(in crate::issue_threads) fn target(app: &Workspace, repo: &str) -> Option<String> {
    let routes = bindings(&root(app)).ok()?;
    let route = routes.get(repo)?;
    (route.workspace == app.id)
        .then(|| local(app, repo))
        .flatten()
        .filter(|session| session == &route.session)
}

pub(in crate::issue_threads) async fn record_closed(app: &Workspace, link: &GitHubLink) -> Result<()> {
    let _change = CHANGES.lock().await;
    let routes = bindings(&root(app))?;
    let owner = routes.get(&link.repo)
        .filter(|route| route.workspace != app.id)
        .and_then(|route| app.registry.upgrade()?.loaded(Some(&route.workspace)).ok());
    // Intake state is shared by workspace. Use the same lock as its poller.
    let owner = owner.as_deref().unwrap_or(app);
    let _serial = owner.issue_threads.serial.lock().await;
    if let Some(mut state) = repository(owner, &link.repo)? {
        if let Some(issue) = state.issues.get_mut(&link.number) {
            issue.status = "closed".into();
            issue.disposition = Disposition::Handled;
            state.save(owner, &link.repo)?;
            owner.issues_refresh.notify_one();
        }
    }
    Ok(())
}

pub(in crate::issue_threads) async fn set(
    app: &Workspace,
    session: &str,
    repo: &str,
    move_from: Option<&str>,
) -> Result<Option<ServerEvent>> {
    let config = GitHubConfig::from_workspace(&app.project_dir)?;
    let repo = allowed(app, &config, repo)?;
    let _change = CHANGES.lock().await;
    let _serial = app.issue_threads.serial.lock().await;
    let root = root(app);
    let mut routes = bindings(&root)?;
    let foreign = routes
        .get(&repo)
        .filter(|route| route.workspace != app.id)
        .and_then(|route| {
            app.registry
                .upgrade()?
                .known_workspace(&route.workspace)
                .ok()
        });
    let _foreign_serial = if let Some(foreign) = &foreign {
        Some(foreign.issue_threads.serial.lock().await)
    } else {
        None
    };
    let (owner, current) = if let Some(foreign) = &foreign {
        (foreign.as_ref(), local(foreign, &repo))
    } else {
        (app, local(app, &repo))
    };
    let current = current.as_deref();
    if owner.id == app.id && current == Some(session) && routes.contains_key(&repo) {
        return Ok(None);
    }
    app.hub.read_threads(|store| {
        ensure!(store.is_live(session), "Thread is archived or missing");
        ensure!(
            store
                .github_state(session)
                .and_then(|state| state.intake_repo)
                .is_none_or(|existing| existing == repo),
            "Remove this thread's existing intake designation first"
        );
        Ok(())
    })?;
    if current != move_from && current != Some(session) {
        if let Some(from) = current {
            let title = owner
                .hub
                .read_threads(|store| store.title(from))
                .unwrap_or_else(|| from.into());
            return Ok(Some(ServerEvent::IssueIntakeMove {
                session: session.into(),
                repo,
                from: from.into(),
                title,
            }));
        }
        ensure!(move_from.is_none(), "The intake designation changed");
    }
    if current.is_none() {
        let now = Utc::now().timestamp();
        let mut state = repository(app, &repo)?.unwrap_or_else(|| Repository::new(now));
        state.from = now;
        state.scanned_at = now;
        state.save(app, &repo)?;
    }
    let cross_workspace = owner.id != app.id && current.is_some();
    if let Some(from) = current.filter(|_| cross_workspace) {
        // Carry tracked cursors so that the new watcher starts with the same
        // baseline and event IDs, including when the source repo is secondary.
        let (_, numbers) = tracked(app, &repo)?;
        for number in numbers {
            let link = GitHubLink {
                repo: repo.clone(),
                number,
            };
            if let Some(cursor) = load(&cursor_path(owner, &link))? {
                save(&cursor_path(app, &link), &cursor)?;
            }
        }
        owner
            .hub
            .edit_threads(|store| store.github_edit(from, |state| state.intake_repo = None))?;
    }
    let same_workspace_from = current.filter(|from| !cross_workspace && *from != session);
    let result = app
        .hub
        .edit_threads(|store| store.github_set_intake(session, &repo, same_workspace_from));
    if let Err(error) = result {
        if let Some(from) = current.filter(|_| cross_workspace) {
            restore(owner, from, &repo)?;
        }
        return Err(error);
    }
    routes.insert(
        repo.clone(),
        Binding {
            workspace: app.id.clone(),
            session: session.into(),
        },
    );
    if let Err(error) = save_json(&index_path(&root), &routes) {
        if current != Some(session) || cross_workspace {
            clear(app, session)?;
        }
        if let Some(from) = current {
            restore(owner, from, &repo)?;
        }
        return Err(error);
    }
    app.issues_refresh.notify_one();
    if cross_workspace {
        owner.issues_refresh.notify_one();
    }
    Ok(None)
}

fn clear(app: &Workspace, session: &str) -> Result<()> {
    app.hub
        .edit_threads(|store| store.github_edit(session, |state| state.intake_repo = None))?;
    Ok(())
}

fn restore(app: &Workspace, session: &str, repo: &str) -> Result<()> {
    app.hub.edit_threads(|store| {
        store.github_edit(session, |state| state.intake_repo = Some(repo.into()))
    })?;
    Ok(())
}

pub(in crate::issue_threads) async fn remove(
    app: &Workspace,
    session: &str,
    repo: &str,
) -> Result<()> {
    let _change = CHANGES.lock().await;
    let _serial = app.issue_threads.serial.lock().await;
    ensure!(
        target(app, repo).as_deref() == Some(session),
        "The intake designation changed"
    );
    let path = index_path(&root(app));
    let mut routes = bindings(&root(app))?;
    clear(app, session)?;
    routes.remove(repo);
    if let Err(error) = save_json(&path, &routes) {
        restore(app, session, repo)?;
        return Err(error);
    }
    app.issues_refresh.notify_one();
    Ok(())
}
