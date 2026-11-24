//! Native, read-only views for the chat Markdown component vocabulary.

use fuzukue_proto::MathMacro;
use fuzukue_view::xml_components::{self, Component, Severity};
use gpui_kit::base::text::{
    MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
};
use gpui_kit::base::{markdown_ast, StyledExt as _};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _};
use gpui_kit::{div, prelude::*, App, Role, WeakEntity, Window};

use crate::{math, Workbench};

const PLUGIN: &str = "xml-component";

pub(super) fn extensions(
    macros: &[MathMacro],
    workbench: WeakEntity<Workbench>,
) -> MarkdownExtensions {
    math::extensions_with_macros(macros).plugin(XmlComponents(workbench))
}

struct XmlComponents(WeakEntity<Workbench>);

impl MarkdownPlugin for XmlComponents {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        PLUGIN
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let component = xml_components::parse_block(node, cx.source())?;
        let text = match &component {
            Component::Callout { title, body, .. } => format!("{title}\n{body}"),
            Component::Table(rows) => table_text(rows),
            Component::Finding { deck, id } => format!("{deck} · {id}"),
        };
        Some(
            MarkdownNode::new(PLUGIN, component)
                .text(text)
                .markdown(cx.node_source(node)?.to_owned())
                .with_atomic_selection(),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let offset = node.source_range().map_or(0, |range| range.start);
        let panel = |tag| {
            v_flex()
                .id(format!("xml-component-{tag}-{offset}"))
                .role(Role::Group)
                .w_full()
                .min_w_0()
                .rounded_md()
                .overflow_hidden()
                .border_1()
                .border_color(theme.border)
        };

        let Some(component) = node.data::<Component>() else {
            return div().child(node.as_text().to_owned()).into_any_element();
        };
        match component {
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
                let heading = format!("{level} · {title}");
                panel("callout")
                    .aria_label(format!("{heading}\n{body}"))
                    .border_l_4()
                    .border_color(color)
                    .bg(theme.muted.opacity(0.45))
                    .p_3()
                    .gap_1()
                    .child(div().font_semibold().child(heading))
                    .children((!body.is_empty()).then(|| div().child(body.clone())))
                    .into_any_element()
            }
            Component::Table(rows) => panel("table")
                .aria_label(format!(
                    "{}\n{}",
                    tr!("xml-component-table"),
                    node.as_text()
                ))
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
                .into_any_element(),
            Component::Finding { deck, id } => {
                let (label, preview) = self
                    .0
                    .upgrade()
                    .and_then(|workbench| workbench.read(cx).finding_chat_preview(deck, id, cx))
                    .map(|(label, preview)| {
                        (
                            label,
                            div()
                                .id(format!("xml-component-finding-preview-{offset}"))
                                .child(preview)
                                .into_any_element(),
                        )
                    })
                    .unwrap_or_else(|| {
                        let label = tr!("xml-component-finding-unavailable");
                        (
                            label.to_owned(),
                            div().text_sm().child(label).into_any_element(),
                        )
                    });
                let reference = format!("{deck} · {id}");
                panel("finding")
                    .aria_label(format!("{}\n{label}\n{reference}", tr!("findings-title")))
                    .p_3()
                    .gap_1()
                    .child(preview)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(reference),
                    )
                    .into_any_element()
            }
        }
    }
}

fn table_text(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|row| row.join(" · "))
        .collect::<Vec<_>>()
        .join("\n")
}
