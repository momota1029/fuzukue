//! Native chat views for the canonical UI tree.

use fuzukue_proto::MathMacro;
use fuzukue_view::{
    ui_kit::{Component, FlowNode, ListKind, Segment, Severity, Span, StepStatus, TextKind, Tone},
    xml_components,
};
use gpui_kit::base::text::{MarkdownExtensions, MarkdownNode};
use gpui_kit::base::{
    AccordionPanel, AccordionTrigger, GlobalState, Progress, StyledExt as _,
};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Theme};
use gpui_kit::{
    canvas, div, prelude::*, px, relative, AnyElement, App, Div, FontWeight, HighlightStyle, Hsla,
    MouseButton, Role, Stateful, StyledText, WeakEntity, Window,
};

use crate::{appearance, icons::Symbol, math, Workbench};

const PLUGIN: &str = "xml-component";

pub(super) fn extensions(
    macros: &[MathMacro],
    workbench: WeakEntity<Workbench>,
) -> MarkdownExtensions {
    math::extensions_with_macros(macros)
        .block_renderer(PLUGIN, move |node, window, cx| render_block(&workbench, node, window, cx))
        .source_blocks(|source, options| source_blocks(source, false, options))
}

pub(super) fn streaming_extensions(extensions: &MarkdownExtensions) -> MarkdownExtensions {
    extensions.clone().source_blocks(|source, options| source_blocks(source, true, options))
}

fn source_blocks(
    source: &str,
    streaming: bool,
    options: &markdown::ParseOptions,
) -> Vec<(std::ops::Range<usize>, MarkdownNode)> {
    xml_components::reply_blocks(source, streaming, options).into_iter().filter_map(|block| {
        use xml_components::BlockContent;
        let literal = &source[block.range.clone()];
        let node = match block.content {
            BlockContent::Component(component) => {
                let text = component.plain_text();
                MarkdownNode::new(PLUGIN, component).text(text).markdown(literal.to_owned())
            }
            BlockContent::Pending => MarkdownNode::new(PLUGIN, Pending).text(""),
            BlockContent::Literal if literal.starts_with("~~~") => return None,
            BlockContent::Literal => MarkdownNode::new(PLUGIN, ())
                .text(literal.to_owned())
                .markdown(literal.to_owned()),
        };
        Some((block.range, node.with_atomic_selection()))
    }).collect()
}

struct Pending;

fn render_block(
    workbench: &WeakEntity<Workbench>,
    node: &MarkdownNode,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if node.data::<Pending>().is_some() {
        return div()
            .id("xml-component-pending")
            .w_full()
            .min_h(px(26.))
            .flex()
            .items_center()
            .gap_1()
            .children((0..3).map(|_| {
                div().size(px(4.)).rounded_full().bg(cx.theme().muted_foreground)
            }))
            .into_any_element();
    }
    let Some(component) = node.data::<Component>() else {
        return div().id("xml-component-literal").w_full().min_w_0().whitespace_normal()
            .font_family("monospace").child(node.as_text().to_owned()).into_any_element();
    };
    let offset = node.source_range().map_or(0, |range| range.start);
    UiTree {
        workbench,
        window,
        cx,
    }
    .component(component, &offset.to_string())
}

struct UiTree<'a> {
    workbench: &'a WeakEntity<Workbench>,
    window: &'a mut Window,
    cx: &'a mut App,
}

