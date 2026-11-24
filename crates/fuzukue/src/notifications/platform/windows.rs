use std::{cell::RefCell, collections::BTreeMap, mem::size_of};

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::{App, Window};
use raw_window_handle::RawWindowHandle;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{
            DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON,
            NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIIF_RESPECT_QUIET_TIME,
            NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT,
            NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
        },
        WindowsAndMessaging::{LoadIconW, IDI_APPLICATION, WM_APP, WM_NCDESTROY},
    },
};

use super::{Backend, Event, Presentation};

const CALLBACK: u32 = WM_APP + 0x51;
const SUBCLASS: usize = 0x465a;

type Balloons = BTreeMap<u32, (u64, UnboundedSender<Event>)>;

thread_local! {
    // The tray icon and its subclass are created and removed on the window's UI thread.
    static WINDOWS: RefCell<BTreeMap<usize, Balloons>> = const { RefCell::new(BTreeMap::new()) };
}

#[derive(Default)]
pub(in super::super) struct Platform;

impl Backend for Platform {
    fn note(&self) -> &'static str {
        "notifications-windows-note"
    }

    fn present(
        &mut self,
        notice: Presentation,
        events: UnboundedSender<Event>,
        window: &Window,
        _: &mut App,
    ) {
        if notice.outputs.sound {
            window.play_system_bell();
        }
        if !window.is_window_active() {
            window.request_attention();
        }
        if notice.outputs.banner && !present_balloon(window, &notice, events.clone()) {
            let _ =
                events.unbounded_send(Event::Delivery(Some("notifications-banner-unavailable")));
        }
    }
}

fn present_balloon(window: &Window, notice: &Presentation, events: UnboundedSender<Event>) -> bool {
    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return false;
    };
    let hwnd = HWND(handle.hwnd.get() as _);
    let key = hwnd.0 as usize;

    if !WINDOWS.with(|windows| windows.borrow().contains_key(&key)) {
        // The callback owns no raw pointers and unregisters when the window is destroyed.
        if !unsafe { SetWindowSubclass(hwnd, Some(on_message), SUBCLASS, 0) }.as_bool() {
            return false;
        }
        WINDOWS.with(|windows| windows.borrow_mut().insert(key, Balloons::new()));
    }
    let Ok(icon) = (unsafe { LoadIconW(None, IDI_APPLICATION) }) else {
        return false;
    };
    let Ok(id) = u32::try_from(notice.id) else {
        return false;
    };
    let mut data = icon_data(hwnd, id);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.hIcon = icon;
    data.uCallbackMessage = CALLBACK;
    wide_text("fuzukue", &mut data.szTip);
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        return false;
    }
    let oldest = WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let Some(balloons) = windows.get_mut(&key) else {
            return None;
        };
        let oldest = (balloons.len() >= 8)
            .then(|| balloons.pop_first())
            .flatten();
        balloons.insert(id, (notice.id, events));
        oldest
    });
    if let Some((oldest, _)) = oldest {
        delete_icon(hwnd, oldest);
    }
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_NOSOUND
        | NIIF_RESPECT_QUIET_TIME
        | if notice.urgent {
            NIIF_WARNING
        } else {
            NIIF_INFO
        };
    wide_text(&notice.title, &mut data.szInfoTitle);
    wide_text(&notice.body, &mut data.szInfo);
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        true
    } else {
        remove_balloon(hwnd, id);
        false
    }
}

fn icon_data(hwnd: HWND, id: u32) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: id,
        ..Default::default()
    }
}

fn wide_text(value: &str, buffer: &mut [u16]) {
    let capacity = buffer.len() - 1;
    for (target, unit) in buffer.iter_mut().zip(value.encode_utf16().take(capacity)) {
        *target = unit;
    }
    if (0xd800..=0xdbff).contains(&buffer[capacity - 1]) {
        buffer[capacity - 1] = 0;
    }
}

fn delete_icon(hwnd: HWND, id: u32) {
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd, id)) };
}

fn remove_balloon(hwnd: HWND, id: u32) -> Option<(u64, UnboundedSender<Event>)> {
    let route = WINDOWS.with(|windows| {
        windows
            .borrow_mut()
            .get_mut(&(hwnd.0 as usize))?
            .remove(&id)
    });
    if route.is_some() {
        delete_icon(hwnd, id);
    }
    route
}

unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    if message == CALLBACK {
        // With the original Shell callback format, WPARAM identifies the icon.
        let event = lparam.0 as u32;
        if matches!(
            event,
            NIN_BALLOONUSERCLICK | NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT
        ) {
            let route = remove_balloon(hwnd, wparam.0 as u32);
            if let Some((id, events)) = route.filter(|_| event == NIN_BALLOONUSERCLICK) {
                let _ = events.unbounded_send(Event::Clicked(id));
            }
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let balloons = WINDOWS.with(|windows| windows.borrow_mut().remove(&(hwnd.0 as usize)));
        if let Some(balloons) = balloons {
            for id in balloons.into_keys() {
                delete_icon(hwnd, id);
            }
        }
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(on_message), SUBCLASS) };
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
