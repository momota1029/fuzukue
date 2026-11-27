use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(any(windows, target_os = "linux"))]
use anyhow::Context as _;
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use gpui_kit::{
    base::{Disableable as _, StyledExt as _},
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Textarea, TextareaState},
        v_flex, ActiveTheme as _, WindowExt as _,
    },
    div, img,
    prelude::*,
    px, App, Context, Entity, Focusable as _, Image, ImageFormat, ObjectFit, WeakFocusHandle, Window,
};
use fuzukue_proto::{
    ClientMsg, ScreenshotDestination, ScreenshotResponse, CLIENT_SCREENSHOT_PROTOCOL,
    MAX_SCREENSHOT_NOTE_BYTES, SCREENSHOT_TIMEOUT_SECS,
};

#[cfg(any(windows, target_os = "linux"))]
use fuzukue_proto::MAX_SCREENSHOT_BYTES;

use crate::{icons::Symbol, RightPane, Workbench};

#[derive(Default)]
pub(super) struct Screenshots {
    prompt: Option<(u64, Instant)>,
    capture: Option<(u64, Option<u64>)>,
    pub(super) preview: Option<Preview>,
    feedback: Option<String>,
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    capture_fixture: Option<Result<Vec<u8>>>,
}

impl Screenshots {
    pub(super) fn preview_open(&self) -> bool {
        self.preview.is_some()
    }
}

pub(super) struct Preview {
    image: Arc<Image>,
    note: Entity<TextareaState>,
    previous_focus: Option<WeakFocusHandle>,
    pane: String,
    session: String,
    saving: Option<u64>,
    error: Option<String>,
}

impl Preview {
    fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.note.read(cx).focus_handle(cx).is_focused(window)
            || window.context_stack().iter().any(|context| context.contains("ScreenshotPreview"))
    }

    fn restore_focus(&self, window: &mut Window, cx: &mut App) {
        if self.owns_focus(window, cx) {
            if let Some(previous) = self.previous_focus.as_ref().and_then(WeakFocusHandle::upgrade) {
                previous.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
    }
}

enum Target {
    #[cfg(target_os = "linux")]
    X11(u64),
    #[cfg(windows)]
    Windows(isize),
    Unsupported,
}

impl Target {
    fn window(window: &Window) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return Self::Unsupported;
        };
        match handle.as_raw() {
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xcb(handle) => Self::X11(u64::from(handle.window.get())),
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(handle) => Self::X11(handle.window as u64),
            #[cfg(windows)]
            RawWindowHandle::Win32(handle) => Self::Windows(handle.hwnd.get()),
            _ => Self::Unsupported,
        }
    }

    fn capture(self) -> Result<Option<Vec<u8>>> {
        match self {
            #[cfg(target_os = "linux")]
            Self::X11(xid) => {
                let bytes = match capture_x11(xid) {
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        return Ok(None)
                    }
                    result => result?,
                };
                if bytes.len() <= MAX_SCREENSHOT_BYTES {
                    return Ok(Some(bytes));
                }
                let pixels = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?;
                Ok(Some(limit_png(pixels)?))
            }
            #[cfg(windows)]
            Self::Windows(hwnd) => {
                let pixels = image::DynamicImage::ImageRgba8(capture_own_window(hwnd)?);
                Ok(Some(limit_png(pixels)?))
            }
            Self::Unsupported => Ok(None),
        }
    }
}

/// xcap's window list deliberately skips windows of the calling process, so the
/// workbench cannot be found there. Capture its on-screen rectangle from the monitor.
#[cfg(windows)]
fn capture_own_window(hwnd: isize) -> Result<image::RgbaImage> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    anyhow::ensure!(!unsafe { IsIconic(hwnd) }.as_bool(), "Workbench window is minimized");
    let mut rect = RECT::default();
    // The extended frame bounds exclude the invisible resize border and shadow.
    let framed = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if framed.is_err() {
        unsafe { GetWindowRect(hwnd, &mut rect) }.context("reading the workbench window bounds")?;
    }
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    anyhow::ensure!(width > 0 && height > 0, "Workbench window has no visible area");
    let monitor = xcap::Monitor::from_point(rect.left + width / 2, rect.top + height / 2)?;
    let (mx, my) = (monitor.x()?, monitor.y()?);
    let (mw, mh) = (monitor.width()? as i32, monitor.height()? as i32);
    let left = rect.left.clamp(mx, mx + mw);
    let top = rect.top.clamp(my, my + mh);
    let right = rect.right.clamp(mx, mx + mw);
    let bottom = rect.bottom.clamp(my, my + mh);
    anyhow::ensure!(right > left && bottom > top, "Workbench window is off screen");
    Ok(monitor.capture_region(
        (left - mx) as u32,
        (top - my) as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )?)
}

