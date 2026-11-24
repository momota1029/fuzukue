use std::collections::HashMap;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{
    div, prelude::*, px, relative, transparent_black, Anchor, AnyElement, Context,
    Focusable, MouseButton, Pixels, Point, SharedString, Window,
};
use fuzukue_proto::{ClientCommand, ClientMsg, TabView, ThreadGroup as ThreadSection, ThreadView};

use crate::sidebar::{section_header, sidebar_icon, sidebar_row, SIDEBAR_ICON_GAP, SIDEBAR_ROW_INSET};
use crate::{appearance::UI_LINE_HEIGHT, icons::Symbol, wire_deck, Workbench};

const THREAD_DRAG_PREVIEW_OFFSET: Pixels = px(8.);
const THREAD_DRAG_PREVIEW_OPACITY: f32 = 0.85;
const THREAD_DRAG_SOURCE_OPACITY: f32 = 0.5;
const THREAD_ARCHIVE_HIGHLIGHT_OPACITY: f32 = 0.28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveSection {
    Hidden,
    Header,
    DropZone,
}

fn archive_section(has_archived_threads: bool, can_drop_thread: bool) -> ArchiveSection {
    match (has_archived_threads, can_drop_thread) {
        (true, _) => ArchiveSection::Header,
        (false, true) => ArchiveSection::DropZone,
        (false, false) => ArchiveSection::Hidden,
    }
}

fn apply_thread_order<'a>(threads: &'a [ThreadView], order: &[String]) -> Vec<&'a ThreadView> {
    let ranks: HashMap<_, _> = order.iter().enumerate()
        .map(|(index, id)| (id.as_str(), index + 1)).collect();
    let mut threads: Vec<_> = threads.iter().collect();
    // Stable sorting leaves unseen threads in the server's order above the saved ones.
    threads.sort_by_key(|thread| ranks.get(thread.id.as_str()).copied().unwrap_or(0));
    threads
}

fn move_thread_in_order(
    mut threads: Vec<&ThreadView>, id: &str, target: &str, after: bool,
) -> Option<Vec<String>> {
    let from = threads.iter().position(|thread| thread.id == id)?;
    let to = threads.iter().position(|thread| thread.id == target)?;
    if from == to || ThreadSection::of(threads[from]) != ThreadSection::of(threads[to]) {
        return None;
    }
    let insertion = to + usize::from(after) - usize::from(from < to);
    if from == insertion {
        return None;
    }
    let thread = threads.remove(from);
    threads.insert(insertion, thread);
    Some(threads.into_iter().map(|thread| thread.id.clone()).collect())
}

#[derive(Clone)]
struct ThreadDrag {
    id: String,
    workspace: Option<String>,
    section: ThreadSection,
}

impl ThreadDrag {
    fn can_archive(&self, workspace: Option<&str>) -> bool {
        self.workspace.as_deref() == workspace && self.section != ThreadSection::Archived
    }

    fn can_reorder(&self, workspace: Option<&str>, section: ThreadSection, target: &str) -> bool {
        self.workspace.as_deref() == workspace && self.section == section && self.id != target
    }
}

pub(super) struct ThreadDragPreview {
    drag: ThreadDrag,
    title: SharedString,
    icon: Symbol,
    cursor_offset: Point<Pixels>,
}

impl Render for ThreadDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // GPUI subtracts the initial pointer offset when placing the preview root.
        div().child(
            h_flex().id("thread-drag-preview")
                .relative()
                .left(self.cursor_offset.x + THREAD_DRAG_PREVIEW_OFFSET)
                .top(self.cursor_offset.y + THREAD_DRAG_PREVIEW_OFFSET)
                .w(px(190.)).gap(SIDEBAR_ICON_GAP).px(SIDEBAR_ROW_INSET).py_1()
                .rounded_md().border_1().border_color(theme.border).bg(theme.background)
                .opacity(THREAD_DRAG_PREVIEW_OPACITY)
                .text_sm().text_color(theme.foreground)
                .child(sidebar_icon(self.icon.tinted(theme.muted_foreground)).id("thread-drag-preview-icon"))
                .child(div().id("thread-drag-preview-title").min_w_0().truncate().child(self.title.clone())),
        )
    }
}

