use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::Path,
    time::Duration,
};

use futures::future::{join, join_all, select};
use gpui_kit::base::StyledExt as _;
use gpui_kit::{
    component::{h_flex, scroll::ScrollableElement as _, text::TextView, v_flex, ActiveTheme as _},
    div,
    prelude::*,
    px, App, Context, MouseButton, SharedString, Task, Window,
};
use fuzukue_proto::{ClientMsg, LspOutcome, LspQuery};
use fuzukue_view::lean::{self, Declaration, Item, Page, Step};

use super::{lsp, source_button, SourceTab, Workbench};

#[derive(Default)]
pub(super) struct Reading {
    page: Page,
    selected: Option<usize>,
    card_request: u64,
    cards: HashMap<String, fuzukue_proto::LeanReadingCard>,
    implicit: HashSet<usize>,
    proof: HashSet<usize>,
    goals: HashMap<(usize, usize), Goals>,
    expanded: HashSet<(usize, usize)>,
    pending: HashMap<(usize, usize), Task<()>>,
}

#[derive(Default)]
struct Goals {
    before: Option<String>,
    after: Option<String>,
}

impl Goals {
    fn diff(&self) -> String {
        let (Some(before), Some(after)) = (&self.before, &self.after) else {
            return String::new();
        };
        if before == after {
            return String::new();
        }
        similar::TextDiff::from_lines(before, after)
            .iter_all_changes()
            .map(|change| {
                let prefix = match change.tag() {
                    similar::ChangeTag::Delete => "- ",
                    similar::ChangeTag::Insert => "+ ",
                    similar::ChangeTag::Equal => "  ",
                };
                format!("{prefix}{}\n", change.value().trim_end_matches('\n'))
            })
            .collect()
    }
}

