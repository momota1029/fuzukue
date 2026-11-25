use std::collections::HashMap;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        v_flex, ActiveTheme as _, Sizable as _,
    },
    div,
    prelude::*,
    px, relative, AnyElement, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent,
    ScrollHandle, SharedString, Window,
};
use fuzukue_proto::{ClientMsg, WorkspaceDirectory, WorkspaceEntry, WorkspaceInfo, WorkspaceKind};

use crate::sidebar::{section_header, sidebar_icon, sidebar_row};
use crate::{chat::TranscriptPosition, composing, icons::Symbol, Workbench};

#[derive(Default)]
pub(super) struct WorkspaceState {
    pub active: Option<String>,
    pub entries: Vec<WorkspaceInfo>,
    pub pending: bool,
    pending_draft_inbox: Option<String>,
    last_thread: HashMap<String, String>,
    replay_positions: HashMap<String, TranscriptPosition>,
    browser: Option<Browser>,
    next_request: u64,
    error: Option<SharedString>,
}

struct Browser {
    input: Entity<InputState>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
    directory: Option<WorkspaceDirectory>,
    loading: Option<u64>,
    hidden: bool,
    selected: Option<usize>,
    create: Option<CreateForm>,
}

struct CreateForm {
    name: Entity<InputState>,
    kind: WorkspaceKind,
    pending: Option<u64>,
}

const KINDS: [(WorkspaceKind, &str); 4] = [
    (WorkspaceKind::Research, "workspace-research"),
    (WorkspaceKind::Software, "workspace-software"),
    (WorkspaceKind::Paper, "workspace-paper"),
    (WorkspaceKind::Empty, "workspace-empty"),
];

fn child_path(parent: &str, name: &str) -> String {
    format!("{parent}/{name}/")
}

fn newly_selected(previous: Option<usize>, next: usize) -> Option<usize> {
    (previous != Some(next)).then_some(next)
}

impl WorkspaceState {
    pub(super) fn current(&self) -> Option<&WorkspaceInfo> {
        self.entries
            .iter()
            .find(|entry| Some(&entry.id) == self.active.as_ref())
    }
}

impl Workbench {
    pub(super) fn workspace_selected(
        &mut self,
        id: String,
        replay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.workspaces.active.replace(id.clone());
        let first = previous.is_none();
        self.http.select_workspace(Some(id.clone()));
        self.out.select_workspace(Some(id.clone()));
        if previous.as_deref().is_some_and(|previous| previous != id) {
            self.question_drafts.clear();
            self.workspaces.replay_positions.clear();
        } else if replay && self.threads_loaded {
            // A full replay rebuilds chat rows. Keep the last measured reader positions
            // even if another disconnect interrupts the replacement.
            self.workspaces.replay_positions = self.recent_transcripts.iter()
                .filter_map(|id| Some((id.clone(), self.chats.get(id)?.transcript_position()?)))
                .collect();
        }
        if let Some(previous) = previous.filter(|previous| replay || previous != &id) {
            self.workspaces
                .last_thread
                .insert(previous, self.active.clone());
            self.clear_workspace(window, cx);
            self.connection_epoch += 1;
            if let Some(thread) = self.workspaces.last_thread.get(&id).cloned() {
                self.switch_session(thread, cx);
            }
        }
        self.workspaces.pending = false;
        self.close_workspace_browser(window, cx);
        self.workspaces.error = None;
        self.refresh_workspace_git();
        if let Some((_, target)) = self
            .workshop
            .pending_navigation
            .take()
            .filter(|(workspace, _)| workspace == &id)
        {
            self.open_workshop_target(target, cx);
        }
        if self.workspaces.pending_draft_inbox.take().as_ref() == Some(&id) {
            self.show_workspace_request_inbox(cx);
        }
        if first && self.auto_prompt.is_some() {
            let _ = self.out.send(ClientMsg::NewThread);
        }
    }

    pub(super) fn restore_replay_positions(&mut self) {
        for (id, position) in self.workspaces.replay_positions.drain() {
            if let Some(chat) = self.chats.get(&id) {
                chat.restore_transcript_position(position);
            }
        }
    }