impl Workbench {
    pub(super) fn select_thread(&mut self, id: String, cx: &mut Context<Self>) {
        if matches!(self.right, super::RightPane::Library | super::RightPane::Handoff | super::RightPane::ResearchIssues | super::RightPane::ResearchIssue(_) | super::RightPane::Exploration) {
            self.close_right_pane(cx);
        }
        self.thread_title_edit = None;
        self.settings_open = false;
        self.switch_session(id, cx);
    }

    pub(super) fn tab_attachment(tab: &TabView) -> Option<&str> {
        tab.deck
            .as_deref()
            .map(|path| if path.is_empty() { "." } else { path })
    }

    pub(super) fn render_thread_header(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self
            .threads
            .iter()
            .find(|thread| thread.id == self.active)?;
        let theme = cx.theme().clone();
        let mut chips = h_flex().gap_2().items_center().flex_wrap();
        for deck in &thread.attached {
            let id = thread.id.clone();
            let path = deck.clone();
            let open_path = deck.clone();
            chips = chips.child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .text_xs()
                    .min_w_0()
                    .max_w_full()
                    .child(
                        Button::new(SharedString::from(format!("open-attachment-{deck}")))
                            .ghost()
                            .xsmall()
                            .line_height(UI_LINE_HEIGHT)
                            .min_w_0()
                            .flex_shrink_1()
                            .label(self.deck_title(deck))
                            .tooltip(tr!("threads-open", deck = deck.to_string()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_attachment(&open_path, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("detach-{deck}")))
                            .ghost()
                            .xsmall()
                            .flex_none()
                            .icon(Symbol::X.icon())
                            .accessibility_label(tr!("threads-detach", deck = deck.to_string()))
                            .tooltip(tr!("threads-detach", deck = deck.to_string()))
                            .on_click(cx.listener(move |this, _, _, _| {
                                let _ = this.out.send(ClientMsg::DetachThread {
                                    id: id.clone(),
                                    deck: path.clone(),
                                });
                            })),
                    ),
            );
        }

        if self.protocol >= 13 {
            for route in &thread.suggestions {
                let id = thread.id.clone();
                let deck = route.deck.clone();
                chips = chips.child(Button::new(SharedString::from(format!("suggest-{}", route.deck)))
                    .ghost().xsmall().line_height(UI_LINE_HEIGHT)
                    .label(tr!("threads-confirm-attach", title = route.title.to_string()))
                    .on_click(cx.listener(move |this, _, _, _| {
                        let _ = this.out.send(ClientMsg::AttachThread { id: id.clone(), decks: vec![deck.clone()] });
                    })));
            }
        }

        Some(
            v_flex()
                .id("thread-header")
                .flex_none()
                .px_5()
                .py_2()
                .gap_1()
                .bg(theme.title_bar)
                .border_b_1()
                .border_color(theme.title_bar_border)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .id("thread-header-title")
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_bold()
                                .truncate()
                                .child(crate::thread_title(thread).to_owned()),
                        )
                        .children((self.protocol >= 6).then(|| self.thread_info_toggle(cx))),
                )
                .children((!thread.attached.is_empty()
                    || (self.protocol >= 13 && !thread.suggestions.is_empty())).then_some(chips))
                .children(self.github_chips(thread, cx))
                .children(self.github_watch_status(cx))
                .into_any_element(),
        )
    }

    fn thread_attachment_menu(
        &self, thread: &ThreadView,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let choices: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let deck = Self::tab_attachment(tab)?;
                (!thread.attached.iter().any(|path| path == deck))
                    .then(|| (deck.to_owned(), format!("{} · {deck}", tab.title)))
            })
            .collect();
        let id = thread.id.clone();
        let out = self.out.clone();
        move |mut menu, _, _| {
            if choices.is_empty() {
                return menu.label(tr!("threads-no-decks-available"));
            }
            for (path, label) in &choices {
                let deck = path.clone();
                let id = id.clone();
                let out = out.clone();
                menu = menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, _| {
                    let _ = out.send(ClientMsg::AttachThread {
                        id: id.clone(),
                        decks: vec![deck.clone()],
                    });
                }));
            }
            menu
        }
    }

    pub(super) fn thread_menu(
        &self, mut menu: PopupMenu, thread: &ThreadView, row_menu: bool, view: gpui_kit::WeakEntity<Self>,
    ) -> impl FnOnce(&mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let id = thread.id.clone();
        let title = thread.title.clone();
        let rename_view = view.clone();
        let rename_id = id.clone();
        menu = menu.item(PopupMenuItem::new(tr!("threads-rename")).on_click(move |_, window, cx| {
            let _ = rename_view.update(cx, |this, cx| {
                // The inline rename editor lives in the sidebar, including for
                // threads whose section the user has collapsed.
                this.begin_thread_rename(rename_id.clone(), title.clone(), window, cx);
            });
        }));

        let out = self.out.clone();
        let archived = thread.archived;
        if row_menu && self.protocol >= 15 && !archived {
            let out = out.clone();
            let id = id.clone();
            let pinned = thread.pinned;
            menu = menu.item(PopupMenuItem::new(if pinned { tr!("threads-unpin") } else { tr!("threads-pinned") })
                .icon(Symbol::Pin.icon())
                .on_click(move |_, _, _| {
                    let _ = out.send(ClientMsg::PinThread { id: id.clone(), pinned: !pinned });
                }));
        }
        if row_menu && self.protocol >= fuzukue_proto::CLOSE_TRASH_PROTOCOL {
            let view = view.clone();
            let id = id.clone();
            let label = crate::thread_title(thread).to_owned();
            menu = menu.item(PopupMenuItem::new(tr!("trash-delete"))
                .icon(Symbol::Trash.icon())
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.confirm_removal(ClientMsg::Trash {
                            action: fuzukue_proto::TrashAction::Delete {
                                target: fuzukue_proto::TrashTarget::Thread { id: id.clone() },
                            },
                        }, label.clone(), window, cx);
                    });
                }));
        }

        let attachment = self.thread_attachment_menu(thread);
        menu = self.intake_menu(menu, thread);
        let link = (self.protocol >= fuzukue_proto::ISSUE_THREADS_PROTOCOL)
            .then(|| self.issue_link_menu(thread, view));
        let can_archive = self.protocol >= 7;
        move |window, cx| {
            if row_menu {
                menu = menu.separator();
            }
            menu = menu.submenu(tr!("menu-attach-deck"), window, cx, attachment);
            if let Some(link) = link {
                menu = menu.submenu(tr!("menu-link-issue"), window, cx, link);
            }
            if can_archive {
                let label = if archived { tr!("threads-restore-from-archive") } else { tr!("threads-archive") };
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, _| {
                    let _ = out.send(ClientMsg::ArchiveThread { id: id.clone(), archived: !archived });
                }));
            }
            menu
        }
    }

    fn begin_thread_rename(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.threads.iter().find(|thread| thread.id == id) else { return };
        self.set_thread_group_open(ThreadSection::of(thread), true, cx);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit_thread_title(cx);
            }
        })
        .detach();
        self.thread_title_edit = Some((id, input));
        cx.notify();
    }

    fn commit_thread_title(&mut self, cx: &mut Context<Self>) {
        let Some((id, input)) = &self.thread_title_edit else {
            return;
        };
        let title = input.read(cx).value().trim().to_string();
        if !title.is_empty() {
            let _ = self.out.send(ClientMsg::RenameThread {
                id: id.clone(),
                title,
            });
            self.thread_title_edit = None;
            cx.notify();
        }
    }

    pub(super) fn deck_title(&self, path: &str) -> String {
        if let Some((workspace, deck)) = path.split_once(':') {
            if let Some(entry) = self.workspaces.entries.iter().find(|entry| entry.id == workspace) {
                return format!("{} · {deck}", entry.title);
            }
        }
        self.tabs
            .iter()
            .find(|tab| Self::tab_attachment(tab) == Some(path))
            .map(|tab| tab.title.clone())
            .or_else(|| {
                self.tabs
                    .iter()
                    .flat_map(|tab| &tab.papers)
                    .find(|paper| paper.decks.iter().any(|deck| deck == wire_deck(path)))
                    .map(|paper| paper.name.clone())
            })
            .unwrap_or_else(|| path.to_owned())
    }

    pub(super) fn render_threads(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let threads = apply_thread_order(&self.threads, self.thread_manual_order());
        let active_drag = self.sidebar.thread_drag.as_ref()
            .filter(|_| cx.has_active_drag())
            .and_then(|preview| preview.upgrade())
            .and_then(|preview| {
                let drag = &preview.read(cx).drag;
                (drag.workspace == self.workspaces.active).then(|| drag.clone())
            });
        let is_dragged = |thread: &ThreadView| active_drag.as_ref().is_some_and(|drag| drag.id == thread.id);
        let section = |section| threads.iter().copied()
            .filter(move |thread| ThreadSection::of(thread) == section);
        let pinned = section(ThreadSection::Pinned);
        let current = section(ThreadSection::Current);
        let archived = section(ThreadSection::Archived);
        let mut list = v_flex().gap_0p5();
        if pinned.clone().next().is_some() {
            let open = self.thread_group_open(ThreadSection::Pinned);
            list = list.child(section_header("pinned-threads-toggle", tr!("threads-pinned"), open, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    let group = ThreadSection::Pinned;
                    this.set_thread_group_open(group, !this.thread_group_open(group), cx);
                })))
                .when(open, |list| list.children(pinned.map(|thread| self.render_thread(thread, is_dragged(thread), cx))));
        }
        if current.clone().next().is_some() {
            let open = self.thread_group_open(ThreadSection::Current);
            list = list.child(section_header("recent-threads-toggle", tr!("threads-threads"), open, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    let group = ThreadSection::Current;
                    this.set_thread_group_open(group, !this.thread_group_open(group), cx);
                })))
                .when(open, |list| list.children(current.map(|thread| self.render_thread(thread, is_dragged(thread), cx))));
        }
        let workspace = self.workspaces.active.as_deref();
        let archive = archive_section(
            archived.clone().next().is_some(),
            active_drag.as_ref().is_some_and(|drag| drag.can_archive(workspace)),
        );
        if self.protocol >= 7 && archive != ArchiveSection::Hidden {
            let open = self.thread_group_open(ThreadSection::Archived);
            let highlight = cx.theme().link.opacity(THREAD_ARCHIVE_HIGHLIGHT_OPACITY);
            let foreground = cx.theme().foreground;
            let workspace = self.workspaces.active.clone();
            let target = if archive == ArchiveSection::Header {
                section_header("archived-threads-toggle", tr!("threads-archive"), open, cx)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let group = ThreadSection::Archived;
                        this.set_thread_group_open(group, !this.thread_group_open(group), cx);
                    }))
            } else {
                div().id("archived-threads-drop-zone")
                    .mx(SIDEBAR_ROW_INSET).px_2().py_1()
                    .rounded_sm().border_1().border_dashed().border_color(cx.theme().border)
                    .text_sm().text_color(cx.theme().muted_foreground)
                    .child(tr!("threads-archive-drop-zone"))
            };
            list = list.child(target.can_drop(move |drag, _, _| drag.downcast_ref::<ThreadDrag>().is_some_and(|drag| {
                    drag.can_archive(workspace.as_deref())
                }))
                .drag_over::<ThreadDrag>(move |style, _, _, _| style.bg(highlight).text_color(foreground))
                .on_drop(cx.listener(|this, drag: &ThreadDrag, _, _| {
                    if drag.can_archive(this.workspaces.active.as_deref())
                        && this.threads.iter().any(|thread| thread.id == drag.id && !thread.archived)
                    {
                        let _ = this.out.send(ClientMsg::ArchiveThread { id: drag.id.clone(), archived: true });
                    }
                })))
                .when(open, |list| list.children(archived.map(|thread| self.render_thread(thread, is_dragged(thread), cx))));
        }
        list
    }

    pub(super) fn on_client_command(&mut self, command: ClientCommand, cx: &mut Context<Self>) {
        match command {
            ClientCommand::ReorderThread { id, target, after } => self.reorder_thread(&id, &target, after, cx),
            ClientCommand::SetGroupState { group, open } => self.set_thread_group_open(group, open, cx),
        }
    }

    fn reorder_thread(&mut self, id: &str, target: &str, after: bool, cx: &mut Context<Self>) {
        let threads = apply_thread_order(&self.threads, self.thread_manual_order());
        if let Some(order) = move_thread_in_order(threads, id, target, after) {
            self.remember_thread_manual_order(order);
            cx.notify();
        }
    }

    fn move_thread_with_key(&mut self, id: &str, down: bool, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.iter().find(|thread| thread.id == id) else { return };
        let section = ThreadSection::of(thread);
        let threads = apply_thread_order(&self.threads, self.thread_manual_order());
        let siblings: Vec<_> = threads.iter().filter(|thread| ThreadSection::of(thread) == section).collect();
        let Some(index) = siblings.iter().position(|thread| thread.id == id) else { return };
        if let Some(target) = index.checked_add_signed(if down { 1 } else { -1 })
            .and_then(|index| siblings.get(index))
        {
            let target = target.id.clone();
            self.reorder_thread(id, &target, down, cx);
        }
    }

    fn render_thread(&self, thread: &ThreadView, is_dragged: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = thread.id == self.active;
        let mark = if self.thread_usage_paused(&thread.id) {
            Symbol::Pause
        } else if self.thread_working(&thread.id) {
            Symbol::CircleDot
        } else if thread.archived {
            Symbol::Archive
        } else if thread.pinned {
            Symbol::Pin
        } else {
            Symbol::MessageSquare
        };
        let id = thread.id.clone();
        let attachments = thread
            .attached
            .iter()
            .map(|deck| self.deck_title(deck))
            .collect::<Vec<_>>()
            .join(" · ");
        if let Some((_, input)) = self
            .thread_title_edit
            .as_ref()
            .filter(|(editing, _)| editing == &id)
        {
            return v_flex().child(Input::new(input)).child(
                h_flex()
                    .px_2()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .id("save-thread-title")
                            .cursor_pointer()
                            .text_color(theme.link)
                            .child(tr!("threads-save"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.commit_thread_title(cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("cancel-thread-title")
                            .cursor_pointer()
                            .child(tr!("threads-cancel"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.thread_title_edit = None;
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }
        let focus = self.sidebar.thread_focus[&id].clone();
        let section = ThreadSection::of(thread);
        let drag = ThreadDrag { id: id.clone(), workspace: self.workspaces.active.clone(), section };
        let title = SharedString::from(crate::thread_title(thread).to_owned());
        let preview_title = title.clone();
        let drag_view = cx.weak_entity();
        let drag_focus = focus.clone();
        let select_id = id.clone();
        let keyboard_id = id.clone();
        let goal = self.thread_info.plans.get(&id)
            .and_then(|(plan, _)| crate::thread_info::active_goal_title(plan))
            .map(|title| {
                let label = tr!("thread-details-goal-marker", title = title);
                div().id(SharedString::from(format!("thread-goal-marker-{id}")))
                    .flex_none().text_color(theme.link).aria_label(label.clone())
                    .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                    .child("⚑")
            });
        let mark_icon = mark.tinted(theme.muted_foreground).flex_none();
        let mark_icon = if matches!(mark, Symbol::CircleDot) {
            super::chat::activity_spinner(SharedString::from(format!("thread-busy-{id}")), theme.muted_foreground).into_any_element()
        } else {
            mark_icon.into_any_element()
        };
        let content = h_flex()
            .id("thread-select")
            .track_focus(&focus)
            .rounded_sm()
            .focus_visible(|content| content.bg(theme.accent))
            .flex_1()
            .self_stretch()
            .min_w_0()
            .gap(SIDEBAR_ICON_GAP)
            .cursor_pointer()
            .aria_label(title.clone())
            .aria_description(tr!("threads-reorder"))
            .child(sidebar_icon(mark_icon).id(SharedString::from(format!("thread-row-icon-{id}"))))
            .child(v_flex().min_w_0().flex_1().line_height(UI_LINE_HEIGHT)
                .child(div().id(SharedString::from(format!("thread-row-title-{id}")))
                    .text_sm().truncate().child(title))
                .when(!attachments.is_empty(), |row| {
                    row.child(div().truncate().text_xs().text_color(theme.muted_foreground).child(attachments))
                }))
            .on_click(cx.listener(move |this, _, _, cx| this.select_thread(select_id.clone(), cx)))
            .on_drag(drag, move |drag, cursor_offset, window, cx| {
                drag_focus.focus(window, cx);
                let preview = cx.new(|_| ThreadDragPreview {
                    drag: drag.clone(),
                    title: preview_title.clone(),
                    icon: mark,
                    cursor_offset,
                });
                let _ = drag_view.update(cx, |this, _| {
                    this.sidebar.thread_drag = Some(preview.downgrade());
                });
                // GPUI can discard a rejected drop without redrawing the source row.
                window.observe_release(&preview, cx, |_, window, _| window.refresh()).detach();
                preview
            })
            .capture_key_down(cx.listener(move |this, event: &gpui_kit::KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                match key.key.as_str() {
                    "up" | "down" if key.modifiers.alt && key.modifiers.number_of_modifiers() == 1 => {
                        this.move_thread_with_key(&keyboard_id, key.key == "down", cx);
                    }
                    "enter" | "space" if key.modifiers.number_of_modifiers() == 0 => {
                        this.select_thread(keyboard_id.clone(), cx);
                    }
                    "escape" if this.sidebar.thread_drag.as_ref().is_some_and(|preview| preview.upgrade().is_some())
                        && cx.stop_active_drag(window) => {}
                    _ => return,
                }
                cx.stop_propagation();
            }));

        let indicator_id = id.clone();
        let intake = self.intake_badge(thread, cx);
        let indicators = (thread.github.unread > 0 || goal.is_some() || intake.is_some()).then(|| h_flex()
            .id("thread-indicators")
            .flex_none().gap(SIDEBAR_ICON_GAP).cursor_pointer()
            .children((thread.github.unread > 0).then(|| {
                div().flex_none().px_1().rounded_md().bg(theme.secondary).text_xs()
                    .child(tr!("github-unread", count = thread.github.unread))
            }))
            .children(goal)
            .children(intake)
            .on_click(cx.listener(move |this, _, window, cx| {
                focus.focus(window, cx);
                this.select_thread(indicator_id.clone(), cx);
            })));

        let view = cx.weak_entity();
        let menu_id = id.clone();
        let menu = Button::new(SharedString::from(format!("thread-menu-{id}")))
            .ghost()
            .xsmall()
            .icon(Symbol::Ellipsis.icon())
            .accessibility_label(tr!("threads-thread-actions"))
            .when(!selected, |button| {
                button
                    .opacity(0.)
                    .group_hover("thread-row", |d| d.opacity(1.))
                    .focus_visible(|d| d.opacity(1.))
            })
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let Some(entity) = view.upgrade() else { return menu };
                let workbench = entity.read(cx);
                let Some(thread) = workbench.threads.iter().find(|thread| thread.id == menu_id) else {
                    return menu;
                };
                let build = workbench.thread_menu(menu, thread, true, view.clone());
                build(window, cx)
            });
        let drop_targets = [false, true].map(|after| {
            let (target_id, workspace) = (id.clone(), self.workspaces.active.clone());
            let drop_id = target_id.clone();
            let line = theme.link;
            div().id(if after { "thread-drop-after" } else { "thread-drop-before" })
                .absolute().left_0().right_0().h(relative(0.5))
                .map(|half| if after { half.bottom_0().border_b_1() } else { half.top_0().border_t_1() })
                .border_color(transparent_black())
                .can_drop(move |drag, _, _| drag.downcast_ref::<ThreadDrag>().is_some_and(|drag| {
                    drag.can_reorder(workspace.as_deref(), section, &target_id)
                }))
                .drag_over::<ThreadDrag>(move |style, _, _, _| style.border_color(line))
                .on_drop(cx.listener(move |this, drag: &ThreadDrag, _, cx| {
                    this.reorder_thread(&drag.id, &drop_id, after, cx);
                }))
        });
        v_flex().child(
            sidebar_row()
                .id(SharedString::from(format!("thread-row-all-{id}")))
                .group("thread-row")
                .relative()
                .min_w_0()
                .gap_1()
                .rounded_md()
                .when(selected, |row| row.bg(theme.muted))
                .when(!selected, |row| row.hover(|row| row.bg(theme.accent.opacity(0.65))))
                .when(is_dragged, |row| row.opacity(THREAD_DRAG_SOURCE_OPACITY))
                .when(selected, |row| row.child(
                    div().absolute().left_0().top_1().bottom_1().w(px(2.))
                        .rounded_full().bg(theme.link),
                ))
                .child(content)
                .children(indicators)
                .child(menu)
                .children(drop_targets),
        )
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    fn thread(id: &str, pinned: bool, archived: bool) -> ThreadView {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "updated": 0, "pinned": pinned, "archived": archived,
        })).unwrap()
    }

    #[test]
    fn manual_order_uses_saved_positions_and_keeps_new_threads_at_the_top() {
        let threads = [
            thread("newest", false, false),
            thread("second", false, false),
            thread("also-new", false, false),
            thread("first", false, false),
        ];
        let order = ["deleted".into(), "first".into(), "second".into()];
        let ids: Vec<_> = apply_thread_order(&threads, &order).into_iter()
            .map(|thread| thread.id.as_str()).collect();
        assert_eq!(ids, ["newest", "also-new", "first", "second"]);
    }

    #[test]
    fn manual_order_moves_before_or_after_targets_in_the_same_section() {
        let threads = [
            thread("a", false, false),
            thread("b", false, false),
            thread("c", false, false),
            thread("pinned", true, false),
            thread("archived", false, true),
        ];
        let move_to = |id, target, after| move_thread_in_order(threads.iter().collect(), id, target, after);
        assert_eq!(move_to("a", "c", true).unwrap(), ["b", "c", "a", "pinned", "archived"]);
        assert_eq!(move_to("c", "a", false).unwrap(), ["c", "a", "b", "pinned", "archived"]);
        assert_eq!(move_to("a", "b", false), None);
        assert_eq!(move_to("a", "a", true), None);
        assert_eq!(move_to("a", "pinned", true), None);
        assert_eq!(move_to("a", "archived", false), None);
    }

    #[test]
    fn archive_section_tracks_archived_threads_and_eligible_drags() {
        assert_eq!(archive_section(false, false), ArchiveSection::Hidden);
        assert_eq!(archive_section(true, false), ArchiveSection::Header);
        assert_eq!(archive_section(false, true), ArchiveSection::DropZone);
        assert_eq!(archive_section(false, false), ArchiveSection::Hidden);
        assert_eq!(archive_section(true, true), ArchiveSection::Header);
    }

    #[test]
    fn thread_drop_targets_respect_the_workspace_and_section() {
        let current = ThreadDrag { id: "a".into(), workspace: Some("project".into()), section: ThreadSection::Current };
        assert!(current.can_archive(Some("project")));
        assert!(!current.can_archive(Some("other")));
        assert!(!current.can_archive(None));
        assert!(current.can_reorder(Some("project"), ThreadSection::Current, "b"));
        assert!(!current.can_reorder(Some("project"), ThreadSection::Current, "a"));
        assert!(!current.can_reorder(Some("project"), ThreadSection::Pinned, "b"));
        assert!(!current.can_reorder(Some("other"), ThreadSection::Current, "b"));

        let pinned = ThreadDrag { section: ThreadSection::Pinned, ..current.clone() };
        assert!(pinned.can_archive(Some("project")));
        let archived = ThreadDrag { workspace: None, section: ThreadSection::Archived, ..current };
        assert!(!archived.can_archive(None));
        assert!(archived.can_reorder(None, ThreadSection::Archived, "b"));
    }
}
