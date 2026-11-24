//! ワークスペース内のファイルを名前とフォルダーで開く。

mod open;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use fuzukue_proto::{
    ClientMsg, FileEntry, FileKind, FilePage, ResolvedFile, FILE_BROWSER_PROTOCOL,
};
use gpui_kit::{
    base::{Button as BaseButton, Disableable as _, Popover, PopoverState},
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        menu::{PopupMenu, PopupMenuItem},
        scroll::ScrollableElement as _,
        v_flex, ActiveTheme as _, Side, Sizable as _,
    },
    div, point,
    prelude::*,
    px, relative, Anchor, AnyElement, Context, DismissEvent, Entity, FocusHandle, Focusable as _,
    Keystroke, MouseButton, Pixels, ScrollHandle, Task, WeakEntity, Window,
};

use crate::{focused_composition, icons::Symbol, RightPane, Workbench};
use open::{open_external, open_target, DownloadedFile, InApp, OpenTarget};

type Page = Result<FilePage, String>;

#[derive(Default)]
pub(super) struct Files {
    next_request: u64,
    listings: HashMap<String, Page>,
    pending: HashMap<u64, String>,
    last_dir: String,
    hidden: bool,
    recent: VecDeque<String>,
    browser: Option<Browser>,
    pub(super) paper_file: Option<(String, String)>,
    downloads: Vec<DownloadedFile>,
}

struct Browser {
    previous: RightPane,
    previous_settings: bool,
    previous_focus: Option<FocusHandle>,
    input: Entity<InputState>,
    tree_focus: FocusHandle,
    tree_open: bool,
    expanded: HashSet<String>,
    dir: String,
    typed_base: String,
    filter: String,
    search_on: bool,
    search: Option<u64>,
    search_task: Option<Task<()>>,
    matches: Option<Page>,
    selected: usize,
    scroll: ScrollHandle,
    opening: Option<(u64, FileEntry)>,
    error: Option<String>,
}

impl Files {
    fn request(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    pub(super) fn reset(&mut self) {
        let next_request = self.next_request;
        let downloads = std::mem::take(&mut self.downloads);
        *self = Self {
            next_request,
            downloads,
            ..Self::default()
        };
    }

    fn page(&self) -> Option<&Page> {
        let browser = self.browser.as_ref()?;
        if browser.searching() {
            browser.matches.as_ref()
        } else {
            self.listings.get(&browser.dir)
        }
    }

    fn rows(&self) -> Vec<&FileEntry> {
        let Some(browser) = &self.browser else {
            return Vec::new();
        };
        let Some(Ok(page)) = self.page() else {
            return Vec::new();
        };
        if browser.searching() {
            page.entries.iter().collect()
        } else {
            let (_, query) = typed_location(&browser.typed_base, &browser.filter);
            filter_entries(&page.entries, query)
        }
    }
}

impl Browser {
    fn searching(&self) -> bool {
        self.search_on || self.filter.starts_with('/')
    }