    pub(super) fn workspace_error(&mut self, message: String) {
        self.workshop.pending_navigation = None;
        self.workspaces.pending_draft_inbox = None;
        self.workspaces.pending = false;
        self.workspaces.error = Some(message.into());
    }

    pub(super) fn switch_workspace(&mut self, id: String, cx: &mut Context<Self>) {
        if self.workspaces.active.as_ref() != Some(&id) && !self.workspaces.pending {
            self.request_workspace(ClientMsg::SwitchWorkspace { id }, cx);
        }
    }

    pub(super) fn open_other_workspace_request_inbox(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.workspaces.pending {
            self.switch_workspace(id.clone(), cx);
            if self.workspaces.pending {
                self.workspaces.pending_draft_inbox = Some(id);
            }
        }
    }

    fn request_workspace(&mut self, message: ClientMsg, cx: &mut Context<Self>) {
        if self.source.has_unsaved() {
            self.workspace_error(tr!("workspace-finish-edits-before-switching").into());
            cx.notify();
            return;
        }
        self.workspaces.pending = match message {
            ClientMsg::SwitchWorkspace { id } if matches!(self.status, crate::ConnectionStatus::Reconnecting(_)) => {
                self.connection.reconnect_to_workspace(id)
            }
            message => self.out.send(message).is_ok(),
        };
        self.workspaces.error =
            (!self.workspaces.pending).then(|| tr!("workspace-cannot-send-request-to-server").into());
        cx.notify();
    }

    fn request_open_workspace(&mut self, path: String, cx: &mut Context<Self>) {
        if !path.trim().is_empty() && !self.workspaces.pending {
            self.request_workspace(ClientMsg::OpenWorkspace { path }, cx);
        }
    }

    pub(super) fn render_workspace_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let has_others = self
            .workspaces
            .entries
            .iter()
            .any(|entry| Some(&entry.id) != self.workspaces.active.as_ref());
        let open = !self.sidebar.workspaces_collapsed;
        v_flex()
            .flex_none()
            .when(has_others, |section| {
                section
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .pt_1()
                    .child(
                        section_header(
                            "other-workspaces-toggle",
                            tr!("workspace-workspace"),
                            open,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar.workspaces_collapsed = !this.sidebar.workspaces_collapsed;
                            cx.notify();
                        })),
                    )
                    .when(open, |section| {
                        section.child(
                            div()
                                .id("other-workspaces")
                                .max_h(px(164.))
                                .overflow_y_scrollbar()
                                .children(
                                    self.workspaces
                                        .entries
                                        .iter()
                                        .filter(|entry| {
                                            Some(&entry.id) != self.workspaces.active.as_ref()
                                        })
                                        .map(|entry| {
                                            let id = entry.id.clone();
                                            let remove_id = id.clone();
                                            let label = entry.title.clone();
                                            sidebar_row()
                                                .id(SharedString::from(format!("workspace-{id}")))
                                                .rounded_md()
                                                .text_sm()
                                                .cursor_pointer()
                                                .hover(|row| row.bg(theme.background))
                                                .child(sidebar_icon(
                                                    Symbol::PanelsTopLeft
                                                        .tinted(theme.muted_foreground),
                                                ))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .child(entry.title.clone()),
                                                )
                                                .children(workspace_path_hint(entry).map(|tail| {
                                                    div()
                                                        .max_w(relative(0.35))
                                                        .truncate()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(tail.to_owned())
                                                }))
                                                .when(entry.last_opened.is_none(), |row| {
                                                    row.child(
                                                        div()
                                                            .flex_none()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .child(tr!("workspace-new")),
                                                    )
                                                })
                                                .when(entry.running, |row| {
                                                    row.child(
                                                        div().flex_none().text_xs().child(tr!("workspace-running")),
                                                    )
                                                })
                                                .children((self.protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL).then(|| {
                                                    Button::new(format!("workspace-remove-{remove_id}")).ghost().xsmall().icon(Symbol::X.icon())
                                                        .tooltip(tr!("trash-workspace-remove")).accessibility_label(tr!("trash-workspace-remove"))
                                                        .on_click(cx.listener(move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.confirm_removal(ClientMsg::RemoveWorkspace { id: remove_id.clone() }, label.clone(), window, cx);
                                                        }))
                                                }))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.switch_workspace(id.clone(), cx)
                                                }))
                                        }),
                                ),
                        )
                    })
            })
            .children(
                self.workspaces
                    .error
                    .clone()
                    .filter(|_| self.workspaces.browser.is_none())
                    .map(|error| div().text_xs().text_color(theme.danger).child(error)),
            )
    }
}

