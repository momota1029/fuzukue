//! Shared rendering for Git's unified patch format.

use gpui_kit::component::{v_flex, ActiveTheme as _};
use gpui_kit::{prelude::*, App, HighlightStyle, IntoElement, StyledText};

pub(super) fn render(text: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let mut hunk = false;
    let mut offset = 0;
    let highlights = text.split_inclusive('\n').filter_map(|line| {
        let start = offset;
        offset += line.len();
        let color = if line.starts_with("@@") {
            hunk = true;
            theme.muted_foreground
        } else if line.starts_with("diff --git ") {
            hunk = false;
            theme.muted_foreground
        } else if hunk && line.starts_with('+') {
            theme.success
        } else if hunk && line.starts_with('-') {
            theme.danger
        } else {
            return None;
        };
        Some((
            start..offset,
            HighlightStyle {
                color: Some(color),
                ..Default::default()
            },
        ))
    });
    v_flex()
        .items_start()
        .w_full()
        .p_2()
        .bg(theme.secondary)
        .rounded_md()
        .font(crate::appearance::code_font(cx))
        .text_xs()
        .whitespace_nowrap()
        .child(StyledText::new(text.to_owned()).with_highlights(highlights))
}
