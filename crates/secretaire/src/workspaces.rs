//! 既知のワークスペースと、開いているワークスペースの管理。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{WorkspaceDirectory, WorkspaceEntry, WorkspaceInfo, WorkspaceKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::{mail, Hub, Workspace};

pub fn prompt() -> &'static str {
    harness_text!(
        "\nUse list_workspaces to see available workspaces. Use create_workspace only when the user asks or explicitly agrees. Then tell the user its name and location. The user switches workspaces from the sidebar.",
        "\nワークスペースは list_workspaces で確認できる。利用者から依頼があるか、明確な了承を得たときだけ create_workspace で作成する。作成したら、名前と保存先を利用者に伝える。画面は利用者がサイドバーから切り替える。",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    name: String,
    kind: WorkspaceKind,
    path: Option<String>,
}

pub struct App {
    home: PathBuf,
    state_dir: PathBuf,
    pub(crate) shutdown_drain_seconds: std::sync::OnceLock<u64>,
    pub(crate) mutations: crate::mutations::Mutations,
    pub(crate) delegate_admission: Arc<crate::delegate::admission::Scheduler>,
    hold_planner_during_update: bool,
    #[cfg(test)]
    pub(crate) test_claude_program: std::sync::OnceLock<PathBuf>,
    default: String,
    addr: String,
    mail: Option<Arc<mail::Store>>,
    models: tokio::sync::OnceCell<Vec<fuzukue_proto::CodexModel>>,
    entries: Mutex<BTreeMap<String, Entry>>,
    pub changed: watch::Sender<()>,
    pub(crate) notices: tokio::sync::broadcast::Sender<fuzukue_proto::Envelope>,
    pub tailnet_addr: std::sync::OnceLock<std::net::SocketAddr>,
    pub self_update: std::sync::OnceLock<Arc<crate::self_update::Update>>,
    pub distribution: crate::distribution::Distribution,
    pub workshop: crate::workshop::Workshop,
    pub(crate) workspace_requests: crate::workspace_requests::Store,
}

#[derive(Clone, Serialize, Deserialize)]
struct Known {
    path: PathBuf,
    title: String,
    last_opened: Option<i64>,
    #[serde(default)]
    hidden: bool,
}

struct Entry {
    known: Known,
    loaded: Option<Arc<Workspace>>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    workspaces: BTreeMap<String, Known>,
    #[serde(default)]
    research: crate::workshop::Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockerScan {
    Drain,
    Preflight,
    Cutoff,
}

impl App {
    pub fn new(
        home: PathBuf,
        project: PathBuf,
        state_dir: PathBuf,
        addr: String,
        mail: Option<Arc<mail::Store>>,
    ) -> Result<Arc<Self>> {
        Self::with_delegate_config(home, project, state_dir, addr, mail, Default::default())
    }