fn goal_text(outcome: LspOutcome) -> Option<String> {
    let LspOutcome::Value { value } = outcome else {
        return None;
    };
    let value = value.get("goal").unwrap_or(&value);
    if let Some(goals) = value.get("goals").and_then(|goals| goals.as_array()) {
        return Some(
            goals
                .iter()
                .filter_map(|goal| goal.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    value
        .get("rendered")
        .or_else(|| value.get("goal"))
        .unwrap_or(&value)
        .as_str()
        .map(str::to_owned)
}

type GoalReceiver = futures::channel::oneshot::Receiver<LspOutcome>;

fn query_step(server: &lsp::Remote, step: &Step) -> (Option<GoalReceiver>, Option<GoalReceiver>) {
    (
        server.request(LspQuery::LeanGoal {
            position: step.before,
        }),
        server.request(LspQuery::LeanGoal {
            position: step.after,
        }),
    )
}

async fn receive_step(
    (before, after): (Option<GoalReceiver>, Option<GoalReceiver>),
    timeout: impl Future<Output = ()>,
) -> Goals {
    async fn receive(request: Option<GoalReceiver>) -> Option<String> {
        goal_text(request?.await.ok()?)
    }
    let mut goals = Goals::default();
    let queries = join(
        async {
            goals.before = receive(before).await;
        },
        async {
            goals.after = receive(after).await;
        },
    );
    let result = select(Box::pin(queries), Box::pin(timeout)).await;
    drop(result);
    goals
}

fn status(state: Option<&str>) -> SharedString {
    match state {
        Some("proved") => tr!("workbench-proved").into(),
        Some("defined") => tr!("workbench-declaration-checked").into(),
        Some("sorry") => tr!("workbench-contains-sorry").into(),
        Some(state) if state.starts_with("axioms:") => tr!(
            "literate-lean-axioms",
            axioms = state.trim_start_matches("axioms:").trim().to_owned()
        )
        .into(),
        Some(state) => state.to_owned().into(),
        None => tr!("workbench-unchecked").into(),
    }
}

fn markup(id: String, prose: String, cx: &App) -> TextView {
    crate::paper::paper_text(TextView::markdown(SharedString::from(id), prose), cx)
}

fn code(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
    div()
        .min_w_0()
        .whitespace_normal()
        .text_sm()
        .font(crate::appearance::code_font(cx))
        .child(text.into())
}

fn code_lines(text: &str, cx: &App) -> gpui_kit::Div {
    v_flex()
        .min_w_0()
        .children(text.lines().map(|line| code(line.to_owned(), cx)))
}

impl Workbench {
    pub(crate) fn open_lean_declaration(
        &mut self,
        path: String,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_source_file(path, None, window, cx);
        if let Some(id) = self.source.active {
            self.leave_source_node(id, window, cx);
            self.show_lean_reading(id, Some(line), cx);
        }
    }

    pub(super) fn show_lean_reading(
        &mut self,
        id: u64,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let request = self.source.id();
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        let Some(doc) = &tab.document else {
            tab.reading_line = line;
            return;
        };
        let page = lean::parse(&doc.local_file(cx));
        let selected = line.and_then(|line| {
            page.items.iter().rposition(
                |item| matches!(item, Item::Declaration(d) if (d.position.line as usize) < line),
            )
        });
        let declarations = page
            .declarations()
            .map(|d| (d.name.clone(), d.card.clone()))
            .collect();
        tab.reading = Some(Reading {
            page,
            selected,
            card_request: request,
            ..Reading::default()
        });
        let _ = self.out.send(ClientMsg::GetLeanReadingCards {
            request,
            deck: self.deck.clone(),
            declarations,
        });
        cx.notify();
    }

    pub(super) fn toggle_lean_reading(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.source.tab_mut(id) {
            if let Some(read) = tab.reading.take() {
                let line = read
                    .selected
                    .and_then(|index| match &read.page.items[index] {
                        Item::Declaration(decl) => Some(decl.position.line as usize + 1),
                        Item::Prose(_) => None,
                    });
                if let Some(line) = line {
                    let path = tab.path.clone();
                    self.open_source_file(path, Some(line..=line), window, cx);
                } else {
                    self.select_source(id, window, cx);
                }
                return;
            }
        }
        self.show_lean_reading(id, None, cx);
    }

    pub(crate) fn receive_lean_cards(
        &mut self,
        request: u64,
        cards: Vec<fuzukue_proto::LeanReadingCard>,
    ) {
        if let Some(read) = self
            .source
            .tabs
            .iter_mut()
            .filter_map(|tab| tab.reading.as_mut())
            .find(|read| read.card_request == request)
        {
            read.cards = cards
                .into_iter()
                .map(|card| (card.declaration.clone(), card))
                .collect();
        }
    }

    fn card_claim(&self, read: &Reading, decl: &Declaration) -> Option<CardClaim> {
        if let Some(card) = read
            .cards
            .get(&decl.name)
            .filter(|card| card.deck != self.deck)
        {
            return Some((
                card.deck.clone(),
                card.id.clone(),
                card.title.clone(),
                fuzukue_view::read_card_tex(&card.statement, None, &[]).markdown,
            ));
        }
        let card = decl
            .card
            .as_ref()
            .and_then(|id| self.cards.iter().find(|card| &card.id == id))
            .or_else(|| {
                self.cards
                    .iter()
                    .find(|card| card.lean.contains(&decl.name))
            })?;
        let statement = fuzukue_view::card_section_ranges(&card.body)
            .into_iter()
            .filter(|(heading, _)| crate::paper::STATEMENT_HEADINGS.contains(heading))
            .map(|(_, range)| &card.body[range])
            .collect::<Vec<_>>()
            .join("\n\n");
        let body = self.display_card_tex(&statement).markdown;
        Some((self.deck.clone(), card.id.clone(), card.title.clone(), body))
    }

    pub(super) fn render_lean_reading(
        &self,
        tab: &SourceTab,
        read: &Reading,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = tab.id;
        let title = Path::new(&tab.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&tab.path);
        let toolbar = h_flex()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .flex_1()
                    .font_bold()
                    .truncate()
                    .child(title.to_owned()),
            )
            .child(
                source_button(
                    "literate-lean-check",
                    tr!("literate-lean-check"),
                    !self.lean_running,
                    cx,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        if !this.lean_running {
                            let _ = this.out.send(ClientMsg::CheckLean);
                        }
                    }),
                ),
            )
            .child(
                source_button(
                    "literate-lean-export",
                    tr!("literate-lean-export"),
                    true,
                    cx,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.export_lean(id, window, cx)),
                ),
            );
        let outline = v_flex()
            .id("literate-lean-outline")
            .w(px(210.))
            .flex_none()
            .h_full()
            .overflow_y_scrollbar()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                source_button("literate-lean-all", tr!("literate-lean-whole"), true, cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(read) =
                                this.source.tab_mut(id).and_then(|tab| tab.reading.as_mut())
                            {
                                read.selected = None;
                                cx.notify();
                            }
                        }),
                    ),
            )
            .children(
                read.page
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        let Item::Declaration(decl) = item else {
                            return None;
                        };
                        Some(
                            v_flex()
                                .id(SharedString::from(format!("literate-lean-outline-{index}")))
                                .rounded_md()
                                .px_2()
                                .py_1()
                                .cursor_pointer()
                                .hover(|row| row.bg(theme.secondary))
                                .when(read.selected == Some(index), |row| row.bg(theme.accent))
                                .child(div().text_sm().truncate().child(decl.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(decl.doc.lines().next().unwrap_or("").to_owned()),
                                )
                                .child(
                                    div().text_xs().text_color(theme.muted_foreground).child(
                                        status(self.lean.get(&decl.name).map(String::as_str)),
                                    ),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(read) =
                                        this.source.tab_mut(id).and_then(|tab| tab.reading.as_mut())
                                    {
                                        read.selected = Some(index);
                                        cx.notify();
                                    }
                                })),
                        )
                    }),
            );
        let page = v_flex()
            .id(SharedString::from(format!(
                "literate-lean-page-{id}-{:?}",
                read.selected
            )))
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scrollbar()
            .gap_4()
            .p_4()
            .when(read.page.incomplete, |body| {
                body.child(
                    div()
                        .text_color(theme.danger)
                        .text_sm()
                        .child(tr!("literate-lean-incomplete")),
                )
            })
            .children(
                read.page
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| read.selected.is_none_or(|selected| *i == selected))
                    .map(|(index, item)| match item {
                        Item::Prose(prose) => {
                            markup(format!("lean-prose-{id}-{index}"), prose.clone(), cx)
                                .into_any_element()
                        }
                        Item::Declaration(decl) => self
                            .render_lean_decl(tab, read, index, decl, cx)
                            .into_any_element(),
                    }),
            );
        v_flex()
            .id("literate-lean-reading")
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when(self.source.outline_open, |row| row.child(outline))
                    .child(page),
            )
    }

    fn render_lean_decl(
        &self,
        tab: &SourceTab,
        read: &Reading,
        index: usize,
        decl: &Declaration,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = tab.id;
        let path = tab.path.clone();
        let line = decl.position.line as usize + 1;
        let has_folded =
            !decl.context.is_empty() || decl.binders.iter().any(|binder| binder.implicit);
        let statement = v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .when(has_folded, |column| {
                column.child(
                    source_button(
                        format!("literate-lean-implicit-{index}"),
                        tr!("literate-lean-implicit"),
                        true,
                        cx,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(read) =
                                this.source.tab_mut(id).and_then(|tab| tab.reading.as_mut())
                            {
                                if !read.implicit.remove(&index) {
                                    read.implicit.insert(index);
                                }
                                cx.notify();
                            }
                        }),
                    ),
                )
            })
            .when(read.implicit.contains(&index), |column| {
                column.children(decl.context.iter().map(|ctx| code_lines(ctx, cx)))
            })
            .children(
                decl.binders
                    .iter()
                    .filter(|b| !b.implicit || read.implicit.contains(&index))
                    .map(|b| code(b.text.clone(), cx)),
            )
            .when(!decl.conclusion.is_empty(), |column| {
                column.child(code_lines(&decl.conclusion, cx).font_bold())
            })
            .when(decl.conclusion.is_empty(), |column| {
                column.child(code(decl.statement.clone(), cx))
            });
        let paired = self.card_claim(read, decl);
        v_flex()
            .id(SharedString::from(format!(
                "literate-lean-declaration-{index}"
            )))
            .gap_3()
            .p_3()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_bold()
                            .child(decl.name.clone()),
                    )
                    .child(
                        div()
                            .rounded_md()
                            .bg(theme.secondary)
                            .px_2()
                            .py_1()
                            .text_xs()
                            .child(status(self.lean.get(&decl.name).map(String::as_str))),
                    ),
            )
            .when(!decl.doc.is_empty(), |body| {
                body.child(markup(
                    format!("lean-doc-{id}-{index}"),
                    decl.doc.clone(),
                    cx,
                ))
            })
            .children(
                self.lean_doc_findings
                    .iter()
                    .filter(|point| point.path == tab.path && point.declaration == decl.name)
                    .map(|point| {
                        div().text_sm().text_color(theme.danger).child(
                            if point.code == "missing_docstring" {
                                tr!("literate-lean-missing-doc")
                            } else {
                                tr!("literate-lean-stale-doc")
                            },
                        )
                    }),
            )
            .child(h_flex().items_start().gap_3().child(statement).when_some(
                paired,
                |row, (deck, card, title, body)| {
                    row.child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .pl_3()
                            .border_l_1()
                            .border_color(theme.border)
                            .child(
                                source_button(format!("lean-card-{index}"), &title, true, cx)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.open_paper_from_chat(
                                                deck.clone(),
                                                Some(card.clone()),
                                                cx,
                                            );
                                        }),
                                    ),
                            )
                            .child(markup(format!("lean-card-claim-{id}-{index}"), body, cx)),
                    )
                },
            ))
            .when(!decl.proof.is_empty(), |block| {
                block
                    .child(
                        source_button(
                            format!("literate-lean-proof-{index}"),
                            tr!("literate-lean-proof"),
                            true,
                            cx,
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if let Some(read) =
                                    this.source.tab_mut(id).and_then(|tab| tab.reading.as_mut())
                                {
                                    if !read.proof.remove(&index) {
                                        read.proof.insert(index);
                                    }
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .when(read.proof.contains(&index), |body| {
                        body.child(code_lines(&decl.proof, cx))
                    })
            })
            .children(decl.steps.iter().enumerate().map(|(i, step)| {
                let key = (index, i);
                let expanded = read.expanded.contains(&key);
                v_flex()
                    .gap_1()
                    .child(
                        source_button(
                            format!("literate-lean-step-{index}-{i}"),
                            &step.heading,
                            true,
                            cx,
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.toggle_lean_step(id, key, cx);
                            }),
                        ),
                    )
                    .when(expanded, |body| {
                        body.child(code_lines(&step.code, cx)).child(
                            if let Some(goals) = read.goals.get(&key) {
                                v_flex()
                                    .gap_1()
                                    .child(goal_pair(goals, cx))
                                    .when(!goals.diff().is_empty(), |row| {
                                        row.child(code_lines(&goals.diff(), cx))
                                    })
                                    .into_any_element()
                            } else {
                                div()
                                    .text_xs()
                                    .child(tr!("literate-lean-goal-loading"))
                                    .into_any_element()
                            },
                        )
                    })
            }))
            .child(
                source_button(
                    format!("literate-lean-dependencies-{index}"),
                    tr!("literate-lean-dependencies"),
                    true,
                    cx,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.show_lean_dependencies(path.clone(), line, cx);
                    }),
                ),
            )
    }

    fn toggle_lean_step(&mut self, id: u64, key: (usize, usize), cx: &mut Context<Self>) {
        let Some(tab) = self.source.tab_mut(id) else {
            return;
        };
        let (Some(read), Some(doc)) = (&mut tab.reading, &tab.document) else {
            return;
        };
        if read.expanded.remove(&key) {
            read.pending.remove(&key);
            cx.notify();
            return;
        }
        let Some(Item::Declaration(decl)) = read.page.items.get(key.0) else {
            return;
        };
        let Some(step) = decl.steps.get(key.1) else {
            return;
        };
        read.expanded.insert(key);
        let requests = query_step(&doc.lsp, step);
        let timer = cx.background_executor().timer(Duration::from_secs(25));
        read.pending.insert(
            key,
            cx.spawn(async move |this, cx| {
                let goals = receive_step(requests, timer).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(read) = this.source.tab_mut(id).and_then(|tab| tab.reading.as_mut())
                    {
                        read.goals.insert(key, goals);
                        read.pending.remove(&key);
                        cx.notify();
                    }
                });
            }),
        );
        cx.notify();
    }
}