    fn reset_results(&mut self) {
        self.selected = 0;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.search = None;
        self.search_task = None;
        self.matches = None;
        self.opening = None;
        self.error = None;
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.into()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn breadcrumbs(root: &str, dir: &str) -> Vec<(String, String)> {
    let mut segments = vec![(root.into(), String::new())];
    let mut path = String::new();
    for segment in dir.split('/').filter(|segment| !segment.is_empty()) {
        path = join(&path, segment);
        segments.push((segment.into(), path.clone()));
    }
    segments
}

#[derive(Debug, PartialEq, Eq)]
enum BreadcrumbItem<'a> {
    Folder {
        name: &'a str,
        path: &'a str,
    },
    Arrow {
        dir: &'a str,
        selected: Option<&'a str>,
    },
    File(&'a str),
}

fn breadcrumb_items<'a>(
    folders: &'a [(String, String)],
    file: Option<&'a str>,
) -> Vec<BreadcrumbItem<'a>> {
    let mut items = Vec::new();
    for (index, (name, path)) in folders.iter().enumerate() {
        items.push(BreadcrumbItem::Folder { name, path });
        let selected = folders
            .get(index + 1)
            .map(|(_, path)| path.as_str())
            .or(file);
        if selected.is_some() || folders.len() == 1 {
            items.push(BreadcrumbItem::Arrow {
                dir: path,
                selected,
            });
        }
    }
    if let Some(file) = file {
        items.push(BreadcrumbItem::File(
            file.rsplit('/').next().unwrap_or(file),
        ));
    }
    items
}

fn folders(page: &FilePage) -> impl Iterator<Item = &FileEntry> {
    page.entries
        .iter()
        .filter(|entry| entry.kind == FileKind::Directory)
}

fn breadcrumb_menu(
    mut menu: PopupMenu,
    listing: Option<&Page>,
    selected: Option<&str>,
    view: &WeakEntity<Workbench>,
    popover: &WeakEntity<PopoverState>,
) -> PopupMenu {
    let label = match listing {
        Some(Ok(page)) if !page.entries.is_empty() => {
            for entry in &page.entries {
                let current = selected == Some(entry.path.as_str());
                let icon = match entry.kind {
                    FileKind::Directory => Symbol::Folder,
                    FileKind::File => Symbol::FileText,
                };
                let entry = entry.clone();
                let view = view.clone();
                let popover = popover.clone();
                menu = menu.item(
                    PopupMenuItem::new(entry.name.clone())
                        .icon(icon.icon())
                        .checked(current)
                        .on_click(move |_, window, cx| {
                            if !(current && entry.kind == FileKind::File) {
                                // Restore focus before the file browser saves it for its own dismissal.
                                let _ =
                                    popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                                let _ = view.update(cx, |this, cx| {
                                    this.open_file_entry(entry.clone(), window, cx);
                                });
                            }
                        }),
                );
            }
            return menu;
        }
        Some(Ok(_)) => tr!("file-browser-empty-folder").to_owned(),
        Some(Err(error)) => tr!("file-browser-list-error", detail = error.clone()),
        None => tr!("workbench-loading").to_owned(),
    };
    menu.label(label)
}

fn filter_entries<'a>(entries: &'a [FileEntry], query: &str) -> Vec<&'a FileEntry> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return entries.iter().collect();
    }
    let mut matches: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.name.to_lowercase();
            let offset = name.find(&query)?;
            Some(((name != query, offset), entry))
        })
        .collect();
    matches.sort_by_key(|(rank, _)| *rank);
    matches.into_iter().map(|(_, entry)| entry).collect()
}

