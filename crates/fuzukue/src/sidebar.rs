use std::{collections::{HashMap, HashSet}, path::PathBuf};

use fuzukue_i18n::{Locale, STATUS_SEPARATOR};
use fuzukue_proto::{ClientMsg, GitStatus, TabView, WorkspaceInfo};
use gpui_kit::base::{Disableable as _, Popover, Selectable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{
    div, prelude::*, px, relative, Anchor, Context, Div, ElementId, FocusHandle, Pixels,
    SharedString, Stateful, WeakEntity, Window,
};
use toml::{Table, Value};

use crate::{appearance::UI_LINE_HEIGHT, icons::Symbol, threads::ThreadDragPreview, wire_deck, RightPane, Workbench};

pub(super) const SIDEBAR_ROW_INSET: Pixels = px(12.);
pub(super) const SIDEBAR_ICON_GAP: Pixels = px(8.);
const SIDEBAR_ICON_SIZE: Pixels = px(16.);
const SIDEBAR_ROW_MIN_HEIGHT: Pixels = px(34.);

pub(super) fn sidebar_row() -> Div {
    h_flex()
        .px(SIDEBAR_ROW_INSET)
        .py_1()
        .gap(SIDEBAR_ICON_GAP)
        .min_h(SIDEBAR_ROW_MIN_HEIGHT)
}

pub(super) fn sidebar_icon(icon: impl IntoElement) -> Div {
    h_flex().size(SIDEBAR_ICON_SIZE).flex_none().justify_center().child(icon)
}

#[derive(Default)]
pub(super) struct Sidebar {
    workspace_popover_open: bool,
    pub server_settings: bool,
    pub workspaces_collapsed: bool,
    preferences: SidebarPreferences,
    pub(super) thread_focus: HashMap<String, FocusHandle>,
    pub(super) thread_drag: Option<WeakEntity<ThreadDragPreview>>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct SidebarPreferences {
    more_open: Option<bool>,
    manual_order: HashMap<String, Vec<String>>,
    group_open: HashMap<String, HashMap<String, bool>>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl SidebarPreferences {
    fn load(settings_path: Option<&std::path::Path>, state_path: Option<PathBuf>) -> Self {
        let read = |path: &std::path::Path| -> Option<Self> {
            let settings = crate::settings_file::read(path).ok()?;
            settings.get("sidebar")?.clone().try_into().ok()
        };
        // Older clients stored sidebar preferences together with appearance settings.
        let mut preferences = settings_path.and_then(read).unwrap_or_default();
        if let Some(state) = state_path.as_deref().and_then(read) {
            preferences.more_open = state.more_open.or(preferences.more_open);
            preferences.manual_order.extend(state.manual_order);
            preferences.group_open.extend(state.group_open);
        }
        preferences.path = state_path;
        preferences
    }
}

fn preference_table<'a>(settings: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    settings.entry(key.to_owned()).or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut().ok_or_else(|| anyhow::anyhow!("invalid settings table: {key}"))
}

fn card_collection_target<'a>(
    attached: &'a [String],
    open: Option<&'a str>,
    tabs: &[TabView],
    is_paper: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    let is_collection = |deck: &str| {
        !is_paper(deck) && tabs.iter().any(|tab| tab.deck.as_deref() == Some(deck))
    };
    open.filter(|deck| is_collection(deck)).or_else(|| {
        attached.iter().map(|path| wire_deck(path)).find(|deck| is_collection(deck))
    })
}

#[derive(Clone, Copy)]
pub(super) enum ViewDestination {
    Library,
    Overview,
    Inbox,
    ResearchIssues,
    BranchReview,
    Trash,
}

impl ViewDestination {
    pub(super) const ALL: [Self; 6] = [
        Self::Library, Self::Overview, Self::Inbox, Self::ResearchIssues,
        Self::BranchReview, Self::Trash,
    ];

    pub(super) fn item(self) -> (&'static str, &'static str, Symbol, u32) {
        match self {
            Self::Library => ("tool-library", tr!("library-title"), Symbol::Library, fuzukue_proto::LIBRARY_PROTOCOL),
            Self::Overview => ("tool-overview", tr!("sidebar-overview"), Symbol::LayoutList, 0),
            Self::Inbox => ("tool-inbox", tr!("inbox-title"), Symbol::Inbox, 13),
            Self::ResearchIssues => ("tool-research-issues", tr!("sidebar-research-issues"), Symbol::CircleDot, 13),
            Self::BranchReview => ("tool-branch-review", tr!("branch-review-title"), Symbol::GitBranch, fuzukue_proto::BRANCH_REVIEW_PROTOCOL),
            Self::Trash => ("tool-trash", tr!("trash-title"), Symbol::Trash, fuzukue_proto::CLOSE_TRASH_PROTOCOL),
        }
    }

    fn pane(self, current: RightPane) -> RightPane {
        match self {
            Self::Library => RightPane::Library,
            Self::Overview => RightPane::Overview,
            Self::Inbox => RightPane::Inbox,
            Self::ResearchIssues if matches!(current, RightPane::ResearchIssue(_)) => current,
            Self::ResearchIssues => RightPane::ResearchIssues,
            Self::BranchReview => RightPane::BranchReview,
            Self::Trash => RightPane::Trash,
        }
    }

    pub(super) fn run(self, view: &mut Workbench, window: &mut Window, cx: &mut Context<Workbench>) {
        match self {
            Self::Library => view.toggle_library(window, cx),
            Self::BranchReview => view.toggle_branch_review(cx),
            Self::Trash => view.open_trash(cx),
            _ => {
                view.settings_open = false;
                let pane = self.pane(view.right);
                view.right = if view.right == pane { RightPane::None } else { pane };
                cx.notify();
            }
        }
    }
}

pub(super) fn section_header(
    id: &'static str,
    label: &'static str,
    open: bool,
    cx: &Context<Workbench>,
) -> Stateful<Div> {
    let theme = cx.theme();
    sidebar_row()
        .id(id)
        .rounded_md()
        .text_xs()
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|row| row.bg(theme.accent))
        .child(sidebar_icon(
            (if open {
                Symbol::ChevronDown
            } else {
                Symbol::ChevronRight
            })
            .tinted(theme.muted_foreground)
            .size(px(13.)),
        ))
        .child(label)
}