    pub(crate) fn with_delegate_config(
        home: PathBuf,
        project: PathBuf,
        state_dir: PathBuf,
        addr: String,
        mail: Option<Arc<mail::Store>>,
        delegate: crate::delegate::admission::Overrides,
    ) -> Result<Arc<Self>> {
        let home = home
            .canonicalize()
            .context("ホームディレクトリを開けない")?;
        let project = checked_dir(&home, &project)?;
        let hold_planner_during_update = delegate.hold_planner_during_update.unwrap_or(true);
        let delegate = crate::delegate::admission::Config::load(&project, delegate)?;
        let default = id_for(&project);
        let registry = match fs::read_to_string(state_dir.join("workspaces.toml")) {
            Ok(contents) => {
                toml::from_str::<Registry>(&contents).context("workspaces.toml を読めない")?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(err) => return Err(err.into()),
        };
        let mut entries: BTreeMap<_, _> = registry
            .workspaces
            .into_iter()
            .filter(|(id, known)| *id == id_for(&known.path))
            .map(|(id, known)| {
                (
                    id,
                    Entry {
                        known,
                        loaded: None,
                    },
                )
            })
            .collect();
        entries.entry(default.clone()).or_insert_with(|| Entry {
            known: known(project),
            loaded: None,
        });
        let (changed, _) = watch::channel(());
        let app = Arc::new(Self {
            home,
            tailnet_addr: Default::default(),
            self_update: Default::default(),
            shutdown_drain_seconds: Default::default(),
            mutations: Default::default(),
            delegate_admission: Arc::new(crate::delegate::admission::Scheduler::new(delegate)),
            hold_planner_during_update,
            #[cfg(test)]
            test_claude_program: Default::default(),
            distribution: crate::distribution::Distribution::new(&state_dir),
            state_dir: state_dir.clone(),
            default,
            addr,
            mail,
            models: Default::default(),
            workshop: crate::workshop::Workshop::new(registry.research),
            workspace_requests: crate::workspace_requests::Store::open(&state_dir)?,
            entries: Mutex::new(entries),
            changed,
            notices: tokio::sync::broadcast::channel(128).0,
        });
        app.save(&app.entries.lock().unwrap())?;
        Ok(app)
    }

    // 既存の単一プロジェクトのテストは、一時ディレクトリをそのまま使う。
    #[cfg(test)]
    pub fn fixture(workspace: Arc<Workspace>) -> Arc<Self> {
        let (changed, _) = watch::channel(());
        let path = workspace.project_dir.clone();
        let id = workspace.id.clone();
        let notices = tokio::sync::broadcast::channel(128).0;
        workspace.hub.attach_notification_feed(&id, notices.clone());
        Arc::new(Self {
            home: path.parent().unwrap().to_owned(),
            tailnet_addr: Default::default(),
            self_update: Default::default(),
            shutdown_drain_seconds: Default::default(),
            mutations: Default::default(),
            delegate_admission: workspace.worker_admission(),
            hold_planner_during_update: workspace.hold_planner_during_update,
            #[cfg(test)]
            test_claude_program: Default::default(),
            distribution: crate::distribution::Distribution::new(&workspace.state_dir),
            state_dir: workspace.state_dir.clone(),
            workshop: crate::workshop::Workshop::new(Default::default()),
            workspace_requests: crate::workspace_requests::Store::open(&workspace.state_dir).unwrap(),
            default: id.clone(),
            addr: workspace.addr.clone(),
            mail: workspace.mail.clone(),
            models: Default::default(),
            entries: Mutex::new(BTreeMap::from([(
                id,
                Entry {
                    known: known(path),
                    loaded: Some(workspace),
                },
            )])),
            changed,
            notices,
        })
    }

    fn save(&self, entries: &BTreeMap<String, Entry>) -> Result<()> {
        let registry = Registry {
            research: self.workshop.settings.clone(),
            workspaces: entries
                .iter()
                .map(|(id, entry)| (id.clone(), entry.known.clone()))
                .collect(),
        };
        fs::create_dir_all(&self.state_dir)?;
        let temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        fs::write(temp.path(), toml::to_string_pretty(&registry)?)?;
        crate::durable::persist(temp, self.state_dir.join("workspaces.toml"))?;
        Ok(())
    }

    pub fn list(&self) -> Vec<WorkspaceInfo> {
        let entries = self.entries.lock().unwrap();
        let mut workspaces: Vec<_> = entries
            .iter()
            .filter(|(_, entry)| !entry.known.hidden)
            .map(|(id, entry)| WorkspaceInfo {
                id: id.clone(),
                path: entry.known.path.to_string_lossy().into_owned(),
                title: entry.known.title.clone(),
                last_opened: entry.known.last_opened,
                running: entry
                    .loaded
                    .as_ref()
                    .is_some_and(|ws| ws.hub.has_running_turns()),
            })
            .collect();
        workspaces.sort_by(|a, b| {
            b.last_opened
                .cmp(&a.last_opened)
                .then(a.title.cmp(&b.title))
                .then(a.id.cmp(&b.id))
        });
        workspaces
    }

    pub(crate) fn request_destination(&self, id: &str) -> Result<fuzukue_proto::WorkspaceRequestDestination> {
        let entries = self.entries.lock().unwrap();
        let entry = entries.get(id).filter(|entry| !entry.known.hidden)
            .context("Workspace request destination is unavailable")?;
        ensure!(checked_dir(&self.home, &entry.known.path)? == entry.known.path,
            "Workspace request destination moved");
        Ok(fuzukue_proto::WorkspaceRequestDestination::Workspace {
            id: id.into(), title: entry.known.title.clone(),
        })
    }

    pub async fn codex_models(&self, workspace: &Workspace) -> Vec<fuzukue_proto::CodexModel> {
        if crate::canary() {
            return crate::codex::cached_models();
        }
        self.models
            .get_or_init(|| async {
                // Model discovery only needs one temporary process per server.
                #[cfg(test)]
                let _ = workspace;
                #[cfg(not(test))]
                {
                    let hub = Arc::new(Hub::new());
                    let usage = Arc::new(crate::Usage::new(hub.clone()));
                    if let Ok(codex) =
                        crate::Codex::spawn(&workspace.project_dir, hub, usage, Arc::default())
                            .await
                    {
                        return codex.models().await;
                    }
                }
                crate::codex::cached_models()
            })
            .await
            .clone()
    }

    pub fn complete(&self, prefix: &str) -> Vec<String> {
        let (location, stem) = prefix
            .rsplit_once('/')
            .map_or(("", prefix), |(dir, stem)| (dir, stem));
        let location_to_list = match (location, prefix.starts_with('/')) {
            ("", true) => "/",
            ("", false) => "~",
            (location, _) => location,
        };
        let Ok(dir) = self.browse_dir(location_to_list) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                if !name.starts_with(stem) || (name.starts_with('.') && !stem.starts_with('.')) {
                    return None;
                }
                checked_dir(&self.home, &entry.path()).ok()?;
                Some(if prefix.contains('/') {
                    format!("{location}/{name}/")
                } else {
                    format!("{name}/")
                })
            })
            .collect();
        paths.sort();
        paths.truncate(8);
        paths
    }

