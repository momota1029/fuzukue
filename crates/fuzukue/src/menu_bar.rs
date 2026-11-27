use std::{cell::Cell, rc::Rc};

use fuzukue_proto::{ClientMsg, TabView};
use gpui_kit::base::{ElementExt as _, Popup, Selectable as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{PopupMenu, PopupMenuItem},
    ActiveTheme as _, Sizable as _, TitleBar, TITLE_BAR_HEIGHT,
};
use gpui_kit::{
    div, prelude::*, px, rems, App, Bounds, Context, DismissEvent, Entity, FocusHandle,
    Focusable as _, Keystroke, Pixels, Role, SharedString, Subscription, WeakEntity, Window,
};

use crate::{appearance::UI_LINE_HEIGHT, icons::Symbol, wire_deck, RightPane, Workbench};

pub(super) const SEARCH_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "⌘K"
} else {
    "Ctrl+K"
};
const SCREENSHOT_SHORTCUT: &str = "Ctrl+Shift+S";
const MENU_OFFSET: Pixels = px(4.);
// Match the popup positioner's margin at the window edge.
const MENU_WINDOW_MARGIN: Pixels = px(8.);
pub(super) const MENU_BUTTON: &str = "app-menu";
pub(super) const SETTINGS_BUTTON: &str = "title-bar-settings";
pub(super) const SEARCH_BUTTON: &str = "title-bar-search";

pub(super) fn is_menu_shortcut(key: &Keystroke) -> bool {
    // GPUI emits an unmodified "alt" on release when Alt is pressed alone.
    match key.key.as_str() {
        "alt" => key.modifiers.number_of_modifiers() == 0,
        "f10" => key.modifiers.alt && key.modifiers.number_of_modifiers() == 1,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    File,
    Edit,
    View,
    Thread,
    Help,
}

impl Menu {
    const ALL: [Self; 5] = [Self::File, Self::Edit, Self::View, Self::Thread, Self::Help];

    fn id(self) -> &'static str {
        match self {
            Self::File => "menu-file",
            Self::Edit => "menu-edit",
            Self::View => "menu-view",
            Self::Thread => "menu-thread",
            Self::Help => "menu-help",
        }
    }

    fn adjacent(self, offset: isize) -> Self {
        let count = Self::ALL.len() as isize;
        Self::ALL[(self as isize + offset).rem_euclid(count) as usize]
    }
}

struct MenuRows {
    items: usize,
    separators: usize,
}

impl MenuRows {
    fn from_items(items: &[PopupMenuItem]) -> Self {
        // PopupMenu omits a trailing separator.
        let items = match items.split_last() {
            Some((PopupMenuItem::Separator, rest)) => rest,
            _ => items,
        };
        let separators = items
            .iter()
            .filter(|item| matches!(item, PopupMenuItem::Separator))
            .count();
        Self {
            items: items.len() - separators,
            separators,
        }
    }

