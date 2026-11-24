use super::*;
use gpui_kit::{AppContext, Bounds, WindowBounds, WindowOptions};

#[test]
fn card_tex_preview_conflict_and_build_result() {
    const DRAFT: &str = "% user's draft\r\n\\begin{align}x &= 2\\end{align}\r\n";
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::math::init(cx);
            crate::appearance::register_window_typography(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    gpui_kit::size(px(1440.), px(900.)),
                ))),
                ..gpui_kit::component::TitleBar::window_options()
            };
            let (handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected"))))
            }).unwrap();
            cx.spawn(async move |cx| {
                let check = cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |view, cx| {
                    let (out, messages) = std::sync::mpsc::channel();
                    view.out = out.into();
                    view.deck = "papers/test".into();
                    let part = ReadPart { card: "lemma".into(), proof: false };
                    let source = |tex| CardEditSource {
                        body: "Original statement.\r\n".into(),
                        base_hash: "original".into(),
                        tex,
                    };
                    view.open_card_editor(
                        part.clone(), source(Access::ImportedManuscript), true, window, cx,
                    );
                    assert!(view.card_editor.is_none());
                    assert_eq!(view.notice.as_deref(), Some(tr!("paper-tex-imported-manuscript")));

                    view.open_card_editor(part, source(Access::Editable), true, window, cx);
                    let input = view.card_editor.as_ref().unwrap().input.clone();
                    input.update(cx, |input, cx| input.set_value("Invalid:\n{", window, cx));
                    view.request_save_card_edit(window, cx);
                    let invalid = view.card_editor.as_ref().unwrap().preview(cx).unwrap();
                    assert_eq!(invalid.validation.unwrap_err().line, 2);
                    view.save_card_edit(false, window, cx);
                    assert!(messages.try_recv().is_err());

                    input.update(cx, |input, cx| input.set_value(DRAFT, window, cx));
                    assert!(view.card_editor.as_ref().unwrap().preview(cx).is_none());
                    view.request_save_card_edit(window, cx);
                    let preview = view.card_editor.as_ref().unwrap().preview(cx).unwrap();
                    assert!(preview.diff.contains("Original statement."));
                    assert!(preview.diff.contains("% user's draft"));
                    assert!(messages.try_recv().is_err());
                    view.save_card_edit(false, window, cx);
                    let ClientMsg::SaveCard { request, part, body, base_hash, .. } = messages.try_recv().unwrap()
                    else { panic!("expected versioned TeX save") };
                    assert_eq!((part, body.as_str(), base_hash.as_str()), (CardEditPart::TexBody, DRAFT, "original"));

                    view.on_card_save_result(request, CardSaveOutcome::Conflict {
                        latest: CardEditSource { body: "On disk.\r\n".into(), base_hash: "latest".into(), tex: Access::Editable },
                    }, cx);
                    let preview = view.card_editor.as_ref().unwrap().preview(cx).unwrap();
                    assert!(preview.diff.contains("On disk."));
                    assert_eq!(preview.draft, DRAFT);
                    assert!(messages.try_recv().is_err());

                    view.save_card_edit(true, window, cx);
                    let ClientMsg::SaveCard { request, body, base_hash, .. } = messages.try_recv().unwrap()
                    else { panic!("expected reviewed retry") };
                    assert_eq!((body.as_str(), base_hash.as_str()), (DRAFT, "latest"));
                    view.on_card_save_result(request, CardSaveOutcome::Saved, cx);
                    assert!(view.card_editor.is_none());
                    view.on_card_tex_build_result(request - 1, BuildOutcome::Built { pdf: "stale.pdf".into() });
                    assert!(view.card_tex_build.as_ref().unwrap().outcome.is_none());
                    view.on_card_tex_build_result(request, BuildOutcome::Failed {
                        log: "../cards/lemma/statement.tex:2: Undefined control sequence.".into(),
                        log_path: Some("papers/test/build/fuzukue-latexmk-output.txt".into()),
                    });
                    assert!(matches!(view.card_tex_build.as_ref().unwrap().outcome,
                        Some(BuildOutcome::Failed { ref log, .. }) if log.contains("statement.tex:2:")));
                    });
                });
                result.send(check).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
    completed.recv().unwrap().unwrap();
}
