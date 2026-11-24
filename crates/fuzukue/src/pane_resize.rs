use std::{cell::Cell, path::PathBuf, rc::Rc};

use gpui_kit::base::ElementExt as _;
use gpui_kit::component::{h_flex, tooltip::Tooltip, v_flex, ActiveTheme as _};
use gpui_kit::{
    canvas, div, prelude::*, px, AnyElement, Context, Div, FocusHandle, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Role, Window,
};
use toml::{Table, Value};

use crate::{settings_file, RightPane, Workbench};

const HANDLE_WIDTH: f32 = 8.;
const DIVIDER_WIDTH: f32 = 1.;
const CHAT_MIN: f32 = 360.;
const PANE_MIN: f32 = 280.;
pub(super) const PANE_INSET: Pixels = px(12.);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitKind {
    RightPane,
    SideChat,
    Sidebar,
}

impl SplitKind {
    fn splitter_id(self) -> &'static str {
        match self {
            Self::RightPane | Self::SideChat => "right-pane-splitter",
            Self::Sidebar => "sidebar-splitter",
        }
    }

    fn divider_id(self) -> &'static str {
        match self {
            Self::RightPane | Self::SideChat => "right-pane-divider",
            Self::Sidebar => "sidebar-divider",
        }
    }

    fn width_delta(self, pointer_delta: f32) -> f32 {
        match self {
            Self::RightPane | Self::SideChat => -pointer_delta,
            Self::Sidebar => pointer_delta,
        }
    }

    fn state_key(self) -> &'static str {
        match self {
            Self::RightPane => "right_pane_width",
            Self::SideChat => "side_chat_width",
            Self::Sidebar => unreachable!("sidebar width is stored in appearance settings"),
        }
    }

    fn load_width(self, state: &Table) -> Option<f32> {
        let value = state.get(self.state_key())?;
        let width = match value {
            Value::Float(width) => *width as f32,
            Value::Integer(width) => *width as f32,
            _ => return None,
        };
        (width.is_finite() && width > 0.).then_some(width)
    }

    fn store_width(self, state: &mut Table, width: Option<f32>) -> bool {
        let key = self.state_key();
        match width {
            Some(width) => {
                let value = Value::Float(width.into());
                if state.get(key) == Some(&value) {
                    return false;
                }
                state.insert(key.into(), value);
                true
            }
            None => state.remove(key).is_some(),
        }
    }
}

#[derive(Clone, Copy)]
struct PaneLayout {
    kind: SplitKind,
    available: f32,
    default_width: f32,
}

impl PaneLayout {
    fn clamp(self, width: f32) -> f32 {
        let available = (self.available - DIVIDER_WIDTH).max(0.);
        let (center_min, right_min) = match self.kind {
            SplitKind::RightPane => (CHAT_MIN, PANE_MIN),
            SplitKind::SideChat => (PANE_MIN, CHAT_MIN),
            SplitKind::Sidebar => {
                let limits = fuzukue_view::appearance::SIDEBAR_WIDTH;
                let max = (available - 160.).clamp(*limits.start(), *limits.end());
                return width.clamp(*limits.start(), max);
            }
        };
        // At small window sizes shrink both minima by the same proportion.
        let scale = (available / (center_min + right_min)).min(1.);
        let min = right_min * scale;
        let max = (available - center_min * scale).max(min);
        width.clamp(min, max)
    }
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    kind: SplitKind,
    start_x: Pixels,
    start_width: f32,
    preview: f32,
    pending: Option<f32>,
}

impl ResizeDrag {
    fn width_at(&self, x: Pixels, layout: PaneLayout) -> f32 {
        layout.clamp(self.start_width + self.kind.width_delta(f32::from(x - self.start_x).round()))
    }
}

pub(super) struct PaneResize {
    path: Option<PathBuf>,
    widths: [Option<f32>; 2],
    focus: [FocusHandle; 3],
    drag: Rc<Cell<Option<ResizeDrag>>>,
}

impl PaneResize {
    pub(super) fn load(settings_path: Option<PathBuf>, cx: &Context<Workbench>) -> Self {
        let path = settings_path.map(|path| path.with_extension("state.toml"));
        let state = path
            .as_deref()
            .and_then(|path| settings_file::read(path).ok())
            .unwrap_or_default();
        Self {
            path,
            widths: [SplitKind::RightPane, SplitKind::SideChat].map(|kind| kind.load_width(&state)),
            focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            drag: Rc::default(),
        }
    }

