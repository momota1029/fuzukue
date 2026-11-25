//! RaTeX のレイアウトを GPUI の文字描画、矩形、パスで表示する Markdown 拡張。
//! 選択とコピーは TextView が扱い、式は区切り付きの TeX として選択に含まれる。

mod glyphs;
mod spacing;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin,
};
use gpui_kit::base::InteractiveElementExt as _;
use gpui_kit::component::{scroll::ScrollableElement as _, ActiveTheme as _};
use gpui_kit::{
    div, prelude::*, px, AnyElement, App, Global, Hsla, Rgba, ScrollHandle, SharedString, Window,
};
use fuzukue_proto::MathMacro;
use markdown::mdast::Node;
use ratex_parser::{macro_expander::MacroDefinition, Parser};

use glyphs::{Formula, GlyphFonts};

pub(super) struct MathScale(pub f32);
impl Global for MathScale {}

pub(super) fn init(cx: &mut App) {
    if !cx.has_global::<FormulaCache>() {
        glyphs::register_fonts(cx);
        cx.set_global(FormulaCache::default());
    }
}

fn scaled_size(size: f32, cx: &App) -> f32 {
    size * cx.try_global::<MathScale>().map_or(1.0, |scale| scale.0)
}

fn text_size(window: &Window) -> f32 {
    window
        .text_style()
        .font_size
        .to_pixels(window.rem_size())
        .into()
}

type Macros = Arc<[MathMacro]>;
type Key = (SharedString, Macros, u32, [u32; 4], bool);

#[derive(Default)]
struct FormulaCache {
    formulas: RefCell<HashMap<Key, FormulaRender>>,
    fonts: RefCell<GlyphFonts>,
}
impl Global for FormulaCache {}

#[derive(Clone)]
struct FormulaRender {
    formula: Option<Rc<Formula>>,
    unknown_macros: Vec<String>,
}

fn set_macro(parser: &mut Parser<'_>, name: String, body: &str, num_args: usize) {
    let body = aliases(body);
    let mut tokens: Vec<_> = Parser::new(&body).gullet.lexer.collect();
    tokens.reverse();
    parser.gullet.set_macro(name, MacroDefinition::Tokens { tokens, num_args });
}

fn parse_formula(
    latex: &str,
    macros: &[MathMacro],
) -> (Option<Vec<ratex_parser::ParseNode>>, Vec<String>) {
    let latex = aliases(latex);
    let mut unknown = Vec::<String>::new();
    loop {
        let mut parser = Parser::new(&latex);
        for definition in macros {
            set_macro(&mut parser, format!("\\{}", definition.name), &definition.body, definition.arguments.into());
        }
        for name in &unknown {
            set_macro(&mut parser, name.clone(), r"\text{?}", 0);
        }
        match parser.parse() {
            Ok(nodes) => return (Some(nodes), unknown),
            Err(error) => {
                let Some(name) = error.message.strip_prefix("Undefined control sequence: ") else {
                    return (None, unknown);
                };
                // Retry with a placeholder so the surrounding formula remains legible.
                if unknown.len() == 16 || unknown.iter().any(|seen| seen == name) {
                    return (None, unknown);
                }
                unknown.push(name.to_owned());
            }
        }
    }
}

fn rendered(
    latex: &SharedString,
    macros: &Macros,
    font_size: f32,
    color: Hsla,
    display: bool,
    window: &Window,
    cx: &mut App,
) -> FormulaRender {
    init(cx);
    let rgba: Rgba = color.into();
    let key = (
        latex.clone(),
        macros.clone(),
        font_size.to_bits(),
        [rgba.r, rgba.g, rgba.b, rgba.a].map(f32::to_bits),
        display,
    );
    let cache = cx.global::<FormulaCache>();
    cache
        .formulas
        .borrow_mut()
        .entry(key)
        .or_insert_with(|| {
            let (nodes, unknown_macros) = parse_formula(latex, macros);
            let formula = nodes.and_then(|nodes| {
                Formula::new(&nodes, font_size, rgba, display, window, &mut cache.fonts.borrow_mut())
                    .map(Rc::new)
            });
            FormulaRender { formula, unknown_macros }
        })
        .clone()
}

