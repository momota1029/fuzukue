use std::{collections::VecDeque, path::PathBuf, time::Instant};

use futures::{channel::mpsc, StreamExt as _};
use fuzukue_proto::{NotificationKind, SelfUpdateStatus};
use gpui_kit::{
    component::{button::Button, v_flex, ActiveTheme as _, Disableable as _},
    div,
    prelude::*,
    Context, Div, Window,
};

use crate::{appearance::settings_switch, thread_title, Workbench};

mod platform;
mod policy;

use platform::{Backend as _, Event, Platform, Presentation};
use policy::{Action, Destination, Kind, Policy, Settings, Thread};

#[derive(Default)]
struct Routes {
    next: u64,
    active: VecDeque<(u64, Destination, Kind)>,
}

impl Routes {
    fn insert(&mut self, target: Destination, kind: Kind) -> u64 {
        self.next += 1;
        if self.active.len() >= 64 {
            self.active.pop_front();
        }
        self.active.push_back((self.next, target, kind));
        self.next
    }

    fn take(&mut self, id: u64) -> Option<(Destination, Kind)> {
        let index = self.active.iter().position(|(key, _, _)| *key == id)?;
        self.active.remove(index).map(|(_, target, kind)| (target, kind))
    }
}

pub(super) struct Notifications {
    pub(super) settings_selected: bool,
    settings: Settings,
    policy: Policy,
    routes: Routes,
    pending: Option<(Thread, Kind)>,
    backend: Platform,
    sender: mpsc::UnboundedSender<Event>,
    isolated: bool,
    feedback: Option<&'static str>,
}

impl Notifications {
    pub fn new(
        path: Option<PathBuf>,
        isolated: bool,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) -> Self {
        let (sender, mut events) = mpsc::unbounded();
        cx.spawn_in(window, async move |view, cx| {
            while let Some(event) = events.next().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        this.notification_event(event, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            settings_selected: false,
            settings: path
                .as_deref()
                .filter(|_| !isolated)
                .and_then(|path| Settings::load(path).ok())
                .unwrap_or_default(),
            policy: Policy::default(),
            routes: Routes::default(),
            pending: None,
            backend: Platform::default(),
            sender,
            isolated,
            feedback: None,
        }
    }

    pub fn reset_connection(&mut self) {
        self.routes.active.clear();
        self.pending = None;
        self.policy = Policy::default();
    }
}

impl Workbench {
    pub(super) fn notify_thread(
        &mut self,
        workspace: String,
        session: String,
        kind: NotificationKind,
        mut title: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let same_workspace = self.workspaces.active.as_ref() == Some(&workspace);
        let looking = window.is_window_active() && same_workspace && self.active == session;
        if same_workspace {
            if let Some(thread) = self.threads.iter().find(|thread| thread.id == session) {
                title = Some(thread_title(thread).to_owned());
            }
        }
        let target = Destination::Thread(Thread { workspace, session });
        self.present_notification(
            target,
            Kind::Thread(kind),
            title.unwrap_or_else(|| "fuzukue".into()),
            looking,
            window,
            cx,
        );
    }

    pub(super) fn notify_update(
        &mut self,
        status: &SelfUpdateStatus,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(kind) = self.notifications.policy.update(status) {
            let looking =
                window.is_window_active() && self.settings_open && self.sidebar.server_settings;
            self.present_notification(
                Destination::Update,
                kind,
                "fuzukue".into(),
                looking,
                window,
                cx,
            );
        }
    }

    pub(super) fn test_notification(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.present_notification(
            Destination::Test,
            Kind::Test,
            "fuzukue".into(),
            false,
            window,
            cx,
        );
    }

    fn present_notification(
        &mut self,
        target: Destination,
        kind: Kind,
        title: String,
        looking: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let state = &mut self.notifications;
        let Some(outputs) =
            state
                .policy
                .decide(&state.settings, &target, kind, looking, Instant::now())
        else {
            return;
        };
        if kind == Kind::Test {
            state.feedback = Some("notifications-test-sent");
        }
        if !state.isolated {
            let id = if outputs.banner {
                state.routes.insert(target, kind)
            } else {
                0
            };
            let presentation = Presentation {
                id,
                title,
                body: tr!(kind.message()).into(),
                outputs,
                urgent: kind.urgent(),
            };
            state
                .backend
                .present(presentation, state.sender.clone(), window, cx);
        }
        cx.notify();
    }

    fn notification_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Clicked(id) => {
                let Some((target, kind)) = self.notifications.routes.take(id) else {
                    return;
                };
                window.activate_window();
                if let Destination::Thread(thread) = &target {
                    if self.workspaces.pending {
                        self.notifications.pending = Some((thread.clone(), kind));
                        return;
                    }
                }
                match target.action(self.workspaces.active.as_deref()) {
                    Action::Select(session) => self.select_notification_thread(session, kind, cx),
                    Action::Switch(workspace) => {
                        let Destination::Thread(thread) = target.clone() else {
                            return;
                        };
                        self.switch_workspace(workspace.to_owned(), cx);
                        if self.workspaces.pending {
                            self.notifications.pending = Some((thread, kind));
                        }
                    }
                    Action::Update => {
                        self.settings_open = true;
                        self.sidebar.server_settings = true;
                    }
                    Action::Test => {}
                }
            }
            Event::Delivery(message) => self.notifications.feedback = message,
            #[cfg(target_os = "linux")]
            Event::SystemBell => window.play_system_bell(),
        }
        cx.notify();
    }