fn typed_location<'a>(base: &str, input: &'a str) -> (String, &'a str) {
    match input.rsplit_once('/') {
        Some((dir, tail))
            if !dir.starts_with('/')
                && dir.split('/').all(|part| !matches!(part, "" | "." | "..")) =>
        {
            (join(base, dir), tail)
        }
        _ => (base.into(), input),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PickerKey {
    Select(usize),
    Open,
    Parent,
    Close,
}

fn picker_key(
    key: &str,
    selected: usize,
    count: usize,
    empty: bool,
    folder: bool,
) -> Option<PickerKey> {
    match key {
        "up" if count > 0 => Some(PickerKey::Select(selected.saturating_sub(1))),
        "down" if count > 0 => Some(PickerKey::Select((selected + 1).min(count - 1))),
        "enter" if count > 0 => Some(PickerKey::Open),
        "right" if folder => Some(PickerKey::Open),
        "left" | "backspace" if empty => Some(PickerKey::Parent),
        "escape" => Some(PickerKey::Close),
        _ => None,
    }
}

impl Workbench {
    pub(super) fn file_browser_supported(&self) -> bool {
        self.protocol >= FILE_BROWSER_PROTOCOL
    }

    pub(super) fn open_file_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.files.last_dir.clone();
        self.open_file_browser_at(dir, window, cx);
    }

    pub(super) fn toggle_file_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.right == RightPane::Files && !self.settings_open {
            self.close_file_browser(window, cx);
        } else {
            self.open_file_browser(window, cx);
        }
    }

    fn open_file_browser_at(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.file_browser_supported() {
            return;
        }
        self.close_palette();
        if self.right == RightPane::Files {
            self.settings_open = false;
            self.navigate_files(dir, true, window, cx);
            return;
        }
        self.files.listings.clear();
        self.files.pending.clear();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("file-browser-name-placeholder"))
        });
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change)
                && this.right == RightPane::Files
                && this
                    .files
                    .browser
                    .as_ref()
                    .is_some_and(|browser| browser.input == *input)
            {
                let value = input.read(cx).value().to_string();
                this.file_filter_changed(value, cx);
            }
        })
        .detach();
        self.files.browser = Some(Browser {
            previous: self.right,
            previous_settings: self.settings_open,
            previous_focus: window.focused(cx),
            input,
            tree_focus: cx.focus_handle(),
            tree_open: true,
            expanded: HashSet::from([String::new()]),
            dir: dir.clone(),
            typed_base: dir.clone(),
            filter: String::new(),
            search_on: false,
            search: None,
            search_task: None,
            matches: None,
            selected: 0,
            scroll: ScrollHandle::new(),
            opening: None,
            error: None,
        });
        self.settings_open = false;
        self.right = RightPane::Files;
        self.navigate_files(dir, true, window, cx);
    }

    pub(super) fn close_file_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(browser) = self.files.browser.take() {
            self.right = browser.previous;
            self.settings_open = browser.previous_settings;
            browser
                .previous_focus
                .unwrap_or_else(|| self.view_focus.clone())
                .focus(window, cx);
        } else {
            self.right = RightPane::None;
        }
        cx.notify();
    }

    fn request_file_list(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.files.listings.contains_key(dir)
            || self.files.pending.values().any(|path| path == dir)
        {
            return;
        }
        let request = self.files.request();
        if self
            .out
            .send(ClientMsg::ListFiles {
                request,
                dir: dir.into(),
                hidden: self.files.hidden,
            })
            .is_ok()
        {
            self.files.pending.insert(request, dir.into());
        } else {
            self.files
                .listings
                .insert(dir.into(), Err(tr!("file-browser-disconnected").into()));
        }
        cx.notify();
    }

    pub(super) fn on_file_list(&mut self, request: u64, result: Page) {
        if let Some(dir) = self.files.pending.remove(&request) {
            self.files.listings.insert(dir, result);
        }
    }

    fn show_file_dir(&mut self, dir: &str, cx: &mut Context<Self>) {
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        browser.dir = dir.into();
        let segments = breadcrumbs("", dir);
        for (_, path) in segments.iter().take(segments.len() - 1) {
            browser.expanded.insert(path.clone());
        }
        for (_, path) in segments {
            self.request_file_list(&path, cx);
        }
    }

    fn navigate_files(
        &mut self,
        dir: String,
        focus_search: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        browser.typed_base = dir.clone();
        browser.search_on = false;
        browser.filter.clear();
        browser.reset_results();
        let input = browser.input.clone();
        self.files.last_dir = dir.clone();
        self.show_file_dir(&dir, cx);
        input.update(cx, |input, cx| input.set_value("", window, cx));
        if focus_search {
            input.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn file_filter_changed(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        browser.filter = value;
        browser.reset_results();
        if browser.searching() {
            self.queue_file_search(cx);
        } else {
            let (dir, _) = typed_location(&browser.typed_base, &browser.filter);
            self.show_file_dir(&dir, cx);
        }
        cx.notify();
    }

    fn queue_file_search(&mut self, cx: &mut Context<Self>) {
        let request = self.files.request();
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        let query = browser
            .filter
            .strip_prefix('/')
            .unwrap_or(&browser.filter)
            .to_owned();
        let message = ClientMsg::FindFiles {
            request,
            query,
            hidden: self.files.hidden,
            recent: self.files.recent.iter().cloned().collect(),
        };
        browser.search = Some(request);
        browser.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.out.send(message).is_err() {
                    this.on_file_matches(request, Err(tr!("file-browser-disconnected").into()));
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn on_file_matches(&mut self, request: u64, result: Page) {
        if let Some(browser) = &mut self.files.browser {
            if browser.search == Some(request) {
                browser.matches = Some(result);
            }
        }
    }

    fn toggle_hidden_files(&mut self, cx: &mut Context<Self>) {
        self.files.hidden = !self.files.hidden;
        self.files.listings.clear();
        self.files.pending.clear();
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        browser.reset_results();
        let expanded: Vec<_> = browser.expanded.iter().cloned().collect();
        let dir = browser.dir.clone();
        let searching = browser.searching();
        self.show_file_dir(&dir, cx);
        for path in expanded {
            self.request_file_list(&path, cx);
        }
        if searching {
            self.queue_file_search(cx);
        }
        cx.notify();
    }

    fn toggle_file_tree_dir(&mut self, dir: &str, expand: bool, cx: &mut Context<Self>) {
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        if expand {
            browser.expanded.insert(dir.into());
            self.request_file_list(dir, cx);
        } else {
            browser.expanded.remove(dir);
        }
        cx.notify();
    }

    pub(super) fn file_picker_key(
        &mut self,
        event: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.right != RightPane::Files
            || self.settings_open
            || event.modifiers.number_of_modifiers() != 0
        {
            return false;
        }
        let Some(browser) = &self.files.browser else {
            return false;
        };
        let key = event.key.as_str();
        if browser.tree_focus.is_focused(window) {
            let dir = browser.dir.clone();
            let expanded = browser.expanded.contains(&dir);
            match key {
                "right" if !expanded => self.toggle_file_tree_dir(&dir, true, cx),
                "right" => {
                    if let Some(child) = self
                        .files
                        .listings
                        .get(&dir)
                        .and_then(|page| page.as_ref().ok())
                        .and_then(|page| folders(page).next())
                    {
                        let path = child.path.clone();
                        self.navigate_files(path, false, window, cx);
                    }
                }
                "left" if expanded => self.toggle_file_tree_dir(&dir, false, cx),
                "left" if !dir.is_empty() => {
                    self.navigate_files(parent(&dir).into(), false, window, cx)
                }
                "escape" => self.close_file_browser(window, cx),
                _ => return false,
            }
            return true;
        }
        let input = &browser.input;
        if !input.read(cx).focus_handle(cx).is_focused(window)
            || focused_composition(input, window, cx)
        {
            return false;
        }
        let rows = self.files.rows();
        let folder = rows
            .get(browser.selected)
            .is_some_and(|entry| entry.kind == FileKind::Directory);
        let empty = input.read(cx).value().is_empty();
        let dir = browser.dir.clone();
        let selected = browser.selected;
        match picker_key(key, selected, rows.len(), empty, folder) {
            Some(PickerKey::Select(index)) => {
                let browser = self.files.browser.as_mut().unwrap();
                if browser.selected != index {
                    browser.selected = index;
                    browser.scroll.scroll_to_item(index);
                    cx.notify();
                }
            }
            Some(PickerKey::Open) => self.open_selected_file(window, cx),
            Some(PickerKey::Parent) => self.navigate_files(parent(&dir).into(), true, window, cx),
            Some(PickerKey::Close) => self.close_file_browser(window, cx),
            None => return false,
        }
        true
    }

    fn open_selected_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(browser) = &self.files.browser else {
            return;
        };
        let Some(entry) = self
            .files
            .rows()
            .get(browser.selected)
            .map(|entry| (*entry).clone())
        else {
            return;
        };
        self.open_file_entry(entry, window, cx);
    }

    fn open_file_entry(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.kind == FileKind::Directory {
            self.open_file_browser_at(entry.path, window, cx);
            return;
        }
        if self.right != RightPane::Files {
            self.open_file_browser_at(parent(&entry.path).into(), window, cx);
        }
        let request = self.files.request();
        let lookup_deck = open_target(&entry.path) == OpenTarget::InApp(InApp::DeckOrSource);
        let Some(browser) = &mut self.files.browser else {
            return;
        };
        browser.error = None;
        if self
            .out
            .send(ClientMsg::ResolveFile {
                request,
                path: entry.path.clone(),
                lookup_deck,
            })
            .is_ok()
        {
            browser.opening = Some((request, entry));
        } else {
            browser.error = Some(tr!("file-browser-disconnected").into());
        }
        cx.notify();
    }

    pub(super) fn on_file_resolved(
        &mut self,
        request: u64,
        result: Result<ResolvedFile, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = self
            .files
            .browser
            .as_mut()
            .filter(|_| self.right == RightPane::Files)
        else {
            return;
        };
        if !browser
            .opening
            .as_ref()
            .is_some_and(|(pending, _)| *pending == request)
        {
            return;
        }
        let (_, entry) = browser.opening.take().unwrap();
        let resolved = match result {
            Ok(resolved) => resolved,
            Err(error) => {
                browser.error = Some(tr!("workbench-cannot-open", path = entry.path, e = error));
                return;
            }
        };
        let path = entry.path;
        self.files.last_dir = parent(&path).into();
        self.files.recent.retain(|recent| recent != &path);
        self.files.recent.push_front(path.clone());
        self.files.recent.truncate(50);
        match open_target(&path) {
            OpenTarget::InApp(target) => {
                self.files.browser = None;
                self.view_focus.focus(window, cx);
                match (target, resolved.deck) {
                    (InApp::Pdf, _) => self.open_pdf(path, Some(window), cx),
                    (InApp::DeckOrSource, Some(deck)) => {
                        let key = deck.path;
                        self.open_paper_target(key.clone(), deck.card, false, cx);
                        self.files.paper_file = Some((key, path));
                    }
                    _ => {
                        self.source.centered = true;
                        self.open_source_file(path, None, window, cx);
                    }
                }
            }
            OpenTarget::External => {
                let http = self.http.clone();
                let epoch = self.connection_epoch;
                let display_path = path.clone();
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            open_external(&http, &path, &entry.name, resolved.absolute.as_deref())
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(Some(download)) => this.files.downloads.push(download),
                            Ok(None) => {}
                            Err(error) if this.connection_epoch == epoch => {
                                let error =
                                    tr!("workbench-cannot-open", path = display_path, e = error);
                                if let Some(browser) = &mut this.files.browser {
                                    browser.error = Some(error);
                                } else {
                                    this.notice = Some(error.into());
                                }
                            }
                            Err(_) => {}
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }
}

fn tree_rows<'a>(
    listings: &'a HashMap<String, Page>,
    expanded: &HashSet<String>,
    dir: &str,
    depth: usize,
    rows: &mut Vec<(usize, &'a FileEntry)>,
) {
    if expanded.contains(dir) {
        if let Some(Ok(page)) = listings.get(dir) {
            for folder in folders(page) {
                rows.push((depth, folder));
                tree_rows(listings, expanded, &folder.path, depth + 1, rows);
            }
        }
    }
}

fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    }
}

impl Workbench {
    pub(super) fn file_browser_disconnected(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.files.listings.clear();
        self.files.pending.clear();
        if self.right == RightPane::Files {
            self.close_file_browser(window, cx);
        }
    }

    fn file_root_name(&self) -> &str {
        self.workspaces
            .current()
            .and_then(|workspace| workspace.path.rsplit('/').find(|part| !part.is_empty()))
            .unwrap_or(tr!("file-browser-root"))
    }

    pub(super) fn render_file_header(
        &self,
        path: &str,
        id: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        if !self.file_browser_supported()
            || path.starts_with('/')
            || path.split('/').any(|part| part == "..")
        {
            return div().id(id).truncate().child(path.to_owned()).into_any_element();
        }
        self.render_file_breadcrumb(id, parent(path), Some(path), cx)
            .into_any_element()
    }

    fn render_file_breadcrumb(
        &self,
        id: &'static str,
        dir: &str,
        file: Option<&str>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let folders = breadcrumbs(self.file_root_name(), dir);
        h_flex()
            .id(id)
            .w_full()
            .min_w_0()
            .gap_0p5()
            .overflow_x_scroll()
            .children(breadcrumb_items(&folders, file).into_iter().map(|item| {
                match item {
                    BreadcrumbItem::Folder { name, path } => {
                        let target = path.to_owned();
                        Button::new(format!("{id}-folder-{path}"))
                            .flex_none()
                            .xsmall()
                            .ghost()
                            .label(name.to_owned())
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_file_browser_at(target.clone(), window, cx);
                            }))
                            .into_any_element()
                    }
                    BreadcrumbItem::Arrow { dir, selected } => self
                        .file_folder_flyout(id, dir, selected, cx)
                        .flex_none()
                        .into_any_element(),
                    BreadcrumbItem::File(name) => div()
                        .flex_none()
                        .px_1()
                        .text_sm()
                        .child(name.to_owned())
                        .into_any_element(),
                }
            }))
    }

    fn file_folder_flyout(
        &self,
        id: &'static str,
        dir: &str,
        selected: Option<&str>,
        cx: &Context<Self>,
    ) -> Popover {
        let trigger_id = format!("{id}-arrow-{dir}");
        let popover_id = format!("{id}-flyout-{dir}-{selected:?}");
        let menu_id = format!("{popover_id}-menu");
        let listing_dir = dir.to_owned();
        let dir = listing_dir.clone();
        let selected = selected.map(str::to_owned);
        let view = cx.entity().downgrade();
        let content_view = view.clone();
        Popover::new(popover_id)
            .anchor(Anchor::TopLeft)
            .offset(px(3.))
            .overlay_closable(false)
            .trigger_with(move |open, _, cx| {
                let theme = cx.theme();
                let hover = theme.accent;
                let ring = theme.ring;
                let symbol = if open {
                    Symbol::ChevronDown
                } else {
                    Symbol::ChevronRight
                };
                BaseButton::new(trigger_id)
                    .size(px(22.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .focus_visible(move |style| style.border_1().border_color(ring))
                    .aria_expanded(open)
                    .accessibility_label(tr!("file-browser-folder-contents"))
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tr!(
                            "file-browser-folder-contents"
                        ))
                        .build(window, cx)
                    })
                    .child(
                        symbol
                            .tinted(theme.muted_foreground)
                            .size(px(12.))
                            .flex_none(),
                    )
                    .into_any_element()
            })
            .on_open_change(move |opening, _, cx| {
                if *opening {
                    let _ = view.update(cx, |this, cx| this.request_file_list(&dir, cx));
                }
            })
            .content(move |_, window, cx| {
                let view = content_view.upgrade();
                let ready = view
                    .as_ref()
                    .is_some_and(|view| view.read(cx).files.listings.contains_key(&listing_dir));
                let popover_entity = cx.weak_entity();
                // Keep the menu's focus and selection across renders. Replace the
                // loading menu once the listing arrives so the checked row scrolls into view.
                let menu_state =
                    window.use_keyed_state(format!("{menu_id}-{ready}"), cx, |window, cx| {
                        let menu = PopupMenu::build(window, cx, |menu, _, cx| {
                            let listing = view
                                .as_ref()
                                .and_then(|view| view.read(cx).files.listings.get(&listing_dir));
                            breadcrumb_menu(
                                menu.min_w(px(250.))
                                    .max_w(px(320.))
                                    .max_h(px(280.))
                                    .scrollable(true)
                                    .check_side(Side::Right),
                                listing,
                                selected.as_deref(),
                                &content_view,
                                &popover_entity,
                            )
                        });
                        menu.focus_handle(cx).focus(window, cx);
                        window
                            .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                                let _ = popover_entity
                                    .update(cx, |popover, cx| popover.dismiss(window, cx));
                            })
                            .detach();
                        menu
                    });
                menu_state.read(cx).clone()
            })
    }

    fn render_file_tree(
        &self,
        browser: &Browser,
        width: Pixels,
        stacked: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // Leave room for the controls and a few glyphs even in deeply nested rows.
        let max_indent = width - theme.font_size * 5.5 - px(16.);
        let mut rows = Vec::new();
        tree_rows(&self.files.listings, &browser.expanded, "", 1, &mut rows);
        // The scrollbar duplicates sizing styles on its wrapper and content.
        // Size the column here so its scroll content can take the full width.
        div()
            .id("file-browser-tree")
            .w(width)
            .flex_none()
            .when(stacked, |tree| tree.h(relative(0.4)).border_b_1())
            .when(!stacked, |tree| tree.h_full().border_r_1())
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .id("file-browser-tree-scroll")
                    .size_full()
                    .track_focus(&browser.tree_focus)
                    .tab_index(0)
                    .overflow_y_scrollbar()
                    .child(self.file_tree_row(self.file_root_name(), "", px(0.), browser, cx))
                    .children(rows.into_iter().map(|(depth, entry)| {
                        let indent = px(depth as f32 * 13.).min(max_indent);
                        self.file_tree_row(&entry.name, &entry.path, indent, browser, cx)
                    })),
            )
    }

    fn file_tree_row(
        &self,
        name: &str,
        path: &str,
        indent: Pixels,
        browser: &Browser,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let expanded = browser.expanded.contains(path);
        let dir = path.to_owned();
        let target = dir.clone();
        let focus = browser.tree_focus.clone();
        let nav_focus = focus.clone();
        h_flex()
            .w_full()
            .h_8()
            .flex_none()
            .pr_2()
            .pl(px(5.) + indent)
            .when(browser.dir == path, |row| row.bg(theme.secondary))
            .child(
                Button::new(format!("file-tree-expand-{path}"))
                    .xsmall()
                    .ghost()
                    .icon(if expanded {
                        Symbol::ChevronDown.icon()
                    } else {
                        Symbol::ChevronRight.icon()
                    })
                    .accessibility_label(if expanded {
                        tr!("file-browser-collapse-folder")
                    } else {
                        tr!("file-browser-expand-folder")
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_file_tree_dir(&dir, !expanded, cx);
                        focus.focus(window, cx);
                    })),
            )
            .child(
                h_flex()
                    .id(format!("file-tree-dir-{path}"))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|row| row.bg(theme.secondary))
                    .child(Symbol::Folder.tinted(theme.muted_foreground))
                    .child(
                        div()
                            .id(format!("file-tree-label-{path}"))
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(name.to_owned()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.navigate_files(target.clone(), false, window, cx);
                            nav_focus.focus(window, cx);
                        }),
                    ),
            )
    }

    pub(super) fn render_file_browser(&self, width: Pixels, cx: &Context<Self>) -> AnyElement {
        let Some(browser) = &self.files.browser else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let tree_min = theme.font_size * 9.;
        let tree_available = width >= tree_min;
        let stacked = tree_available && browser.tree_open && width < tree_min + theme.font_size * 8.;
        let tree_width = if stacked {
            width
        } else {
            (width * 0.38).clamp(tree_min, theme.font_size * 13.)
        };
        let rows = self.files.rows();
        let listing = self.files.page();
        let searching = browser.searching();
        let result = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_4()
                    .pr_7()
                    .h_8()
                    .flex_none()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(tr!("file-browser-name")))
                    .child(
                        div()
                            .w(relative(0.16))
                            .max_w(px(83.))
                            .truncate()
                            .text_right()
                            .child(tr!("file-browser-size")),
                    )
                    .child(
                        div()
                            .w(relative(0.28))
                            .max_w(px(138.))
                            .truncate()
                            .text_right()
                            .child(tr!("file-browser-modified")),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        v_flex()
                            .id("file-browser-list")
                            .size_full()
                            .pr_3()
                            .overflow_y_scroll()
                            .track_scroll(&browser.scroll)
                            .children(
                                rows.iter()
                                    .enumerate()
                                    .map(|(index, entry)| self.file_row(index, entry, browser, cx)),
                            )
                            .children(rows.is_empty().then(|| {
                                let label = match listing {
                                    None => tr!("workbench-loading").into(),
                                    Some(Err(error)) if searching => {
                                        tr!("file-browser-search-error", detail = error.clone())
                                    }
                                    Some(Err(error)) => {
                                        tr!("file-browser-list-error", detail = error.clone())
                                    }
                                    Some(Ok(_)) => tr!("file-browser-no-matches").into(),
                                };
                                div()
                                    .p_4()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(label)
                            })),
                    )
                    .vertical_scrollbar(&browser.scroll),
            )
            .children(
                listing
                    .and_then(|result| result.as_ref().ok())
                    .filter(|page| page.truncated)
                    .map(|_| {
                        div()
                            .px_4()
                            .py_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if searching {
                                tr!("file-browser-search-truncated")
                            } else {
                                tr!("file-browser-list-truncated")
                            })
                    }),
            )
            .children(browser.error.as_ref().map(|error| {
                div()
                    .px_4()
                    .py_1()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone())
            }));
        v_flex()
            .id("file-browser")
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .child(
                v_flex()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .gap_2()
                    .w_full()
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        h_flex()
                            .id("file-browser-header")
                            .min_w_0()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(self.render_file_breadcrumb(
                                "file-browser-breadcrumb",
                                &browser.dir,
                                None,
                                cx,
                            )))
                            .child(self.pane_close_button("close-file-browser", cx)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                div()
                                    .id("file-name-input")
                                    .flex_1()
                                    .min_w(px(135.))
                                    .child(Input::new(&browser.input)),
                            )
                            .child(
                                Button::new("file-search-all")
                                    .small()
                                    .outline()
                                    .label(tr!("file-browser-find-all"))
                                    .when(searching, |button| button.bg(theme.secondary))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let Some(browser) = &mut this.files.browser else {
                                            return;
                                        };
                                        let was_searching = browser.searching();
                                        let value = browser
                                            .filter
                                            .strip_prefix('/')
                                            .unwrap_or(&browser.filter)
                                            .to_owned();
                                        browser.search_on = !was_searching;
                                        let input = browser.input.clone();
                                        input.update(cx, |input, cx| {
                                            input.set_value(value.clone(), window, cx)
                                        });
                                        this.file_filter_changed(value, cx);
                                        input.read(cx).focus_handle(cx).focus(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("file-browser-open")
                                    .small()
                                    .primary()
                                    .label(tr!("file-browser-open"))
                                    .disabled(rows.is_empty() || browser.opening.is_some())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_selected_file(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .children(tree_available.then(|| {
                                Button::new("file-browser-toggle-tree")
                                    .small()
                                    .ghost()
                                    .label(tr!("file-browser-folders"))
                                    .when(browser.tree_open, |button| button.bg(theme.secondary))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(browser) = &mut this.files.browser {
                                            browser.tree_open = !browser.tree_open;
                                            cx.notify();
                                        }
                                    }))
                            }))
                            .child(
                                Button::new("file-browser-hidden")
                                    .small()
                                    .ghost()
                                    .label(if self.files.hidden {
                                        tr!("file-browser-hide-hidden")
                                    } else {
                                        tr!("file-browser-show-hidden")
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_hidden_files(cx)),
                                    ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(stacked, |body| body.flex_col().items_stretch())
                    .children(
                        (tree_available && browser.tree_open)
                            .then(|| self.render_file_tree(browser, tree_width, stacked, cx)),
                    )
                    .child(result),
            )
            .into_any_element()
    }

    fn file_row(
        &self,
        index: usize,
        entry: &FileEntry,
        browser: &Browser,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let directory = entry.kind == FileKind::Directory;
        let name = if browser.searching() {
            &entry.path
        } else {
            &entry.name
        };
        let time = entry
            .modified_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map_or_else(
                || "—".into(),
                |time| {
                    time.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                },
            );
        let external = !directory && open_target(&entry.path) == OpenTarget::External;
        h_flex()
            .id(format!("file-row-{}", entry.path))
            .h_8()
            .flex_none()
            .w_full()
            .px_4()
            .gap_3()
            .cursor_pointer()
            .hover(|row| row.bg(theme.secondary))
            .when(browser.selected == index, |row| row.bg(theme.secondary))
            .child(
                h_flex()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .child(
                        if directory {
                            Symbol::Folder
                        } else {
                            Symbol::FileText
                        }
                        .tinted(theme.muted_foreground),
                    )
                    .child(div().text_sm().min_w_0().truncate().child(name.clone())),
            )
            .child(
                div()
                    .w(relative(0.16))
                    .max_w(px(83.))
                    .truncate()
                    .flex_none()
                    .text_right()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if directory {
                        "—".into()
                    } else {
                        file_size(entry.size)
                    }),
            )
            .child(
                div()
                    .w(relative(0.28))
                    .max_w(px(138.))
                    .truncate()
                    .flex_none()
                    .text_right()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(time),
            )
            .when(external, |row| {
                row.tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tr!("file-browser-external-tip"))
                        .build(window, cx)
                })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui_kit::MouseDownEvent, window, cx| {
                    if let Some(browser) = &mut this.files.browser {
                        browser.selected = index;
                        browser.input.read(cx).focus_handle(cx).focus(window, cx);
                        if event.click_count >= 2 {
                            this.open_selected_file(window, cx);
                        }
                        cx.notify();
                    }
                }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: FileKind) -> FileEntry {
        FileEntry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            kind,
            size: 0,
            modified_ms: None,
        }
    }

    #[test]
    fn segments_breadcrumbs_and_targets() {
        assert_eq!(
            breadcrumbs("研究室", "papers/levy"),
            [
                ("研究室".into(), "".into()),
                ("papers".into(), "papers".into()),
                ("levy".into(), "papers/levy".into()),
            ]
        );
        assert_eq!(breadcrumbs("lab", "").len(), 1);
    }

    #[test]
    fn breadcrumb_names_navigate_and_arrows_list_the_folder_on_the_left() {
        use BreadcrumbItem::{Arrow, File, Folder};

        let segments = breadcrumbs("lab", "papers/levy");
        let expected = [
            Folder {
                name: "lab",
                path: "",
            },
            Arrow {
                dir: "",
                selected: Some("papers"),
            },
            Folder {
                name: "papers",
                path: "papers",
            },
            Arrow {
                dir: "papers",
                selected: Some("papers/levy"),
            },
            Folder {
                name: "levy",
                path: "papers/levy",
            },
        ];
        assert_eq!(breadcrumb_items(&segments, None), expected);

        let items = breadcrumb_items(&segments, Some("papers/levy/README.md"));
        assert_eq!(items[..expected.len()], expected);
        assert_eq!(
            items[expected.len()..],
            [
                Arrow {
                    dir: "papers/levy",
                    selected: Some("papers/levy/README.md")
                },
                File("README.md"),
            ]
        );
    }

    #[test]
    fn breadcrumb_arrow_next_to_root_works_with_or_without_a_file() {
        use BreadcrumbItem::{Arrow, File, Folder};

        let segments = breadcrumbs("lab", "");
        assert_eq!(
            breadcrumb_items(&segments, None),
            [
                Folder {
                    name: "lab",
                    path: ""
                },
                Arrow {
                    dir: "",
                    selected: None
                },
            ]
        );
        assert_eq!(
            breadcrumb_items(&segments, Some("README.md")),
            [
                Folder {
                    name: "lab",
                    path: ""
                },
                Arrow {
                    dir: "",
                    selected: Some("README.md")
                },
                File("README.md"),
            ]
        );
    }

    #[test]
    fn tree_lists_only_folders() {
        let page = FilePage {
            entries: vec![
                entry("papers/levy", FileKind::Directory),
                entry("papers/book", FileKind::Directory),
                entry("papers/README.md", FileKind::File),
            ],
            truncated: false,
        };
        assert_eq!(
            folders(&page)
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["levy", "book"]
        );
    }

    #[test]
    fn filters_names_and_resolves_typed_directory_paths() {
        let files = [
            entry("docs/z_readme", FileKind::File),
            entry("docs/README", FileKind::File),
            entry("docs/readme.md", FileKind::File),
        ];
        let names = filter_entries(&files, "ReadMe")
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["README", "readme.md", "z_readme"]);
        assert_eq!(
            typed_location("docs", "nested/deeper/read"),
            ("docs/nested/deeper".into(), "read")
        );
        assert_eq!(
            typed_location("docs", "nested/"),
            ("docs/nested".into(), "")
        );
        assert_eq!(typed_location("docs", "read"), ("docs".into(), "read"));
        assert_eq!(
            typed_location("docs", "../read"),
            ("docs".into(), "../read")
        );
    }

    #[test]
    fn picker_keys_keep_selection_bounded_and_allow_text_editing() {
        assert_eq!(
            picker_key("down", 1, 2, true, false),
            Some(PickerKey::Select(1))
        );
        assert_eq!(
            picker_key("up", 1, 2, true, false),
            Some(PickerKey::Select(0))
        );
        assert_eq!(picker_key("up", 0, 0, true, false), None);
        assert_eq!(
            picker_key("enter", 0, 1, false, false),
            Some(PickerKey::Open)
        );
        assert_eq!(
            picker_key("right", 0, 1, false, true),
            Some(PickerKey::Open)
        );
        assert_eq!(picker_key("right", 0, 1, false, false), None);
        assert_eq!(picker_key("backspace", 0, 0, false, false), None);
        assert_eq!(
            picker_key("backspace", 0, 0, true, false),
            Some(PickerKey::Parent)
        );
        assert_eq!(
            picker_key("escape", 0, 0, false, false),
            Some(PickerKey::Close)
        );
    }
}