/// ratex が知らない、よく使う記号の命令を同じ記号に読み替える（amssymb の別名など）。
fn aliases(latex: &str) -> std::borrow::Cow<'_, str> {
    const ALIASES: &[(&str, &str)] = &[(r"\Box", r"\square"), (r"\Diamond", r"\lozenge")];
    let mut out = std::borrow::Cow::Borrowed(latex);
    for (from, to) in ALIASES {
        // 後ろに英字が続くもの（\Boxed など）は別の命令なので触らない。
        let mut rest = out.as_ref();
        let mut replaced = String::new();
        let mut hit = false;
        while let Some(i) = rest.find(from) {
            let after = &rest[i + from.len()..];
            replaced.push_str(&rest[..i]);
            if after.starts_with(|c: char| c.is_ascii_alphabetic()) {
                replaced.push_str(from);
            } else {
                replaced.push_str(to);
                hit = true;
            }
            rest = after;
        }
        if hit {
            replaced.push_str(rest);
            out = std::borrow::Cow::Owned(replaced);
        }
    }
    out
}

#[derive(Clone)]
struct MathNode {
    latex: SharedString,
    source: SharedString,
    offset: usize,
}

impl MathNode {
    fn parse(node: &Node, latex: &str, display: bool, cx: &MarkdownParseContext<'_>) -> Self {
        Self {
            latex: latex.to_owned().into(),
            source: fuzukue_view::math_markdown(latex, display).into(),
            offset: cx.offset() + node.position().map_or(0, |pos| pos.start.offset),
        }
    }

    fn element(&self, formula: Rc<Formula>) -> impl IntoElement {
        let element = div().child(formula.element());
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        let element = element.id(format!("formula:{}", self.latex.trim()));
        element
    }

    fn fallback(&self, cx: &App) -> AnyElement {
        // 描けなかった式は TeX のまま出す。長い 1 行で周りを押し広げないよう、折り返す。
        div()
            .min_w_0()
            .max_w_full()
            .whitespace_normal()
            .font(crate::appearance::code_font(cx))
            .text_color(cx.theme().muted_foreground)
            .child(self.source.clone())
            .into_any_element()
    }
}

fn macro_error(names: &[String], cx: &App) -> Option<AnyElement> {
    (!names.is_empty()).then(|| {
        let error = div().min_w_0().max_w_full().whitespace_normal().text_xs().text_color(cx.theme().danger)
            .child(tr!("paper-math-unknown-macros", names = names.join(", ")));
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        let error = error.id(format!("math-unknown:{}", names.join(",")));
        error.into_any_element()
    })
}

const INLINE_MATH: &str = "fuzukue-inline-math";

struct InlineMath(Macros);

impl MarkdownPlugin for InlineMath {
    fn name(&self) -> &str {
        INLINE_MATH
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::InlineMath(math) = node else {
            return None;
        };
        let math = MathNode::parse(node, &math.value, false, cx);
        let source = math.source.clone();
        Some(
            MarkdownNode::new(self.name(), math)
                .text(source.clone())
                .markdown(source)
                .with_atomic_selection(),
        )
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let math = node.data::<MathNode>()?;
        let size = scaled_size(context.font_size().into(), cx);
        let result = rendered(
                &math.latex,
                &self.0,
                size,
                context.text_style().color,
                false,
                window,
                cx,
            );
        let error = macro_error(&result.unknown_macros, cx);
        Some(
            match result.formula {
                Some(formula) => {
                    let baseline = formula.baseline;
                    let element = math.element(formula);
                    if let Some(error) = error {
                        InlineElement::new(div().flex().items_center().gap_1()
                            .child(element).child(error)).with_baseline(baseline)
                    } else {
                        InlineElement::new(element).with_baseline(baseline)
                    }
                }
                None => InlineElement::new(div().child(math.fallback(cx)).children(error)),
            },
        )
    }
}