fn action(
    id: impl Into<ElementId>,
    icon: Symbol,
    label: impl Into<SharedString>,
    active: bool,
    cx: &Context<Workbench>,
) -> Stateful<Div> {
    let theme = cx.theme();
    sidebar_row()
        .id(id)
        .min_w_0()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(active, |row| row.bg(theme.accent))
        .hover(|row| row.bg(theme.accent))
        .child(sidebar_icon(icon.tinted(theme.muted_foreground)))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

struct WorkspaceDetails {
    title: String,
    rows: [(&'static str, String); 5],
}

fn workspace_details(
    locale: Locale,
    workspace: Option<&WorkspaceInfo>,
    title: &str,
    git: Option<&GitStatus>,
    address: &str,
    status: &str,
    build: Option<&str>,
    client_update: Option<bool>,
) -> WorkspaceDetails {
    let unknown = locale.text("settings-workspace-unknown");
    let path = workspace.map_or(unknown, |workspace| workspace.path.as_str());
    let branch = git.map_or_else(
        || unknown.to_owned(),
        |git| {
            let branch = git.branch.as_deref().unwrap_or("HEAD");
            let changes = locale.text(if git.files.is_empty() {
                "settings-workspace-clean"
            } else {
                "settings-workspace-dirty"
            });
            format!("{branch}{STATUS_SEPARATOR}{changes}")
        },
    );
    let build = build
        .filter(|id| !id.is_empty())
        .map_or_else(|| unknown.to_owned(), |id| id.chars().take(12).collect());
    let client_update = match client_update {
        Some(true) => locale.text("settings-workspace-update-available"),
        Some(false) => locale.text("settings-workspace-update-none"),
        None => unknown,
    };
    let server = if address == "/ws" { unknown } else { address };
    WorkspaceDetails {
        title: title.to_owned(),
        rows: [
            (locale.text("settings-workspace-path"), path.to_owned()),
            (locale.text("settings-workspace-git"), branch),
            (locale.text("settings-connection"), format!("{status}{STATUS_SEPARATOR}{server}")),
            (locale.text("settings-workspace-build"), build),
            (locale.text("settings-workspace-update"), client_update.to_owned()),
        ],
    }
}

impl Workbench {
    fn load_sidebar_preferences(&mut self) {
        let settings_path = self.appearance.settings_path();
        let state_path = settings_path.as_ref().map(|path| path.with_extension("state.toml"));
        if self.sidebar.preferences.path == state_path {
            return;
        }
        self.sidebar.preferences = SidebarPreferences::load(settings_path.as_deref(), state_path);
    }

    fn save_sidebar_preference(
        &mut self,
        write: impl FnOnce(&mut Table) -> anyhow::Result<()>,
    ) {
        if let Some(path) = &self.sidebar.preferences.path {
            if let Err(error) = crate::settings_file::update(path, |settings| {
                write(preference_table(settings, "sidebar")?)?;
                Ok(true)
            }) {
                self.notice = Some(tr!("settings-cannot-save-settings", e = error.to_string()).into());
            }
        }
    }

    pub(super) fn thread_manual_order(&self) -> &[String] {
        let workspace = self.workspaces.active.as_deref().unwrap_or_default();
        self.sidebar.preferences.manual_order.get(workspace).map_or(&[], Vec::as_slice)
    }

    pub(super) fn remember_thread_manual_order(&mut self, order: Vec<String>) {
        let workspace = self.workspaces.active.clone().unwrap_or_default();
        self.sidebar.preferences.manual_order.insert(workspace.clone(), order.clone());
        self.save_sidebar_preference(move |settings| {
            preference_table(settings, "manual_order")?.insert(workspace, Value::try_from(order)?);
            Ok(())
        });
    }

    pub(super) fn thread_group_open(&self, group: fuzukue_proto::ThreadGroup) -> bool {
        let workspace = self.workspaces.active.as_deref().unwrap_or_default();
        self.sidebar.preferences.group_open.get(workspace)
            .and_then(|groups| groups.get(group.key())).copied()
            .unwrap_or(group != fuzukue_proto::ThreadGroup::Archived)
    }

    pub(super) fn set_thread_group_open(&mut self, group: fuzukue_proto::ThreadGroup, open: bool, cx: &mut Context<Self>) {
        if self.thread_group_open(group) == open {
            return;
        }
        let workspace = self.workspaces.active.clone().unwrap_or_default();
        self.sidebar.preferences.group_open.entry(workspace.clone()).or_default().insert(group.key().into(), open);
        self.save_sidebar_preference(move |settings| {
            let groups = preference_table(settings, "group_open")?;
            preference_table(groups, &workspace)?.insert(group.key().into(), Value::Boolean(open));
            Ok(())
        });
        cx.notify();
    }

    pub(super) fn close_sidebar_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = std::mem::take(&mut self.sidebar.workspace_popover_open);
        if was_open {
            cx.notify();
        }
        was_open
    }

    pub(super) fn show_sidebar_settings(&mut self, server: bool, cx: &mut Context<Self>) {
        self.sidebar.workspace_popover_open = false;
        self.sidebar.server_settings = server;
        self.agent_management.tab = Default::default();
        self.settings_open = true;
        if !server {
            self.refresh_github_report_status(cx);
        }
        if server && self.protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL {
            let _ = self.out.send(fuzukue_proto::ClientMsg::SelfUpdateStatus);
        }
        cx.notify();
    }

    pub(super) fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_open {
            self.settings_open = false;
            self.trash_error = None;
            self.close_sidebar_menu(cx);
            cx.notify();
        } else {
            self.show_sidebar_settings(false, cx);
        }
    }

    pub(super) fn show_thread_info_from_sidebar(&mut self, cx: &mut Context<Self>) {
        let was_visible = !self.settings_open && self.details_view_visible();
        self.settings_open = false;
        self.close_sidebar_menu(cx);
        if was_visible {
            self.close_thread_info();
        } else if !self.thread_info_visible() {
            if let Err(error) = self.appearance.toggle_thread_info() {
                self.notice = Some(tr!("sidebar-cannot-save-thread-details-visibility", error = error.to_string()).into());
            }
            self.request_thread_artifacts();
        }
        cx.notify();
    }

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_sidebar_preferences();
        let thread_ids: HashSet<_> = self.threads.iter().map(|thread| thread.id.as_str()).collect();
        self.sidebar.thread_focus.retain(|id, _| thread_ids.contains(id.as_str()));
        for thread in &self.threads {
            self.sidebar.thread_focus.entry(thread.id.clone())
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
        let theme = cx.theme().clone();
        let paper = self.paper_edit_target();
        let attached = self.threads.iter().find(|thread| thread.id == self.active)
            .map_or(&[][..], |thread| thread.attached.as_slice());
        let collection = card_collection_target(
            attached,
            matches!(self.right, RightPane::Overview | RightPane::DeckCards | RightPane::Card).then_some(self.deck.as_str()),
            &self.tabs,
            |deck| self.paper_deck(deck),
        );

        let navigation = v_flex()
            .flex_none()
            .gap_0p5()
            .when(self.protocol >= 4, |rows| {
                rows.child(
                    action("new-thread", Symbol::Plus, tr!("sidebar-new"), false, cx).on_click(
                        cx.listener(|this, _, _, _| {
                            let _ = this.out.send(ClientMsg::NewThread);
                        }),
                    ),
                )
            })
            .child(
                action(
                    "tool-search",
                    Symbol::Search,
                    tr!("sidebar-search"),
                    self.palette.is_some(),
                    cx,
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(crate::menu_bar::SEARCH_SHORTCUT))
                .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
            .children(paper.map(|deck| {
                let deck = deck.to_owned();
                action("sidebar-edit-paper", Symbol::FilePenLine, tr!("sidebar-edit-paper"),
                    !self.settings_open && self.right == RightPane::Read && self.deck == deck, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_paper_deck(deck.clone(), cx)))
            }))
            .children(collection.map(|deck| {
                let deck = deck.to_owned();
                action("sidebar-edit-collection", Symbol::FilePenLine, tr!("sidebar-edit-collection"),
                    !self.settings_open && matches!(self.right, RightPane::Overview | RightPane::DeckCards) && self.deck == deck, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_attachment(&deck, cx)))
            }));

        v_flex()
            .id("workspace-sidebar")
            .size_full()
            .bg(theme.sidebar)
            .p_3()
            .gap_2()
            .child(navigation)
            .child(
                div()
                    .id("workspace-navigation")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .when(self.protocol >= 4, |list| {
                        list.child(self.render_threads(cx))
                    }),
            )
            .child(self.render_sidebar_more(cx))
            .when(self.protocol >= 11, |sidebar| {
                sidebar.child(self.render_workspace_tabs(cx))
            })
            .child(self.render_sidebar_footer(cx))
    }

    fn render_sidebar_more(&self, cx: &mut Context<Self>) -> Div {
        let open = self.sidebar.preferences.more_open.unwrap_or(false);
        v_flex().flex_none().min_h_0().max_h(relative(0.42))
            .child(section_header("sidebar-more-toggle", tr!("sidebar-more"), open, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    let more_open = !this.sidebar.preferences.more_open.unwrap_or(false);
                    this.sidebar.preferences.more_open = Some(more_open);
                    this.save_sidebar_preference(|settings| {
                        settings.insert("more_open".into(), Value::Boolean(more_open));
                        Ok(())
                    });
                    cx.notify();
                })))
            .when(open, |group| group.child(
                v_flex().id("sidebar-more-items").min_h_0().overflow_y_scrollbar()
                    .children(ViewDestination::ALL.map(|destination| {
                        let (id, label, icon, protocol) = destination.item();
                        let selected = !self.settings_open && self.right == destination.pane(self.right);
                        Button::new(SharedString::from(format!("sidebar-{id}")))
                            .ghost().small().w_full().h_auto().p_0().disabled(self.protocol < protocol)
                            .selected(selected)
                            .accessibility_label(label)
                            .child(sidebar_row().w_full()
                                .child(sidebar_icon(icon.icon().size(SIDEBAR_ICON_SIZE)))
                                .child(div().min_w_0().truncate().child(label)))
                            .on_click(cx.listener(move |this, _, window, cx| destination.run(this, window, cx)))
                    })),
            ))
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let workspace = self.workspaces.current();
        let title = workspace
            .map(|workspace| workspace.title.as_str())
            .or(self.workspace_title.as_deref())
            .unwrap_or(tr!("sidebar-server-connection"));
        let status = match self.status {
            super::ConnectionStatus::Connected => tr!("sidebar-connected"),
            super::ConnectionStatus::ServerSwap => tr!("self-update-notice"),
            super::ConnectionStatus::Reconnecting(_) => tr!("sidebar-reconnecting"),
            super::ConnectionStatus::Switching => tr!("settings-switching-connection"),
            _ => tr!("sidebar-waiting-for-a-connection"),
        };
        let branch = self.thread_info.git.branch_indicator();
        let summary = branch.map_or_else(
            || status.to_owned(),
            |branch| format!("{status} · {branch}"),
        );
        let border_width = px(1.);
        let tile_size = px(24.);
        let tile_offset = (SIDEBAR_ICON_SIZE - tile_size) * 0.5;
        let popover = Popover::new("workspace-connection-popover")
            .anchor(Anchor::BottomLeft)
            .offset(px(4.))
            .flex_none()
            .w_full()
            .open(self.sidebar.workspace_popover_open)
            .trigger(
                Button::new("workspace-account")
                    .ghost()
                    .w_full()
                    .h_auto()
                    .p_0()
                    .rounded_lg()
                    .border(border_width)
                    .border_color(theme.border)
                    .accessibility_label(format!("{title} · {summary}"))
                    .tooltip(tr!("settings-workspace-details"))
                    .child(
                        sidebar_row()
                            .w_full()
                            .min_w_0()
                            .min_h(px(44.))
                            .py_0p5()
                            .px(SIDEBAR_ROW_INSET - border_width)
                            .child(
                                sidebar_icon(div().absolute().top(tile_offset).left(tile_offset)
                                    .size(tile_size).rounded_md().bg(theme.background))
                                    .relative()
                                    .child(Symbol::PanelsTopLeft.tinted(theme.foreground)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .line_height(UI_LINE_HEIGHT)
                                    .child(div().text_sm().truncate().child(title.to_owned()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(theme.muted_foreground)
                                            .child(summary),
                                    ),
                            )
                            .child(
                                Symbol::ChevronDown
                                    .tinted(theme.muted_foreground)
                                    .size(px(13.))
                                    .flex_none(),
                            ),
                    ),
            )
            .on_open_change(cx.listener(|this, &open, _, cx| {
                this.sidebar.workspace_popover_open = open;
                cx.notify();
            }));

        popover.when(self.sidebar.workspace_popover_open, |popover| {
            let connected = matches!(self.status, super::ConnectionStatus::Connected);
            let details = workspace_details(
                crate::i18n::current(),
                workspace,
                title,
                self.thread_info.git.status.as_ref(),
                &self.http.url(),
                status,
                self.self_update
                    .status
                    .as_ref()
                    .map(|status| status.build_id.as_str()),
                (connected
                    && self.protocol >= fuzukue_proto::DISTRIBUTION_PROTOCOL
                    && crate::updater::platform().is_some())
                    .then_some(self.distribution.manifest.is_some()),
            );
            let content = self.render_workspace_details(details, cx);
            popover.content(move |_, window, _| {
                let size = window.viewport_size();
                content
                    .w(px(300.).min(size.width - px(16.)))
                    .max_h(size.height * 0.75)
                    .overflow_y_scrollbar()
                    .id("workspace-account-menu")
            })
        })
    }

    fn render_workspace_details(
        &self,
        details: WorkspaceDetails,
        cx: &Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        v_flex()
            .p_2()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_lg()
            .child(div().px_2().text_sm().font_semibold().child(details.title))
            .child(
                v_flex().gap_1().px_2().children(details.rows.map(|(label, value)| {
                    v_flex()
                        .text_xs()
                        .child(div().text_color(theme.muted_foreground).child(label))
                        .child(crate::chat::chat_activity::status_row(&value, theme.muted_foreground))
                })),
            )
            .child(
                v_flex()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .pt_1()
                    .when(self.protocol >= 11, |actions| {
                        actions.child(
                            workspace_action("open-workspace", Symbol::Folder, tr!("settings-workspace-switch"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_sidebar_menu(cx);
                                    this.open_workspace_browser("~", window, cx);
                                })),
                        )
                    })
                    .child(
                        workspace_action("account-server", Symbol::Server, tr!("settings-workspace-open-server"))
                            .on_click(cx.listener(|this, _, _, cx| this.show_sidebar_settings(true, cx))),
                    ),
            )
    }

    pub(super) fn render_inbox(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows: Vec<_> = self
            .threads
            .iter()
            .filter_map(|thread| self.chats.get(&thread.id).map(|chat| (thread, chat)))
            .flat_map(|(thread, chat)| chat.inbox_files().map(move |path| (thread, path)))
            .enumerate()
            .map(|(index, (thread, path))| {
                let session = thread.id.clone();
                action(
                    ("inbox-file", index),
                    Symbol::FileText,
                    path.rsplit('/').next().unwrap_or(path).to_owned(),
                    false,
                    cx,
                )
                .child(
                    div()
                        .max_w(px(140.))
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(crate::thread_title(thread).to_owned()),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_thread(session.clone(), cx)),
                )
            })
            .collect();
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                self.pane_heading(div().font_bold().child(tr!("inbox-title")), cx)
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .border_b_1()
                    .border_color(theme.sidebar_border),
            )
            .child(
                div()
                    .id("inbox-files")
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .overflow_y_scrollbar()
                    .children(self.render_workspace_request_inbox(cx))
                    .children(rows.is_empty().then(|| {
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("sidebar-no-attachments"))
                    }))
                    .children(rows),
            )
    }
}

