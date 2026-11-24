use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use fuzukue_cards::{DeckConfig, HarnessBuild};
use fuzukue_proto::ServerEvent;
use serde::Deserialize;
use tokio::time::Instant;

use crate::build::{self, Built};
use crate::cancellation::{Cancellation, Cancelled};
use crate::{auto_paths, cards, safe_join, Workspace};

const DEBOUNCE: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct Builds {
    pub(crate) serial: tokio::sync::Mutex<()>,
    pending: Mutex<HashMap<String, Pending>>,
    active: Mutex<Option<(String, Arc<Cancellation>)>>,
    batch: Mutex<Option<Arc<Cancellation>>>,
    auto_failure: Mutex<Option<String>>,
}

struct Pending {
    deadline: Instant,
    cancel: Arc<Cancellation>,
}

#[derive(Deserialize, Default)]
struct Settings {
    #[serde(default)]
    build: HarnessBuild,
}

fn auto_enabled(project: &Path, cfg: &DeckConfig) -> Result<bool> {
    if fuzukue_proto::compat::env("FUZUKUE_AUTO_BUILD").as_deref() == Ok("0") {
        return Ok(false);
    }
    let project_auto = match std::fs::read_to_string(fuzukue_proto::compat::workspace_config(project)) {
        Ok(text) => toml::from_str::<Settings>(&text)?.build.auto,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(cfg
        .meta
        .build
        .as_ref()
        .and_then(|build| build.auto)
        .or(project_auto)
        .unwrap_or(true))
}

pub(crate) fn card_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("md" | "tex")
    )
}