    fn width(&self, layout: PaneLayout, saved: Option<f32>) -> f32 {
        let width = self
            .drag
            .get()
            .filter(|drag| drag.kind == layout.kind)
            .map(|drag| drag.preview)
            .or(saved)
            .unwrap_or(layout.default_width);
        layout.clamp(width)
    }

    fn save(&mut self, kind: SplitKind, width: Option<f32>) -> anyhow::Result<()> {
        self.widths[kind as usize] = width;
        if let Some(path) = &self.path {
            settings_file::update(path, |state| Ok(kind.store_width(state, width)))?;
        }
        Ok(())
    }
}

impl Workbench {
    fn split_width(&self, layout: PaneLayout) -> f32 {
        let saved = match layout.kind {
            SplitKind::Sidebar => Some(self.appearance.sidebar_width()),
            kind => self.pane_resize.widths[kind as usize],
        };
        self.pane_resize.width(layout, saved)
    }

    pub(super) fn render_workbench_layout(
        &mut self,
        chat: Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let available = f32::from(window.viewport_size().width);
        let layout = PaneLayout {
            kind: SplitKind::Sidebar,
            available,
            default_width: fuzukue_view::appearance::AppearanceSettings::default().sidebar_width,
        };
        let width = self.split_width(layout);
        let content = self.render_workbench_columns(chat, available - width - DIVIDER_WIDTH, window, cx);
        h_flex()
            .relative()
            .size_full()
            .child(
                div().id("workbench-sidebar").w(px(width)).h_full().flex_none()
                    .child(self.render_sidebar(cx)),
            )
            .child(splitter_space())
            .child(div().flex_1().min_w_0().h_full().child(content))
            .child(self.render_pane_splitter(layout, cx).left(px(width)))
    }