fn goal_pair(goals: &Goals, cx: &App) -> impl IntoElement {
    h_flex().items_start().gap_3().children(
        [
            (tr!("literate-lean-before"), &goals.before),
            (tr!("literate-lean-after"), &goals.after),
        ]
        .into_iter()
        .map(|(label, goal)| {
            v_flex()
                .flex_1()
                .min_w_0()
                .bg(cx.theme().secondary)
                .rounded_md()
                .p_2()
                .child(div().text_xs().child(label))
                .child(code_lines(goal_label(goal), cx))
        }),
    )
}

fn goal_label(goal: &Option<String>) -> &str {
    match goal.as_deref() {
        None => tr!("literate-lean-goal-unavailable"),
        Some("") => tr!("literate-lean-no-goals"),
        Some(text) => text,
    }
}

impl Workbench {
    fn export_lean(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.source.active_tab().filter(|tab| tab.id == id) else {
            return;
        };
        let (Some(read), Some(doc)) = (&tab.reading, &tab.document) else {
            return;
        };
        let page = read.page.clone();
        let selected = read.selected;
        let remote = doc.lsp.clone();
        let title = Path::new(&tab.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Lean")
            .to_owned();
        let filename = format!("{title}.html");
        let directory = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let picker = cx.prompt_for_new_path(&directory, Some(&filename));
        let details: HashMap<_, _> = page
            .declarations()
            .map(|decl| {
                (
                    decl.name.clone(),
                    (
                        status(self.lean.get(&decl.name).map(String::as_str)),
                        self.card_claim(read, decl),
                    ),
                )
            })
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let result: anyhow::Result<Option<_>> = async {
                let Some(path) = picker.await?? else {
                    return Ok(None);
                };
                let jobs = page
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| selected.is_none_or(|s| *i == s))
                    .filter_map(|(i, item)| match item {
                        Item::Declaration(decl) => Some((i, decl)),
                        Item::Prose(_) => None,
                    })
                    .flat_map(|(i, decl)| {
                        decl.steps
                            .iter()
                            .enumerate()
                            .map(move |(j, step)| ((i, j), step))
                    })
                    .map(|(key, step)| {
                        let requests = query_step(&remote, step);
                        let timeout = cx.background_executor().timer(Duration::from_secs(25));
                        async move { (key, receive_step(requests, timeout).await) }
                    });
                let goals = join_all(jobs).await.into_iter().collect();
                let html = export_html(&title, &page, selected, &goals, &details);
                cx.background_executor()
                    .spawn(async move {
                        std::fs::write(&path, html)?;
                        Ok::<_, std::io::Error>(Some(path))
                    })
                    .await
                    .map_err(Into::into)
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.notice = match result {
                    Ok(Some(path)) => Some(
                        tr!("literate-lean-exported", path = path.display().to_string()).into(),
                    ),
                    Ok(None) => return,
                    Err(err) => {
                        Some(tr!("literate-lean-export-error", error = err.to_string()).into())
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }
}

type CardClaim = (String, String, String, String);
type ExportDetails = HashMap<String, (SharedString, Option<CardClaim>)>;

fn html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn export_markdown(text: &str) -> String {
    let mut options = markdown::Options::gfm();
    options.parse.constructs.math_text = true;
    options.parse.constructs.math_flow = true;
    markdown::to_html_with_options(text, &options).unwrap_or_else(|_| html_text(text))
}

fn pre(text: &str) -> String {
    format!("<pre>{}</pre>", html_text(text))
}

fn details(label: &str, body: &str) -> String {
    format!(
        "<details><summary>{}</summary>{body}</details>",
        html_text(label)
    )
}

fn export_html(
    title: &str,
    page: &Page,
    selected: Option<usize>,
    goals: &HashMap<(usize, usize), Goals>,
    metadata: &ExportDetails,
) -> String {
    use std::fmt::Write as _;
    let title = html_text(title);
    let mut html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title>
<link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/KaTeX/0.16.22/katex.min.css">
<script defer src="https://cdnjs.cloudflare.com/ajax/libs/KaTeX/0.16.22/katex.min.js"></script>
<script>document.addEventListener("DOMContentLoaded", () => {{
  if (!window.katex) return;
  for (const node of document.querySelectorAll(".math-inline, .math-display")) {{
    katex.render(node.textContent, node, {{displayMode: node.classList.contains("math-display"), throwOnError: false}});
  }}
}});</script>
<style>
body {{ font: 1rem/1.55 system-ui, sans-serif; max-width: 68rem; margin: 2rem auto; padding: 0 1rem }}
article {{ border: 1px solid #999; border-radius: .5rem; padding: 1rem; margin-block: 1rem; }}
.columns {{ display: grid; grid-template-columns: repeat(auto-fit,minmax(min(100%,19rem),1fr)); gap: 1rem }}
pre {{ white-space: pre-wrap; overflow-wrap: anywhere; background: #ededed; color: #222; padding: .45rem; }}
summary {{ cursor: pointer; }} details {{ margin-block: .5rem; }}
small {{ font-weight: normal; margin-inline-start: .5em }}
</style></head><body><h1>{title}</h1>"#
    );
    if selected.is_none() {
        html.push_str("<nav><ul>");
        for (i, item) in page.items.iter().enumerate() {
            if let Item::Declaration(decl) = item {
                let _ = write!(
                    html,
                    "<li><a href=\"#decl-{i}\">{}</a> · {}</li>",
                    html_text(&decl.name),
                    html_text(decl.doc.lines().next().unwrap_or(""))
                );
            }
        }
        html.push_str("</ul></nav>");
    }
    if page.incomplete {
        let _ = write!(
            html,
            "<p>{}</p>",
            html_text(tr!("literate-lean-incomplete"))
        );
    }
    for (i, item) in page
        .items
        .iter()
        .enumerate()
        .filter(|(i, _)| selected.is_none_or(|s| *i == s))
    {
        match item {
            Item::Prose(prose) => html.push_str(&export_markdown(prose)),
            Item::Declaration(decl) => {
                let Some((status, card)) = metadata.get(&decl.name) else {
                    continue;
                };
                let _ = write!(
                    html,
                    "<article id=\"decl-{i}\"><h2>{}<small>{}</small></h2>{}",
                    html_text(&decl.name),
                    html_text(status),
                    export_markdown(&decl.doc)
                );
                let mut folded = decl.context.clone();
                folded.extend(
                    decl.binders
                        .iter()
                        .filter(|b| b.implicit)
                        .map(|b| b.text.clone()),
                );
                html.push_str("<section class=\"columns\"><div>");
                if !folded.is_empty() {
                    html.push_str(&details(
                        tr!("literate-lean-implicit"),
                        &pre(&folded.join("\n")),
                    ));
                }
                let explicit = decl
                    .binders
                    .iter()
                    .filter(|b| !b.implicit)
                    .map(|b| b.text.as_str())
                    .collect::<Vec<_>>();
                if !explicit.is_empty() {
                    html.push_str(&pre(&explicit.join("\n")));
                }
                let _ = write!(
                    html,
                    "<strong>{}</strong></div>",
                    pre(if decl.conclusion.is_empty() {
                        &decl.statement
                    } else {
                        &decl.conclusion
                    })
                );
                if let Some((_, _, name, claim)) = card {
                    let _ = write!(
                        html,
                        "<aside><h3>{}</h3>{}</aside>",
                        html_text(name),
                        export_markdown(claim)
                    );
                }
                html.push_str("</section>");
                if !decl.proof.is_empty() {
                    html.push_str(&details(tr!("literate-lean-proof"), &pre(&decl.proof)));
                }
                for (j, step) in decl.steps.iter().enumerate() {
                    let mut body = pre(&step.code);
                    if let Some(goals) = goals.get(&(i, j)) {
                        let _ = write!(
                            body,
                            "<section class=\"columns\"><div>{}{}</div><div>{}{}</div></section>",
                            html_text(tr!("literate-lean-before")),
                            pre(goal_label(&goals.before)),
                            html_text(tr!("literate-lean-after")),
                            pre(goal_label(&goals.after))
                        );
                        let diff = goals.diff();
                        if !diff.is_empty() {
                            body.push_str(&pre(&diff));
                        }
                    }
                    html.push_str(&details(&step.heading, &body));
                }
                html.push_str("</article>");
            }
        }
    }
    html.push_str("</body></html>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escapes_lean_and_includes_the_captured_goals() {
        let page = lean::parse("/-! Proof of $1 < 2$. -/\n/-- A strict inequality. -/\ntheorem small : 1 < 2 := by\n  -- Decide.\n  decide\n");
        let decl = page.declarations().next().unwrap();
        let metadata = HashMap::from([(decl.name.clone(), ("Proved".into(), None))]);
        let outcome = LspOutcome::Value {
            value: serde_json::json!({
                "goal": {"goals": ["⊢ 1 < 2"]}, "term_goal": null
            }),
        };
        let goals = HashMap::from([(
            (1, 0),
            Goals {
                before: goal_text(outcome),
                after: Some(String::new()),
            },
        )]);
        let html = export_html("<&>", &page, None, &goals, &metadata);
        assert!(html.contains("<title>&lt;&amp;&gt;</title>"));
        assert!(html.contains("math-inline"));
        assert!(html.contains("<summary>Decide.</summary>"));
        assert!(html.contains("⊢ 1 &lt; 2"));
    }
}
