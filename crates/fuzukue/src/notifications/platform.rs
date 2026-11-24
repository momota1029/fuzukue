use futures::channel::mpsc::UnboundedSender;
use gpui_kit::{App, Window};

use super::policy::Outputs;

pub(super) enum Event {
    Clicked(u64),
    Delivery(Option<&'static str>),
    #[cfg(target_os = "linux")]
    SystemBell,
}

pub(super) struct Presentation {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub outputs: Outputs,
    pub urgent: bool,
}

pub(super) trait Backend: Default {
    fn note(&self) -> &'static str;
    fn present(
        &mut self,
        notice: Presentation,
        events: UnboundedSender<Event>,
        window: &Window,
        cx: &mut App,
    );
}

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(super) use self::windows::Platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::Platform;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) use linux::Platform;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[derive(Default)]
pub(super) struct Platform;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
impl Backend for Platform {
    fn note(&self) -> &'static str {
        "notifications-click-unavailable"
    }

    fn present(
        &mut self,
        notice: Presentation,
        _: UnboundedSender<Event>,
        window: &Window,
        _: &mut App,
    ) {
        if notice.outputs.sound {
            window.play_system_bell();
        }
    }
}