impl UiTree<'_> {
    fn body(&mut self, body: &[Component], path: &str) -> Vec<AnyElement> {
        body.iter()
            .enumerate()
            .map(|(index, child)| self.component(child, &format!("{path}-{index}")))
            .collect()
    }

    fn component(&mut self, component: &Component, path: &str) -> AnyElement {
        let id = |tag| format!("xml-component-{tag}-{path}");
        let theme = self.cx.theme();
        match component {
            Component::Card { title, tone, body } => {
                let heading = div()
                    .id(id("card-title"))
                    .role(Role::Heading)
                    .aria_level(3)
                    .font_semibold()
                    .child(title.clone());
                let panel = panel(id("card"), theme)
                    .when(!theme.is_dark(), |panel| panel.bg(theme.popover))
                    .rounded_lg()
                    .p_4()
                    .gap_3()
                    .when_some(*tone, |panel, tone| {
                        panel.border_color(tone_color(theme, tone))
                    })
                    .child(heading);
                panel.children(self.body(body, path)).into_any_element()
            }
            Component::Grid { columns, items } => {
                let id = id("grid");
                // Measure the grid itself, so resizing a split pane also reflows its contents.
                let effective_columns = (*columns).min(items.len() as u16);
                let wide = self
                    .window
                    .use_keyed_state(format!("{id}-wide"), self.cx, |_, _| false);
                let columns = if *wide.read(self.cx) {
                    effective_columns
                } else {
                    1
                };
                let measure = canvas(
                    move |bounds, window, cx| {
                        let count = f32::from(effective_columns);
                        let fits = bounds.size.width >= px(144. * count + 8. * (count - 1.));
                        if *wide.read(cx) != fits {
                            window.defer(cx, move |_, cx| {
                                wide.update(cx, |wide, cx| {
                                    *wide = fits;
                                    cx.notify();
                                });
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h_0();
                div()
                    .id(id)
                    .relative()
                    .w_full()
                    .min_w_0()
                    .grid()
                    .grid_cols(columns)
                    .gap(px(8.))
                    .child(measure)
                    .children(
                        self.body(items, path)
                            .into_iter()
                            .map(|item| div().flex().min_w_0().child(item)),
                    )
                    .into_any_element()
            }
            Component::Stat {
                label,
                value,
                unit,
                tone,
                hint,
            } => {
                let surface = if theme.is_dark() {
                    theme.secondary
                } else {
                    theme.popover.blend(theme.secondary.opacity(0.15))
                };
                let foreground = tone.map_or(theme.foreground, |tone| {
                    tone_foreground(theme, tone, surface)
                });
                panel(id("stat"), theme)
                    .bg(surface)
                    .p_3()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_semibold()
                            .text_color(foreground)
                            .child(format!("{value}{}", unit.as_deref().unwrap_or(""))),
                    )
                    .when_some(hint.clone(), |tile, hint| {
                        tile.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(hint),
                        )
                    })
                    .into_any_element()
            }
            Component::Progress {
                value,
                max,
                label,
                caption,
                tone,
                segments,
            } => progress(
                id("progress"),
                value / max,
                *max,
                label.as_deref(),
                caption.as_deref(),
                tone.unwrap_or(Tone::Accent),
                segments,
                theme,
            )
            .into_any_element(),
            Component::Text { kind, spans } => {
                let text = self.text(spans);
                div()
                    .id(id("text"))
                    .min_w_0()
                    .when(*kind == TextKind::Heading, |text| {
                        text.role(Role::Heading).aria_level(4).font_semibold()
                    })
                    .child(text)
                    .into_any_element()
            }
            Component::List { kind, items } => {
                let muted = theme.muted_foreground;
                v_flex()
                    .id(id("list"))
                    .role(Role::List)
                    .gap_1()
                    .children(items.iter().enumerate().map(|(index, item)| {
                        let (marker, state) = match kind {
                            ListKind::Unordered => ("•".into(), None),
                            ListKind::Ordered => (format!("{}.", index + 1), None),
                            ListKind::Checklist if item.checked == Some(true) => {
                                ("✓".into(), Some(tr!("xml-component-checked")))
                            }
                            ListKind::Checklist => {
                                ("□".into(), Some(tr!("xml-component-unchecked")))
                            }
                        };
                        let item_path = format!("{path}-{index}");
                        h_flex()
                            .id(id("list-item") + &format!("-{index}"))
                            .items_start()
                            .gap_2()
                            .role(Role::ListItem)
                            .when_some(state, |row, state| {
                                row.aria_label(format!(
                                    "{state}: {}",
                                    item.body
                                        .iter()
                                        .map(Component::plain_text)
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                ))
                            })
                            .child(div().flex_none().text_color(muted).child(marker))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .children(self.body(&item.body, &item_path)),
                            )
                    }))
                    .into_any_element()
            }
            Component::Flowchart { nodes } => {
                let mut flow = v_flex().id(id("flowchart")).role(Role::List).w_full().min_w_0();
                for (index, node) in nodes.iter().enumerate() {
                    if index > 0 {
                        flow = flow.child(div().ml_4().text_color(theme.muted_foreground).child("↓"));
                    }
                    let item = panel(id("flow-node") + &format!("-{index}"), theme)
                        .role(Role::ListItem).w_full().min_w_0().rounded_md().p_2().gap_2();
                    flow = flow.child(match node {
                        FlowNode::Step { label, tone } => item
                            .when_some(*tone, |item, tone| item.border_color(tone_color(theme, tone)))
                            .child(div().min_w_0().whitespace_normal().child(label.clone())),
                        FlowNode::Decision { question, branches } => item
                            .border_color(tone_color(theme, Tone::Accent))
                            .child(div().font_semibold().whitespace_normal().child(question.clone()))
                            .children(branches.iter().enumerate().map(|(branch_index, branch)| {
                                let tone = branch.tone.unwrap_or(Tone::Muted);
                                div().id(id("flow-branch") + &format!("-{index}-{branch_index}"))
                                    .min_w_0().border_l_2().border_color(tone_color(theme, tone))
                                    .pl_2().whitespace_normal()
                                    .child(format!("{} → {}", branch.label, branch.result))
                            })),
                    });
                }
                flow.into_any_element()
            }
            Component::Steps { steps } => {
                let border = theme.border;
                let colors = [
                    theme.muted_foreground,
                    tone_foreground(theme, Tone::Accent, theme.background),
                    tone_foreground(theme, Tone::Success, theme.background),
                ];
                v_flex()
                    .id(id("steps"))
                    .role(Role::List)
                    .gap_2()
                    .children(steps.iter().enumerate().map(|(index, step)| {
                        let (marker, color, state) = match step.status {
                            StepStatus::Pending => (
                                (index + 1).to_string(),
                                colors[0],
                                tr!("xml-component-step-pending"),
                            ),
                            StepStatus::Current => {
                                ("●".into(), colors[1], tr!("xml-component-step-current"))
                            }
                            StepStatus::Done => {
                                ("✓".into(), colors[2], tr!("xml-component-step-done"))
                            }
                        };
                        h_flex()
                            .id(id("step") + &format!("-{index}"))
                            .role(Role::ListItem)
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .px_1()
                                    .min_w(px(22.))
                                    .text_center()
                                    .flex_none()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(border)
                                    .text_color(color)
                                    .child(marker),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id(id("step-title") + &format!("-{index}"))
                                            .aria_label(format!("{} · {state}", step.title))
                                            .font_semibold()
                                            .text_color(color)
                                            .child(step.title.clone()),
                                    )
                                    .children(self.body(&step.body, &format!("{path}-{index}"))),
                            )
                    }))
                    .into_any_element()
            }
            Component::Details { title, open, body } => self
                .details(id("details"), path, title, *open, body)
                .into_any_element(),
            Component::Divider => div()
                .id(id("divider"))
                .h(px(1.))
                .w_full()
                .bg(theme.border)
                .into_any_element(),
            Component::Callout {
                severity,
                title,
                body,
            } => {
                let (color, level) = match severity {
                    Severity::Info => (theme.info, tr!("xml-component-info")),
                    Severity::Warn => (theme.warning, tr!("xml-component-warn")),
                    Severity::Error => (theme.danger, tr!("xml-component-error")),
                };
                let surface = if theme.is_dark() {
                    theme.muted.opacity(0.45)
                } else {
                    theme.popover
                };
                let heading = format!("{level} · {title}");
                let panel = panel(id("callout"), theme)
                    .border_l_4()
                    .border_color(color)
                    .bg(surface)
                    .p_3()
                    .gap_1()
                    .child(div().font_semibold().child(heading));
                panel.children(self.body(body, path)).into_any_element()
            }
            Component::Table { rows } => {
                table(id("table"), rows, theme, component.plain_text()).into_any_element()
            }
            Component::Finding { deck, id: finding } => self
                .finding(id("finding"), id("finding-preview"), deck, finding)
                .into_any_element(),
        }
    }

    fn text(&self, spans: &[Span]) -> StyledText {
        let theme = self.cx.theme();
        let font = appearance::code_font(self.cx).family;
        let mut text = String::new();
        let mut highlights = Vec::new();
        let mut fonts = Vec::new();
        for span in spans {
            let start = text.len();
            text.push_str(span.text());
            let range = start..text.len();
            let highlight = match span {
                Span::Plain { .. } => continue,
                Span::Badge { tone, .. } => HighlightStyle {
                    color: Some(tone_foreground(
                        theme,
                        tone.unwrap_or(Tone::Muted),
                        theme.muted,
                    )),
                    background_color: Some(theme.muted),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
                Span::Code { .. } | Span::Kbd { .. } => {
                    fonts.push((range.clone(), font.clone()));
                    HighlightStyle {
                        background_color: Some(theme.secondary),
                        font_weight: matches!(span, Span::Kbd { .. })
                            .then_some(FontWeight::SEMIBOLD),
                        ..Default::default()
                    }
                }
            };
            highlights.push((range, highlight));
        }
        StyledText::new(text)
            .with_highlights(highlights)
            .with_font_family_overrides(fonts)
    }

    fn details(
        &mut self,
        id: String,
        path: &str,
        title: &str,
        initial: bool,
        body: &[Component],
    ) -> impl IntoElement {
        let state = self
            .window
            .use_keyed_state(format!("{id}-open"), self.cx, |_, cx| {
                (initial, cx.focus_handle())
            });
        let (open, focus) = state.read(self.cx).clone();
        let theme = self.cx.theme();
        let (hover, ring) = (theme.muted, theme.ring);
        let chevron = if open {
            Symbol::ChevronDown
        } else {
            Symbol::ChevronRight
        };
        let trigger = AccordionTrigger::new(format!("{id}-toggle"))
            .open(open)
            .track_focus(&focus)
            .tab_index(0)
            .aria_label(title.to_owned())
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx)
            })
            .on_change(move |next, _, _, cx| {
                state.update(cx, |(open, _), cx| {
                    *open = next;
                    cx.notify();
                });
            })
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .p_2()
            .font_semibold()
            .rounded_md()
            .border_1()
            .border_color(theme.transparent)
            .hover(move |style| style.bg(hover))
            .focus_visible(move |style| style.border_color(ring))
            .cursor_pointer()
            .child(
                div()
                    .flex_none()
                    .child(chevron.tinted(theme.muted_foreground)),
            )
            .child(div().min_w_0().child(title.to_owned()));
        let panel = panel(id.clone(), theme).child(trigger);
        panel.when(open, |container| {
            container.child(
                AccordionPanel::new()
                    .id(format!("{id}-body"))
                    .open(true)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(self.cx.theme().border)
                    .children(self.body(body, path)),
            )
        })
    }

    fn finding(
        &self,
        id: String,
        preview_id: String,
        deck: &str,
        finding: &str,
    ) -> impl IntoElement {
        let (label, preview) = self
            .workbench
            .upgrade()
            .and_then(|workbench| {
                workbench
                    .read(self.cx)
                    .finding_chat_preview(deck, finding, self.cx)
            })
            .map(|(label, preview)| {
                (
                    label,
                    div().id(preview_id).child(preview).into_any_element(),
                )
            })
            .unwrap_or_else(|| {
                let label = tr!("xml-component-finding-unavailable");
                (
                    label.to_owned(),
                    div().text_sm().child(label).into_any_element(),
                )
            });
        let reference = format!("{deck} · {finding}");
        let theme = self.cx.theme();
        panel(id, theme)
            .p_3()
            .gap_1()
            .aria_label(format!("{}\n{label}\n{reference}", tr!("findings-title")))
            .child(preview)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(reference),
            )
    }
}

