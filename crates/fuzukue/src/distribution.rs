use std::{
    cell::Cell,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use futures::{channel::mpsc, StreamExt as _};
use fuzukue_proto::ClientManifest;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    v_flex, Disableable as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, Context, Div, SharedString, Stateful};

use crate::{
    appearance::settings_switch,
    settings_file,
    tailnet::{self, Peer, SendJob},
    updater, Workbench,
};

const SEEN_ROLLBACK: &str = "seen_client_rollback";

struct RollbackNotice {
    commit: String,
    seen: Cell<bool>,
    dismissed: bool,
}

impl RollbackNotice {
    fn new(commit: &str, state_path: Option<&Path>) -> Self {
        let seen = state_path
            .and_then(|path| settings_file::read(path).ok())
            .is_some_and(|state| state.get(SEEN_ROLLBACK).and_then(toml::Value::as_str) == Some(commit));
        Self {
            commit: commit.into(),
            seen: Cell::new(seen),
            dismissed: seen,
        }
    }

    fn mark_seen(&self, state_path: Option<&Path>) {
        if self.seen.get() {
            return;
        }
        if let Some(path) = state_path {
            let saved = settings_file::update(path, |state| {
                if state.get(SEEN_ROLLBACK).and_then(toml::Value::as_str) == Some(&self.commit) {
                    return Ok(false);
                }
                state.insert(SEEN_ROLLBACK.into(), toml::Value::String(self.commit.clone()));
                Ok(true)
            });
            if saved.is_err() {
                return;
            }
        }
        self.seen.set(true);
    }

    fn dismiss(&mut self, state_path: Option<&Path>) {
        self.mark_seen(state_path);
        self.dismissed = true;
    }
}

#[derive(Default)]
pub struct Distribution {
    pub shot: bool,
    pub manifest: Option<ClientManifest>,
    pub public_url: Option<String>,
    pub handoff: Option<updater::UpdateHandoff>,
    checking: bool,
    updating: bool,
    update_progress: Option<updater::DownloadProgress>,
    rollback: Option<RollbackNotice>,
    update_error: Option<String>,
    peers: Vec<Peer>,
    jobs: Vec<SendJob>,
    queue_path: Option<PathBuf>,
    polling: bool,
    send_open: bool,
    send_status: Option<String>,
}

impl Distribution {
    pub fn new(shot: bool, queue_path: Option<PathBuf>) -> Self {
        let mut state = Self {
            shot,
            queue_path,
            ..Self::default()
        };
        if !shot {
            if let Some(path) = &state.queue_path {
                match tailnet::load_jobs(path) {
                    Ok(jobs) => state.jobs = jobs,
                    Err(err) => {
                        state.send_status =
                            Some(tr!("distribution-send-error", detail = format!("{err:#}")));
                        state.queue_path = None;
                    }
                }
            }
        }
        state
    }

    fn save_jobs(&mut self, jobs: Vec<SendJob>) -> anyhow::Result<()> {
        let path = self
            .queue_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(tr!("settings-settings-directory-not-found")))?;
        tailnet::save_jobs(path, &jobs)?;
        self.jobs = jobs;
        Ok(())
    }

    pub fn reset_connection(&mut self) {
        self.manifest = None;
        self.rollback = None;
        self.update_error = None;
        self.update_progress = None;
        self.public_url = None;
        self.checking = false;
    }

    fn set_rollback(&mut self, commit: Option<&str>, state_path: Option<&Path>) {
        if self.rollback.as_ref().map(|notice| notice.commit.as_str()) != commit {
            self.rollback = commit.map(|commit| RollbackNotice::new(commit, state_path));
        }
    }
}