fn workspace_path_hint(entry: &WorkspaceInfo) -> Option<&str> {
    entry.path.rsplit('/').next().filter(|tail| *tail != entry.title)
}

impl Workbench {
    pub(super) fn open_workspace_browser(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.protocol < 11 || self.workspaces.pending || self.workspaces.browser.is_some() {
            return;
        }
        self.close_palette();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(path.to_owned()));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } if !composing(&input, window, cx) => {
                    let path = input.read(cx).value().to_string();
                    if this.protocol >= 15 {
                        this.browse_workspace(path, window, cx);
                    } else {
                        this.request_open_workspace(path, cx);
                    }
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )
        .detach();
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        self.workspaces.browser = Some(Browser {
            input,
            focus,
            previous_focus,
            scroll: ScrollHandle::new(),
            directory: None,
            loading: None,
            hidden: false,
            selected: None,
            create: None,
        });
        self.workspaces.error = None;
        if self.protocol >= 15 {
            self.browse_workspace(path.to_owned(), window, cx);
        } else if let Some(browser) = &self.workspaces.browser {
            browser.input.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_workspace_browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(browser) = self.workspaces.browser.take() else {
            return false;
        };
        if let Some(previous) = browser.previous_focus {
            previous.focus(window, cx);
        }
        self.workspaces.error = None;
        cx.notify();
        true
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn browsed_workspace_path(&self) -> Option<&str> {
        self.workspaces
            .browser
            .as_ref()?
            .directory
            .as_ref()
            .map(|d| d.path.as_str())
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn shot_workspace_path_input(&self) -> Option<&Entity<InputState>> {
        self.workspaces.browser.as_ref().map(|browser| &browser.input)
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn shot_workspace_browser_state(&self) -> String {
        let pending = self.workspaces.browser.as_ref().and_then(|browser| browser.loading);
        format!(
            "protocol={}, folder={:?}, pending browse={pending:?}, workspace error={:?}",
            self.protocol,
            self.browsed_workspace_path(),
            self.workspaces.error,
        )
    }

    #[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
    pub(super) fn workspace_browser_scroll(&self) -> gpui_kit::Pixels {
        self.workspaces.browser.as_ref().unwrap().scroll.offset().y
    }

    pub(super) fn workspace_browser_open(&self) -> bool {
        self.workspaces.browser.is_some()
    }

    pub(super) fn workspace_browser_disconnected(&mut self) {
        if let Some(browser) = &mut self.workspaces.browser {
            browser.loading = None;
            if let Some(form) = &mut browser.create {
                form.pending = None;
            }
            self.workspaces.error = Some(tr!("workspace-disconnected-from-server").into());
        }
        self.workspaces.pending = false;
    }

    fn browse_workspace(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(browser) = self.workspaces.browser.as_mut() else {
            return;
        };
        if self.workspaces.pending || browser.create.is_some() {
            return;
        }
        self.workspaces.error = None;
        self.workspaces.next_request += 1;
        let request = self.workspaces.next_request;
        match self.out.send(ClientMsg::BrowseWorkspace {
            request,
            path,
            hidden: browser.hidden,
        }) {
            Ok(()) => {
                browser.loading = Some(request);
                browser.focus.focus(window, cx);
            }
            Err(_) => self.workspaces.error = Some(tr!("workspace-cannot-request-folder-list").into()),
        }
        cx.notify();
    }

    pub(super) fn workspace_directory(
        &mut self,
        request: u64,
        result: Result<WorkspaceDirectory, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = self
            .workspaces
            .browser
            .as_mut()
            .filter(|b| b.loading == Some(request))
        else {
            return;
        };
        browser.loading = None;
        match result {
            Ok(directory) => {
                browser.input.update(cx, |input, cx| {
                    input.set_value(directory.path.clone(), window, cx)
                });
                browser.directory = Some(directory);
                browser.selected = None;
                browser.scroll = ScrollHandle::new();
            }
            Err(error) => {
                self.workspaces.error = Some(error.into());
                browser.input.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
    }

    fn open_create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("workspace-folder-name")));
        cx.subscribe_in(
            &name,
            window,
            |this, name, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) && !composing(&name, window, cx) {
                    this.create_workspace(cx);
                }
            },
        )
        .detach();
        name.read(cx).focus_handle(cx).focus(window, cx);
        if let Some(browser) = &mut self.workspaces.browser {
            browser.create = Some(CreateForm {
                name,
                kind: WorkspaceKind::Empty,
                pending: None,
            });
            self.workspaces.error = None;
            cx.notify();
        }
    }

    fn create_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = &mut self.workspaces.browser else {
            return;
        };
        let Some(directory) = &browser.directory else {
            return;
        };
        let Some(form) = browser
            .create
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.workspaces.error = Some(tr!("workspace-enter-a-folder-name").into());
            cx.notify();
            return;
        }
        self.workspaces.next_request += 1;
        let request = self.workspaces.next_request;
        let parent = format!("{}/", directory.path);
        match self.out.send(ClientMsg::CreateWorkspace {
            request,
            parent,
            name,
            kind: form.kind,
        }) {
            Ok(()) => {
                form.pending = Some(request);
                self.workspaces.error = None;
            }
            Err(_) => self.workspaces.error = Some(tr!("workspace-cannot-request-workspace-creation").into()),
        }
        cx.notify();
    }

    pub(super) fn workspace_created(
        &mut self,
        request: u64,
        result: Result<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = &mut self.workspaces.browser else {
            return;
        };
        let Some(form) = browser
            .create
            .as_mut()
            .filter(|form| form.pending == Some(request))
        else {
            return;
        };
        form.pending = None;
        match result {
            Ok(path) => {
                browser.create = None;
                self.browse_workspace(path, window, cx);
            }
            Err(error) => self.workspaces.error = Some(error.into()),
        }
    }

    fn workspace_browser_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = self.workspaces.browser.as_mut() else {
            return;
        };
        let Some(directory) = browser.directory.as_ref() else {
            return;
        };
        if browser.loading.is_some() || self.workspaces.pending || browser.create.is_some() {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" | "down" if !directory.entries.is_empty() => {
                let last = directory.entries.len() - 1;
                let index = if event.keystroke.key == "up" {
                    browser
                        .selected
                        .map_or(last, |index| index.saturating_sub(1))
                } else {
                    browser.selected.map_or(0, |index| (index + 1).min(last))
                };
                if let Some(index) = newly_selected(browser.selected, index) {
                    browser.selected = Some(index);
                    browser.scroll.scroll_to_item(index);
                    cx.notify();
                }
            }
            "enter" | "right" => {
                if let Some(entry) = browser
                    .selected
                    .and_then(|i| directory.entries.get(i))
                    .filter(|e| e.directory)
                {
                    let target = child_path(&directory.path, &entry.name);
                    self.browse_workspace(target, window, cx);
                }
            }
            "backspace" | "left" => {
                if let Some((parent, _)) = directory.path.rsplit_once('/') {
                    let target = format!("{parent}/");
                    self.browse_workspace(target, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(super) fn render_workspace_browser(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let browser = self.workspaces.browser.as_ref()?;
        let theme = cx.theme().clone();
        let directory = browser.directory.as_ref();
        let path = directory.map(|dir| dir.path.as_str()).unwrap_or("~");
        let form = browser.create.as_ref();
        let busy = browser.loading.is_some()
            || self.workspaces.pending
            || form.is_some_and(|form| form.pending.is_some());
        let legacy = self.protocol < 15;
        let can_open = if legacy {
            !browser.input.read(cx).value().trim().is_empty()
        } else {
            directory.is_some() && path != "~" && browser.input.read(cx).value().as_str() == path
        };
        let mut prefix = String::new();
        let breadcrumb = h_flex()
            .id("workspace-breadcrumb")
            .gap_1()
            .w_full()
            .overflow_x_scroll()
            .children(path.split('/').enumerate().map(|(index, name)| {
                if index != 0 {
                    prefix.push('/');
                }
                prefix.push_str(name);
                let target = format!("{prefix}/");
                h_flex()
                    .gap_1()
                    .flex_none()
                    .children(
                        (index != 0).then(|| Symbol::ChevronRight.tinted(theme.muted_foreground)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("workspace-crumb-{index}")))
                            .ghost()
                            .small()
                            .label(if index == 0 { tr!("workspace-home") } else { name }.to_owned())
                            .disabled(busy || form.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browse_workspace(target.clone(), window, cx)
                            })),
                    )
            }));

        let mut panel = v_flex()
            .id("workspace-browser")
            .track_focus(&browser.focus)
            .occlude()
            .w_full()
            .max_w(px(780.))
            .h_full()
            .max_h(px(620.))
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_lg()
            .p_4()
            .gap_3()
            .on_key_down(cx.listener(|this, event, window, cx| {
                if this
                    .workspaces
                    .browser
                    .as_ref()
                    .is_some_and(|b| b.focus.is_focused(window))
                {
                    this.workspace_browser_key(event, window, cx);
                }
            }))
            .child(
                h_flex()
                    .justify_between()
                    .child(div().font_bold().child(tr!("workspace-open-workspace")))
                    .child(
                        Button::new("cancel-open-workspace")
                            .ghost()
                            .small()
                            .icon(Symbol::X.icon())
                            .accessibility_label(tr!("workspace-close"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_workspace_browser(window, cx);
                            })),
                    ),
            )
            .when(!legacy, |panel| panel.child(breadcrumb))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("workspace-path-input")
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&browser.input).disabled(form.is_some())),
                    )
                    .when(!legacy, |row| {
                        row.child(
                            Button::new("workspace-go-to-path")
                                .outline()
                                .small()
                                .label(tr!("workspace-go"))
                                .disabled(busy || form.is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(browser) = &this.workspaces.browser {
                                        let path = browser.input.read(cx).value().to_string();
                                        this.browse_workspace(path, window, cx);
                                    }
                                })),
                        )
                    }),
            )
            .when(!legacy, |panel| {
                panel.child(
                    h_flex()
                        .justify_between()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if browser.loading.is_some() {
                            tr!("workspace-loading")
                        } else {
                            tr!("workspace-server-folders")
                        })
                        .child(
                            Button::new("workspace-hidden")
                                .ghost()
                                .small()
                                .label(if browser.hidden {
                                    tr!("workspace-hide-hidden-items")
                                } else {
                                    tr!("workspace-show-hidden-items")
                                })
                                .disabled(busy || form.is_some() || directory.is_none())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(browser) = &mut this.workspaces.browser {
                                        if let Some(directory) = &browser.directory {
                                            browser.hidden = !browser.hidden;
                                            let path = format!("{}/", directory.path);
                                            this.browse_workspace(path, window, cx);
                                        }
                                    }
                                })),
                        ),
                )
            })
            .when(!legacy, |panel| {
                panel.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .bg(theme.secondary)
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .id("workspace-directory-list")
                                .size_full()
                                .flex()
                                .flex_col()
                                .pr_3()
                                .overflow_y_scroll()
                                .track_scroll(&browser.scroll)
                                .when(directory.is_some_and(|d| d.entries.is_empty()), |list| {
                                    list.child(
                                        div()
                                            .p_3()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(tr!("workspace-this-folder-is-empty")),
                                    )
                                })
                                .children(directory.into_iter().flat_map(|dir| {
                                    dir.entries.iter().enumerate().map(|(index, entry)| {
                                        self.render_workspace_entry(
                                            index, entry, dir, busy || form.is_some(), cx,
                                        )
                                    }).collect::<Vec<_>>()
                                })),
                        )
                        // Keep the scrollbar fixed above the rows. The scroll handle's
                        // child indices then match the directory entry indices.
                        .vertical_scrollbar(&browser.scroll),
                )
            })
            .children(
                self.workspaces
                    .error
                    .clone()
                    .map(|error| div().text_sm().text_color(theme.danger).child(error)),
            );

        if let Some(form) = form {
            panel = panel.child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().child(tr!("workspace-new-workspace")))
                    .child(
                        div()
                            .id("workspace-create-name")
                            .child(Input::new(&form.name).disabled(busy)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(KINDS.map(|(kind, key)| {
                                Button::new(key)
                                    .outline()
                                    .small()
                                    .label(tr!(key))
                                    .toggled(kind == form.kind)
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(form) = this
                                            .workspaces
                                            .browser
                                            .as_mut()
                                            .and_then(|browser| browser.create.as_mut())
                                        {
                                            form.kind = kind;
                                            cx.notify();
                                        }
                                    }))
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("workspace-create-cancel")
                                    .ghost()
                                    .label(tr!("workspace-back"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if let Some(browser) = &mut this.workspaces.browser {
                                            browser.create = None;
                                            browser.focus.focus(window, cx);
                                        }
                                        this.workspaces.error = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("workspace-create-confirm")
                                    .primary()
                                    .label(tr!("workspace-create"))
                                    .disabled(busy)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.create_workspace(cx)),
                                    ),
                            ),
                    ),
            );
        } else {
            panel = panel.child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .justify_end()
                    .when(!legacy, |row| {
                        row.child(
                            Button::new("workspace-create")
                                .outline()
                                .label(tr!("workspace-create-a-workspace"))
                                .disabled(busy || directory.is_none())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_create_workspace(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("confirm-open-workspace")
                            .primary()
                            .label(tr!("workspace-open-this-folder"))
                            .disabled(busy || !can_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(browser) = &this.workspaces.browser else {
                                    return;
                                };
                                let path = if let Some(directory) = &browser.directory {
                                    format!("{}/", directory.path)
                                } else {
                                    browser.input.read(cx).value().to_string()
                                };
                                this.request_open_workspace(path, cx);
                            })),
                    ),
            );
        }
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.foreground.opacity(0.20))
                .p_4()
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_workspace_entry(
        &self,
        index: usize,
        entry: &WorkspaceEntry,
        dir: &WorkspaceDirectory,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.workspaces.browser.as_ref().and_then(|b| b.selected) == Some(index);
        let target = child_path(&dir.path, &entry.name);
        h_flex()
            .id(("workspace-directory-entry", index))
            .flex_none()
            .min_h(px(34.))
            .py_0p5()
            .px_3()
            .gap_2()
            .text_sm()
            .when(selected, |row| row.bg(theme.accent))
            .hover(|row| row.bg(theme.accent))
            .child(
                (if entry.directory {
                    Symbol::Folder
                } else {
                    Symbol::FileText
                })
                .tinted(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(entry.name.clone()),
            )
            .children(
                [
                    (entry.workspace, tr!("workspace-workspace")),
                    (entry.git, "Git"),
                    (entry.fuzukue, "fuzukue.toml"),
                ]
                .into_iter()
                .filter(|(show, _)| *show)
                .map(|(_, label)| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(label)
                }),
            )
            .when(!busy, |row| {
                let directory = entry.directory;
                row.cursor_pointer().on_click(cx.listener(
                    move |this, event: &gpui_kit::ClickEvent, window, cx| {
                        if directory && event.click_count() == 2 {
                            this.browse_workspace(target.clone(), window, cx);
                        } else if let Some(browser) = &mut this.workspaces.browser {
                            browser.selected = Some(index);
                            browser.focus.focus(window, cx);
                            cx.notify();
                        }
                    },
                ))
            })
    }
}

#[cfg(test)]
mod browser_tests {
    use super::*;

    #[test]
    fn workspace_path_hint_omits_a_duplicate_title() {
        let mut entry = WorkspaceInfo {
            id: "labo".into(),
            path: "/home/user/labo".into(),
            title: "labo".into(),
            last_opened: None,
            running: false,
        };
        assert_eq!(workspace_path_hint(&entry), None);
        entry.title = "Laboratory".into();
        assert_eq!(workspace_path_hint(&entry), Some("labo"));
    }

    #[test]
    fn folder_keyboard_reveals_only_a_changed_selection() {
        assert_eq!(newly_selected(None, 0), Some(0));
        assert_eq!(newly_selected(Some(0), 1), Some(1));
        assert_eq!(newly_selected(Some(0), 0), None);
        assert_eq!(newly_selected(Some(8), 8), None);
    }
}