fn panel(id: String, theme: &Theme) -> Stateful<Div> {
    v_flex()
        .id(id)
        .role(Role::Group)
        .w_full()
        .min_w_0()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
}

fn tone_color(theme: &Theme, tone: Tone) -> Hsla {
    match tone {
        Tone::Info => theme.info,
        Tone::Success => theme.success,
        Tone::Warn => theme.warning,
        Tone::Error => theme.danger,
        Tone::Accent => theme.link,
        Tone::Muted => theme.muted_foreground,
    }
}

fn tone_foreground(theme: &Theme, tone: Tone, surface: Hsla) -> Hsla {
    appearance::accessible_accent(tone_color(theme, tone), surface, theme.is_dark(), 4.5)
}

fn progress(
    id: String,
    fraction: f64,
    max: f64,
    label: Option<&str>,
    caption: Option<&str>,
    tone: Tone,
    segments: &[Segment],
    theme: &Theme,
) -> impl IntoElement {
    let total = fraction as f32;
    let mut track = Progress::new(format!("{id}-track"))
        .value(100. * total)
        .accessibility_label(
            label.map_or_else(|| tr!("xml-component-progress").to_string(), str::to_owned),
        )
        .flex()
        .w_full()
        .h(px(7.))
        .rounded_full()
        .overflow_hidden()
        .bg(theme.muted);
    let mut remaining = fraction;
    for segment in segments {
        let portion = (segment.value / max).min(remaining);
        remaining -= portion;
        track = track.child(
            div()
                .flex_none()
                .h_full()
                .w(relative(portion as f32))
                .bg(tone_color(theme, segment.tone)),
        );
    }
    track = track.child(
        div()
            .id(format!("{id}-fill"))
            .flex_none()
            .h_full()
            .w(relative(remaining as f32))
            .bg(tone_color(theme, tone)),
    );
    v_flex()
        .id(id)
        .gap_1()
        .w_full()
        .when_some(label, |progress, label| {
            progress.child(div().text_sm().child(label.to_owned()))
        })
        .child(track)
        .when_some(caption, |progress, caption| {
            progress.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(caption.to_owned()),
            )
        })
}

fn table(id: String, rows: &[Vec<String>], theme: &Theme, text: String) -> impl IntoElement {
    panel(id, theme)
        .aria_label(format!("{}\n{text}", tr!("xml-component-table")))
        .text_sm()
        .children(rows.iter().enumerate().map(|(row_index, row)| {
            h_flex()
                .items_stretch()
                .when(row_index == 0, |row| row.bg(theme.muted).font_semibold())
                .when(row_index > 0, |row| {
                    row.border_t_1().border_color(theme.border)
                })
                .children(row.iter().enumerate().map(|(col, cell)| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .px_2()
                        .py_1()
                        .when(col > 0, |cell| cell.border_l_1().border_color(theme.border))
                        .child(cell.clone())
                }))
        }))
}