#[cfg(target_os = "linux")]
pub(super) fn capture_x11(xid: u64) -> Result<Vec<u8>> {
    let captured = std::process::Command::new("import")
        .args(["-window", &format!("0x{xid:x}"), "png:-"])
        .output()
        .context("running ImageMagick import")?;
    anyhow::ensure!(
        captured.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&captured.stderr)
    );
    Ok(captured.stdout)
}

#[cfg(any(windows, target_os = "linux"))]
fn limit_png(mut pixels: image::DynamicImage) -> Result<Vec<u8>> {
    loop {
        let mut png = std::io::Cursor::new(Vec::new());
        pixels.write_to(&mut png, image::ImageFormat::Png)?;
        let png = png.into_inner();
        if png.len() <= MAX_SCREENSHOT_BYTES {
            return Ok(png);
        }
        pixels = pixels.resize(
            (pixels.width() * 3 / 4).max(1),
            (pixels.height() * 3 / 4).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
}

impl Workbench {
    pub(super) fn capture_screenshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_screenshot(None, window, cx);
    }

    pub(super) fn screenshot_requested(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        if self.shot_activity.is_some() {
            self.send_screenshot_response(id, ScreenshotResponse::Unsupported);
            return;
        }
        if self.screenshot_without_prompt() {
            self.start_screenshot(Some(id), window, cx);
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(SCREENSHOT_TIMEOUT_SECS);
        self.screenshots.prompt = Some((id, deadline));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(deadline.saturating_duration_since(Instant::now()))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .screenshots
                    .prompt
                    .is_some_and(|(active, _)| active == id)
                {
                    this.screenshots.prompt = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn answer_screenshot(&mut self, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, _)) = self
            .screenshots
            .prompt
            .take()
            .filter(|(_, deadline)| *deadline > Instant::now())
        {
            if allow {
                self.start_screenshot(Some(id), window, cx);
            } else {
                self.send_screenshot_response(id, ScreenshotResponse::Denied);
            }
        }
        cx.notify();
    }

    fn send_screenshot_response(&self, id: u64, response: ScreenshotResponse) {
        let _ = self
            .out
            .send(ClientMsg::ClientScreenshotResponse { id, response });
    }

    fn start_screenshot(
        &mut self,
        request: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.screenshots.preview.is_some()
            || self.screenshots.capture.is_some()
            || self.workspace_browser_open()
            || window.has_active_dialog(cx)
        {
            if let Some(id) = request {
                self.send_screenshot_response(id, ScreenshotResponse::Failed);
            }
            return;
        }
        self.close_palette(window, cx);
        self.close_sidebar_menu(cx);
        self.screenshots.feedback = None;
        let token = next_id();
        self.screenshots.capture = Some((token, request));
        let session = self.active.clone();
        let pane = if self.settings_open {
            "Settings".into()
        } else if self.symbol_view_visible() {
            "Symbols".into()
        } else if self.paper_pdf_visible() {
            "Paper PDF".into()
        } else if self.details_view_visible() {
            format!("{:?} / Thread details", self.right)
        } else if self.right == RightPane::None {
            "Chat".into()
        } else {
            format!("{:?}", self.right)
        };
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        if let Some(result) = self.screenshots.capture_fixture.take() {
            self.finish_screenshot_capture(request, result.map(Some), session, pane, window, cx);
            return;
        }
        let target = Target::window(window);
        let entity = cx.weak_entity();
        cx.notify();
        // This GPUI version runs frame callbacks before drawing. Wait one more
        // frame so the menu and the consent prompt have already been presented closed.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let _ = entity.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move { target.capture() })
                            .await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if this.screenshots.capture != Some((token, request)) {
                                return;
                            }
                            this.finish_screenshot_capture(
                                request, result, session, pane, window, cx,
                            );
                        });
                    })
                    .detach();
                });
            });
        });
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn stub_screenshot_capture(&mut self, result: Result<Vec<u8>>) {
        self.screenshots.capture_fixture = Some(result);
    }

    fn finish_screenshot_capture(
        &mut self,
        request: Option<u64>,
        result: Result<Option<Vec<u8>>>,
        session: String,
        pane: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.screenshots.capture = None;
        if let Some(id) = request {
            let response = match result {
                Ok(Some(png)) => ScreenshotResponse::Captured(STANDARD.encode(png)),
                Ok(None) => ScreenshotResponse::Unsupported,
                Err(_) => ScreenshotResponse::Failed,
            };
            self.send_screenshot_response(id, response);
        } else {
            match result {
                Ok(Some(png)) => self.show_screenshot(png, session, pane, window, cx),
                Ok(None) => {
                    self.screenshots.feedback = Some(tr!("screenshot-unsupported").into());
                }
                Err(error) => {
                    self.screenshots.feedback =
                        Some(tr!("screenshot-failed", detail = error.to_string()));
                }
            }
        }
        cx.notify();
    }

    fn show_screenshot(
        &mut self,
        png: Vec<u8>,
        session: String,
        pane: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 4)
                .placeholder(tr!("screenshot-note"))
        });
        let previous_focus = window.focused(cx).map(|focus| focus.downgrade());
        note.read(cx).focus_handle(cx).focus(window, cx);
        self.screenshots.preview = Some(Preview {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, png)),
            note,
            previous_focus,
            session,
            pane,
            saving: None,
            error: None,
        });
    }

    pub(super) fn close_screenshot(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self
            .screenshots
            .preview
            .as_ref()
            .is_some_and(|preview| preview.saving.is_some())
        {
            return true;
        }
        self.dismiss_screenshot_preview(window, cx)
    }

    pub(super) fn dismiss_screenshot_preview(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(preview) = self.screenshots.preview.take() else { return false };
        preview.restore_focus(window, cx);
        true
    }

    pub(super) fn screenshot_disconnected(&mut self) {
        self.screenshots.prompt = None;
        if self
            .screenshots
            .capture
            .is_some_and(|(_, request)| request.is_some())
        {
            self.screenshots.capture = None;
        }
        if let Some(preview) = self.screenshots.preview.as_mut() {
            if preview.saving.take().is_some() {
                preview.error = Some(tr!("screenshot-connection-lost").into());
            }
        }
    }

    fn report_screenshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.screenshots.preview.as_ref() else {
            return;
        };
        if preview.saving.is_some() {
            return;
        }
        let note = preview.note.read(cx).value().trim().to_owned();
        let screenshot = preview.image.clone();
        let preview_focus = window.focused(cx);
        // The dialog needs to capture the preview's return target before the note disappears.
        preview.restore_focus(window, cx);
        if self.open_github_report_draft(Some((screenshot, note)), window, cx) {
            self.screenshots.preview = None;
        } else if let Some(preview) = self.screenshots.preview.as_mut() {
            if let Some(focus) = preview_focus {
                focus.focus(window, cx);
            }
            preview.error = Some(tr!("github-report-close-dialog").into());
        }
        cx.notify();
    }

    pub(super) fn screenshot_saved(
        &mut self,
        request: u64,
        path: Option<String>,
        error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self
            .screenshots
            .preview
            .as_mut()
            .filter(|preview| preview.saving == Some(request))
        else {
            return;
        };
        if let Some(path) = path {
            self.dismiss_screenshot_preview(window, cx);
            self.screenshots.feedback = Some(tr!("screenshot-saved", path = path));
        } else {
            preview.error =
                Some(tr!("screenshot-failed", detail = error.unwrap_or_default()).into());
            preview.saving = None;
        }
    }

    fn send_screenshot(
        &mut self,
        destination: Option<ScreenshotDestination>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.screenshots.preview.as_mut() else {
            return;
        };
        if self.protocol < CLIENT_SCREENSHOT_PROTOCOL || preview.saving.is_some() {
            return;
        }
        let note = preview.note.read(cx).value().trim().to_owned();
        if note.len() > MAX_SCREENSHOT_NOTE_BYTES {
            preview.error = Some(tr!("screenshot-note-too-long").into());
        } else if let Some(destination) = destination {
            let request = next_id();
            let message = ClientMsg::SaveScreenshot {
                request,
                destination,
                data: STANDARD.encode(&preview.image.bytes),
                note,
                pane: preview.pane.clone(),
                session: (!preview.session.is_empty()).then(|| preview.session.clone()),
            };
            if self.out.send(message).is_ok() {
                preview.saving = Some(request);
                preview.error = None;
            } else {
                preview.error = Some(tr!("screenshot-connection-lost").into());
            }
        } else if preview.session == self.active {
            let transfer_focus = preview.owns_focus(window, cx);
            let png = preview.image.bytes.clone();
            if self.attach_screenshot(png, cx) {
                if !note.is_empty() {
                    self.input.update(cx, |input, cx| {
                        let draft = input.value();
                        let next = if draft.is_empty() {
                            note
                        } else {
                            format!("{draft}\n\n{note}")
                        };
                        input.set_value(next, window, cx);
                    });
                }
                self.screenshots.preview = None;
                if transfer_focus {
                    self.input.read(cx).focus_handle(cx).focus(window, cx);
                }
            } else if let Some(preview) = self.screenshots.preview.as_mut() {
                preview.error = Some(tr!("chat-cannot-send-attachments").into());
            }
        }
        cx.notify();
    }

    pub(super) fn render_screenshots(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        let theme = cx.theme();
        if let Some(preview) = &self.screenshots.preview {
            let ready = self.protocol >= CLIENT_SCREENSHOT_PROTOCOL
                && matches!(self.status, crate::ConnectionStatus::Connected)
                && preview.saving.is_none();
            let action = |id, label, destination, disabled| {
                Button::new(id)
                    .label(label)
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.send_screenshot(destination, window, cx)
                    }))
            };
            return Some(
                gpui_kit::deferred(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_4()
                        .occlude()
                        .bg(gpui_kit::black().opacity(0.35))
                        .child(
                            v_flex()
                                .id("screenshot-preview")
                                .key_context("ScreenshotPreview")
                                .w_full()
                                .max_w(px(580.))
                                .max_h_full()
                                .overflow_y_scroll()
                                .p_4()
                                .gap_3()
                                .rounded_lg()
                                .shadow_lg()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(
                                            div().font_semibold().child(tr!("screenshot-preview")),
                                        )
                                        .child(
                                            Button::new("screenshot-close")
                                                .ghost()
                                                .icon(Symbol::X.icon())
                                                .accessibility_label(tr!("pane-close"))
                                                .disabled(preview.saving.is_some())
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.close_screenshot(window, cx);
                                                    cx.notify();
                                                })),
                                        ),
                                )
                                .child(
                                    img(preview.image.clone())
                                        .w_full()
                                        .h(window.viewport_size().height * 0.36)
                                        .object_fit(ObjectFit::Contain)
                                        .rounded_md()
                                        .bg(theme.secondary),
                                )
                                .child(div().id("screenshot-note").child(
                                    Textarea::new(&preview.note).aria_label(tr!("screenshot-note")),
                                ))
                                .children(
                                    preview
                                        .error
                                        .as_ref()
                                        .map(|error| div().text_sm().child(error.clone())),
                                )
                                .children(
                                    preview
                                        .saving
                                        .map(|_| div().text_sm().child(tr!("screenshot-saving"))),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .flex_wrap()
                                        .child(
                                            action(
                                                "screenshot-attach",
                                                tr!("screenshot-attach"),
                                                None,
                                                !ready
                                                    || preview.session != self.active
                                                    || !self.can_attach_screenshot(),
                                            )
                                            .primary(),
                                        )
                                        .child(action(
                                            "screenshot-memo",
                                            tr!("screenshot-memo"),
                                            Some(ScreenshotDestination::Memo),
                                            !ready,
                                        ))
                                        .child(
                                            Button::new("screenshot-report")
                                                .label(tr!("screenshot-report"))
                                                .disabled(preview.saving.is_some())
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.report_screenshot(window, cx);
                                                })),
                                        ),
                                ),
                        ),
                )
                .with_priority(12)
                .into_any_element(),
            );
        }
        if let Some(message) = &self.screenshots.feedback {
            if self.screenshots.prompt.is_none() {
                return Some(
                    gpui_kit::deferred(
                        h_flex()
                            .id("screenshot-feedback")
                            .absolute()
                            .bottom_4()
                            .right_4()
                            .max_w(px(440.))
                            .p_3()
                            .gap_2()
                            .rounded_md()
                            .shadow_lg()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(div().flex_1().min_w_0().text_sm().child(message.clone()))
                            .child(
                                Button::new("screenshot-dismiss-feedback")
                                    .ghost()
                                    .icon(Symbol::X.icon())
                                    .accessibility_label(tr!("pane-close"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.screenshots.feedback = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .with_priority(12)
                    .into_any_element(),
                );
            }
        }
        self.screenshots.prompt.map(|_| {
            gpui_kit::deferred(
                v_flex()
                    .id("screenshot-request")
                    .absolute()
                    .top_3()
                    .right_3()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .shadow_lg()
                    .child(tr!("screenshot-request"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("screenshot-allow")
                                    .primary()
                                    .label(tr!("screenshot-allow"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer_screenshot(true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("screenshot-deny")
                                    .label(tr!("screenshot-deny"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer_screenshot(false, window, cx)
                                    })),
                            ),
                    ),
            )
            .with_priority(12)
            .into_any_element()
        })
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
