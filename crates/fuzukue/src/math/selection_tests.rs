use gpui_kit::base::{
    text::{TextView, TextViewState},
    TextSelection, TextSelectionLayer,
};
use gpui_kit::{
    div, point, px, size, AnyWindowHandle, App, AppContext, Bounds, Context, Entity, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    PlatformInput, Point, Render, Styled, Window, WindowBounds, WindowOptions,
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
                    .markdown_extensions(super::extensions()),
            )
    }
}

fn drag(window: &mut Window, cx: &mut App, from: Point<Pixels>, to: Point<Pixels>) -> String {
    for event in [
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            click_count: 1,
            ..Default::default()
        }),
        PlatformInput::MouseMove(MouseMoveEvent {
            position: to,
            pressed_button: Some(MouseButton::Left),
            ..Default::default()
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            click_count: 1,
            ..Default::default()
        }),
    ] {
        window.dispatch_event(event, cx);
    }
    TextSelection::selected_text(window, cx).trim().to_owned()
}

#[test]
fn partial_math_selections_round_trip() {
    const FIRST: &str = "x_1 + x_2";
    const SECOND: &str = "y_1 + y_2";
    const DISPLAY: &str = "z_1 + z_2";
    let source =
        format!("Before \\$5 ${FIRST}$ and ${SECOND}$ after.\n\n> $$\n> {DISPLAY}\n> $$\n\nEnd.");
    gpui_kit::platform::headless().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        let state = cx.new(|cx| TextViewState::markdown(&source, cx));
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(0.), px(0.)),
                        size(px(600.), px(240.)),
                    ))),
                    show: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| SelectionView(state.clone())),
            )
            .unwrap();
        AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                window.draw(cx).clear(cx);
                let formula = |tex| {
                    window
                        .shot_element_bounds(&format!("formula:{tex}"))
                        .unwrap()
                };
                let first = formula(FIRST);
                let second = formula(SECOND);
                let display = formula(DISPLAY);
                let before = point(state.read(cx).bounds().left() + px(1.), first.center().y);
                let after = point(second.right() + px(200.), second.center().y);
                for (from, to, expected, maths) in [
                    (
                        first.center(),
                        after,
                        format!("${FIRST}$ and ${SECOND}$ after."),
                        &[FIRST, SECOND][..],
                    ),
                    (
                        before,
                        first.center(),
                        format!("Before \\$5 ${FIRST}$"),
                        &[FIRST][..],
                    ),
                    (
                        first.center(),
                        second.center(),
                        format!("${FIRST}$ and ${SECOND}$"),
                        &[FIRST, SECOND][..],
                    ),
                    (
                        first.origin + point(first.size.width / 10., first.size.height / 2.),
                        first.origin + point(first.size.width / 3., first.size.height / 2.),
                        format!("${FIRST}$"),
                        &[FIRST][..],
                    ),
                    (
                        display.origin + point(display.size.width / 5., display.size.height / 2.),
                        display.origin + point(display.size.width / 2., display.size.height / 2.),
                        format!("$$\n{DISPLAY}\n$$"),
                        &[DISPLAY][..],
                    ),
                    (
                        before,
                        display.center(),
                        format!("Before \\$5 ${FIRST}$ and ${SECOND}$ after.\n$$\n{DISPLAY}\n$$"),
                        &[FIRST, SECOND, DISPLAY][..],
                    ),
                ] {
                    for (from, to) in [(from, to), (to, from)] {
                        window.draw(cx).clear(cx);
                        let copied = drag(window, cx, from, to);
                        assert_eq!(copied, expected);
                        assert_eq!(state.read(cx).selected_text().trim(), copied);
                        let range = state.read(cx).selected_source_range().unwrap();
                        let selected_source = &source[range];
                        if maths == [DISPLAY] {
                            assert_eq!(selected_source, format!("$$\n> {DISPLAY}\n> $$"));
                        } else {
                            assert_eq!(formulas(selected_source), maths);
                        }
                        assert_eq!(formulas(&copied), maths);
                    }
                }
            })
            .unwrap();
        cx.spawn(async move |cx| {
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

fn formulas(markdown: &str) -> Vec<String> {
    use markdown::mdast::Node;

    fn collect(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::InlineMath(math) => out.push(math.value.clone()),
            Node::Math(math) => out.push(math.value.clone()),
            _ => {
                for child in node.children().into_iter().flatten() {
                    collect(child, out);
                }
            }
        }
    }

    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    let mut out = Vec::new();
    collect(&markdown::to_mdast(markdown, &options).unwrap(), &mut out);
    out
}
