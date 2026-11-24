use gpui_kit::base::text::InlineSpacingEdge::{self, Custom, Text};
use unicode_properties::{
    GeneralCategory as Category, GeneralCategoryGroup, UnicodeGeneralCategory as _,
};
use unicode_script::{Script, UnicodeScript as _};

use super::INLINE_MATH;

const INTER_SCRIPT_EM: f32 = 0.25;
const INLINE_MATH_EM: f32 = 0.2;

pub(super) fn between(left: InlineSpacingEdge<'_>, right: InlineSpacingEdge<'_>) -> f32 {
    match (left, right) {
        (Custom(INLINE_MATH), Custom(INLINE_MATH)) => INLINE_MATH_EM,
        (Custom(INLINE_MATH), Text(text)) | (Text(text), Custom(INLINE_MATH))
            if text.chars().next().is_some_and(math_neighbor) =>
        {
            INLINE_MATH_EM
        }
        (Text(left), Text(right)) => match (left.chars().next(), right.chars().next()) {
            (Some(left), Some(right))
                if (japanese(left) && western(right)) || (western(left) && japanese(right)) =>
            {
                INTER_SCRIPT_EM
            }
            _ => 0.,
        },
        _ => 0.,
    }
}

fn western(c: char) -> bool {
    c.is_ascii_digit()
        || (c.script() == Script::Latin
            && c.is_alphabetic()
            && !matches!(c, 'Ａ'..='Ｚ' | 'ａ'..='ｚ'))
}

fn japanese(c: char) -> bool {
    matches!(
        c.script(),
        Script::Han | Script::Hiragana | Script::Katakana
    ) || matches!(c, 'ー' | 'ｰ')
        || (matches!(
            c,
            '\u{3000}'..='\u{303f}'
                | '\u{30fb}'
                | '\u{fe10}'..='\u{fe1f}'
                | '\u{fe30}'..='\u{fe4f}'
                | '\u{ff01}'..='\u{ff65}'
        ) && matches!(
            c.general_category(),
            Category::DashPunctuation | Category::ConnectorPunctuation | Category::OtherPunctuation
        ) && !matches!(
            c,
            // 句読点と中点類は字面の中に空きを持つので、和欧間を足さない。
            '、' | '。' | '，' | '．' | '､' | '｡' | '︐' | '︑' | '︒' | '・' | '･' | '：' | '；'
        ))
}

fn math_neighbor(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(c.general_category(), Category::Control | Category::Format)
        && c.general_category_group() != GeneralCategoryGroup::Punctuation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_and_western_boundaries_in_both_directions() {
        for (ja, west) in [
            ("日", "A"),
            ("あ", "1"),
            ("カ", "é"),
            ("か\u{3099}", "e\u{301}"),
            ("𠮷", "2"),
            ("ー", "x"),
            ("！", "3"),
        ] {
            assert_eq!(between(Text(ja), Text(west)), INTER_SCRIPT_EM);
            assert_eq!(between(Text(west), Text(ja)), INTER_SCRIPT_EM);
        }
        for ja in [
            "（", "）", "「", "」", "〈", "〉", "、", "。", "，", "．", "　", " ",
        ] {
            assert_eq!(between(Text(ja), Text("A")), 0., "{ja}");
            assert_eq!(between(Text("1"), Text(ja)), 0., "{ja}");
        }
        for (left, right) in [
            ("日", "本"),
            ("A", "1"),
            ("字", "Ａ"),
            ("字", "１"),
            ("字", "α"),
        ] {
            assert_eq!(between(Text(left), Text(right)), 0.);
        }
    }

    #[test]
    fn middle_dot_and_colons_hug_western_text() {
        for (left, right) in [("A", "・"), ("・", "B"), ("A", "･"), ("x", "："), ("；", "y")] {
            assert_eq!(between(Text(left), Text(right)), 0., "{left}{right}");
        }
    }

    #[test]
    fn inline_math_hugs_punctuation_and_respects_existing_spacing() {
        let math = Custom(INLINE_MATH);
        for text in ["日", "3", "a"] {
            assert_eq!(between(Text(text), math), INLINE_MATH_EM);
            assert_eq!(between(math, Text(text)), INLINE_MATH_EM);
        }
        for text in [
            "，", "．", "、", "。", "（", "）", "「", "」", "!", ".", ",", ":", " ", "\n",
        ] {
            assert_eq!(between(Text(text), math), 0., "{text:?}");
            assert_eq!(between(math, Text(text)), 0., "{text:?}");
        }
        assert_eq!(between(math, math), INLINE_MATH_EM);
        assert_eq!(between(math, Custom("fuzukue-block-math")), 0.);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dragging_across_gaps_copies_the_original_bytes_and_projects_to_source() {
        use gpui_kit::base::{
            text::{SelectionFormat, TextView, TextViewState},
            TextSelectionLayer,
        };
        use gpui_kit::{
            div, point, px, size, AnyWindowHandle, App, AppContext, Bounds, Context, Entity,
            IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
            PlatformInput, Render, Styled, Window, WindowBounds, WindowOptions,
        };

        struct SelectionView(Entity<TextViewState>);

        impl Render for SelectionView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .w_full()
                    .text_size(px(16.))
                    .line_height(px(24.))
                    .child(TextSelectionLayer)
                    .child(
                        TextView::new(&self.0)
                            .w_full()
                            .selectable(true)
                            .markdown_extensions(super::super::extensions()),
                    )
            }
        }

        const SELECTED: &str = "漢字ABC$x^2$かな123終";
        const SOURCE: &str = "漢字ABC$x^2$かな123終\n\n続き";
        gpui_kit::platform::headless().run(|cx: &mut App| {
            gpui_kit::init(cx);
            let state = cx.new(|cx| TextViewState::markdown(SOURCE, cx));
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.), px(0.)),
                            size(px(600.), px(180.)),
                        ))),
                        show: false,
                        ..Default::default()
                    },
                    |_, cx| cx.new(|_| SelectionView(state.clone())),
                )
                .expect("open headless text view");
            let window = AnyWindowHandle::from(window);
            window
                .update(cx, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let bounds = state.read(cx).bounds();
            let start = bounds.origin + point(px(1.), px(8.));
            let end = point(bounds.right() - px(1.), start.y);
            for event in [
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    click_count: 1,
                    ..Default::default()
                }),
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: end,
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                }),
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: end,
                    click_count: 1,
                    ..Default::default()
                }),
            ] {
                window
                    .update(cx, |_, window, cx| {
                        window.dispatch_event(event, cx);
                        window.draw(cx).clear(cx);
                    })
                    .unwrap();
            }
            state.update(cx, |state, cx| {
                // Plain copy includes the paragraph's trailing newline.
                assert_eq!(state.selected_text(), &SOURCE[..=SELECTED.len()]);
                assert_eq!(state.selected_source_range(), Some(0..SELECTED.len()));
                state.set_selection_format(SelectionFormat::Source, cx);
                assert_eq!(state.selected_text(), SELECTED);
            });
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }
}