    pub fn browse(&self, path: &str, hidden: bool) -> Result<WorkspaceDirectory> {
        let dir = self.browse_dir(path)?;
        let workspaces: HashSet<_> = self.entries.lock().unwrap().iter()
            .filter(|(_, entry)| !entry.known.hidden)
            .map(|(id, _)| id.clone()).collect();
        let mut listing = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !hidden && name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let folder = if kind.is_file() {
                None
            } else if let Ok(folder) = checked_dir(&self.home, &entry.path()) {
                Some(folder)
            } else {
                continue;
            };
            listing.push(WorkspaceEntry {
                name,
                directory: folder.is_some(),
                workspace: folder.as_ref().is_some_and(|p| workspaces.contains(&id_for(p))),
                git: folder.as_ref().is_some_and(|p| {
                    p.join(".git").symlink_metadata().is_ok_and(|entry| entry.is_dir() || entry.is_file())
                }),
                fuzukue: folder.as_ref().is_some_and(|p| {
                    fuzukue_proto::compat::workspace_config(p).symlink_metadata().is_ok_and(|entry| entry.is_file())
                }),
            });
        }
        listing.sort_by_cached_key(|entry| (!entry.directory, entry.name.to_lowercase(), entry.name.clone()));
        Ok(WorkspaceDirectory { path: self.home_relative(&dir)?, entries: listing })
    }

    pub fn create_in(&self, parent: &str, name: String, kind: WorkspaceKind) -> Result<String> {
        let parent = self.browse_dir(parent)?;
        let destination = parent.join(workspace_name(&name)?);
        let path = self.home_relative(&destination)?;
        let (_, created) = self.create(CreateWorkspace { name, kind, path: Some(path) })?;
        self.home_relative(&created)
    }

    fn browse_dir(&self, path: &str) -> Result<PathBuf> {
        let path = resolve(&self.home, path)?;
        if path == self.home {
            Ok(path)
        } else {
            checked_dir(&self.home, &path)
        }
    }

    fn home_relative(&self, path: &Path) -> Result<String> {
        let relative = path.strip_prefix(&self.home)?;
        if relative.as_os_str().is_empty() {
            Ok("~".into())
        } else {
            Ok(format!("~/{}", relative.to_str().context("UTF-8 のパスを指定する")?))
        }
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(id).context("Workspace not found")?;
        if !entry.known.hidden {
            entry.known.hidden = true;
            if let Err(error) = self.save(&entries) {
                entries.get_mut(id).unwrap().known.hidden = false;
                return Err(error);
            }
            self.changed.send_replace(());
        }
        Ok(())
    }

    pub fn register(&self, path: &str) -> Result<String> {
        let path = checked_dir(&self.home, &resolve(&self.home, path)?)?;
        let id = id_for(&path);
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(&id) {
            entries.insert(
                id.clone(),
                Entry {
                    known: known(path),
                    loaded: None,
                },
            );
            if let Err(err) = self.save(&entries) {
                entries.remove(&id);
                return Err(err);
            }
            self.changed.send_replace(());
        }
        if entries[&id].known.hidden {
            entries.get_mut(&id).unwrap().known.hidden = false;
            if let Err(error) = self.save(&entries) {
                entries.get_mut(&id).unwrap().known.hidden = true;
                return Err(error);
            }
            self.changed.send_replace(());
        }
        Ok(id)
    }

    pub fn create(&self, args: CreateWorkspace) -> Result<(String, PathBuf)> {
        let name = workspace_name(&args.name)?;
        let requested = match &args.path {
            Some(path) => resolve(&self.home, path)?,
            None => self.home.join(name),
        };
        let (parent, destination, target, nested) = creation_paths(&self.home, &requested)?;
        let id = id_for(&target);

        let staging = tempfile::Builder::new()
            .prefix(".fuzukue-workspace-")
            .tempdir_in(&parent)?;
        let staged_workspace = staging.path().join(nested);
        fs::create_dir_all(&staged_workspace)?;
        write_skeleton(&staged_workspace, name, args.kind)?;
        let git = Command::new("git")
            .args(["init", "--quiet", "--template="])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(staged_workspace)
            .output()
            .context("git init を実行できない")?;
        ensure!(
            git.status.success(),
            "git init に失敗した: {}",
            String::from_utf8_lossy(&git.stderr).trim()
        );

        let mut entries = self.entries.lock().unwrap();
        ensure!(!entries.contains_key(&id), "保存先はすでに登録されている");
        ensure!(
            parent.canonicalize()? == parent,
            "親ディレクトリのパスが変わった"
        );
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        renameat_with(
            CWD,
            staging.path(),
            CWD,
            &destination,
            RenameFlags::NOREPLACE,
        )
        .context("ワークスペースを指定した場所に作れない")?;
        entries.insert(
            id.clone(),
            Entry {
                known: Known {
                    path: target.clone(),
                    title: name.to_owned(),
                    last_opened: None,
                    hidden: false,
                },
                loaded: None,
            },
        );
        if let Err(err) = self.save(&entries) {
            entries.remove(&id);
            // 同じファイルシステム内でステージへ戻し、TempDir に片づけさせる。
            if let Err(rollback) = renameat_with(
                CWD,
                &destination,
                CWD,
                staging.path(),
                RenameFlags::NOREPLACE,
            ) {
                anyhow::bail!(
                    "登録に失敗した: {err:#}。{} に作成したファイルが残っている: {rollback}",
                    target.display()
                );
            }
            return Err(err);
        }
        self.changed.send_replace(());
        Ok((id, target))
    }

    pub fn select(self: &Arc<Self>, id: Option<&str>) -> Result<Arc<Workspace>> {
        let id = id.unwrap_or(&self.default);
        let workspace = self.load(id)?;
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(id).unwrap();
        let previous = entry.known.last_opened.replace(crate::usage::now());
        if let Err(err) = self.save(&entries) {
            entries.get_mut(id).unwrap().known.last_opened = previous;
            return Err(err);
        }
        self.changed.send_replace(());
        Ok(workspace)
    }

    pub(crate) fn user_extension_config_changed(&self) {
        let loaded: Vec<_> = self.entries.lock().unwrap().values()
            .filter_map(|entry| entry.loaded.clone())
            .collect();
        for app in loaded {
            crate::extensions::invalidate_config(&app);
        }
    }

    fn loaded_workspaces(&self) -> Vec<Arc<Workspace>> {
        self.entries.lock().unwrap().values().filter_map(|entry| entry.loaded.clone()).collect()
    }

    pub(crate) fn try_loaded_workspaces(&self) -> Option<Vec<Arc<Workspace>>> {
        Some(self.entries.try_lock().ok()?.values().filter_map(|entry| entry.loaded.clone()).collect())
    }

    pub(crate) fn held_planner_count(&self) -> usize {
        self.loaded_workspaces().iter()
            .filter(|workspace| workspace.hold_planner_during_update)
            .map(|workspace| workspace.drain.queued_planner_count(&workspace.state_dir))
            .sum()
    }

    pub(crate) fn shutdown_limit(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.shutdown_drain_seconds.get().copied().unwrap_or(crate::remote::DEFAULT_DRAIN_SECONDS))
    }

    pub(crate) fn last_partner_progress(&self) -> Option<tokio::time::Instant> {
        self.loaded_workspaces().into_iter().filter_map(|workspace| workspace.hub.last_partner_progress()).max()
    }

    pub(crate) fn drain_job_count(&self, reason: &fuzukue_proto::SelfUpdateWaitReason) -> usize {
        let mut count = self.mutations.count() + reason.threads.iter().filter(|thread| thread.running).count();
        for workspace in self.loaded_workspaces() {
            count += Self::background_job_count(&workspace);
        }
        count.max(usize::from(reason.gate_held))
    }

    fn background_job_count(workspace: &Workspace) -> usize {
        workspace.delegates.running_count() + Self::background_operation_count(workspace)
    }

    pub(crate) fn background_operation_count(workspace: &Workspace) -> usize {
        workspace.running_test_count() + usize::from(workspace.active_build().is_some()) + workspace.running_review_count()
    }

    pub(crate) fn shutdown_work(&self) -> fuzukue_proto::ShutdownWork {
        let mut threads = Vec::new();
        let mut jobs_running = self.mutations.count();
        for workspace in self.loaded_workspaces() {
            let workers = workspace.delegates.running_sessions();
            let sessions = workspace.sessions.try_lock().ok();
            threads.extend(workspace.hub.swap_threads(&workspace.id, sessions.as_deref(), &workers)
                .into_iter().filter(|thread| thread.running || thread.worker));
            jobs_running += Self::background_job_count(&workspace);
        }
        jobs_running += threads.iter().filter(|thread| thread.running).count();
        threads.sort_by(|a, b| (&a.workspace, &a.id).cmp(&(&b.workspace, &b.id)));
        fuzukue_proto::ShutdownWork { jobs_running, threads, deadline_secs: self.shutdown_limit().as_secs() }
    }

    pub(crate) fn swap_blockers(&self) -> fuzukue_proto::SelfUpdateWaitReason {
        Self::workspace_blockers(&self.loaded_workspaces(), BlockerScan::Drain)
    }

    pub(crate) fn workspace_blockers(workspaces: &[Arc<Workspace>], scan: BlockerScan) -> fuzukue_proto::SelfUpdateWaitReason {
        let planned = scan != BlockerScan::Drain;
        let mut reason = fuzukue_proto::SelfUpdateWaitReason::default();
        for workspace in workspaces {
            let workers = workspace.delegates.running_sessions();
            let sessions = workspace.sessions.try_lock().ok();
            reason.checking_sessions |= sessions.is_none();
            reason.threads.extend(workspace.hub.swap_threads(&workspace.id, sessions.as_deref(), &workers));
            let mut blockers = workspace.hub.drain_blockers(&workspace.id);
            for blocker in &mut blockers {
                if blocker.kind == fuzukue_proto::SelfUpdateBlockerKind::Worker && blocker.title.is_none() {
                    blocker.title = workspace.delegates.drain_title(&blocker.id);
                }
            }
            reason.blockers.extend(blockers);
            let is_live = |session: &str| workspace.hub.read_threads(|store| store.is_live(session));
            // Preflight checks runnable journal entries before closing admission.
            // At cutoff, new journal entries wait for replay. The caller still
            // checks journal read and save errors before declaring readiness.
            if scan != BlockerScan::Cutoff {
                if let Ok(records) = workspace.drain.records(&workspace.state_dir) {
                    use crate::self_update::drain::Work;
                    let now = crate::self_update::drain::delivery_now();
                    let planner_held = planned && workspace.planner_work_held();
                    for record in records {
                        let background = matches!(record.work, Work::Build { .. } | Work::HumanReview { .. });
                        // Deferred automatic reviews can be reopened from their saved baseline.
                        let deferred_review = matches!(record.work, Work::AutomaticReview { .. });
                        if !record.claimed && record.review.is_none()
                            && !matches!(record.work, Work::Checkpoint { .. } | Work::Usage(_)
                                | Work::LegacyPlan(_) | Work::LegacyGitHub(_) | Work::LegacyWatch { .. })
                            && (!planned || background || record.ready(now) && is_live(&record.session)
                                && !deferred_review
                                && (!record.work.is_planner()
                                    || !planner_held && !crate::planner::is_paused(workspace, &record.session))
                                && (!matches!(&record.work, Work::Desk(turn) if turn.is_worker_report())
                                    || !crate::planner::is_paused(workspace, &record.session)))
                        {
                            update_thread_reason(workspace, &mut reason, &record.session).queued = true;
                        }
                    }
                }
            }
            // The journal entries above cover durable desk work. In-memory
            // entries still need a check even when their thread was archived.
            for session in workspace.delegates.queued_desk_sessions(!planned) {
                update_thread_reason(workspace, &mut reason, &session).queued = true;
            }
            for session in workspace.watches.held_threads(planned) {
                if !planned || is_live(&session) {
                    update_thread_reason(workspace, &mut reason, &session).queued = true;
                }
            }
            if planned {
                for session in crate::issue_threads::queued_for_dispatch(workspace) {
                    update_thread_reason(workspace, &mut reason, &session).queued = true;
                }
            }
        }
        reason.threads.retain(|thread| thread.running || thread.worker || thread.checking || thread.queued || thread.paused);
        reason.threads.sort_by(|a, b| (&a.workspace, &a.id).cmp(&(&b.workspace, &b.id)));
        reason
    }

    /// Segmented threads of the workspaces that are open now.
    pub(crate) fn segmented_threads(&self) -> Vec<String> {
        self.loaded_workspaces().iter().flat_map(|workspace| {
            workspace.hub.read_threads(|store| store.segmented_threads().collect::<Vec<_>>())
        }).collect()
    }

    pub(crate) fn protect_legacy_rollback(&self) -> Result<()> {
        for workspace in self.loaded_workspaces() {
            crate::watch::protect_legacy_rollback(&workspace)?;
            let records = workspace.drain.records(&workspace.state_dir)?;
            let mut sessions: std::collections::BTreeSet<_> = workspace.hub.plan_sessions().into_iter().collect();
            workspace.hub.edit_threads(|store| {
                for (session, _) in store.github_threads() {
                    if let Some(hold) = store.github_holds(&session) {
                        workspace.drain.enqueue(&workspace.state_dir, &session, format!("legacy-github:{session}"),
                            crate::self_update::drain::Work::LegacyGitHub(hold))?;
                        store.protect_github_for_rollback(&session)?;
                        sessions.insert(session);
                    }
                }
                Ok(())
            })?;
            sessions.extend(records.iter().filter(|record| workspace.hub.has_thread(&record.session))
                .map(|record| record.session.clone()));
            for session in sessions {
                crate::planner::protect_legacy_rollback(&workspace, &session)?;
                let note = format!("Held work and checkpoints are in {}. Start a server with drain support to recover unclaimed work. Review claimed entries before retrying.",
                    workspace.state_dir.join("held-work.json").display());
                workspace.hub.deliver_system_note(&session, "legacy-rollback:held-work", note)?;
            }
        }
        Ok(())
    }

    pub(crate) fn drain_errors(&self, reason: &mut fuzukue_proto::SelfUpdateWaitReason) -> Vec<String> {
        Self::workspace_drain_errors(&self.loaded_workspaces(), reason)
    }

    pub(crate) fn workspace_drain_errors(workspaces: &[Arc<Workspace>], reason: &mut fuzukue_proto::SelfUpdateWaitReason) -> Vec<String> {
        let mut errors = Vec::new();
        for workspace in workspaces {
            if let Some(error) = workspace.test_save_error() {
                errors.push(format!("{}: saving test results: {error}", workspace.id));
            }
            for (session, error) in workspace.drain.errors() {
                if session.is_empty() {
                    errors.push(format!("{}: {error}", workspace.id));
                } else {
                    update_thread_reason(&workspace, reason, &session).checking = true;
                    errors.push(format!("{} / {session}: saving work: {error}", workspace.id));
                }
            }
            if let Err(error) = workspace.drain.records(&workspace.state_dir) {
                reason.checking_sessions = true;
                errors.push(format!("{}: reading held work: {error:#}", workspace.id));
            }
        }
        errors
    }

    pub(crate) async fn prepare_drain(&self, wait: i64, reason: fuzukue_proto::DrainReason) {
        self.notify_drain(wait, reason, false).await;
        self.retry_drain_saves();
    }

    pub(crate) async fn flush_dirty_transcripts(&self) -> Result<()> {
        let workspaces = self.try_loaded_workspaces().context("workspace registry is busy")?;
        tokio::task::spawn_blocking(move || {
            let errors: Vec<_> = workspaces.into_iter().filter_map(|workspace| {
                workspace.hub.flush_dirty_transcripts(false).err()
                    .map(|error| format!("{}: {error:#}", workspace.id))
            }).collect();
            ensure!(errors.is_empty(), "{}", errors.join("\n"));
            Ok(())
        }).await?
    }

    pub(crate) async fn finish_transcript_flush(&self) -> Result<()> {
        let flush = async {
            for _ in 0..2 {
                if self.flush_dirty_transcripts().await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            self.flush_dirty_transcripts().await
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), flush).await
            .context("timed out saving dirty transcripts at shutdown")?
    }

    pub(crate) fn retry_drain_saves(&self) {
        for workspace in self.loaded_workspaces() {
            workspace.drain.retry(&workspace.state_dir);
            let windows = workspace.usage.current().into_iter()
                .filter(|window| window.resets_at.is_some()).collect();
            workspace.drain.save_usage(&workspace.state_dir, windows);
            workspace.retry_test_saves();
        }
    }

    pub(crate) async fn notify_drain_cancelled(&self, wait: i64, reason: fuzukue_proto::DrainReason) {
        self.notify_drain(wait, reason, true).await;
    }

    async fn notify_drain(&self, wait: i64, reason: fuzukue_proto::DrainReason, cancelled: bool) {
        let workspaces = self.loaded_workspaces();
        futures_util::future::join_all(workspaces.iter().map(|workspace| async {
            workspace.delegates.notify_drain(wait, reason, cancelled);
            let claude: Vec<_> = workspace.sessions.try_lock().ok()
                .map(|sessions| sessions.values().cloned().collect()).unwrap_or_default();
            for session in claude {
                session.notify_drain(wait, reason, cancelled).await;
            }
            if let Some(codex) = workspace.codex.try_lock().ok().and_then(|codex| codex.clone()) {
                let active = workspace.hub.chat_activity().0;
                let sessions: Vec<_> = workspace.session_records.lock().unwrap().iter()
                    .filter_map(|(session, record)| active.contains(session).then_some((session.clone(), record.codex_thread_id.clone()?)))
                    .collect();
                for (session, thread) in sessions {
                    if (workspace.drain_codex.lock().unwrap().get(&session) == Some(&wait)) != cancelled {
                        continue;
                    }
                    let text = crate::self_update::drain::instruction(wait, reason, &thread, cancelled);
                    if tokio::time::timeout(std::time::Duration::from_millis(200),
                        codex.steer_with_images(&thread, &text, &[])).await.is_ok_and(|result| result.is_ok())
                    {
                        let mut notices = workspace.drain_codex.lock().unwrap();
                        if cancelled { notices.remove(&session); } else { notices.insert(session, wait); }
                    }
                }
            }
        })).await;
    }

    pub(crate) async fn interrupt_for_swap(&self, wait: i64, reap: bool) -> Result<()> {
        self.mutations.interrupt().await;
        let workspaces = self.loaded_workspaces();
        futures_util::future::try_join_all(workspaces.iter().map(|workspace| async {
            workspace.delegates.interrupt_for_swap(wait);
            workspace.stop_tests_for_swap();
            workspace.stop_builds_for_swap();
            workspace.stop_reviews_for_swap();
            let sessions: Vec<_> = workspace.sessions.lock().await.values().cloned().collect();
            tokio::try_join!(
                futures_util::future::try_join_all(sessions.iter().map(|session| session.interrupt_for_swap(wait, reap))),
                async {
                    if let Some(codex) = workspace.running_codex().await {
                        codex.interrupt_for_swap().await?;
                    }
                    Ok(())
                },
            )?;
            Ok::<_, anyhow::Error>(())
        })).await?;
        Ok(())
    }

    pub(crate) fn stop_reviews(&self) {
        for workspace in self.entries.lock().unwrap().values().filter_map(|entry| entry.loaded.as_ref()) {
            workspace.stop_reviews();
        }
    }

    pub fn loaded(&self, id: Option<&str>) -> Result<Arc<Workspace>> {
        let entries = self.entries.lock().unwrap();
        let entry = entries
            .get(id.unwrap_or(&self.default))
            .context("ワークスペースが見つからない")?;
        let ws = entry
            .loaded
            .clone()
            .context("ワークスペースが開かれていない")?;
        ensure!(
            checked_dir(&self.home, &entry.known.path)? == ws.project_dir,
            "ワークスペースのパスが変わった"
        );
        Ok(ws)
    }

    pub async fn route(self: Arc<Self>, id: Option<String>) -> Result<Arc<Workspace>> {
        match id {
            None => tokio::task::spawn_blocking(move || self.load(&self.default)).await?,
            Some(id) => self.loaded(Some(&id)),
        }
    }

    pub(crate) fn has_workspace(&self, id: &str) -> bool {
        self.entries.lock().unwrap().contains_key(id)
    }

    pub fn known_workspace(self: &Arc<Self>, id: &str) -> Result<Arc<Workspace>> {
        self.load(id)
    }

    pub(crate) fn restore_watches(self: &Arc<Self>) {
        let intake_workspaces = crate::issue_threads::restored_workspaces(&self.state_dir);
        let projects: Vec<_> = self.entries.lock().unwrap().iter()
            .filter(|(_, entry)| entry.loaded.is_none())
            .map(|(id, entry)| (id.clone(), entry.known.path.clone()))
            .collect();
        for (id, path) in projects {
            if checked_dir(&self.home, &path).as_ref().is_ok_and(|resolved| *resolved == path)
                && (crate::watch::configured(&path, &self.workspace_state_dir(&id))
                    || intake_workspaces.contains(&id)
                    || crate::self_update::drain::Journal::exists(&self.workspace_state_dir(&id)))
            {
                if let Err(error) = self.load(&id) {
                    tracing::warn!(workspace = %id, %error, "could not restore watches");
                }
            }
        }
    }

    fn workspace_state_dir(&self, id: &str) -> PathBuf {
        if id == self.default {
            self.state_dir.clone()
        } else {
            self.state_dir.join("workspaces").join(id)
        }
    }

    pub(crate) fn intake_state_root(&self) -> &Path {
        &self.state_dir
    }

    fn load(self: &Arc<Self>, id: &str) -> Result<Arc<Workspace>> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries
            .get_mut(id)
            .context("ワークスペースが見つからない")?;
        let path = checked_dir(&self.home, &entry.known.path)?;
        ensure!(path == entry.known.path, "ワークスペースのパスが変わった");
        if let Some(workspace) = &entry.loaded {
            return Ok(workspace.clone());
        }
        let state_dir = self.workspace_state_dir(id);
        let hub = Arc::new(Hub::with_timeline(&state_dir));
        hub.watch_activity(self.changed.clone());
        hub.attach_notification_feed(id, self.notices.clone());
        let hold_planner_during_update = crate::delegate::admission::Overrides::load_workspace(&path)?
            .hold_planner_during_update.unwrap_or(self.hold_planner_during_update);
        let root = self.state_dir.parent().unwrap_or(&self.state_dir);
        let mut workspace = Workspace::new_with_root(path, self.addr.clone(), state_dir, hub, root);
        workspace.hold_planner_during_update = hold_planner_during_update;
        workspace.registry = Arc::downgrade(self);
        #[cfg(test)]
        { workspace.claude_program = self.test_claude_program.get().cloned(); }
        workspace.mail = self.mail.clone();
        if id == self.default {
            *workspace.deck.write().unwrap() = fuzukue_proto::compat::env("FUZUKUE_DECK").unwrap_or_default();
        }
        if !crate::canary() {
            if let Err(error) = workspace.extensions.list(&workspace, "en") {
                tracing::warn!(%error, "could not load extensions");
            }
        }
        let workspace = Arc::new(workspace);
        entry.loaded = Some(workspace.clone());
        #[cfg(not(test))]
        if !crate::canary() {
            crate::self_update::drain::start(&workspace);
        }
        drop(entries);
        #[cfg(not(test))]
        workspace.start_watchers();
        Ok(workspace)
    }
}