    fn select_notification_thread(&mut self, session: &str, kind: Kind, cx: &mut Context<Self>) {
        if self
            .threads
            .iter()
            .any(|thread| thread.id == session && !thread.archived)
        {
            self.select_thread(session.to_owned(), cx);
            if kind == Kind::Thread(NotificationKind::Approval) {
                self.reveal_approval_dock();
            }
        }
    }

    pub(super) fn finish_notification_route(&mut self, cx: &mut Context<Self>) {
        if self.workspaces.pending {
            return;
        }
        let Some((thread, kind)) = self.notifications.pending.take() else {
            return;
        };
        if self.workspaces.active.as_deref() == Some(&thread.workspace) {
            self.select_notification_thread(&thread.session, kind, cx);
        } else {
            self.switch_workspace(thread.workspace.clone(), cx);
            if self.workspaces.pending {
                self.notifications.pending = Some((thread, kind));
            }
        }
    }

    pub(super) fn cancel_notification_route(&mut self) {
        self.notifications.pending = None;
    }

    pub(super) fn render_notification_settings(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.notifications;
        let settings = &state.settings;
        let view = cx.weak_entity();
        type Edit = fn(&mut Settings, bool);
        let switches: [(&str, &str, bool, Edit); 4] = [
            (
                "notifications-enabled",
                tr!("notifications-enabled"),
                settings.enabled,
                |s, next| s.enabled = next,
            ),
            (
                "notifications-away",
                tr!("notifications-away"),
                settings.only_when_away,
                |s, next| s.only_when_away = next,
            ),
            (
                "notifications-banner",
                tr!("notifications-banner"),
                settings.banner,
                |s, next| s.banner = next,
            ),
            (
                "notifications-sound",
                tr!("notifications-sound"),
                settings.sound,
                |s, next| s.sound = next,
            ),
        ];
        let mut section = v_flex().gap_1().child(
            div()
                .id("notification-settings")
                .text_lg()
                .child(tr!("notifications-heading")),
        );
        for (id, label, checked, edit) in switches {
            let view = view.clone();
            section = section.child(settings_switch(
                id,
                label,
                checked,
                false,
                cx,
                move |checked, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.save_notification_settings(|s| edit(s, *checked), cx)
                    });
                },
            ));
        }
        if let Some(workspace) = self
            .workspaces
            .active
            .as_ref()
            .filter(|_| !self.active.is_empty())
        {
            let thread = Thread {
                workspace: workspace.clone(),
                session: self.active.clone(),
            };
            let preferences = settings.thread(&thread);
            for (id, label, checked, mute) in [
                (
                    "notifications-mute-thread",
                    tr!("notifications-mute-thread"),
                    preferences.muted,
                    true,
                ),
                (
                    "notifications-worker-thread",
                    tr!("notifications-worker-thread"),
                    preferences.workers,
                    false,
                ),
            ] {
                let (view, thread) = (view.clone(), thread.clone());
                section = section.child(settings_switch(
                    id,
                    label,
                    checked,
                    !mute && preferences.muted,
                    cx,
                    move |checked, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.save_notification_settings(
                                |s| {
                                    s.edit_thread(&thread, |entry| {
                                        if mute {
                                            entry.muted = *checked
                                        } else {
                                            entry.workers = *checked
                                        }
                                    })
                                },
                                cx,
                            );
                        });
                    },
                ));
            }
        }
        let muted = cx.theme().muted_foreground;
        section
            .child(
                Button::new("notifications-test")
                    .label(tr!("notifications-test"))
                    .disabled(!settings.banner && !settings.sound)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.test_notification(window, cx)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(tr!(state.backend.note())),
            )
            .children(policy::running_under_wsl().then(|| {
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(tr!("notifications-wsl"))
            }))
            .children(state.feedback.map(|key| {
                div()
                    .id("notification-feedback")
                    .text_sm()
                    .text_color(muted)
                    .child(tr!(key))
            }))
    }

    fn save_notification_settings(
        &mut self,
        edit: impl FnOnce(&mut Settings),
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.appearance.settings_path() {
            match Settings::save(&path, edit) {
                Ok(settings) => {
                    self.notifications.settings = settings;
                    self.notifications.feedback = None;
                }
                Err(_) => self.notifications.feedback = Some("notifications-save-failed"),
            }
        } else {
            self.notifications.feedback = Some("notifications-save-failed");
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_click_uses_the_original_route_only_once() {
        let mut routes = Routes::default();
        let target = Destination::Thread(Thread {
            workspace: "lab".into(),
            session: "a".into(),
        });
        let kind = Kind::Thread(NotificationKind::Approval);
        let thread_id = routes.insert(target.clone(), kind);
        let test_id = routes.insert(Destination::Test, Kind::Test);
        assert_eq!(routes.take(thread_id), Some((target, kind)));
        assert_eq!(routes.take(thread_id), None);
        assert_eq!(routes.take(test_id), Some((Destination::Test, Kind::Test)));
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn headless_notification_command_selects_the_thread_after_switching_workspaces() {
        use fuzukue_proto::{ClientMsg, Envelope, ServerEvent};
        use gpui_kit::AppContext as _;

        let (done, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                crate::math::init(cx);
                crate::appearance::register_window_typography(cx);
                let (handle, view) = gpui_kit::open_window(
                    gpui_kit::component::TitleBar::window_options(),
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Workbench::new_with_target(window, cx, Some(Err("Not connected")))
                        })
                    },
                )
                .unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                cx.spawn(async move |cx| {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle
                            .update(cx, |_, window, cx| {
                                view.update(cx, |view, cx| {
                                    view.out = out.into();
                                    view.workspaces.active = Some("first".into());
                                    view.test_notification(window, cx);
                                    assert_eq!(
                                        view.notifications.feedback,
                                        Some("notifications-test-sent")
                                    );
                                    let id = view.notifications.routes.insert(
                                        Destination::Thread(Thread {
                                            workspace: "second".into(),
                                            session: "requested".into(),
                                        }),
                                        Kind::Thread(NotificationKind::Approval),
                                    );
                                    view.notification_event(Event::Clicked(id), window, cx);
                                    assert!(matches!(
                                        messages.try_recv().unwrap(),
                                        ClientMsg::SwitchWorkspace { id } if id == "second"
                                    ));
                                    let mut send = |event| {
                                        view.on_conn(
                                            crate::conn::ConnEvent::Event(Envelope {
                                                seq: 0,
                                                workspace: None,
                                                event,
                                            }),
                                            window,
                                            cx,
                                        )
                                    };
                                    send(ServerEvent::WorkspaceSelected {
                                        id: "second".into(),
                                        replay: true,
                                    });
                                    send(ServerEvent::Threads {
                                        threads: serde_json::from_value(serde_json::json!([
                                            {"id": "latest", "title": "Latest", "updated": 2},
                                            {"id": "requested", "title": "Requested", "updated": 1}
                                        ]))
                                        .unwrap(),
                                    });
                                    assert_eq!(view.active, "requested");
                                });
                            })
                            .unwrap();
                    }));
                    let _ = done.send(result);
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
        if let Err(panic) = completed.recv().unwrap() {
            std::panic::resume_unwind(panic);
        }
    }
}