pub(crate) fn changed_decks(project: &Path, paths: HashSet<PathBuf>) -> HashSet<String> {
    let paths = match auto_paths::eligible(project, paths) {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!("automatic PDF builds: {error:#}");
            return HashSet::new();
        }
    };
    let mut configs = HashMap::new();
    let mut decks = HashSet::new();
    for path in paths {
        for root in path
            .ancestors()
            .skip(1)
            .take_while(|root| root.starts_with(project))
        {
            if !auto_paths::is_deck(root) {
                continue;
            }
            let cfg =
                configs.entry(root.to_path_buf()).or_insert_with(|| {
                    match DeckConfig::read(root).and_then(|cfg| {
                        Ok((cfg.harness && auto_enabled(project, &cfg)?).then_some(cfg))
                    }) {
                        Ok(cfg) => cfg,
                        Err(error) => {
                            tracing::warn!("自動ビルド {}: {error:#}", root.display());
                            None
                        }
                    }
                });
            if let Some(cfg) = cfg {
                let relative = path.strip_prefix(root).expect("ancestor");
                if !relative.starts_with("build")
                    && cfg.dirs.iter().any(|dir| path.starts_with(root.join(dir)))
                {
                    decks.insert(
                        root.strip_prefix(project)
                            .expect("project ancestor")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            break;
        }
    }
    decks
}

impl Workspace {
    pub(crate) fn schedule_folder_paper_build(self: &Arc<Self>, deck: String, source: &Path) {
        let Ok(root) = safe_join(&self.project_dir, &deck) else {
            return;
        };
        let Ok(cfg) = DeckConfig::read(&root) else {
            return;
        };
        if !cfg.harness
            && matches!(cfg.meta.kind.as_str(), "paper" | "book")
            && root.join("build").is_dir()
            && auto_enabled(&self.project_dir, &cfg).unwrap_or(false)
            && auto_paths::path_eligible(&self.project_dir, source)
        {
            self.schedule_build(deck);
        }
    }

    pub(crate) async fn paper_pdf_map(
        &self,
        deck: String,
        pdf: String,
        rebuilt: bool,
    ) -> Option<ServerEvent> {
        let root = safe_join(&self.project_dir, &deck).ok()?;
        let output = safe_join(&self.project_dir, &pdf).ok()?;
        if output.parent()? != root.join("build") || output.extension()?.to_str()? != "pdf" {
            return None;
        }
        let project = self.project_dir.clone();
        let (digest, regions) =
            tokio::task::spawn_blocking(move || crate::pdf_cards::read(&project, &output))
                .await
                .ok()?
                .ok()?;
        Some(ServerEvent::PaperPdfMap {
            deck,
            pdf,
            digest,
            regions,
            rebuilt,
        })
    }

    pub(crate) fn schedule_build(self: &Arc<Self>, deck: String) {
        let journal = &self.drain;
        let work = crate::self_update::drain::Work::Build { deck: deck.clone() };
        let id = crate::mail::session_key();
        let id = match journal.enqueue(&self.state_dir, &deck, id.clone(), work.clone()) {
            Ok(id) => id,
            Err(_) => {
                journal.defer(&self.state_dir, &deck, id, work);
                return;
            }
        };
        self.queue_build(deck, id, true);
    }

    pub(crate) fn restore_build(self: &Arc<Self>, deck: String, id: String) {
        self.queue_build(deck, id, false);
    }

    fn queue_build(self: &Arc<Self>, deck: String, id: String, reset_deadline: bool) {
        let mut pending = self.builds.pending.lock().unwrap();
        let deadline = Instant::now() + DEBOUNCE;
        if let Some(job) = pending.get_mut(&deck) {
            if reset_deadline { job.deadline = deadline; }
            return;
        }
        let cancel = Arc::new(Cancellation::default());
        pending.insert(
            deck.clone(),
            Pending {
                deadline,
                cancel: cancel.clone(),
            },
        );
        let app = self.clone();
        tokio::spawn(async move {
            let mut deadline = deadline;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep_until(deadline) => {}
                }
                let serial = tokio::select! {
                    _ = cancel.cancelled() => return,
                    lock = app.builds.serial.lock() => lock,
                };
                let Ok(_work) = app.admit_work() else {
                    let mut pending = app.builds.pending.lock().unwrap();
                    if pending.get(&deck).is_some_and(|job| Arc::ptr_eq(&job.cancel, &cancel)) {
                        pending.remove(&deck);
                    }
                    return;
                };
                let project = app.project_dir.clone();
                let path = project.join(&deck);
                let eligible = tokio::task::spawn_blocking(move || auto_paths::deck_eligible(&project, &path))
                    .await.unwrap_or(false);
                {
                    let mut pending = app.builds.pending.lock().unwrap();
                    let Some(job) = pending
                        .get(&deck)
                        .filter(|job| Arc::ptr_eq(&job.cancel, &cancel))
                    else {
                        return;
                    };
                    if job.deadline > Instant::now() {
                        deadline = job.deadline;
                        continue;
                    }
                    pending.remove(&deck);
                    if !eligible {
                        app.drain.finish(&app.state_dir, &deck, &id, None);
                        return;
                    }
                    if !matches!(app.drain.claim(&app.state_dir, &deck, &id), Ok(true)) {
                        return;
                    }
                    match DeckConfig::read(&app.project_dir.join(&deck))
                        .and_then(|cfg| Ok(auto_enabled(&app.project_dir, &cfg)?))
                    {
                        Ok(true) => app.start_build(&deck, cancel.clone()),
                        Ok(false) => {
                            app.drain.finish(&app.state_dir, &deck, &id, None);
                            return;
                        }
                        Err(error) => {
                            app.drain.finish(&app.state_dir, &deck, &id, Some(format!("{error:#}")));
                            tracing::warn!("自動ビルド {deck}: {error:#}");
                            return;
                        }
                    }
                }
                let _ = app.finish_build(deck.clone(), &cancel, true).await;
                app.drain.finish(&app.state_dir, &deck, &id, None);
                drop(serial);
                return;
            }
        });
    }

    pub(crate) fn cancel_build(&self, deck: &str) {
        let root = safe_join(&self.project_dir, deck).ok();
        let relative = root
            .as_ref()
            .and_then(|path| path.strip_prefix(&self.project_dir).ok());
        let mut pending = self.builds.pending.lock().unwrap();
        let queued = pending
            .remove(deck)
            .or_else(|| relative.and_then(|path| pending.remove(path.to_string_lossy().as_ref())));
        if let Some(job) = queued {
            job.cancel.cancel();
        }
        if let Ok(records) = self.drain.records(&self.state_dir) {
            for record in records {
                if !record.claimed && matches!(&record.work, crate::self_update::drain::Work::Build { deck: saved }
                    if saved == deck || relative.is_some_and(|path| path == std::path::Path::new(saved)))
                {
                    self.drain.finish(&self.state_dir, &record.session, &record.id, None);
                }
            }
        }
        if let Some((running, cancel)) = &*self.builds.active.lock().unwrap() {
            if running == deck
                || root.as_ref().is_some_and(|root| {
                    safe_join(&self.project_dir, running).is_ok_and(|path| &path == root)
                })
            {
                cancel.cancel();
            }
        }
    }

    pub(crate) fn stop_builds_for_swap(&self) {
        for (_, job) in self.builds.pending.lock().unwrap().drain() {
            job.cancel.cancel();
        }
        if let Some((_, cancel)) = &*self.builds.active.lock().unwrap() {
            cancel.cancel();
        }
        if let Some(cancel) = &*self.builds.batch.lock().unwrap() {
            cancel.cancel();
        }
    }

    pub(crate) fn build_paper(
        self: &Arc<Self>,
        deck: String,
    ) -> tokio::task::JoinHandle<Result<Built>> {
        let app = self.clone();
        tokio::spawn(async move {
            let _serial = app.builds.serial.lock().await;
            let _work = app.admit_work()?;
            let cancel = Arc::new(Cancellation::default());
            app.start_build(&deck, cancel.clone());
            app.finish_build(deck, &cancel, false).await
        })
    }

    pub(crate) fn build_papers(
        self: &Arc<Self>,
        output: Option<String>,
    ) -> tokio::task::JoinHandle<Result<build::batch::Report>> {
        let app = self.clone();
        tokio::spawn(async move {
            let _serial = app.builds.serial.lock().await;
            let cancel = Arc::new(Cancellation::default());
            let _work = {
                let mut batch = app.builds.batch.lock().unwrap();
                let work = app.admit_work()?;
                *batch = Some(cancel.clone());
                work
            };
            let result = build::batch::run(&app.project_dir, output, cancel).await;
            app.builds.batch.lock().unwrap().take();
            result
        })
    }

    pub(crate) fn active_build(&self) -> Option<ServerEvent> {
        self.builds
            .active
            .lock()
            .unwrap()
            .as_ref()
            .map(|(deck, _)| ServerEvent::BuildStatus {
                deck: deck.clone(),
                state: "running".into(),
                message: "PDF を作っている…".into(),
                pdf: None,
            })
    }

    fn start_build(&self, deck: &str, cancel: Arc<Cancellation>) {
        let mut active = self.builds.active.lock().unwrap();
        *active = Some((deck.into(), cancel));
        self.hub.publish(ServerEvent::BuildStarted { deck: deck.into() });
        self.build_status(deck, "running", "PDF を作っている…".into(), None);
    }

    async fn finish_build(
        &self,
        deck: String,
        cancel: &Cancellation,
        automatic: bool,
    ) -> Result<Built> {
        let mut result = build::build_cancellable(&self.project_dir, &deck, cancel).await;
        if let Some(built) = result.as_mut().ok().filter(|built| built.ok && cancel.check().is_ok()) {
            for pdf in std::iter::once(&built.pdf).chain(&built.additional_pdfs) {
                built.pdf_audit.push(crate::pdf_audit::audit(&self.project_dir, pdf).await);
            }
        }
        self.report_build(deck, &result, automatic).await;
        result
    }

    async fn report_build(&self, deck: String, result: &Result<Built>, automatic: bool) {
        let (ok, pdf, log) = match &result {
            Ok(built) => (built.ok, built.pdf.clone(), built.log.clone()),
            Err(error) => (false, String::new(), format!("{error:#}")),
        };
        if ok {
            // latexmk は生成した TeX が変わらないと PDF を書き直さない。PDF の時刻が古いままだと、
            // 起動時の番号の読み込み（paper_refs）がカードより古い PDF と見なして番号を出さないので、
            // 作り直しが成功したら時刻だけ新しくする。
            let output = self.project_dir.join(&pdf);
            if let Err(e) = std::fs::File::options()
                .write(true)
                .open(&output)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()))
            {
                tracing::warn!("PDF の時刻を更新できなかった {}: {e}", output.display());
            }
        }
        if ok && deck == self.current_deck() {
            let root = self.project_dir.join(&deck);
            let output = self.project_dir.join(&pdf);
            if let Ok(document) =
                tokio::task::spawn_blocking(move || cards::document_for_deck(&root, Some(&output)))
                    .await
            {
                if deck == self.current_deck() {
                    self.hub.publish_snapshot(document);
                }
            }
        }
        if ok {
            if let Some(map) = self.paper_pdf_map(deck.clone(), pdf.clone(), true).await {
                self.hub.publish_snapshot(map);
            }
        }
        let stopped = result.as_ref().is_err_and(|error| error.is::<Cancelled>());
        let mut active = self.builds.active.lock().unwrap();
        let mut failure = self.builds.auto_failure.lock().unwrap();
        if automatic && !ok && !stopped && !auto_paths::deck_eligible(&self.project_dir, &self.project_dir.join(&deck)) {
            self.build_status(&deck, "discarded", String::new(), None);
            if failure.as_deref() == Some(&deck) {
                *failure = None;
            }
        } else if stopped {
            *failure = None;
            self.build_status(&deck, "cancelled", "止めた".into(), None);
        } else if ok {
            *failure = None;
            self.build_status(&deck, "done", "PDF を作った".into(), Some(pdf.clone()));
        } else {
            *failure = automatic.then(|| deck.clone());
            self.build_status(&deck, "failed", log.clone(), None);
        }
        *active = None;
        if !automatic {
            let pdf_audit = result.as_ref().map(|built| built.pdf_audit.clone()).unwrap_or_default();
            self.hub.publish_snapshot(ServerEvent::BuildResult { deck, ok, pdf, log, pdf_audit });
        }
    }

    pub(crate) fn clear_stale_build_failure(&self) {
        let mut failure = self.builds.auto_failure.lock().unwrap();
        if let Some(deck) = failure.as_ref().filter(|deck| {
            !auto_paths::deck_eligible(&self.project_dir, &self.project_dir.join(deck))
        }) {
            self.build_status(deck, "cleared", String::new(), None);
            *failure = None;
        }
    }

    fn build_status(&self, deck: &str, state: &str, message: String, pdf: Option<String>) {
        self.hub.publish(ServerEvent::BuildStatus {
            deck: deck.into(),
            state: state.into(),
            message,
            pdf,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Hub;
    use crate::test_support::TestDir;
    use fuzukue_proto::ClientMsg;

    fn app(dir: &TestDir) -> Arc<Workspace> {
        Arc::new(Workspace::new(
            dir.path().canonicalize().unwrap(),
            String::new(),
            dir.path().join(".fuzukue"),
            Arc::new(Hub::new()),
        ))
    }

    #[test]
    fn auto_build_uses_only_configured_harness_card_paths() {
        let dir = TestDir::new();
        dir.write("fuzukue.toml", "[build]\nauto = false\n");
        dir.write("off/harness.toml", "");
        dir.write(
            "on/harness.toml",
            "cards_dir = 'draft'\n[build]\nauto = true\n",
        );
        dir.write("folder/deck.toml", "");
        let app = app(&dir);
        let paths = [
            "off/cards/one.md",
            "on/draft/one.md",
            "on/build/generated.tex",
            "on/README.md",
            "folder/cards/one.md",
        ]
        .into_iter()
        .map(|path| app.project_dir.join(path))
        .collect();

        assert_eq!(
            changed_decks(&app.project_dir, paths),
            HashSet::from(["on".into()])
        );
    }

    #[test]
    fn auto_build_excludes_staging_and_gitignored_sources() {
        let dir = TestDir::new();
        dir.write(".gitignore", "ignored-copy/\n:ignored-copy/\n");
        dir.write("nested/.gitignore", "generated/\n");
        let mut paths = HashSet::new();
        for deck in [
            ".compact-target-review-abc", "nested/.clone", "target", "node_modules",
            "_build", ".lake", "compact-target-review-abc", "ignored-copy", ":ignored-copy",
            "nested/generated", "nested/paper",
        ] {
            dir.write(&format!("{deck}/harness.toml"), "");
            let path = format!("{deck}/cards/one.md");
            dir.write(&path, "");
            paths.insert(dir.path().join(path));
        }
        assert_eq!(changed_decks(dir.path(), paths), HashSet::from(["nested/paper".into()]));
    }

    #[tokio::test]
    async fn watcher_clears_deleted_and_newly_ignored_auto_failures() -> Result<()> {
        for ignored in [false, true] {
            let dir = TestDir::new();
            dir.write("paper/harness.toml", "");
            let app = app(&dir);
            cards::watch(app.clone(), Default::default())?;
            let (_, mut events) = app.hub.subscribe(u64::MAX);
            app.report_build("paper".into(), &Err(anyhow::anyhow!("fixture failed")), true).await;
            assert!(matches!(events.recv().await?.event,
                ServerEvent::BuildStatus { state, .. } if state == "failed"));
            if ignored {
                dir.write(".gitignore", "paper/\n");
            } else {
                std::fs::remove_dir_all(dir.path().join("paper"))?;
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let ServerEvent::BuildStatus { deck, state, .. } = events.recv().await?.event {
                        assert_eq!(deck, "paper");
                        assert_eq!(state, "cleared");
                        break anyhow::Ok(());
                    }
                }
            }).await??;
            app.report_build("paper".into(), &Err(anyhow::anyhow!("fixture failed")), true).await;
            assert!(matches!(events.recv().await?.event,
                ServerEvent::BuildStatus { state, .. } if state == "discarded"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn stop_build_reaps_latex_and_clears_the_queued_rerun() -> Result<()> {
        let dir = TestDir::new();
        let app = app(&dir);
        let (history, mut events) = app.hub.subscribe(0);
        assert!(history.is_empty());

        let serial = app.builds.serial.lock().await;
        let cancel = Arc::new(Cancellation::default());
        app.start_build("", cancel.clone());
        app.schedule_build(String::new());
        let run = tokio::spawn({
            let root = app.project_dir.clone();
            async move {
                let mut command = tokio::process::Command::new("sh");
                command
                    .current_dir(root)
                    .args(["-c", "sleep 30 & echo $$ > driver; wait"]);
                build::run_latex(&mut command, &cancel).await
            }
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            assert!(matches!(events.recv().await?.event, ServerEvent::BuildStarted { deck } if deck.is_empty()));
            assert!(matches!(events.recv().await?.event,
                ServerEvent::BuildStatus { state, .. } if state == "running"
            ));
            let marker = dir.path().join("driver");
            let driver = loop {
                if let Ok(driver) = std::fs::read_to_string(&marker) {
                    if !driver.trim().is_empty() {
                        break driver;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            crate::handle(&app, ClientMsg::CancelBuild { deck: String::new() }).await?;
            assert!(app.builds.pending.lock().unwrap().is_empty());
            let error = run.await?.expect_err("stopped build");
            assert!(error.is::<Cancelled>());
            #[cfg(target_os = "linux")]
            assert!(!Path::new("/proc").join(driver.trim()).exists(), "driver must be reaped");
            // The child inherits the output pipes. run_latex could only have
            // completed above once the child also exited and closed them.
            app.report_build(String::new(), &Err(error), true).await;
            assert!(app.active_build().is_none());
            assert!(matches!(events.recv().await?.event,
                ServerEvent::BuildStatus { state, message, .. } if state == "cancelled" && message == "止めた"
            ));
            anyhow::Ok(())
        }).await??;
        drop(serial);
        Ok(())
    }

    #[tokio::test]
    async fn changes_during_a_build_queue_one_serial_rerun() -> Result<()> {
        let dir = TestDir::new();
        dir.write("harness.toml", "");
        let app = app(&dir);
        let (_, mut events) = app.hub.subscribe(0);
        let serial = app.builds.serial.lock().await;
        app.start_build("", Arc::new(Cancellation::default()));
        app.schedule_build(String::new());
        tokio::time::sleep(Duration::from_millis(100)).await;
        app.schedule_build(String::new());
        assert_eq!(app.builds.pending.lock().unwrap().len(), 1);

        tokio::time::sleep(DEBOUNCE).await;
        assert!(matches!(events.try_recv()?.event, ServerEvent::BuildStarted { deck } if deck.is_empty()));
        assert!(
            matches!(events.try_recv()?.event, ServerEvent::BuildStatus { state, .. } if state == "running")
        );
        assert!(
            events.try_recv().is_err(),
            "rerun must wait for the build lock"
        );

        app.report_build(String::new(), &Err(Cancelled.into()), false)
            .await;
        drop(serial);

        tokio::time::timeout(Duration::from_secs(3), async {
            let mut starts = 0;
            loop {
                match events.recv().await?.event {
                    ServerEvent::BuildStatus { state, .. } if state == "running" => starts += 1,
                    ServerEvent::BuildStatus { state, .. } if state == "failed" => break,
                    _ => {}
                }
            }
            assert_eq!(starts, 1);
            assert!(app.builds.pending.lock().unwrap().is_empty());
            anyhow::Ok(())
        })
        .await??;
        Ok(())
    }
}