fn update_thread_reason<'a>(
    workspace: &Workspace, reason: &'a mut fuzukue_proto::SelfUpdateWaitReason, session: &str,
) -> &'a mut fuzukue_proto::SelfUpdateBlockingThread {
    let index = reason.threads.iter().position(|thread| thread.workspace == workspace.id && thread.id == session);
    if let Some(index) = index {
        return &mut reason.threads[index];
    }
    reason.threads.push(fuzukue_proto::SelfUpdateBlockingThread {
        workspace: workspace.id.clone(), id: session.into(),
        title: workspace.hub.read_threads(|store| store.title(session)),
        running: false, worker: false, queued: false, paused: false, checking: false,
    });
    reason.threads.last_mut().unwrap()
}

fn workspace_name(name: &str) -> Result<&str> {
    let name = name.trim();
    ensure!(
        matches!(Path::new(name).components().collect::<Vec<_>>().as_slice(), [Component::Normal(part)] if *part == name)
            && !name.chars().any(char::is_control),
        "name にはフォルダ名を一つ指定する"
    );
    Ok(name)
}

/// Build the whole missing subtree in a temporary directory so a failed git init
/// cannot leave a partial workspace or empty parent directories.
fn creation_paths(home: &Path, requested: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    use std::io::ErrorKind::NotFound;

    match requested.symlink_metadata() {
        Ok(_) => anyhow::bail!("保存先はすでに存在する"),
        Err(err) if err.kind() == NotFound => {}
        Err(err) => return Err(err.into()),
    }
    for ancestor in requested.ancestors().skip(1) {
        match ancestor.symlink_metadata() {
            Err(err) if err.kind() == NotFound => continue,
            Err(err) => return Err(err.into()),
            Ok(_) => {}
        }
        let parent = ancestor.canonicalize()?;
        if parent != home {
            checked_dir(home, ancestor)?;
        }
        let suffix = requested.strip_prefix(ancestor)?;
        let mut components = suffix.components();
        let Some(Component::Normal(first)) = components.next() else {
            anyhow::bail!("パスの末尾にフォルダ名を指定する");
        };
        let destination = parent.join(first);
        let target = parent.join(suffix);
        return Ok((
            parent,
            destination,
            target,
            components.as_path().to_path_buf(),
        ));
    }
    anyhow::bail!("親ディレクトリがない")
}