    fn needs_scroll(
        &self,
        max_height: Pixels,
        rem_size: Pixels,
        snap: impl Fn(Pixels) -> Pixels,
    ) -> bool {
        // Match PopupMenu's default rows and its items box. The window-edge
        // margin is already outside max_height; the list padding is inside it.
        let padding = snap(rem_size * 0.25) * 2.;
        let gap = snap(rem_size * 0.125);
        let row_height = snap(rem_size * 1.625);
        let separator_height = snap(px(2.)) + gap * 2.;
        let gaps = (self.items + self.separators).saturating_sub(1);
        let content = row_height * self.items as f32
            + separator_height * self.separators as f32
            + gap * gaps as f32;
        snap(content + padding) > snap(max_height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dropdown {
    Closed,
    HoverReady,
    Open,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MenuState {
    #[default]
    Collapsed,
    Expanded(Menu, Dropdown),
}

#[derive(Clone, Copy)]
enum MenuInput {
    Toggle { from_button: bool },
    Click { group: Menu, was_open: bool },
    Hover(Menu),
    Move(isize),
    Open,
    Escape,
    Outside,
}

impl MenuState {
    fn next(self, input: MenuInput) -> Self {
        use MenuInput::*;
        match (self, input) {
            (Self::Collapsed, Toggle { from_button }) => Self::Expanded(
                Menu::File,
                if from_button {
                    Dropdown::HoverReady
                } else {
                    Dropdown::Closed
                },
            ),
            (Self::Expanded(group, Dropdown::Open), Escape) => {
                Self::Expanded(group, Dropdown::Closed)
            }
            (_, Toggle { .. } | Escape | Outside) => Self::Collapsed,
            (Self::Expanded(..), Click { group, was_open }) => Self::Expanded(
                group,
                if was_open {
                    Dropdown::Closed
                } else {
                    Dropdown::Open
                },
            ),
            (Self::Expanded(_, Dropdown::HoverReady | Dropdown::Open), Hover(group))
            | (Self::Expanded(group, _), Open) => Self::Expanded(group, Dropdown::Open),
            (Self::Expanded(group, dropdown), Move(offset)) => {
                Self::Expanded(group.adjacent(offset), dropdown)
            }
            _ => self,
        }
    }

    fn click(self, group: Menu) -> MenuInput {
        MenuInput::Click {
            group,
            was_open: self.dropdown() == Some(group),
        }
    }

    fn dropdown(self) -> Option<Menu> {
        match self {
            Self::Expanded(group, Dropdown::Open) => Some(group),
            _ => None,
        }
    }
}

pub(super) struct MenuBar {
    state: MenuState,
    popup: Option<(Entity<PopupMenu>, Subscription)>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl MenuBar {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            state: MenuState::Collapsed,
            popup: None,
            focus: cx.focus_handle(),
            previous_focus: None,
            bounds: Default::default(),
        }
    }

    pub(super) fn is_expanded(&self) -> bool {
        self.state != MenuState::Collapsed
    }

    fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
            || self
                .popup
                .as_ref()
                .is_some_and(|(popup, _)| popup.focus_handle(cx).contains_focused(window, cx))
    }
}

impl Workbench {
    pub(super) fn render_menu_bar(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let expanded = self.app_menu.is_expanded();
        let state = self.app_menu.state;
        let open_group = state.dropdown();
        let menu_bounds = self.app_menu.bounds.clone();
        let keep_quote = self.has_chat_quote(window, cx);
        TitleBar::new()
            .bg(cx.theme().title_bar)
            .on_close_window(move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.close_workbench(window, cx));
            })
            .child(
                // The title bar will not shrink below its content, which would push
                // the last controls out of a narrow window. A zero base width lets
                // the menu scroll instead.
                h_flex()
                    .h_full()
                    .flex_1()
                    .w_0()
                    .min_w_0()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("workbench-menu-bar")
                            .role(Role::MenuBar)
                            .track_focus(&self.app_menu.focus)
                            .occlude()
                            .h_full()
                            .min_w_0()
                            .gap_0p5()
                            .overflow_x_scroll()
                            .when(keep_quote, |bar| bar.keep_text_selection("keep-menu-quote"))
                            .on_prepaint(move |bounds, _, _| menu_bounds.set(bounds))
                            .when(expanded && open_group.is_none(), |bar| {
                                bar.on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                    this.change_app_menu(MenuInput::Outside, window, cx);
                                }))
                            })
                            .child(
                                Button::new(MENU_BUTTON)
                                    .ghost()
                                    .small()
                                    .compact()
                                    .open(expanded)
                                    .toggled(expanded)
                                    .icon(Symbol::Menu.icon())
                                    .accessibility_label(tr!("menu-button"))
                                    .tooltip(tr!("menu-button-tooltip"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.change_app_menu(
                                            MenuInput::Toggle { from_button: true },
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .when_some(self.app_menu_group(), |bar, selected| {
                                bar.children(Menu::ALL.map(|group| {
                                    let id = group.id();
                                    Popup::new(
                                        ("title-bar-menu-group", group as usize),
                                        Button::new(id)
                                            .ghost()
                                            .small()
                                            .compact()
                                            .role(Role::MenuItem)
                                            .label(tr!(id))
                                            .open(selected == group)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                // A mouse move can open the menu before the click
                                                // arrives. Use the dropdown shown in this frame.
                                                this.change_app_menu(state.click(group), window, cx);
                                            }))
                                            .on_hover(cx.listener(move |this, &hovered, window, cx| {
                                                if hovered {
                                                    this.change_app_menu(
                                                        MenuInput::Hover(group),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            })),
                                    )
                                    .offset(MENU_OFFSET)
                                    .when(
                                        open_group == Some(group),
                                        |trigger| {
                                            let (popup, _) =
                                                self.app_menu.popup.as_ref().expect("open dropdown");
                                            if keep_quote && group == Menu::Edit {
                                                trigger.content(div().relative().child(popup.clone())
                                                    .keep_text_selection("keep-edit-menu-quote"))
                                            } else {
                                                trigger.content(popup.clone())
                                            }
                                        },
                                    )
                                }))
                            }),
                    )
                    .when(
                        self.self_update
                            .status
                            .as_ref()
                            .is_some_and(|status| status.canary),
                        |bar| {
                            bar.child(
                                crate::chat::chat_activity::status_row(
                                    tr!("self-update-canary-notice"), cx.theme().muted_foreground,
                                )
                                    .id("self-update-canary-notice")
                                    .overflow_hidden()
                                    .text_sm(),
                            )
                        },
                    )
                    .child(
                        h_flex().occlude().h_full().flex_none().pr_1().gap_2()
                            .child(
                                Button::new(SEARCH_BUTTON)
                                    .ghost()
                                    .small()
                                    .compact()
                                    .toggled(self.palette.is_some())
                                    .icon(Symbol::Search.icon())
                                    .accessibility_label(tr!("sidebar-search"))
                                    .tooltip(tr!("search-button-tooltip", shortcut = SEARCH_SHORTCUT))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.change_app_menu(MenuInput::Outside, window, cx);
                                        this.close_sidebar_menu(cx);
                                        this.open_palette(window, cx);
                                    })),
                            )
                            .child(self.render_github_title_button(cx))
                            .child(
                            Button::new(SETTINGS_BUTTON)
                                .ghost()
                                .small()
                                .compact()
                                .toggled(self.settings_open)
                                .icon(Symbol::Settings.icon())
                                .accessibility_label(tr!("settings-title"))
                                .tooltip(tr!("settings-title"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.change_app_menu(MenuInput::Outside, window, cx);
                                    this.toggle_settings(cx);
                                })),
                        ),
                    )
            )
    }

    fn app_menu_group(&self) -> Option<Menu> {
        match self.app_menu.state {
            MenuState::Collapsed => None,
            MenuState::Expanded(group, _) => Some(group),
        }
    }

    pub(super) fn handle_app_menu_key(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let input = if is_menu_shortcut(key) {
            MenuInput::Toggle { from_button: false }
        } else {
            if !self.app_menu.is_expanded() || key.modifiers.number_of_modifiers() != 0 {
                return false;
            }
            match key.key.as_str() {
                "escape" => MenuInput::Escape,
                _ if !self.app_menu.contains_focus(window, cx) => return false,
                "left" => MenuInput::Move(-1),
                "right" => MenuInput::Move(1),
                // Let PopupMenu handle these keys once a dropdown is open.
                "up" | "down" | "enter" if self.app_menu.state.dropdown().is_none() => {
                    MenuInput::Open
                }
                _ => return false,
            }
        };
        self.change_app_menu(input, window, cx);
        true
    }

    fn change_app_menu(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.app_menu.state;
        let next = previous.next(input);
        if next == previous {
            return;
        }
        let had_focus = self.app_menu.contains_focus(window, cx);
        self.app_menu.state = next;
        if previous == MenuState::Collapsed {
            self.close_sidebar_menu(cx);
            self.app_menu.previous_focus = window.focused(cx);
        }
        if previous.dropdown() != next.dropdown() {
            self.app_menu.popup = next.dropdown().map(|group| {
                let view = cx.weak_entity();
                let popup = PopupMenu::build(window, cx, |menu, window, cx| {
                    let max_height = (window.visual_viewport_bounds().bottom()
                        - self.app_menu.bounds.get().bottom().max(TITLE_BAR_HEIGHT)
                        - MENU_OFFSET
                        - MENU_WINDOW_MARGIN)
                        .max(px(0.));
                    let (menu, rows) = self.app_submenu(group, menu, &view, window, cx);
                    // Only enable GPUI's scroll box when the padded item
                    // list actually needs more room than the window allows.
                    menu.max_h(max_height).scrollable(rows.needs_scroll(
                        max_height,
                        window.rem_size(),
                        |height| window.pixel_snap(height),
                    ))
                });
                popup.focus_handle(cx).focus(window, cx);
                let subscription =
                    cx.subscribe_in(&popup, window, |this, _, _: &DismissEvent, window, cx| {
                        // PopupMenu treats a press on a title-bar trigger as outside.
                        // The trigger handles that press and owns the state transition.
                        if window.last_input_was_keyboard()
                            || !this
                                .app_menu
                                .bounds
                                .get()
                                .contains(&window.mouse_position())
                        {
                            this.change_app_menu(MenuInput::Outside, window, cx);
                        }
                    });
                (popup, subscription)
            });
        }
        if next == MenuState::Collapsed {
            let previous_focus = self.app_menu.previous_focus.take();
            if had_focus {
                previous_focus
                    .unwrap_or_else(|| self.view_focus.clone())
                    .focus(window, cx);
            }
        } else if next.dropdown().is_none() {
            self.app_menu.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn app_submenu(
        &self,
        kind: Menu,
        menu: PopupMenu,
        view: &WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> (PopupMenu, MenuRows) {
        let (menu, items) = match kind {
            Menu::File => (menu, self.file_menu(view)),
            Menu::Edit => (
                menu,
                vec![
                    item(
                        view,
                        "menu-search",
                        tr!("sidebar-search"),
                        Some(SEARCH_SHORTCUT),
                        |this, window, cx| this.open_palette(window, cx),
                    ),
                    item(
                        view,
                        "menu-quote-reply",
                        tr!("menu-quote-reply"),
                        Some(crate::chat::QUOTE_REPLY_SHORTCUT),
                        |this, window, cx| {
                            this.change_app_menu(MenuInput::Outside, window, cx);
                            this.quote_reply(window, cx);
                        },
                    )
                    .disabled(!self.has_chat_quote(window, cx)),
                ],
            ),
            Menu::View => {
                let available_width =
                    (window.viewport_size().width - MENU_WINDOW_MARGIN * 2.).max(px(0.));
                let min_width = rems(16.).to_pixels(window.rem_size()).min(available_width);
                (menu.min_w(min_width), self.view_menu(view))
            }
            Menu::Thread => {
                if let Some(thread) = self.threads.iter().find(|thread| thread.id == self.active) {
                    // The title-bar version of thread_menu has rename and attach,
                    // plus link and archive when supported by the server.
                    let rows = MenuRows {
                        items: 2
                            + usize::from(self.protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL)
                            + usize::from(self.protocol >= 7),
                        separators: 0,
                    };
                    return (
                        self.thread_menu(menu, thread, false, view.clone())(window, cx),
                        rows,
                    );
                }
                (menu, vec![PopupMenuItem::label(tr!("menu-no-thread"))])
            }
            Menu::Help => (menu, self.help_menu(view)),
        };
        let rows = MenuRows::from_items(&items);
        (items.into_iter().fold(menu, PopupMenu::item), rows)
    }

    fn file_menu(&self, view: &WeakEntity<Self>) -> Vec<PopupMenuItem> {
        let mut items = Vec::new();
        if self.file_browser_supported() {
            items.push(item(
                view,
                "menu-open-file",
                tr!("menu-open-file"),
                Some(if cfg!(target_os = "macos") {
                    "⌘O"
                } else {
                    "Ctrl+O"
                }),
                |this, window, cx| {
                    this.change_app_menu(MenuInput::Outside, window, cx);
                    this.toggle_file_browser(window, cx);
                },
            ));
        }
        items.extend([
            item(
                view,
                "menu-new-thread",
                tr!("menu-new-thread"),
                None,
                |this, _, _| {
                    let _ = this.out.send(ClientMsg::NewThread);
                },
            )
            .disabled(self.protocol < 4),
            PopupMenuItem::separator(),
            item(
                view,
                "menu-open-workspace",
                tr!("menu-open-workspace"),
                None,
                |this, window, cx| {
                    this.close_sidebar_menu(cx);
                    this.open_workspace_browser("~", window, cx);
                },
            )
            .disabled(self.protocol < 11),
            item(
                view,
                "menu-close-workspace",
                tr!("menu-close-workspace"),
                None,
                |this, window, cx| this.close_workbench(window, cx),
            ),
            item(
                view,
                "menu-stop-server",
                tr!("menu-stop-server"),
                None,
                |this, window, cx| {
                    this.change_app_menu(MenuInput::Outside, window, cx);
                    this.stop_server(window, cx);
                },
            )
            .disabled(!self.can_request_shutdown()),
        ]);
        items
    }

    fn view_menu(&self, view: &WeakEntity<Self>) -> Vec<PopupMenuItem> {
        let paper_disabled = self.paper_edit_target().is_none();
        let mut items = Vec::new();
        for destination in crate::sidebar::ViewDestination::ALL {
            let (id, label, icon, protocol) = destination.item();
            items.push(
                item(view, id, label, None, move |this, window, cx| {
                    destination.run(this, window, cx);
                })
                .icon(icon.icon())
                .disabled(self.protocol < protocol),
            );
        }
        items.extend([
            item(
                view,
                "tool-paper",
                tr!("sidebar-edit-paper"),
                None,
                |this, _, cx| {
                    if let Some(deck) = this.paper_edit_target().map(str::to_owned) {
                        this.open_paper_deck(deck, cx);
                    }
                },
            )
            .disabled(paper_disabled),
            PopupMenuItem::separator(),
        ]);
        if self.protocol >= fuzukue_proto::HANDOFF_PROTOCOL {
            items.push(item(
                view,
                "more-handoff",
                tr!("handoff-title"),
                None,
                |this, _, cx| this.open_handoffs(cx),
            ));
        }
        if self.protocol >= 6 && !self.active.is_empty() {
            items.push(item(
                view,
                "more-thread-info",
                tr!("sidebar-thread-details"),
                None,
                |this, _, cx| this.show_thread_info_from_sidebar(cx),
            ));
        }
        if self.protocol >= fuzukue_proto::TESTS_PROTOCOL {
            items.push(item(
                view,
                "more-tests",
                tr!("sidebar-tests"),
                None,
                |this, _, cx| this.show_tests(cx),
            ));
        }
        for extension in self
            .extensions
            .items
            .iter()
            .filter(|item| item.enabled && item.view)
        {
            let id = extension.id.clone();
            items.push(item(
                view,
                format!("extension-view-{}", extension.name),
                extension.title.clone(),
                None,
                move |this, window, cx| this.open_extension(id.clone(), window, cx),
            ));
        }
        items.push(item(
            view,
            "more-screenshot",
            tr!("screenshot-capture"),
            Some(SCREENSHOT_SHORTCUT),
            |this, window, cx| this.capture_screenshot(window, cx),
        ));
        if !self.settings_open && self.right != RightPane::None {
            let label = if self.right == RightPane::Read
                || (self.right == RightPane::Source && self.source.centered)
            {
                tr!("sidebar-back-to-chat")
            } else {
                tr!("sidebar-close-right-pane")
            };
            items.push(PopupMenuItem::separator());
            items.push(item(view, "tool-close", label, None, |this, _, cx| {
                this.close_right_pane(cx)
            }));
        }
        items
    }

    fn help_menu(&self, view: &WeakEntity<Self>) -> Vec<PopupMenuItem> {
        vec![
            item(
                view,
                "tool-settings",
                tr!("sidebar-settings"),
                None,
                |this, _, cx| {
                    this.toggle_settings(cx);
                },
            ),
            item(
                view,
                "menu-check-updates",
                tr!("menu-check-updates"),
                None,
                |this, _, cx| this.check_client_update(cx),
            )
            .disabled(crate::updater::platform().is_none() || self.connection_epoch == 0),
            item(
                view,
                "menu-report-bug",
                tr!("github-report-menu"),
                None,
                |this, window, cx| this.open_github_report(window, cx),
            ),
        ]
    }

    pub(super) fn paper_edit_target(&self) -> Option<&str> {
        let attached = self
            .threads
            .iter()
            .find(|thread| thread.id == self.active)
            .map_or(&[][..], |thread| thread.attached.as_slice());
        let open = (self.right == RightPane::Read).then_some(self.deck.as_str());
        paper_for_editing(&self.tabs, attached, open)
    }
}

fn item(
    view: &WeakEntity<Workbench>,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    run: impl Fn(&mut Workbench, &mut Window, &mut Context<Workbench>) + 'static,
) -> PopupMenuItem {
    let (id, label, view) = (id.into(), label.into(), view.clone());
    PopupMenuItem::element(move |_, cx: &mut App| {
        h_flex()
            .id(id.clone())
            .w_full()
            .line_height(UI_LINE_HEIGHT)
            .gap_5()
            .justify_between()
            .child(div().min_w_0().truncate().child(label.clone()))
            .children(shortcut.map(|shortcut| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .pr_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(shortcut)
            }))
    })
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |this, cx| run(this, window, cx));
    })
}