fn workspace_action(id: &'static str, icon: Symbol, label: &'static str) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .w_full()
        .accessibility_label(label)
        .child(h_flex().w_full().gap_2().child(icon.icon().small()).child(label))
}

#[cfg(test)]
mod workspace_popover_tests {
    use super::*;
    use fuzukue_proto::GitFile;
    use gpui_kit::{AlignItems, JustifyContent};

    #[test]
    fn sidebar_rows_align_icons_and_labels_to_shared_columns() {
        let mut row = sidebar_row();
        let row = row.style();
        let mut icon = sidebar_icon(div());
        let icon = icon.style();
        assert_eq!(row.padding.left, Some(SIDEBAR_ROW_INSET.into()));
        assert_eq!(row.gap.width, Some(SIDEBAR_ICON_GAP.into()));
        assert_eq!(row.min_size.height, Some(SIDEBAR_ROW_MIN_HEIGHT.into()));
        assert_eq!(row.align_items, Some(AlignItems::Center));
        assert_eq!(icon.size.width, Some(SIDEBAR_ICON_SIZE.into()));
        assert_eq!(icon.size.height, Some(SIDEBAR_ICON_SIZE.into()));
        assert_eq!(icon.justify_content, Some(JustifyContent::Center));
        assert_eq!(icon.flex_shrink, Some(0.));
    }