fn write_skeleton(root: &Path, name: &str, kind: WorkspaceKind) -> Result<()> {
    fs::write(
        root.join("fuzukue.toml"),
        toml::to_string(&BTreeMap::from([("title", name)]))?,
    )?;
    match kind {
        WorkspaceKind::Empty => {}
        WorkspaceKind::Software => {
            fs::write(root.join("README.md"), format!("# {name}\n"))?;
            fs::write(
                root.join("AGENTS.md"),
                "# 作業の指示\n\nプロジェクト固有の作業手順や検証方法をここに記録する。\n",
            )?;
        }
        WorkspaceKind::Research | WorkspaceKind::Paper => {
            let (path, kind, status, card_id, card_kind, card_title) = match kind {
                WorkspaceKind::Research => (
                    "research/study",
                    "research",
                    "idea",
                    "question",
                    "question",
                    "研究の問い",
                ),
                _ => ("papers/draft", "paper", "drafting", "intro", "expl", "序"),
            };
            let deck = root.join(path);
            let card = deck.join("cards").join(card_kind).join(card_id);
            fs::create_dir_all(&card)?;
            fs::write(
                deck.join("deck.toml"),
                toml::to_string(&BTreeMap::from([
                    ("title", name),
                    ("kind", kind),
                    ("status", status),
                ]))?,
            )?;
            fs::write(card.join("card.md"),
                format!("---\nid: {card_id}\nkind: {card_kind}\ntitle: {card_title}\nstatus: draft\n---\n"))?;
            if kind == "paper" {
                fs::write(card.join("statement.tex"), "% 背景と目的を書く。\n")?;
                fs::write(
                    deck.join("structure.yaml"),
                    format!(
                        "{}sections:\n- title: 序\n  items: [intro]\n",
                        serde_yaml::to_string(&BTreeMap::from([("title", name)]))?
                    ),
                )?;
            }
        }
    }
    Ok(())
}

