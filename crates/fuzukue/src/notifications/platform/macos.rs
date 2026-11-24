use std::process::{Command, Stdio};

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::{App, AppContext as _, SystemNotification, Window};
use objc2_app_kit::NSSound;
use objc2_foundation::{NSBundle, NSString};

use super::{Backend, Event, Presentation};

pub(in super::super) struct Platform {
    bundled: bool,
    listening: bool,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            bundled: NSBundle::mainBundle().bundleIdentifier().is_some(),
            listening: false,
        }
    }
}

impl Backend for Platform {
    fn note(&self) -> &'static str {
        if self.bundled {
            "notifications-macos-note"
        } else {
            "notifications-macos-unbundled"
        }
    }

    fn present(
        &mut self,
        notice: Presentation,
        events: UnboundedSender<Event>,
        window: &Window,
        cx: &mut App,
    ) {
        if !window.is_window_active() {
            window.request_attention();
        }
        if notice.outputs.sound
            && !NSSound::soundNamed(&NSString::from_str("Glass")).is_some_and(|sound| sound.play())
        {
            window.play_system_bell();
        }
        if !notice.outputs.banner {
            return;
        }
        if self.bundled {
            // GPUI handles UNUserNotificationCenter authorization and activation.
            if !self.listening {
                self.listening = true;
                cx.on_system_notification_response(move |response, _| {
                    if let Ok(id) = response.tag.parse() {
                        let _ = events.unbounded_send(Event::Clicked(id));
                    }
                });
            }
            cx.show_system_notification(SystemNotification {
                tag: notice.id.to_string().into(),
                title: notice.title.into(),
                body: notice.body.into(),
                actions: Vec::new(),
            });
        } else {
            // Script arguments carry the localized text as data.
            cx.background_spawn(async move {
                let displayed = Command::new("/usr/bin/osascript")
                    .args(["-e", "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run",
                        "--", &notice.title, &notice.body])
                    .stdout(Stdio::null()).stderr(Stdio::null())
                    .status().is_ok_and(|status| status.success());
                if !displayed {
                    let _ = events.unbounded_send(Event::Delivery(Some("notifications-banner-unavailable")));
                }
            }).detach();
        }
    }
}