    #[test]
    fn sidebar_preferences_keep_disclosure_order_and_group_state_by_workspace() {
        let mut settings: Table = toml::from_str(
            "future = true\n[sidebar]\nfuture = 4\n[sidebar.manual_order]\nother = ['x']\n[sidebar.group_open.other]\narchived = true\n",
        ).unwrap();
        let read = |settings: &Table| -> SidebarPreferences {
            settings["sidebar"].clone().try_into().unwrap()
        };
        assert_eq!(read(&settings).more_open, None);
        let sidebar = preference_table(&mut settings, "sidebar").unwrap();
        sidebar.insert("more_open".into(), Value::Boolean(true));
        preference_table(sidebar, "manual_order").unwrap()
            .insert("lab".into(), Value::try_from(vec!["b", "a"]).unwrap());
        let groups = preference_table(sidebar, "group_open").unwrap();
        preference_table(groups, "lab").unwrap().insert("current".into(), Value::Boolean(false));
        let preferences = read(&settings);
        assert_eq!(preferences.more_open, Some(true));
        assert_eq!(preferences.manual_order["lab"], ["b", "a"]);
        assert_eq!(preferences.manual_order["other"], ["x"]);
        assert_eq!(preferences.group_open["lab"].get("current"), Some(&false));
        assert_eq!(preferences.group_open["other"].get("archived"), Some(&true));
        assert_eq!(settings["future"].as_bool(), Some(true));
        assert_eq!(settings["sidebar"]["future"].as_integer(), Some(4));
    }