fn paper_for_editing<'a>(
    tabs: &[TabView],
    attached: &'a [String],
    open: Option<&'a str>,
) -> Option<&'a str> {
    let is_paper = |deck: &str| {
        tabs.iter().any(|tab| {
            (matches!(tab.kind.as_str(), "paper" | "book") && tab.deck.as_deref() == Some(deck))
                || tab
                    .papers
                    .iter()
                    .any(|paper| paper.decks.iter().any(|path| path == deck))
        })
    };
    let mut attached = attached
        .iter()
        .map(|deck| wire_deck(deck))
        .filter(|deck| is_paper(deck));
    let first = attached.next();
    match (first, open.filter(|deck| is_paper(deck))) {
        (None, open) => open,
        (Some(first), Some(open)) if first == open || attached.any(|deck| deck == open) => {
            Some(open)
        }
        (first, _) => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAMBURGER: MenuInput = MenuInput::Toggle { from_button: true };
    const KEYBOARD_SHORTCUT: MenuInput = MenuInput::Toggle { from_button: false };

    #[test]
    fn menu_bar_toggles_from_its_collapsed_default() {
        let state = MenuState::default();
        assert_eq!(state, MenuState::Collapsed);
        let expanded = state.next(HAMBURGER);
        assert_eq!(
            expanded,
            MenuState::Expanded(Menu::File, Dropdown::HoverReady)
        );
        assert_eq!(expanded.next(HAMBURGER), state);
    }

    #[test]
    fn menu_bar_switches_on_hover_until_dismissed() {
        let collapsed = MenuState::default();
        assert_eq!(collapsed.next(MenuInput::Hover(Menu::View)), collapsed);
        let hamburger = collapsed.next(HAMBURGER);
        assert_eq!(hamburger.dropdown(), None);
        let file = hamburger.next(MenuInput::Hover(Menu::File));
        assert_eq!(file.dropdown(), Some(Menu::File));
        // A click from the frame before hover opened the popup still opens it.
        assert_eq!(file.next(hamburger.click(Menu::File)), file);

        let expanded = collapsed.next(KEYBOARD_SHORTCUT);
        assert_eq!(expanded.next(MenuInput::Hover(Menu::View)), expanded);
        let view = expanded.next(expanded.click(Menu::View));
        assert_eq!(view.dropdown(), Some(Menu::View));
        assert_eq!(view.next(MenuInput::Hover(Menu::View)), view);
        let help = view.next(MenuInput::Hover(Menu::Help));
        assert_eq!(help.dropdown(), Some(Menu::Help));

        for input in [
            help.click(Menu::Help),
            MenuInput::Escape,
            MenuInput::Outside,
        ] {
            let closed = help.next(input);
            assert_eq!(closed.dropdown(), None);
            assert_eq!(closed.next(MenuInput::Hover(Menu::File)), closed);
        }
        let closed = help.next(help.click(Menu::Help));
        assert_eq!(closed.next(closed.click(Menu::Help)), help);
    }

    #[test]
    fn menu_bar_arrows_wrap_with_or_without_a_dropdown() {
        let file = MenuState::default().next(KEYBOARD_SHORTCUT);
        let help = file.next(MenuInput::Move(-1));
        assert_eq!(help, MenuState::Expanded(Menu::Help, Dropdown::Closed));
        assert_eq!(help.next(MenuInput::Move(1)), file);
        let help_dropdown = help.next(MenuInput::Open);
        assert_eq!(
            help_dropdown.next(MenuInput::Move(1)).dropdown(),
            Some(Menu::File)
        );
        let file_dropdown = file.next(MenuInput::Open);
        assert_eq!(file_dropdown.next(MenuInput::Move(-1)), help_dropdown);
    }

    #[test]
    fn menu_bar_escape_closes_in_order_and_outside_click_collapses() {
        let expanded = MenuState::default().next(HAMBURGER);
        let open = expanded.next(expanded.click(Menu::View));
        let expanded = open.next(MenuInput::Escape);
        assert_eq!(expanded, MenuState::Expanded(Menu::View, Dropdown::Closed));
        assert_eq!(expanded.next(MenuInput::Escape), MenuState::Collapsed);
        assert_eq!(open.next(MenuInput::Outside), MenuState::Collapsed);
        assert_eq!(expanded.next(MenuInput::Outside), MenuState::Collapsed);
    }

    #[test]
    fn menu_bar_scrolls_only_when_the_padded_rows_exceed_the_available_height() {
        // At 16 px/rem, normal rows are 26 px, gaps are 2 px, separators
        // are 6 px with their margins, and top/bottom padding totals 8 px.
        let cases = [
            (Menu::File, 3, 1, 98.),
            (Menu::Edit, 2, 0, 62.),
            (
                Menu::View,
                crate::sidebar::ViewDestination::ALL.len() + 2,
                1,
                238.,
            ),
            (Menu::Thread, 2, 0, 62.),
            (Menu::Help, 3, 0, 90.),
        ];
        for (group, items, separators, height) in cases {
            let rows = MenuRows { items, separators };
            assert!(
                !rows.needs_scroll(px(600.), px(16.), |value| value),
                "{group:?}"
            );
            assert!(
                !rows.needs_scroll(px(height), px(16.), |value| value),
                "{group:?}"
            );
            assert!(
                rows.needs_scroll(px(height - 1.), px(16.), |value| value),
                "{group:?}"
            );
        }
    }

    #[test]
    fn menu_bar_counts_only_rendered_separators() {
        let rows = MenuRows::from_items(&[
            PopupMenuItem::new("First"),
            PopupMenuItem::separator(),
            PopupMenuItem::new("Second"),
            PopupMenuItem::separator(),
        ]);
        assert_eq!(rows.items, 2);
        assert_eq!(rows.separators, 1);
    }

    #[test]
    fn menu_bar_does_not_scroll_at_an_exact_fit_with_fractional_scaling() {
        let help = MenuRows {
            items: 2,
            separators: 0,
        };
        // At 125% scaling, each row is 30 device pixels, the gap is 2,
        // and the padding is 5 at each end.
        let snap = |height: Pixels| (height * 1.25).round() / 1.25;
        assert!(!help.needs_scroll(px(57.6), px(15.), snap));
        assert!(help.needs_scroll(px(56.8), px(15.), snap));
    }

    #[test]
    fn paper_editing_uses_only_an_attached_or_open_paper() {
        let tabs: Vec<TabView> = serde_json::from_value(serde_json::json!([
            {"id":"paper","title":"Paper","summary":"","status":"","next":"","deck":"papers/p","kind":"paper",
             "papers":[],"notes":[],"lean":[],"related":[]},
            {"id":"research","title":"Research","summary":"","status":"","next":"","deck":"research/r","kind":"research",
             "papers":[],"notes":[],"lean":[],"related":[]}
        ])).unwrap();
        let paper = "papers/p";
        let research = "research/r";
        assert_eq!(paper_for_editing(&tabs, &[paper.into()], None), Some(paper));
        assert_eq!(paper_for_editing(&tabs, &[], None), None);
        assert_eq!(
            paper_for_editing(&tabs, &[research.into()], Some(research)),
            None
        );
        assert_eq!(
            paper_for_editing(&tabs, &[research.into()], Some(paper)),
            Some(paper)
        );
    }
}
