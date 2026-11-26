//! Shared Markdown code styling and copy controls.

use std::time::Duration;

use fuzukue_view::markdown_code::normalize_code_text;
use gpui_kit::base::text::CodeBlock;
use gpui_kit::component::{
    text::{TextView, TextViewStyle},
    tooltip::Tooltip,
    ActiveTheme as _,
};
use gpui_kit::{div, prelude::*, px, App, ClipboardItem, Role, StyleRefinement, Task, Window};

use super::{Symbol, ThemeSurfaces, Workbench, UI_LINE_HEIGHT};

impl Workbench {
    pub(super) fn chat_code_style(&self, view: TextView, cx: &App) -> TextView {
        self.markdown_code_style(view, TextViewStyle::default(), cx)
            .line_height(UI_LINE_HEIGHT)
    }

    pub(crate) fn markdown_code_style(
        &self,
        view: TextView,
        style: TextViewStyle,
        cx: &App,
    ) -> TextView {
        let theme = cx.theme();
        let surface = ThemeSurfaces::current(cx).code;
        let code_font = &self.appearance.code_font;
        let font_style = || {
            let mut style = StyleRefinement::default().font_family(code_font.family.clone());
            style.text.font_fallbacks = code_font.fallbacks.clone();
            style
        };
        let inline_code = font_style()
            .rounded(px(6.))
            .px(px(3.))
            .bg(surface)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.foreground);
        let code_block = font_style()
            .bg(surface)
            .text_color(theme.foreground)
            .whitespace_normal()
            .line_height(UI_LINE_HEIGHT);
        view.style(TextViewStyle {
            code_block,
            highlight_theme: theme.highlight_theme.clone(),
            ..style
        })
        .inline_code_style(inline_code)
        .code_block_actions(code_copy_button)
    }
}

fn code_copy_button(block: &CodeBlock, window: &mut Window, cx: &mut App) -> impl IntoElement {
    // The scoped render callback gives each block its own feedback timer.
    let feedback = window.use_keyed_state("chat-code-copy-feedback", cx, |_, _| None::<Task<()>>);
    let copied = feedback.read(cx).is_some();
    let label = if copied {
        tr!("chat-code-copied")
    } else {
        tr!("chat-copy-code")
    };
    let code = block.code();
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let hover = theme.muted;
    let ring = theme.ring;
    let always_visible = window.viewport_size().width <= px(600.) || copied;
    div()
        .id("chat-code-copy")
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(gpui_kit::transparent_black())
        .opacity(if always_visible { 1. } else { 0. })
        .group_hover("code-block", |style| style.opacity(1.))
        .focus_visible(move |style| style.opacity(1.).border_color(ring))
        .hover(move |style| style.bg(hover))
        .cursor_pointer()
        .child(if copied { Symbol::Check } else { Symbol::Copy }.tinted(muted))
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .on_click(move |_, window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                normalize_code_text(&code).into_owned(),
            ));
            feedback.update(cx, |task, cx| {
                *task = Some(cx.spawn(async move |state, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    let _ = state.update(cx, |task, cx| {
                        *task = None;
                        cx.notify();
                    });
                }));
                cx.notify();
            });
            window.refresh();
        })
}