    pub(super) fn render_pane_notice(&self, pane: AnyElement, cx: &Context<Self>) -> AnyElement {
        let Some(error) = self.trash_error.as_ref() else {
            return pane;
        };
        let theme = cx.theme();
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                div().flex_none().px(PANE_INSET).py_1()
                    .border_b_1().border_color(theme.sidebar_border)
                    .text_sm().text_color(theme.danger).child(error.clone()),
            )
            .child(h_flex().flex_1().min_h_0().w_full().child(pane))
            .into_any_element()
    }

    pub(super) fn render_workbench_columns(
        &mut self,
        chat: Div,
        available: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let row = h_flex()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground);
        let pane_width = |fraction: f32| (available - DIVIDER_WIDTH) * fraction;
        let details_width = (f32::from(window.viewport_size().width) * 0.29).clamp(210., 340.);
        let (pane, default_width) = if self.settings_open {
            (
                self.render_settings_pane(cx).into_any_element(),
                pane_width(0.4).clamp(300., 520.),
            )
        } else {
            match self.right {
                RightPane::Read => {
                    let pane = self.render_read_pane(window, cx).into_any_element();
                    let center = div()
                        .id("paper-centre-column")
                        .size_full()
                        .min_w_0()
                        .child(self.render_right_pane_notice(pane, cx));
                    let width = self.appearance.paper_chat_width().min(pane_width(0.48));
                    return self.split_columns(
                        row,
                        center,
                        chat.id("paper-chat-column").w_full(),
                        PaneLayout {
                            kind: SplitKind::SideChat,
                            available,
                            default_width: width,
                        },
                        cx,
                    );
                }
                RightPane::Source if self.source.centered => {
                    let pane = self.render_source_pane(cx).into_any_element();
                    let center = self.render_right_pane_notice(pane, cx);
                    return self.split_columns(
                        row,
                        center,
                        chat.w_full(),
                        PaneLayout {
                            kind: SplitKind::SideChat,
                            available,
                            default_width: CHAT_MIN,
                        },
                        cx,
                    );
                }
                RightPane::None if self.thread_info_visible() => {
                    (self.render_thread_info(cx), details_width)
                }
                RightPane::None => {
                    self.cancel_hidden_pane_drag(None);
                    return row.child(chat);
                }
                RightPane::Files => {
                    let layout = PaneLayout {
                        kind: SplitKind::RightPane,
                        available,
                        default_width: pane_width(0.65),
                    };
                    let pane = self.render_file_browser(px(self.split_width(layout)), cx);
                    let pane = self.render_right_pane_notice(pane, cx);
                    return self.split_columns(row, chat, pane, layout, cx);
                }
                RightPane::Library => (self.render_library(cx), pane_width(0.65)),
                RightPane::Handoff => (
                    self.render_handoffs(cx).into_any_element(),
                    pane_width(0.65),
                ),
                RightPane::BranchReview => (
                    self.render_branch_review(cx).into_any_element(),
                    pane_width(0.65),
                ),
                RightPane::Trash => (self.render_trash(cx).into_any_element(), pane_width(0.65)),
                RightPane::ResearchIssues | RightPane::ResearchIssue(_) => (
                    self.render_research_pane(cx).into_any_element(),
                    pane_width(0.65),
                ),
                RightPane::Exploration => (
                    self.render_exploration(cx).into_any_element(),
                    pane_width(0.65),
                ),
                RightPane::Extension => (
                    self.render_extension(cx).into_any_element(),
                    pane_width(0.65),
                ),
                RightPane::DeckComments => (
                    self.render_deck_comments(cx).into_any_element(),
                    pane_width(0.5),
                ),
                RightPane::Overview => (self.render_card_list(cx).into_any_element(), 380.),
                RightPane::DeckCards => (self.render_deck_cards(cx).into_any_element(), 380.),
                RightPane::Findings => (self.render_findings(cx).into_any_element(), 520.),
                RightPane::Inbox => (self.render_inbox(cx).into_any_element(), 360.),
                RightPane::Card => (self.render_card_detail(window, cx).into_any_element(), 520.),
                RightPane::Pdf => (
                    self.render_pdf_pane(cx).into_any_element(),
                    pane_width(0.45),
                ),
                RightPane::Issue(n) => (self.render_issue_detail(n, cx).into_any_element(), 520.),
                RightPane::Note => (
                    self.render_note_pane(cx).into_any_element(),
                    pane_width(0.45),
                ),
                RightPane::Source => (
                    self.render_source_pane(cx).into_any_element(),
                    pane_width(0.45),
                ),
                RightPane::Tests => (
                    self.render_tests_pane(cx).into_any_element(),
                    pane_width(0.45),
                ),
            }
        };

        let pane = self.render_right_pane_notice(pane, cx);

        let show_details =
            self.right != RightPane::None && self.details_view_visible() && !self.settings_open;
        let layout = PaneLayout {
            kind: SplitKind::RightPane,
            available,
            default_width: default_width + if show_details { details_width } else { 0. },
        };
        let pane = if show_details {
            let width = self.split_width(layout);
            let details = self.render_thread_info(cx);
            // Keep both views usable when there is too little room for two columns.
            if width >= PANE_MIN + 210. {
                h_flex()
                    .size_full()
                    .child(h_flex().flex_1().h_full().min_w_0().child(pane))
                    .child(
                        div()
                            .w(px(details_width.min(width - PANE_MIN)))
                            .flex_none()
                            .h_full()
                            .border_l_1()
                            .border_color(theme.sidebar_border)
                            .child(details),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .size_full()
                    .child(h_flex().flex_1().min_h_0().w_full().child(pane))
                    .child(
                        div()
                            .h(gpui_kit::relative(0.4))
                            .w_full()
                            .flex_none()
                            .border_t_1()
                            .border_color(theme.sidebar_border)
                            .child(details),
                    )
                    .into_any_element()
            }
        } else {
            pane
        };
        self.split_columns(row, chat, pane, layout, cx)
    }

    fn render_right_pane_notice(&self, pane: AnyElement, cx: &Context<Self>) -> AnyElement {
        match self.pane_close_id() {
            Some("symbol-close") | None => pane,
            Some(_) => self.render_pane_notice(pane, cx),
        }
    }

    fn cancel_hidden_pane_drag(&self, visible: Option<SplitKind>) {
        if self
            .pane_resize
            .drag
            .get()
            .is_some_and(|drag| drag.kind != SplitKind::Sidebar && Some(drag.kind) != visible)
        {
            self.pane_resize.drag.set(None);
        }
    }

    fn split_columns(
        &mut self,
        row: Div,
        center: impl IntoElement,
        right: impl IntoElement,
        layout: PaneLayout,
        cx: &Context<Self>,
    ) -> Div {
        self.cancel_hidden_pane_drag(Some(layout.kind));
        let width = px(self.split_width(layout));
        let center = h_flex().flex_1().min_w_0().h_full().overflow_hidden().child(center);
        let right = h_flex().id("workbench-right-pane").w(width).flex_none().min_w_0()
            .h_full().overflow_hidden().child(right);
        let scope = self.reader_selection_scope;
        let (center, right) = match layout.kind {
            SplitKind::SideChat => (center.text_selection_region(scope).into_any_element(), right.into_any_element()),
            _ => (center.into_any_element(), right.text_selection_region(scope).into_any_element()),
        };
        row.relative().child(center).child(splitter_space()).child(right)
            .child(self.render_pane_splitter(layout, cx).right(width - px(HANDLE_WIDTH - DIVIDER_WIDTH)))
    }

    fn render_pane_splitter(&self, layout: PaneLayout, cx: &Context<Self>) -> gpui_kit::Stateful<Div> {
        let kind = layout.kind;
        let focus = &self.pane_resize.focus[kind as usize];
        let theme = cx.theme();
        let accent = theme.ring;
        let line = if self.pane_resize.drag.get().is_some_and(|drag| drag.kind == kind) {
            accent
        } else {
            theme.sidebar_border
        };
        // Listen at window scope so the divider keeps receiving movement and release
        // after the pointer outruns the divider's hit area.
        let workbench = cx.weak_entity();
        let drag = self.pane_resize.drag.clone();
        let capture = canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let moving = workbench.clone();
                let moving_drag = drag.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase.capture() && moving_drag.get().is_some_and(|drag| drag.kind == kind) {
                        let _ = moving.update(cx, |this, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.move_pane_splitter(event.position.x, layout, window, cx);
                            } else {
                                this.finish_pane_drag(None, layout, cx);
                            }
                        });
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase.capture() && event.button == MouseButton::Left
                        && drag.get().is_some_and(|drag| drag.kind == kind)
                    {
                        let _ = workbench.update(cx, |this, cx| {
                            this.finish_pane_drag(Some(event.position.x), layout, cx);
                            cx.stop_propagation();
                        });
                    }
                });
            },
        )
        .absolute()
        .size(px(0.));

        div()
            .id(kind.splitter_id())
            .role(Role::Splitter)
            .aria_label(tr!("workbench-resize-pane"))
            .track_focus(focus)
            .absolute()
            .top_0()
            .w(px(HANDLE_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .cursor_col_resize()
            .occlude()
            .focus_visible(move |style| style.bg(accent.opacity(0.18)))
            .tooltip(|window, cx| Tooltip::new(tr!("workbench-resize-pane")).build(window, cx))
            .child(
                div()
                    .id(kind.divider_id())
                    .w(px(DIVIDER_WIDTH))
                    .h_full()
                    .flex_none()
                    .bg(line)
                    .group_hover(kind.splitter_id(), move |style| style.bg(accent)),
            )
            .group(kind.splitter_id())
            .child(capture)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.pane_resize.focus[kind as usize], cx);
                    if event.click_count == 2 {
                        this.pane_resize.drag.set(None);
                        this.save_pane_width(layout.kind, None, cx);
                    } else {
                        let width = this.split_width(layout);
                        this.pane_resize.drag.set(Some(ResizeDrag {
                            kind: layout.kind,
                            start_x: event.position.x,
                            start_width: width,
                            preview: width,
                            pending: None,
                        }));
                        cx.notify();
                    }
                    cx.stop_propagation();
                }),
            )
            .on_key_down(
                cx.listener(move |this, event: &gpui_kit::KeyDownEvent, _, cx| {
                    let modifiers = event.keystroke.modifiers;
                    if modifiers.control
                        || modifiers.alt
                        || modifiers.platform
                        || this.pane_resize.drag.get().is_some()
                    {
                        return;
                    }
                    let step = if modifiers.shift { 64. } else { 16. };
                    let delta = kind.width_delta(match event.keystroke.key.as_str() {
                        "left" => -step,
                        "right" => step,
                        _ => return,
                    });
                    let current = this.split_width(layout);
                    let width = layout.clamp(current + delta);
                    if width != current {
                        this.save_pane_width(layout.kind, Some(width), cx);
                    }
                    cx.stop_propagation();
                }),
            )
    }

    fn move_pane_splitter(
        &mut self,
        x: Pixels,
        layout: PaneLayout,
        window: &mut Window,
        cx: &Context<Self>,
    ) {
        let Some(mut drag) = self.pane_resize.drag.get() else {
            return;
        };
        let width = drag.width_at(x, layout);
        if drag.pending.is_none() {
            if width == drag.preview {
                return;
            }
            // Coalesce pointer events into one transcript width change per frame.
            cx.on_next_frame(window, |this, _, cx| {
                let Some(mut drag) = this.pane_resize.drag.get() else {
                    return;
                };
                if let Some(width) = drag.pending.take().filter(|width| *width != drag.preview) {
                    drag.preview = width;
                    cx.notify();
                }
                this.pane_resize.drag.set(Some(drag));
            });
        }
        drag.pending = Some(width);
        self.pane_resize.drag.set(Some(drag));
    }

    fn finish_pane_drag(&mut self, x: Option<Pixels>, layout: PaneLayout, cx: &mut Context<Self>) {
        let Some(drag) = self.pane_resize.drag.take() else {
            return;
        };
        let width = x.map_or_else(
            || layout.clamp(drag.pending.unwrap_or(drag.preview)),
            |x| drag.width_at(x, layout),
        );
        if width != layout.clamp(drag.start_width) {
            self.save_pane_width(layout.kind, Some(width), cx);
        }
        cx.notify();
    }

    fn save_pane_width(&mut self, kind: SplitKind, width: Option<f32>, cx: &mut Context<Self>) {
        let result = if kind == SplitKind::Sidebar {
            self.appearance.remember_sidebar_width(width.unwrap_or_else(|| {
                fuzukue_view::appearance::AppearanceSettings::default().sidebar_width
            }))
        } else {
            self.pane_resize.save(kind, width)
        };
        if let Err(error) = result {
            self.notice = Some(
                match kind {
                    SplitKind::RightPane => {
                        tr!("workbench-cannot-save-pane-width", err = error.to_string())
                    }
                    SplitKind::SideChat => {
                        tr!("workbench-cannot-save-chat-width", err = error.to_string())
                    }
                    SplitKind::Sidebar => {
                        tr!("workbench-cannot-save-sidebar-width", err = error.to_string())
                    }
                }
                .into(),
            );
        }
        cx.notify();
    }
}

