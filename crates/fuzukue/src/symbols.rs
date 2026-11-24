use std::ops::Range;
use std::sync::Arc;

use gpui_kit::base::text::TextViewState;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, App, Context, Entity, MouseButton, SharedString, Window};
use fuzukue_proto::{
    ClientMsg, SymbolHit, SymbolLocation, SymbolLookup, SymbolResults, SYMBOL_PROTOCOL,
};
use fuzukue_view::TexMarkdown;

use super::{pill, Workbench};

#[derive(Default)]
pub(super) struct SymbolPane {
    request: u64,
    mode: Option<Mode>,
    hit: Option<SymbolHit>,
    results: Option<SymbolResults>,
}

#[derive(Clone, Copy)]
enum Mode {
    Jump,
    Pick,
    LeanJump,
    LeanDependencies,
    References,
}

impl SymbolPane {
    pub(super) fn is_open(&self) -> bool { self.mode.is_some() }

    pub(super) fn clear(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.mode = None;
        self.hit = None;
        self.results = None;
    }
}

impl Workbench {
    pub(super) fn open_card_lean(&mut self, name: String, cx: &mut Context<Self>) {
        self.request_symbol(SymbolLookup::Name {
            deck: self.deck.clone(), name, kind: fuzukue_proto::SymbolKind::Lean,
        }, Mode::LeanJump, cx);
    }

    pub(super) fn show_lean_dependencies(&mut self, path: String, line: usize, cx: &mut Context<Self>) {
        self.request_symbol(SymbolLookup::LeanDependencies { path, line }, Mode::LeanDependencies, cx);
    }

    fn request_symbol(&mut self, lookup: SymbolLookup, mode: Mode, cx: &mut Context<Self>) {
        if self.protocol < SYMBOL_PROTOCOL {
            return;
        }
        let request = self.symbols.request.wrapping_add(1);
        if self
            .out
            .send(ClientMsg::LookupSymbol { request, lookup })
            .is_ok()
        {
            self.symbols.request = request;
            self.symbols.mode = Some(mode);
            self.symbols.results = None;
            if !matches!(mode, Mode::References) {
                self.symbols.hit = None;
            }
            cx.notify();
        }
    }

    fn show_symbol_references(&mut self, hit: SymbolHit, cx: &mut Context<Self>) {
        let id = hit.id.clone();
        self.symbols.hit = Some(hit);
        self.request_symbol(SymbolLookup::References { id }, Mode::References, cx);
    }

    pub(super) fn on_symbol_results(
        &mut self,
        request: u64,
        mut results: SymbolResults,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.symbols.request != request {
            return;
        }
        if matches!(self.symbols.mode, Some(Mode::LeanJump)) {
            results.definitions.retain(|hit| hit.location.file.ends_with(".lean"));
            if results.definitions.len() == 1 && !results.more {
                self.open_symbol_location(&results.definitions[0].location, window, cx);
                self.symbols.clear();
                cx.notify();
                return;
            }
        }
        match self.symbols.mode {
            Some(mode @ (Mode::Jump | Mode::Pick))
                if results.definitions.len() == 1 && !results.more =>
            {
                let hit = results.definitions.pop().unwrap();
                if matches!(mode, Mode::Jump) {
                    let location = hit
                        .alternates
                        .iter()
                        .chain(std::iter::once(&hit.location))
                        .find(|location| super::wire_deck(&location.deck) == self.deck)
                        .unwrap_or(&hit.location);
                    self.open_symbol_location(location, window, cx);
                }
                self.show_symbol_references(hit, cx);
            }
            Some(_) => self.symbols.results = Some(results),
            None => {}
        }
        cx.notify();
    }

    fn open_symbol_location(
        &mut self,
        location: &SymbolLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = &location.card {
            if !std::path::Path::new(&location.deck).is_absolute() {
                self.open_paper_from_chat(location.deck.clone(), Some(id.clone()), cx);
                return;
            }
        }
        if std::path::Path::new(&location.file).is_absolute() {
            self.notice = Some(format!("{}:{}", location.file, location.line).into());
        } else {
            let end = location.end_line.unwrap_or(location.line);
            if location.file.ends_with(".lean") && self.protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL {
                self.open_lean_declaration(location.file.clone(), location.line, window, cx);
            } else {
                self.open_source_file(location.file.clone(), Some(location.line..=end), window, cx);
            }
        }
    }

