mod ledger;

use gpui_kit::base::Disableable as _;
use std::collections::BTreeSet;
use std::time::Duration;

use gpui_kit::component::{
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{
    div, prelude::*, px, Context, Entity, Focusable as _, FontWeight, HighlightStyle, StyledText,
    Task, Window,
};
use fuzukue_proto::{
    ClientMsg, LibraryHit, LibraryKind, LibraryLocation, LibraryReferencePart, LibrarySort,
    LIBRARY_DECKS_PROTOCOL, LIBRARY_PAGE_SIZE, LIBRARY_PROTOCOL, MEMOS_PROTOCOL,
    REVIEW_REPORTS_PROTOCOL,
};

use super::{pill, RightPane, Workbench};

const KINDS: [LibraryKind; 7] = [
    LibraryKind::Reference,
    LibraryKind::Card,
    LibraryKind::Thread,
    LibraryKind::Comment,
    LibraryKind::Memo,
    LibraryKind::ReviewReport,
    LibraryKind::Deck,
];

#[derive(Default)]
pub(super) struct LibraryPane {
    input: Option<Entity<InputState>>,
    query: String,
    kinds: Vec<LibraryKind>,
    deck: Option<String>,
    sort: LibrarySort,
    request: u64,
    debounce: Option<Task<()>>,
    hits: Vec<LibraryHit>,
    pub(super) decks: Vec<String>,
    more: bool,
    pending: bool,
    error: bool,
    scroll: gpui_kit::ScrollHandle,
    ledger: ledger::Pane,
    memo_input: Option<Entity<TextareaState>>,
    memo_request: u64,
    memo_pending: Option<MemoAction>,
    memo_error: Option<&'static str>,
    pub(super) move_target: Option<String>,
}

enum MemoAction {
    Compose(String),
    Move,
    Chat { session: String, item: String },
}

fn kind_label(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Reference => tr!("library-reference"),
        LibraryKind::Card => tr!("library-card"),
        LibraryKind::Thread => tr!("library-thread"),
        LibraryKind::Comment => tr!("library-comment"),
        LibraryKind::Memo => tr!("library-memo"),
        LibraryKind::ReviewReport => tr!("library-review-report"),
        LibraryKind::Deck => tr!("library-deck"),
    }
}

impl LibraryPane {
    pub(super) fn disconnect(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.debounce = None;
        self.memo_pending = None;
        self.pending = false;
        self.error = true;
        self.ledger.disconnect();
    }

    pub(super) fn clear(&mut self) {
        self.ledger.disconnect();
        let ledger_request = self.ledger.request;
        *self = Self {
            request: self.request.wrapping_add(1),
            memo_request: self.memo_request.wrapping_add(1),
            ..Self::default()
        };
        self.ledger.request = ledger_request;
    }
}