struct BlockMath(Macros);

impl MarkdownPlugin for BlockMath {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "fuzukue-block-math"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Math(math) = node else { return None };
        let math = MathNode::parse(node, &math.value, true, cx);
        let source = math.source.clone();
        Some(
            MarkdownNode::new(self.name(), math)
                .text(source.clone())
                .markdown(source)
                .with_atomic_selection(),
        )
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let math = node.data::<MathNode>().expect("block math plugin node");
        let size = scaled_size(text_size(window) * (17.0 / 16.0), cx);
        let result = rendered(&math.latex, &self.0, size, cx.theme().foreground, true, window, cx);
        let error = macro_error(&result.unknown_macros, cx);
        let Some(formula) = result.formula
        else {
            return div().w_full().min_w_0().py_1().child(math.fallback(cx)).children(error);
        };
        let scroll = window
            .use_keyed_state(("display-math-scroll", math.offset), cx, |_, _| {
                ScrollHandle::new()
            })
            .read(cx)
            .clone();
        let height = formula.size.height;
        let element = math.element(formula);
        // Only the formula height participates in the surrounding text's intrinsic
        // size. The scrolling content gets the width of the already laid-out column.
        div().w_full().min_w_0().pt_1().child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .h(height + px(8.))
                .child(
                    div()
                        .id(("display-math", math.offset))
                        .absolute()
                        .inset_0()
                        .flex()
                        .min_w_0()
                        .focusable()
                        .overflow_x_scroll()
                        .lock_scroll_axis()
                        .track_scroll(&scroll)
                        .child(div().flex_none().mx_auto().child(element))
                        .horizontal_scrollbar(&scroll)
                        .on_key_down(move |event, window, cx| {
                            if event.keystroke.modifiers.modified()
                                || scroll.max_offset().x <= px(0.)
                            {
                                return;
                            }
                            let direction = match event.keystroke.key.as_str() {
                                "left" => 1.,
                                "right" => -1.,
                                _ => return,
                            };
                            let mut offset = scroll.offset();
                            offset.x = (offset.x + window.line_height() * direction)
                                .clamp(-scroll.max_offset().x, px(0.));
                            scroll.set_offset(offset);
                            window.refresh();
                            cx.stop_propagation();
                        }),
                ),
        ).children(error)
    }
}

/// 数式を描く Markdown の拡張。TextView に `.markdown_extensions(math::extensions())` で渡す。
pub fn extensions() -> MarkdownExtensions {
    extensions_with_macros(&[])
}

pub(super) fn extensions_with_macros(macros: &[MathMacro]) -> MarkdownExtensions {
    let macros: Macros = Arc::from(macros);
    MarkdownExtensions::default()
        .escape_selection_dollars()
        .inline_spacing(spacing::between)
        .plugin(InlineMath(macros.clone()))
        .plugin(BlockMath(macros))
}

#[cfg(test)]
mod alias_tests {
    use super::aliases;

    #[test]
    fn box_becomes_square_but_longer_commands_stay() {
        assert_eq!(
            aliases(r"\Box^1_\preceq(T,\mathcal A)"),
            r"\square^1_\preceq(T,\mathcal A)"
        );
        assert_eq!(aliases(r"\Boxed{x}"), r"\Boxed{x}");
        assert!(ratex_parser::parse(&aliases(
            r"\triangle(\mathcal A,\mathcal B)\coloneqq\sup_T\frac{|\Box^1_\preceq(T,\mathcal A)-\Box^1_\preceq(T,\mathcal B)|}{\#T}"
        ))
        .is_ok());
    }
}

#[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
mod selection_tests;