    pub(super) fn symbol_menu(
        &self,
        row: gpui_kit::Stateful<gpui_kit::Div>,
        entity: &Entity<Self>,
        find: impl Fn(&Window, &App) -> Option<SymbolLookup> + 'static,
    ) -> gpui_kit::AnyElement {
        if self.protocol < SYMBOL_PROTOCOL {
            return row.into_any_element();
        }
        let find = std::rc::Rc::new(find);
        let on_click = find.clone();
        let workbench = entity.clone();
        let menu_target = entity.clone();
        row.capture_any_mouse_down(move |event, window, cx| {
            if event.button == MouseButton::Left
                && (event.modifiers.control || event.modifiers.platform)
            {
                if let Some(lookup) = on_click(window, cx) {
                    workbench.update(cx, |this, cx| this.request_symbol(lookup, Mode::Jump, cx));
                    cx.stop_propagation();
                }
            }
        })
        .context_menu(move |mut menu, window, cx| {
            if let Some(lookup) = find(window, cx) {
                for (label, mode) in [(tr!("symbols-go-to-definition"), Mode::Jump), (tr!("symbols-find-references"), Mode::Pick)]
                {
                    let entity = menu_target.clone();
                    let lookup = lookup.clone();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.request_symbol(lookup.clone(), mode, cx));
                    }));
                }
            }
            menu
        })
        .into_any_element()
    }

    pub(super) fn render_symbol_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.symbols.mode?;
        let theme = cx.theme().clone();
        let dependencies = matches!(self.symbols.mode, Some(Mode::LeanDependencies));
        let title = if dependencies {
            tr!("literate-lean-dependencies")
        } else if matches!(self.symbols.mode, Some(Mode::References)) {
            tr!("symbols-find-references")
        } else {
            tr!("symbols-possible-definitions")
        };
        let header = h_flex()
            .px(crate::pane_resize::PANE_INSET)
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(div().flex_1().min_w_0().text_sm().child(title))
            .children((self.pane_close_id() == Some("symbol-close"))
                .then(|| self.pane_close_button("symbol-close", cx)));
        let mut body = v_flex().min_w_0().gap_2().p_4().text_sm();
        if let Some(hit) = &self.symbols.hit {
            body = body
                .child(self.symbol_location_row(
                    "symbol-current",
                    &hit.qualified,
                    &hit.location,
                    cx,
                ))
                .children(self.symbol_copies("symbol-current-copies", &hit.alternates, cx));
        }
        if let Some(results) = &self.symbols.results {
            if results.definitions.is_empty() && results.references.is_empty() {
                body = body.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(tr!("symbols-no-matches")),
                );
            }
            if dependencies && !results.definitions.is_empty() {
                body = body.child(div().text_sm().child(tr!("literate-lean-uses")));
            }
            for (i, hit) in results.definitions.iter().enumerate() {
                let chosen = hit.clone();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(self.symbol_location_row(
                            &format!("symbol-definition-{i}"),
                            &hit.qualified,
                            &hit.location,
                            cx,
                        ))
                        .children(self.symbol_copies(
                            &format!("symbol-definition-copies-{i}"),
                            &hit.alternates,
                            cx,
                        ))
                        .child(
                            pill(
                                format!("symbol-find-references-{i}"),
                                tr!("symbols-find-references"),
                                false,
                                cx,
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.show_symbol_references(chosen.clone(), cx)
                                }),
                            ),
                        ),
                );
            }
            if dependencies && !results.references.is_empty() {
                body = body.child(div().text_sm().child(tr!("literate-lean-used-by")));
            }
            let mut last_deck = None;
            for (i, reference) in results.references.iter().enumerate() {
                let loc = &reference.location;
                if last_deck != Some(&loc.deck) {
                    last_deck = Some(&loc.deck);
                    body = body.child(
                        div()
                            .pt_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(loc.deck.clone()),
                    );
                }
                body = body
                    .child(self.symbol_location_row(&format!("symbol-reference-{i}"), "", loc, cx))
                    .children(self.symbol_copies(
                        &format!("symbol-reference-copies-{i}"),
                        &reference.alternates,
                        cx,
                    ));
            }
            if results.more {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr!("symbols-more-results-available")),
                );
            }
        } else {
            body = body.child(div().text_color(theme.muted_foreground).child(tr!("symbols-searching")));
        }
        Some(
            v_flex()
                .id("symbol-panel")
                .size_full()
                .min_w_0()
                .overflow_hidden()
                .child(header)
                .child(
                    div()
                        .id("symbol-results")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(body),
                ),
        )
    }

    fn symbol_copies(
        &self,
        id: &str,
        locations: &[SymbolLocation],
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        if locations.is_empty() {
            return None;
        }
        let locations = locations.to_vec();
        let entity = cx.entity();
        Some(
            Button::new(SharedString::from(id.to_owned()))
                .ghost()
                .small()
                .label(tr!("symbols-identical-copies", count = locations.len().to_string()))
                .dropdown_menu(move |mut menu, _, _| {
                    for location in &locations {
                        let entity = entity.clone();
                        let location = location.clone();
                        let label = format!("{}:{}", location.file, location.line);
                        menu =
                            menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.open_symbol_location(&location, window, cx)
                                });
                            }));
                    }
                    menu
                }),
        )
    }

    fn symbol_location_row(
        &self,
        id: &str,
        title: &str,
        location: &SymbolLocation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let path = format!("{}:{}", location.file, location.line);
        let location = location.clone();
        v_flex()
            .id(SharedString::from(id.to_owned()))
            .w_full()
            .min_w_0()
            .gap_0p5()
            .p_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|row| row.bg(theme.muted))
            .children(
                (!title.is_empty()).then(|| div().text_color(theme.link).child(title.to_owned())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(path),
            )
            .child(div().text_sm().child(location.snippet.clone()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_symbol_location(&location, window, cx)
            }))
    }
}

pub(super) fn text_lookup(
    deck: String,
    card: String,
    body: Arc<str>,
    display: Arc<TexMarkdown>,
    view: Entity<TextViewState>,
) -> impl Fn(&Window, &App) -> Option<SymbolLookup> {
    move |window, cx| {
        let view = view.read(cx);
        if view.rendered_text().source() != display.markdown {
            return None;
        }
        let range = view.source_range_at(window.mouse_position())?;
        let range = display.tex_range(range)?;
        Some(lookup_at(&deck, &card, &body, range))
    }
}

fn lookup_at(deck: &str, card: &str, body: &str, range: Range<usize>) -> SymbolLookup {
    let start = body.floor_char_boundary(range.start.saturating_sub(512));
    let end = body.ceil_char_boundary((range.end + 512).min(body.len()));
    SymbolLookup::At {
        deck: deck.into(),
        card: card.into(),
        text: body[start..end].into(),
        start: range.start - start,
        end: range.end - start,
    }
}