impl Workbench {
    pub fn distribution_timer(&self, cx: &mut Context<Self>) {
        if self.distribution.shot {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut elapsed = 0;
            loop {
                let result = this.update(cx, |this, cx| {
                    if elapsed == 0 {
                        this.check_client_update(cx);
                    }
                    if this.settings_open || !this.distribution.jobs.is_empty() {
                        this.poll_tailnet(cx);
                    }
                });
                if result.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                elapsed = (elapsed + 1) % 3;
            }
        })
        .detach();
    }

    pub fn check_client_update(&mut self, cx: &mut Context<Self>) {
        let Some(platform) = updater::platform() else {
            return;
        };
        if self.connection_epoch == 0 || self.distribution.shot || self.distribution.checking {
            return;
        }
        let http = self.http.clone();
        let epoch = self.connection_epoch;
        self.distribution.checking = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { updater::manifest(&http, platform) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if epoch == this.connection_epoch {
                    this.distribution.checking = false;
                    if let Ok(manifest) = result {
                        this.accept_client_manifest(manifest, cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn accept_client_manifest(
        &mut self,
        manifest: Option<ClientManifest>,
        cx: &mut Context<Self>,
    ) {
        if self.distribution.shot {
            return;
        }
        let rejected = manifest.as_ref().and_then(updater::rejected);
        let rollback = rejected.as_deref().filter(|commit| {
            manifest.as_ref().is_some_and(|manifest| manifest.commit == *commit)
        });
        self.distribution.set_rollback(rollback, self.rollback_state_path().as_deref());
        let manifest = manifest.filter(|manifest| {
            updater::available(updater::BUILD_ID, rejected.as_deref(), manifest)
        });
        if self.distribution.manifest != manifest {
            self.distribution.update_error = None;
            self.distribution.update_progress = None;
            self.distribution.manifest = manifest;
        }
        if self.distribution.update_error.is_none() && self.appearance.settings.server.auto_update {
            self.replace_client(cx);
        }
    }

    pub fn replace_client(&mut self, cx: &mut Context<Self>) {
        if self.distribution.shot || self.distribution.updating {
            return;
        }
        let Some(manifest) = self.distribution.manifest.clone() else {
            return;
        };
        self.distribution.updating = true;
        self.distribution.update_progress = None;
        self.distribution.update_error = None;
        let http = self.http.clone();
        let download_http = http.clone();
        let download_manifest = manifest.clone();
        cx.spawn(async move |this, cx| {
            let (progress_tx, mut progress_rx) = mpsc::unbounded::<updater::DownloadProgress>();
            let task = cx.background_executor().spawn(async move {
                let mut last = None;
                updater::prepare(&download_http, &download_manifest, |progress| {
                    let now = Instant::now();
                    if last.is_none_or(|(phase, time)| {
                        phase != progress.phase || now - time >= Duration::from_millis(250)
                    }) || progress.bytes == progress.total
                    {
                        let _ = progress_tx.unbounded_send(progress);
                        last = Some((progress.phase, now));
                    }
                })
            });
            while let Some(progress) = progress_rx.next().await {
                if this.update(cx, |this, cx| {
                    if this.http.same_distribution_server(&http)
                        && this.distribution.manifest.as_ref() == Some(&manifest)
                    {
                        this.distribution.update_progress = Some(progress);
                        cx.notify();
                    }
                }).is_err() {
                    break;
                }
            }
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.distribution.updating = false;
                this.distribution.update_progress = None;
                // Workspace replay may advance the connection epoch during an HTTP download.
                if this.http.same_distribution_server(&http)
                    && this.distribution.manifest.as_ref() == Some(&manifest)
                {
                    match result.and_then(|file| updater::launch_helper(file, &manifest.commit)) {
                        Ok(handoff) => {
                            this.distribution.handoff = Some(handoff);
                            cx.quit();
                        }
                        Err(err) => {
                            this.distribution.update_error = Some(tr!(
                                "distribution-update-error",
                                detail = format!("{err:#}")
                            ))
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn rollback_state_path(&self) -> Option<PathBuf> {
        if self.distribution.shot {
            return None;
        }
        self.appearance.settings_path().map(|path| path.with_extension("state.toml"))
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn preview_client_rollback(&mut self) {
        self.distribution.manifest = None;
        self.distribution.set_rollback(Some("preview"), None);
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn preview_client_download(&mut self, resuming: bool) {
        self.distribution.updating = true;
        self.distribution.update_progress = Some(updater::DownloadProgress {
            bytes: 47_392_768,
            total: 120_000_000,
            retries: 2,
            phase: if resuming {
                updater::DownloadPhase::Resuming
            } else {
                updater::DownloadPhase::Retrying { timed_out: true }
            },
        });
    }

    pub fn render_update_notice(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if let Some(notice) = self.distribution.rollback.as_ref().filter(|notice| !notice.dismissed) {
            notice.mark_seen(self.rollback_state_path().as_deref());
            let commit = notice.commit.clone();
            let dismiss = tr!("distribution-dismiss-rollback");
            return Some(
                div()
                    .id("client-update-rollback")
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .child(div().flex_1().child(tr!("distribution-rolled-back")))
                    .child(
                        Button::new("client-update-rollback-dismiss")
                            .ghost()
                            .small()
                            .label("✕")
                            .accessibility_label(dismiss)
                            .tooltip(dismiss)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let path = this.rollback_state_path();
                                if let Some(notice) = this.distribution.rollback.as_mut()
                                    .filter(|notice| notice.commit == commit)
                                {
                                    notice.dismiss(path.as_deref());
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        self.distribution.manifest.as_ref()?;
        Some(
            v_flex()
                .id("client-update-notice")
                .gap_1()
                .p_2()
                .child(tr!("distribution-update-available"))
                .child(
                    Button::new("client-update")
                        .label(if self.distribution.updating {
                            tr!("distribution-updating")
                        } else {
                            tr!("distribution-update")
                        })
                        .disabled(self.distribution.updating)
                        .on_click(cx.listener(|this, _, _, cx| this.replace_client(cx))),
                )
                .children(self.distribution.update_progress.map(|progress| {
                    let status = match progress.phase {
                        updater::DownloadPhase::Downloading if progress.retries == 0 => None,
                        updater::DownloadPhase::Downloading => Some(tr!(
                            "distribution-update-retry-count", retries = progress.retries
                        )),
                        updater::DownloadPhase::Resuming => Some(tr!(
                            "distribution-update-resuming", retries = progress.retries
                        )),
                        updater::DownloadPhase::Retrying { timed_out: false } => Some(tr!(
                            "distribution-update-retrying", retries = progress.retries
                        )),
                        updater::DownloadPhase::Retrying { timed_out: true } => Some(tr!(
                            "distribution-update-timeout-retrying", retries = progress.retries
                        )),
                    };
                    v_flex()
                        .id("client-update-progress")
                        .child(tr!("distribution-update-bytes",
                            bytes = progress.bytes.to_string(), total = progress.total.to_string()))
                        .children(status)
                }))
                .children(
                    self.distribution
                        .update_error
                        .as_ref()
                        .map(|error| div().child(error.clone())),
                ),
        )
    }

    fn send_url(&self) -> Option<String> {
        use fuzukue_view::server::ServerMode;
        if self.appearance.settings.server.mode == ServerMode::Remote {
            return self
                .appearance
                .settings
                .server
                .remote_url()
                .ok()
                .filter(|url| *url == self.http.url());
        }
        self.distribution.public_url.clone()
    }

    fn queue_peer(&mut self, peer: &Peer, cx: &mut Context<Self>) {
        let Some(url) = self.send_url() else { return };
        let job = SendJob {
            peer: peer.id.clone(),
            url,
            platform: peer.default_platform().map(str::to_owned),
        };
        if !self.distribution.jobs.contains(&job) {
            let mut jobs = self.distribution.jobs.clone();
            jobs.push(job);
            match self.distribution.save_jobs(jobs) {
                Ok(()) => self.distribution.send_status = None,
                Err(err) => {
                    self.distribution.send_status =
                        Some(tr!("distribution-send-error", detail = format!("{err:#}")))
                }
            }
        }
        self.poll_tailnet(cx);
        cx.notify();
    }

    pub fn poll_tailnet(&mut self, cx: &mut Context<Self>) {
        if self.distribution.shot || self.distribution.polling {
            return;
        }
        self.distribution.polling = true;
        let http = self.http.clone();
        let url = self.send_url();
        let jobs = self.distribution.jobs.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let peers = tailnet::peers()?;
                    let mut results = Vec::new();
                    for job in jobs
                        .into_iter()
                        .filter(|job| Some(&job.url) == url.as_ref())
                    {
                        if let Some(peer) =
                            peers.iter().find(|peer| peer.id == job.peer && peer.online)
                        {
                            let result = tailnet::send(&http, peer, &job);
                            results.push((job, result));
                        }
                    }
                    Ok::<_, anyhow::Error>((peers, results))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.distribution.polling = false;
                match result {
                    Ok((peers, results)) => {
                        this.distribution.peers = peers;
                        let mut jobs = this.distribution.jobs.clone();
                        for (job, result) in results {
                            match result {
                                Ok(()) => {
                                    jobs.retain(|pending| pending != &job);
                                    this.distribution.send_status =
                                        Some(tr!("distribution-sent").into());
                                }
                                Err(err) => {
                                    this.distribution.send_status = Some(tr!(
                                        "distribution-send-error",
                                        detail = format!("{err:#}")
                                    ))
                                }
                            }
                        }
                        if jobs != this.distribution.jobs {
                            if let Err(err) = this.distribution.save_jobs(jobs) {
                                this.distribution.send_status = Some(tr!(
                                    "distribution-send-error",
                                    detail = format!("{err:#}")
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        this.distribution.send_status =
                            Some(tr!("distribution-send-error", detail = format!("{err:#}")))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn render_distribution_settings(&self, cx: &Context<Self>) -> Stateful<Div> {
        use fuzukue_view::server::ServerMode;
        let form = &self.appearance;
        let launchable = matches!(form.server_mode, ServerMode::Wsl | ServerMode::Local);
        let settings = &form.settings.server;
        let state = &self.distribution;
        let mut section = v_flex()
            .id("distribution-settings")
            .gap_2()
            .pt_2()
            .child(
                settings_switch(
                    "distribution-host", tr!("distribution-host"), form.host_tailnet, !launchable, cx,
                    cx.listener(|this, checked, _, cx| {
                        this.appearance.host_tailnet = *checked;
                        cx.notify();
                    }),
                ),
            )
            .children((launchable && form.host_tailnet).then(|| {
                settings_switch(
                    "distribution-keep", tr!("distribution-keep"), form.keep_running, false, cx,
                    cx.listener(|this, checked, _, cx| {
                        this.appearance.keep_running = *checked;
                        cx.notify();
                    }),
                )
            }))
            .children(state.public_url.as_ref().map(|url| {
                let url = url.clone();
                Button::new("distribution-url")
                    .label(url.clone())
                    .tooltip(tr!("distribution-copy-url"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url.clone()))
                    })
            }))
            .child(
                settings_switch(
                    "distribution-auto", tr!("distribution-auto"), settings.auto_update, false, cx,
                    cx.listener(|this, checked, _, cx| this.set_auto_update(*checked, cx)),
                ),
            )
            .children(self.render_update_notice(cx))
            .child(
                Button::new("distribution-send")
                    .label(tr!("distribution-send"))
                    .toggled(state.send_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.distribution.send_open = !this.distribution.send_open;
                        this.poll_tailnet(cx);
                        cx.notify();
                    })),
            );
        if state.send_open || settings.host_tailnet {
            let can_send = state.send_open && self.send_url().is_some();
            section = section
                .child(tr!("distribution-peers"))
                .children(
                    (state.send_open && !can_send)
                        .then(|| div().child(tr!("distribution-send-needs-url"))),
                )
                .children(state.peers.iter().map(|peer| {
                    let label = tr!(
                        "distribution-peer",
                        host = peer.host_name.clone(),
                        status = if peer.online {
                            tr!("distribution-online")
                        } else {
                            tr!("distribution-offline")
                        }
                    );
                    let peer = peer.clone();
                    Button::new(SharedString::from(format!("distribution-peer-{}", peer.id)))
                        .label(label)
                        .disabled(!can_send)
                        .on_click(cx.listener(move |this, _, _, cx| this.queue_peer(&peer, cx)))
                }))
                .children((!state.jobs.is_empty()).then(|| {
                    div().child(tr!(
                        "distribution-queued",
                        count = state.jobs.len().to_string()
                    ))
                }))
                .children(
                    state
                        .send_status
                        .as_ref()
                        .map(|error| div().child(error.clone())),
                );
        }
        section
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;

    #[test]
    fn rollback_render_is_remembered_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.state.toml");
        std::fs::write(&path, "right_pane_width = 420\n").unwrap();
        let mut distribution = Distribution::default();
        distribution.set_rollback(Some("rejected"), Some(&path));
        let notice = distribution.rollback.as_ref().unwrap();
        assert!(!notice.dismissed);
        notice.mark_seen(Some(&path));

        distribution.set_rollback(Some("rejected"), Some(&path));
        assert!(!distribution.rollback.as_ref().unwrap().dismissed);
        let mut restarted = Distribution::default();
        restarted.set_rollback(Some("rejected"), Some(&path));
        assert!(restarted.rollback.as_ref().unwrap().dismissed);
        assert_eq!(settings_file::read(&path).unwrap()["right_pane_width"].as_integer(), Some(420));

        restarted.set_rollback(Some("another-rejection"), Some(&path));
        assert!(!restarted.rollback.as_ref().unwrap().dismissed);
    }

    #[test]
    fn dismissed_rollback_stays_hidden_after_poll_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.state.toml");
        let mut distribution = Distribution::default();
        distribution.set_rollback(Some("rejected"), Some(&path));
        distribution.rollback.as_mut().unwrap().dismiss(Some(&path));
        distribution.set_rollback(Some("rejected"), Some(&path));
        assert!(distribution.rollback.as_ref().unwrap().dismissed);

        let mut restarted = Distribution::default();
        restarted.set_rollback(Some("rejected"), Some(&path));
        assert!(restarted.rollback.as_ref().unwrap().dismissed);
    }
}
