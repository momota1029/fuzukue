use std::{
    process::{Command, Stdio},
    time::Duration,
};

use futures::{
    channel::mpsc::UnboundedSender,
    future::{select, Either},
};
use gpui_kit::{App, AppContext as _, Window};
use notify_rust::{Hint, Notification, NotificationResponse, Urgency};

use super::{Backend, Event, Presentation};

#[derive(Default)]
pub(in super::super) struct Platform;

impl Backend for Platform {
    fn note(&self) -> &'static str {
        "notifications-linux-note"
    }

    fn present(
        &mut self,
        notice: Presentation,
        events: UnboundedSender<Event>,
        _: &Window,
        cx: &mut App,
    ) {
        if notice.outputs.sound {
            let events = events.clone();
            cx.background_spawn(async move {
                let played = Command::new("canberra-gtk-play")
                    .args(["--id", "message-new-instant"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if !played {
                    let _ = events.unbounded_send(Event::SystemBell);
                }
            })
            .detach();
        }
        if !notice.outputs.banner {
            return;
        }
        let timer = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut builder = Notification::new();
            builder
                .appname("fuzukue")
                .summary(&notice.title)
                .body(&escape_markup(&notice.body))
                .urgency(if notice.urgent {
                    Urgency::Critical
                } else {
                    Urgency::Normal
                })
                .hint(Hint::SuppressSound(true))
                .timeout(10_000)
                .action("default", "");
            let handle = match builder.show_async().await {
                Ok(handle) => handle,
                Err(_) => {
                    let _ = events
                        .unbounded_send(Event::Delivery(Some("notifications-banner-unavailable")));
                    return;
                }
            };
            let action = handle.wait_for_action_async(|response: &NotificationResponse| {
                if response.is_default_action() {
                    let _ = events.unbounded_send(Event::Clicked(notice.id));
                }
            });
            let timed_out = matches!(
                select(Box::pin(action), timer.timer(Duration::from_secs(120))).await,
                Either::Right(_)
            );
            if timed_out {
                handle.close_async().await;
            }
        })
        .detach();
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