// The hit area overlays the right column's leading edge, clear of the left
// column's scrollbar. Only the divider consumes layout width.
fn splitter_space() -> Div {
    div().w(px(DIVIDER_WIDTH)).h_full().flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_resize_clamps_to_logical_space_and_scales_both_minima_in_small_windows() {
        let mut layout = PaneLayout {
            kind: SplitKind::RightPane,
            available: 960.,
            default_width: 520.,
        };
        assert_eq!(layout.clamp(layout.default_width), 520.);
        assert_eq!(layout.clamp(50.), PANE_MIN);
        assert_eq!(layout.clamp(900.), 599.);
        layout.available = 641.;
        assert_eq!(layout.clamp(520.), PANE_MIN);
        layout.available = 321.;
        assert_eq!(layout.clamp(520.), PANE_MIN / 2.);
        assert_eq!(layout.clamp(50.), PANE_MIN / 2.);
        layout.kind = SplitKind::SideChat;
        assert_eq!(layout.clamp(520.), CHAT_MIN / 2.);
        layout.available = 0.;
        assert_eq!(layout.clamp(520.), 0.);
    }

    #[test]
    fn pane_resize_state_round_trips_widths_and_resets_only_its_own_key() {
        let right = SplitKind::RightPane;
        let chat = SplitKind::SideChat;
        let mut state: Table = toml::from_str("[sidebar]\nmore_open = true\n").unwrap();
        assert_eq!(right.load_width(&state), None);
        assert!(right.store_width(&mut state, Some(570.5)));
        assert!(!right.store_width(&mut state, Some(570.5)));
        assert!(chat.store_width(&mut state, Some(390.)));
        let mut state: Table = toml::from_str(&toml::to_string(&state).unwrap()).unwrap();
        assert_eq!(right.load_width(&state), Some(570.5));
        assert!(right.store_width(&mut state, None));
        assert!(!right.store_width(&mut state, None));
        assert_eq!(right.load_width(&state), None);
        assert_eq!(chat.load_width(&state), Some(390.));
        assert_eq!(state["sidebar"]["more_open"].as_bool(), Some(true));

        for value in ["0", "-10.0", "inf", "nan", "'wide'"] {
            let state = toml::from_str(&format!("right_pane_width = {value}")).unwrap();
            assert_eq!(right.load_width(&state), None);
        }
        let state = toml::from_str("right_pane_width = 420").unwrap();
        assert_eq!(right.load_width(&state), Some(420.));
    }
}