    #[test]
    fn sidebar_reads_legacy_order_and_prefers_the_state_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = dir.path().join("settings.toml");
        let state = settings.with_extension("state.toml");
        std::fs::write(&settings, "[sidebar]\nmore_open = true\n[sidebar.manual_order]\na = ['x']\nb = ['y']\n")?;
        std::fs::write(&state, "[sidebar.manual_order]\na = ['z']\n[sidebar.group_open.a]\ncurrent = false\n")?;
        let preferences = SidebarPreferences::load(Some(&settings), Some(state));
        assert_eq!(preferences.more_open, Some(true));
        assert_eq!(preferences.manual_order["a"], ["z"]);
        assert_eq!(preferences.manual_order["b"], ["y"]);
        assert_eq!(preferences.group_open["a"]["current"], false);
        Ok(())
    }

    #[test]
    fn card_collection_action_uses_attached_or_open_collections() {
        let tabs: Vec<TabView> = serde_json::from_value(serde_json::json!([
            {"id":"cards","title":"Cards","kind":"research","deck":"cards","summary":"","status":"","next":"",
             "papers":[],"notes":[],"lean":[],"related":[]},
            {"id":"paper","title":"Paper","kind":"paper","deck":"paper","summary":"","status":"","next":"",
             "papers":[],"notes":[],"lean":[],"related":[]}
        ])).unwrap();
        let is_paper = |deck: &str| deck == "paper";
        assert_eq!(card_collection_target(&[], None, &tabs, is_paper), None);
        assert_eq!(card_collection_target(&["paper".into()], Some("paper"), &tabs, is_paper), None);
        assert_eq!(card_collection_target(&["cards".into()], None, &tabs, is_paper), Some("cards"));
        assert_eq!(card_collection_target(&[], Some("cards"), &tabs, is_paper), Some("cards"));
    }

    #[test]
    fn workspace_details_show_current_workspace_and_dirty_git() {
        let workspace = WorkspaceInfo {
            id: "project".into(),
            path: "/srv/lab".into(),
            title: "研究室".into(),
            last_opened: None,
            running: false,
        };
        let git = GitStatus {
            branch: Some("main".into()),
            upstream: None,
            ahead: 0,
            behind: 0,
            files: vec![GitFile { status: " M".into(), path: "paper.tex".into(), old_path: None }],
            read_only: false,
        };
        let details = workspace_details(
            Locale::JAPANESE,
            Some(&workspace),
            &workspace.title,
            Some(&git),
            "ws://server:7878/ws",
            "接続済み",
            Some("0123456789abcdef"),
            Some(true),
        );
        assert_eq!(details.title, "研究室");
        assert_eq!(
            details.rows,
            [
                ("パス", "/srv/lab".into()),
                ("Git ブランチ", format!("main{STATUS_SEPARATOR}変更あり")),
                ("接続先", format!("接続済み{STATUS_SEPARATOR}ws://server:7878/ws")),
                ("サーバービルド ID", "0123456789ab".into()),
                ("クライアントの更新", "更新あり".into()),
            ]
        );
    }

    #[test]
    fn workspace_details_distinguish_clean_detached_head_and_unknown_data() {
        let english = Locale::supported("en").unwrap();
        let git = GitStatus {
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            read_only: false,
        };
        let details = workspace_details(
            english, None, "Server connection", Some(&git), "/ws", "Reconnecting…", Some(""), None,
        );
        assert_eq!(details.title, "Server connection");
        assert_eq!(
            details.rows,
            [
                ("Path", "Unknown".into()),
                ("Git branch", format!("HEAD{STATUS_SEPARATOR}No local changes")),
                ("Connection", format!("Reconnecting…{STATUS_SEPARATOR}Unknown")),
                ("Server build ID", "Unknown".into()),
                ("Client update", "Unknown".into()),
            ]
        );
        let details = workspace_details(
            english, None, "Server connection", None, "ws://localhost:7878/ws", "Connected",
            Some("dev"), Some(false),
        );
        assert_eq!(details.rows[1].1, "Unknown");
        assert_eq!(details.rows[3].1, "dev");
        assert_eq!(details.rows[4].1, "No update notice");
    }
}