pub fn id_for(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

fn known(path: PathBuf) -> Known {
    let title = crate::safe_join(&path, &fuzukue_proto::compat::workspace_config(&path).to_string_lossy())
        .ok()
        .and_then(|config| fs::read_to_string(config).ok())
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|config| {
            config
                .get("title")
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .map(str::to_owned)
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        });
    Known {
        path,
        title,
        last_opened: None,
        hidden: false,
    }
}

fn resolve(home: &Path, path: &str) -> Result<PathBuf> {
    let path = Path::new(path.trim());
    ensure!(
        path.components().next().is_some(),
        "ワークスペースのパスを入力する"
    );
    ensure!(
        !path
            .components()
            .any(|part| part == std::path::Component::ParentDir),
        "パスに .. は使えない"
    );
    if let Ok(relative) = path.strip_prefix("~") {
        Ok(home.join(relative))
    } else {
        Ok(home.join(path))
    }
}

pub fn checked_dir(home: &Path, path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().context("ディレクトリを開けない")?;
    ensure!(
        path.is_dir() && path != home && path.starts_with(home),
        "ホーム配下のディレクトリを指定する"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn browser_lists_workspace_kinds_and_creates_only_under_allowed_folders() -> Result<()> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        fs::create_dir_all(home.join("project"))?;
        fs::create_dir_all(home.join(".hidden"))?;
        fs::write(home.join("project/.git"), "gitdir: /worktree")?;
        fs::write(home.join("project/fuzukue.toml"), "")?;
        fs::write(home.join("a-file"), "")?;
        symlink(root.path(), home.join("outside"))?;
        symlink(home.join("project"), home.join("alias"))?;
        let registry = App::new(home.clone(), home.join("project"), root.path().join("state"), String::new(), None)?;

        let listing = registry.browse("~", false)?;
        assert_eq!(listing.path, "~");
        assert_eq!(listing.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["alias", "project", "a-file"]);
        assert!(listing.entries[..2].iter().all(|e| e.directory && e.workspace && e.git && e.fuzukue));
        assert!(!listing.entries[2].directory);
        assert_eq!(registry.browse("~/alias", false)?.path, "~/project");
        assert_eq!(registry.browse("~", true)?.entries[0].name, ".hidden");

        for path in ["~/outside", "~/project/.."] {
            assert!(registry.browse(path, true).is_err());
            assert!(registry.create_in(path, "new".into(), WorkspaceKind::Empty).is_err());
        }
        assert!(registry.create_in("~", "nested/unsafe".into(), WorkspaceKind::Empty).is_err());

        assert_eq!(registry.create_in("~/project", "paper".into(), WorkspaceKind::Paper)?, "~/project/paper");
        assert!(home.join("project/paper/papers/draft/deck.toml").is_file());
        assert!(registry.create_in("~/project", "paper".into(), WorkspaceKind::Empty).is_err());
        let listing = registry.browse("~/project", false)?;
        assert!(listing.entries.iter().any(|e| e.name == "paper" && e.workspace && e.git && e.fuzukue));
        Ok(())
    }

    #[test]
    fn workspace_paths_must_resolve_inside_home() -> Result<()> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        fs::create_dir_all(home.join("lab"))?;
        fs::create_dir_all(home.join("research/material"))?;
        fs::create_dir_all(&outside)?;
        symlink(&outside, home.join("escape"))?;
        symlink(home.join("research/material"), home.join("protected"))?;
        symlink(home.join("lab"), home.join("alias"))?;

        let registry = App::new(
            home.clone(),
            home.join("lab"),
            root.path().join("state"),
            String::new(),
            None,
        )?;
        for path in [
            outside.to_str().unwrap(),
            "~/escape",
            "~",
            "../outside",
        ] {
            assert!(registry.register(path).is_err(), "{path}");
        }
        assert_eq!(registry.register("~/alias")?, registry.default);
        assert_eq!(registry.list().len(), 1);
        // Nested workspaces and symlinks within the home directory are allowed.
        assert!(registry.register("~/research/material").is_ok());
        assert!(registry.register("~/protected").is_ok());
        Ok(())
    }
}