impl Workbench {
    pub(super) fn toggle_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.right == RightPane::Library && !self.settings_open {
            self.close_right_pane(cx);
            return;
        }
        if self.protocol < LIBRARY_PROTOCOL {
            return;
        }
        if self.protocol >= MEMOS_PROTOCOL && self.library.memo_input.is_none() {
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 5)
                    .placeholder(tr!("library-write-memo"))
            });
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true,
                        shift: false,
                    } => this.save_memo_draft(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            )
            .detach();
            self.library.memo_input = Some(input);
        }
        let input = self.library.input.get_or_insert_with(|| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("library-search")));
            cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.library.query = input.read(cx).value().trim().to_owned();
                    this.reset_library_search(true, cx);
                }
            })
            .detach();
            input
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.close_palette(window, cx);
        self.settings_open = false;
        self.pending_paper_open = None;
        self.right = RightPane::Library;
        if self.ledger_open() {
            self.open_ledger(cx);
        } else {
            self.reset_library_search(false, cx);
        }
    }

    pub(super) fn ledger_open(&self) -> bool {
        self.protocol >= fuzukue_proto::LEDGER_PROTOCOL && self.library.ledger.open
    }

    pub(super) fn reset_library_search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let pane = &mut self.library;
        pane.request = pane.request.wrapping_add(1);
        pane.debounce = None;
        pane.hits.clear();
        pane.more = false;
        pane.error = self.protocol < LIBRARY_PROTOCOL;
        let recent = self.protocol >= MEMOS_PROTOCOL
            && (pane.kinds.is_empty() || pane.kinds.contains(&LibraryKind::Memo));
        pane.pending = (!pane.query.is_empty() || recent) && !pane.error;
        pane.scroll.set_offset(gpui_kit::point(px(0.), px(0.)));
        if pane.pending {
            if debounce {
                pane.debounce = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(200))
                        .await;
                    let _ = this.update(cx, |this, cx| this.send_library_search(cx));
                }));
            } else {
                self.send_library_search(cx);
            }
        }
        cx.notify();
    }

    fn send_library_search(&mut self, cx: &mut Context<Self>) {
        let pane = &mut self.library;
        pane.request = pane.request.wrapping_add(1);
        pane.pending = self
            .out
            .send(ClientMsg::SearchLibrary {
                request: pane.request,
                query: pane.query.clone(),
                kinds: pane.kinds.clone(),
                deck: pane.deck.clone(),
                sort: pane.sort,
                offset: pane.hits.len(),
                limit: LIBRARY_PAGE_SIZE,
            })
            .is_ok();
        pane.error = !pane.pending;
        cx.notify();
    }

    pub(super) fn on_library_results(
        &mut self,
        request: u64,
        hits: Vec<LibraryHit>,
        more: bool,
        decks: Vec<String>,
    ) {
        let pane = &mut self.library;
        if pane.request == request {
            pane.hits.extend(hits);
            pane.more = more;
            pane.decks = decks;
            pane.pending = false;
            pane.error = false;
        }
    }

    fn open_library_hit(&mut self, hit: &LibraryHit, window: &mut Window, cx: &mut Context<Self>) {
        if let LibraryLocation::Memo { path, .. } = &hit.location {
            self.open_memo_file(path.clone(), window, cx);
            return;
        }
        self.close_right_pane(cx);
        match &hit.location {
            LibraryLocation::Memo { .. } => unreachable!(),
            LibraryLocation::Deck => {
                if let Some(deck) = &hit.deck {
                    self.open_attachment(deck, cx);
                }
            }
            LibraryLocation::Card { card } => {
                if let Some(deck) = &hit.deck {
                    self.open_paper_from_chat(deck.clone(), Some(card.clone()), cx);
                }
            }
            LibraryLocation::Reference { path, part, .. } => match part {
                LibraryReferencePart::Page { page } => {
                    self.open_source(path.clone(), *page, window, cx)
                }
                LibraryReferencePart::Lines {
                    lines: [start, end],
                } => {
                    self.open_source_file(path.clone(), Some(*start..=*end), window, cx);
                }
            },
            LibraryLocation::Thread { thread, item, .. } => {
                self.symbols.clear();
                self.select_thread(thread.clone(), cx);
                if let Some(item) = item {
                    self.chat(thread, cx).reveal_message(item);
                }
            }
            LibraryLocation::ReviewReport { submission, report, point } => {
                if let Some(deck) = &hit.deck {
                    self.open_paper_from_chat(deck.clone(), None, cx);
                    self.open_reviews(deck.clone(), Some((submission.clone(), *report, point.clone())), cx);
                }
            }
            LibraryLocation::Comment { card, .. } => {
                if let Some(deck) = &hit.deck {
                    self.open_paper_target(deck.clone(), Some(card.clone()), true, cx);
                }
            }
        }
    }

    fn save_memo_draft(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.library.memo_input else {
            return;
        };
        let text = input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.submit_memo(
            MemoAction::Compose(text.clone()),
            |request| ClientMsg::AddMemo {
                request,
                text,
                from: None,
            },
            cx,
        );
    }

    pub(super) fn save_chat_memo(
        &mut self,
        session: String,
        item: String,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let from = Some(format!("thread:{session}#{item}"));
        self.submit_memo(
            MemoAction::Chat { session, item },
            |request| ClientMsg::AddMemo {
                request,
                text,
                from,
            },
            cx,
        );
    }

    pub(super) fn move_memo(&mut self, id: String, deck: String, cx: &mut Context<Self>) {
        self.submit_memo(
            MemoAction::Move,
            |request| ClientMsg::MoveMemo { request, id, deck },
            cx,
        );
    }

    fn submit_memo(
        &mut self,
        action: MemoAction,
        message: impl FnOnce(u64) -> ClientMsg,
        cx: &mut Context<Self>,
    ) {
        if self.protocol < MEMOS_PROTOCOL || self.library.memo_pending.is_some() {
            return;
        }
        self.library.memo_request = self.library.memo_request.wrapping_add(1);
        self.library.memo_error = None;
        if self.out.send(message(self.library.memo_request)).is_ok() {
            self.library.memo_pending = Some(action);
        } else {
            self.memo_failed(action, cx);
        }
        cx.notify();
    }

    fn memo_failed(&mut self, action: MemoAction, cx: &mut Context<Self>) {
        match action {
            MemoAction::Chat { session, item } => self.chat_memo_result(&session, &item, false, cx),
            MemoAction::Move => self.library.memo_error = Some(tr!("library-memo-move-failed")),
            MemoAction::Compose(_) => {
                self.library.memo_error = Some(tr!("library-memo-save-failed"))
            }
        }
    }

    pub(super) fn on_memo_result(
        &mut self,
        request: u64,
        error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if request != self.library.memo_request {
            return;
        }
        let Some(action) = self.library.memo_pending.take() else {
            return;
        };
        if error.is_some() {
            self.memo_failed(action, cx);
            return;
        }
        match action {
            MemoAction::Chat { session, item } => self.chat_memo_result(&session, &item, true, cx),
            MemoAction::Move => {}
            MemoAction::Compose(text) => {
                if let Some(input) = &self.library.memo_input {
                    if input.read(cx).value().as_ref() == text {
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                    }
                }
                if let Some(input) = &self.library.input {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
                self.library.query.clear();
                self.library.kinds.clear();
                self.library.deck = None;
            }
        }
        if self.right == RightPane::Library {
            self.reset_library_search(false, cx);
        }
    }

    pub(super) fn render_library(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        if self.ledger_open() {
            return self.render_ledger(cx).into_any_element();
        }
        let pane = &self.library;
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let mut decks: BTreeSet<_> = pane.decks.iter().cloned().collect();
        decks.extend(self.tabs.iter().flat_map(|tab| {
            Self::tab_attachment(tab)
                .into_iter()
                .chain(
                    tab.papers
                        .iter()
                        .flat_map(|paper| paper.decks.iter().map(String::as_str)),
                )
                .map(|deck| {
                    if deck.is_empty() {
                        ".".into()
                    } else {
                        deck.to_owned()
                    }
                })
        }));
        let decks: Vec<_> = std::iter::once((None, tr!("library-all-decks").to_owned()))
            .chain(
                decks
                    .into_iter()
                    .map(|deck| (Some(deck.clone()), self.library_deck_title(&deck))),
            )
            .collect();
        let deck_label = pane.deck.as_ref().map_or_else(
            || tr!("library-all-decks").to_owned(),
            |deck| self.library_deck_title(deck),
        );
        let deck_menu = Button::new("library-deck")
            .small()
            .outline()
            .label(deck_label)
            .dropdown_menu(move |mut menu, _, _| {
                for (deck, label) in &decks {
                    let deck = deck.clone();
                    let entity = entity.clone();
                    menu =
                        menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.library.deck = deck.clone();
                                this.reset_library_search(false, cx);
                            });
                        }));
                }
                menu
            });
        let filters = h_flex()
            .gap_2()
            .flex_wrap()
            .children(
                KINDS
                    .into_iter()
                    .enumerate()
                    .filter(|(_, kind)| match kind {
                        LibraryKind::Memo => self.protocol >= MEMOS_PROTOCOL,
                        LibraryKind::ReviewReport => self.protocol >= REVIEW_REPORTS_PROTOCOL,
                        LibraryKind::Deck => self.protocol >= LIBRARY_DECKS_PROTOCOL,
                        _ => true,
                    })
                    .map(|(i, kind)| {
                        pill(
                            format!("library-kind-{i}"),
                            kind_label(kind),
                            pane.kinds.contains(&kind),
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.library.kinds.contains(&kind) {
                                this.library.kinds.retain(|selected| *selected != kind);
                            } else {
                                this.library.kinds.push(kind);
                            }
                            this.reset_library_search(false, cx);
                        }))
                    }),
            )
            .child(deck_menu)
            .children(
                [
                    (
                        LibrarySort::Relevance,
                        "library-relevance",
                        tr!("library-relevance"),
                    ),
                    (LibrarySort::Newest, "library-newest", tr!("library-newest")),
                ]
                .into_iter()
                .map(|(sort, id, label)| {
                    pill(id, label, pane.sort == sort, cx).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.library.sort = sort;
                            this.reset_library_search(false, cx);
                        },
                    ))
                }),
            );

        let status = if pane.error {
            Some(tr!("library-unavailable"))
        } else if pane.pending {
            Some(tr!("library-searching"))
        } else if pane.hits.is_empty() && !pane.query.is_empty() {
            Some(tr!("library-empty"))
        } else {
            None
        };
        let results = v_flex()
            .w_full()
            .max_w(px(860.))
            .mx_auto()
            .px_5()
            .children(
                pane.hits
                    .iter()
                    .enumerate()
                    .map(|(i, hit)| self.library_row(i, hit, cx)),
            )
            .children(status.map(|status| {
                div()
                    .py_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(status)
            }))
            .children((pane.more && !pane.pending).then(|| {
                div().py_3().child(
                    pill("library-more", tr!("library-more"), false, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.send_library_search(cx))),
                )
            }));
        v_flex()
            .id("library-pane")
            .size_full()
            .min_w_0()
            .child(
                div()
                    .w_full()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(860.))
                            .mx_auto()
                            .px(crate::pane_resize::PANE_INSET)
                            .py_5()
                            .gap_3()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(tr!("library-title")),
                                    )
                                    .child(self.pane_close_button("library-close", cx)),
                            )
                            .children((self.protocol >= fuzukue_proto::LEDGER_PROTOCOL).then(|| {
                                pill("library-ledger-tab", tr!("ledger-tab"), false, cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.open_ledger(cx)))
                            }))
                            .children(pane.input.as_ref().map(|input| {
                                div()
                                    .id("library-query")
                                    .child(Input::new(input).aria_label(tr!("library-search")))
                            }))
                            .children(
                                pane.memo_input
                                    .as_ref()
                                    .filter(|_| self.protocol >= MEMOS_PROTOCOL)
                                    .map(|input| {
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .items_end()
                                                    .child(
                                                        div()
                                                            .id("library-memo-input")
                                                            .flex_1()
                                                            .min_w_0()
                                                            .child(
                                                                Textarea::new(input).aria_label(
                                                                    tr!("library-write-memo"),
                                                                ),
                                                            ),
                                                    )
                                                    .child(
                                                        Button::new("library-save-memo")
                                                            .small()
                                                            .outline()
                                                            .label(tr!("library-save-memo"))
                                                            .tooltip(tr!(
                                                                "library-save-memo-shortcut"
                                                            ))
                                                            .disabled(
                                                                pane.memo_pending.is_some()
                                                                    || input
                                                                        .read(cx)
                                                                        .value()
                                                                        .trim()
                                                                        .is_empty(),
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.save_memo_draft(cx)
                                                                },
                                                            )),
                                                    ),
                                            )
                                            .children(pane.memo_error.map(|error| {
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.danger)
                                                    .child(error)
                                            }))
                                    }),
                            )
                            .child(filters),
                    ),
            )
            .child(
                div()
                    .id("library-results")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&pane.scroll)
                    .overflow_y_scrollbar()
                    .child(results),
            ).into_any_element()
    }

    fn library_deck_title(&self, deck: &str) -> String {
        if deck == "memos" {
            tr!("library-memo").to_owned()
        } else {
            self.deck_title(deck)
        }
    }

    fn library_row(&self, index: usize, hit: &LibraryHit, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let date = chrono::DateTime::from_timestamp_millis(hit.updated)
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default();
        let mut last = 0;
        let highlights = hit.highlights.iter().filter_map(|&[start, end]| {
            if start < last || start >= end || hit.snippet.get(start..end).is_none() {
                return None;
            }
            last = end;
            Some((
                start..end,
                HighlightStyle {
                    color: Some(theme.link),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            ))
        });
        let snippet = StyledText::new(hit.snippet.clone()).with_highlights(highlights);
        let kind = kind_label(hit.location.kind());
        let deck = hit
            .deck
            .as_ref()
            .filter(|deck| deck.as_str() != "memos")
            .map(|deck| self.deck_title(deck));
        let move_button = if let LibraryLocation::Memo { card, path } = &hit.location {
            let card = card.clone();
            Some(
                Button::new(format!("library-move-memo-{index}"))
                    .small()
                    .outline()
                    .label(tr!("library-move-memo"))
                    .disabled(
                        self.library.memo_pending.is_some() || self.source.has_open_path(path),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_memo_destination(card.clone(), window, cx);
                    })),
            )
        } else {
            None
        };
        let remove = match &hit.location {
            LibraryLocation::Memo { card, path } => self.trash_button(
                format!("library-delete-memo-{index}"),
                fuzukue_proto::TrashTarget::Memo { id: card.clone() }, hit.title.clone(), cx,
            ).map(|button| button.disabled(self.source.has_open_path(path))),
            _ => None,
        };
        let hit = hit.clone();
        v_flex()
            .id(format!("library-hit-{index}"))
            .gap_2()
            .px_3()
            .py_3()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .cursor_pointer()
            .hover(|row| row.bg(theme.muted))
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.secondary)
                            .text_xs()
                            .child(kind),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(hit.title.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(date),
                    )
                    .children(move_button)
                    .children(remove),
            )
            .children(deck.map(|deck| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(deck)
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(snippet),
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_library_hit(&hit, window, cx)),
            )
    }
}
