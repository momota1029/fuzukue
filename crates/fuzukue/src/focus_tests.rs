use super::*;
use fuzukue_proto::{Actor, Envelope};
use gpui_kit::{
    AppContext as _, ClipboardItem, FocusHandle, Image, ImageFormat, Keystroke,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
};

const WORKSPACE: &str = "focus-fixture";
const THREAD: &str = "thread-a";

fn with_workbench(test: impl FnOnce(&Entity<Workbench>, &mut Window, &mut App) + 'static) {
    run_workbench(move |view, handle, cx| {
        handle.update(cx, |_, window, cx| test(view, window, cx)).unwrap();
    });
}

fn run_workbench(
    test: impl FnOnce(&Entity<Workbench>, gpui_kit::AnyWindowHandle, &mut App) + 'static,
) {
    gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        math::init(cx);
        let (window, view) = gpui_kit::open_window(
            gpui_kit::component::TitleBar::window_options(),
            cx,
            |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Offline fixture")))),
        ).unwrap();
        window.update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.workspaces.active = Some(WORKSPACE.into());
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.threads_loaded = true;
                view.switch_session(THREAD.into(), cx);
                view.input.read(cx).focus_handle(cx).focus(window, cx);
            });
            draw(window, cx);
        }).unwrap();
        test(&view, window, cx);
        cx.spawn(async move |cx| {
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
}

fn draw(window: &mut Window, cx: &mut App) {
    window.draw(cx).clear(cx);
}

fn composer(view: &Entity<Workbench>, cx: &App) -> FocusHandle {
    view.read(cx).input.read(cx).focus_handle(cx)
}

fn event(view: &Entity<Workbench>, window: &mut Window, cx: &mut App, event: ServerEvent) {
    view.update(cx, |view, cx| view.on_conn(ConnEvent::Event(Envelope {
        workspace: Some(WORKSPACE.into()),
        seq: 0,
        event,
    }), window, cx));
    draw(window, cx);
}

fn point_at(window: &Window, id: &str) -> Point<Pixels> {
    window.shot_element_bounds(id).unwrap_or_else(|| panic!("missing {id}")).center()
}

fn click(window: &mut Window, cx: &mut App, id: &str) {
    let position = point_at(window, id);
    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
        position, ..Default::default()
    }), cx);
    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
        position, click_count: 1, ..Default::default()
    }), cx);
    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
        position, click_count: 1, ..Default::default()
    }), cx);
    draw(window, cx);
}

fn key(window: &mut Window, cx: &mut App, key: &'static str) {
    window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx);
    draw(window, cx);
}

#[test]
fn send_stream_paste_switch_and_reconnect_keep_composer_focus() {
    with_workbench(|view, window, cx| {
        let focus = composer(view, cx);
        let (out, sent) = std::sync::mpsc::channel();
        view.update(cx, |view, cx| {
            view.out = out.into();
            view.input.update(cx, |input, cx| input.set_value("Fixture prompt", window, cx));
            cx.notify();
        });
        draw(window, cx);

        click(window, cx, "send-prompt");
        assert!(sent.try_iter().any(|msg| matches!(msg, ClientMsg::Prompt { .. })));
        assert!(view.read(cx).input.read(cx).value().is_empty());
        assert!(focus.is_focused(window), "clicking Send blurred the composer");

        for text in ["Streaming", " again"] {
            event(view, window, cx, ServerEvent::TextDelta {
                session: THREAD.into(), item: "reply".into(), actor: Actor::Claude, text: text.into(),
            });
            assert!(focus.is_focused(window), "streaming blurred the composer");
        }

        event(view, window, cx, ServerEvent::ChatCapabilities {
            effort_levels: vec![], mail_restricted: false, mcp_tools: vec![], folder_attachments: false,
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let image = Image::from_bytes(ImageFormat::Png, png.into_inner());
        let clipboard = ClipboardItem::new_image(&image);
        view.update(cx, |view, cx| assert!(view.paste_attachment(&clipboard, cx)));
        draw(window, cx);
        assert!(focus.is_focused(window), "queueing a pasted attachment blurred the composer");

        let id = view.read(cx).shot_last_attachment_id();
        event(view, window, cx, ServerEvent::AttachmentUploaded {
            id, path: Some("inbox/fixture.png".into()), error: None, proposal: None,
        });
        assert!(focus.is_focused(window), "attachment delivery blurred the composer");

        view.update(cx, |view, cx| view.switch_session("thread-b".into(), cx));
        draw(window, cx);
        assert!(focus.is_focused(window), "switching threads blurred the composer");

        view.update(cx, |view, cx| {
            view.on_conn(ConnEvent::Disconnected("fixture disconnect".into()), window, cx);
        });
        draw(window, cx);
        assert!(focus.is_focused(window), "disconnect blurred the composer");

        for conn in [ConnEvent::Reset(HttpTarget::default()), ConnEvent::Connected] {
            view.update(cx, |view, cx| view.on_conn(conn, window, cx));
            draw(window, cx);
            assert!(focus.is_focused(window), "connection reset blurred the composer");
        }
    });
}

#[test]
fn palette_dismissal_returns_to_previous_field() {
    with_workbench(|view, window, cx| {
        let composer = composer(view, cx);
        view.update(cx, |view, cx| view.open_palette(window, cx));
        draw(window, cx);
        let search = view.read(cx).palette.as_ref().unwrap().read(cx).focus_handle(cx);
        assert!(search.is_focused(window));

        key(window, cx, "escape");
        assert!(view.read(cx).palette.is_none());
        assert!(composer.is_focused(window), "closing search lost the original focus");

        view.update(cx, |view, cx| view.open_source_file("fixture.rs".into(), None, window, cx));
        let id = view.read(cx).source.active_id().unwrap();
        view.update(cx, |view, cx| view.complete_source_load_for_shot(id, "fn fixture() {}", window, cx));
        draw(window, cx);
        let editor = view.read(cx).source.active_editor().unwrap().read(cx).focus_handle(cx);
        assert!(editor.is_focused(window));
        view.update(cx, |view, cx| view.open_palette(window, cx));
        draw(window, cx);
        key(window, cx, "escape");
        assert!(editor.is_focused(window), "closing search should return to the editor that opened it");
    });
}

#[test]
fn approval_updates_keep_composer_focus_and_recover_removed_controls() {
    with_workbench(|view, window, cx| {
        let focus = composer(view, cx);
        view.update(cx, |view, cx| view.seed_approval_dock_fixture(window, cx));
        draw(window, cx);
        assert!(focus.is_focused(window), "opening the approval dock blurred the composer");

        click(window, cx, "approval-details-fixture-0");
        assert!(window.shot_element_bounds("approval-request-details").is_some());
        assert!(!focus.is_focused(window), "the approval control did not receive focus");
        view.update(cx, |view, cx| view.resolve_approval_dock_fixture(window, cx));
        draw(window, cx);
        assert!(focus.is_focused(window), "removing the focused approval left nowhere to type");
    });
}

#[test]
fn incoming_file_open_keeps_composer_focus_for_loading_and_loaded_tabs() {
    with_workbench(|view, window, cx| {
        let focus = composer(view, cx);
        let open = || ServerEvent::OpenFile { session: THREAD.into(), path: "fixture.rs".into(), lines: None };
        event(view, window, cx, open());
        let id = view.read(cx).source.active_id().unwrap();
        view.update(cx, |view, cx| view.complete_source_load_for_shot(id, "fn fixture() {}", window, cx));
        draw(window, cx);
        assert!(view.read(cx).source.active_editor().is_some());
        assert!(focus.is_focused(window), "file delivery stole focus from the composer");

        event(view, window, cx, open());
        assert!(focus.is_focused(window), "an already loaded file stole focus from the composer");

        let editor = view.read(cx).source.active_editor().unwrap().read(cx).focus_handle(cx);
        editor.focus(window, cx);
        draw(window, cx);
        event(view, window, cx, ServerEvent::OpenFile {
            session: THREAD.into(), path: "other.rs".into(), lines: None,
        });
        assert_eq!(view.read(cx).source.active_id(), Some(id));
        assert!(editor.is_focused(window), "the incoming preview replaced the focused editor");
    });
}

#[test]
fn pending_editor_load_leaves_a_newly_focused_field_alone() {
    with_workbench(|view, window, cx| {
        view.update(cx, |view, cx| view.open_source_file("fixture.rs".into(), None, window, cx));
        let id = view.read(cx).source.active_id().unwrap();
        view.update(cx, |view, cx| view.open_palette(window, cx));
        draw(window, cx);
        let search = view.read(cx).palette.as_ref().unwrap().read(cx).focus_handle(cx);
        assert!(search.is_focused(window));

        view.update(cx, |view, cx| view.complete_source_load_for_shot(id, "fn fixture() {}", window, cx));
        draw(window, cx);
        assert!(search.is_focused(window), "editor load stole focus from search");
    });
}

#[test]
fn disconnect_closes_file_browser_without_reverting_a_new_focus() {
    with_workbench(|view, window, cx| {
        view.update(cx, |view, cx| view.open_source_file("fixture.rs".into(), None, window, cx));
        let id = view.read(cx).source.active_id().unwrap();
        view.update(cx, |view, cx| view.complete_source_load_for_shot(id, "fn fixture() {}", window, cx));
        draw(window, cx);
        let editor = view.read(cx).source.active_editor().unwrap().read(cx).focus_handle(cx);
        assert!(editor.is_focused(window));

        view.update(cx, |view, cx| view.open_file_browser(window, cx));
        draw(window, cx);
        click(window, cx, "chat-input");
        let focus = composer(view, cx);
        assert!(focus.is_focused(window));
        view.update(cx, |view, cx| view.on_conn(ConnEvent::Disconnected("fixture disconnect".into()), window, cx));
        draw(window, cx);

        assert!(focus.is_focused(window), "the closed file browser reverted focus to its old owner");
    });
}

#[test]
fn screenshot_preview_dismissal_restores_the_composer() {
    with_workbench(|view, window, cx| {
        let focus = composer(view, cx);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1).write_to(&mut png, image::ImageFormat::Png).unwrap();
        view.update(cx, |view, cx| {
            view.stub_screenshot_capture(Ok(png.into_inner()));
            view.capture_screenshot(window, cx);
        });
        draw(window, cx);
        assert!(view.read(cx).screenshots.preview_open());
        assert!(!focus.is_focused(window), "the screenshot note did not take focus");

        key(window, cx, "escape");
        assert!(!view.read(cx).screenshots.preview_open());
        assert!(focus.is_focused(window), "closing the screenshot note lost the original focus");
    });
}

#[test]
fn file_resolution_preserves_focus_moved_after_the_request() {
    use fuzukue_proto::{FileEntry, FileKind, FilePage, ResolvedFile};

    with_workbench(|view, window, cx| {
        let focus = composer(view, cx);
        let (out, sent) = std::sync::mpsc::channel();
        view.update(cx, |view, cx| {
            view.out = out.into();
            view.open_file_browser(window, cx);
        });
        draw(window, cx);
        let request = sent.try_iter().find_map(|msg| match msg {
            ClientMsg::ListFiles { request, .. } => Some(request),
            _ => None,
        }).unwrap();
        event(view, window, cx, ServerEvent::FileList {
            request,
            result: Ok(FilePage {
                entries: vec![FileEntry {
                    path: "fixture.rs".into(), name: "fixture.rs".into(),
                    kind: FileKind::File, size: 10, modified_ms: None,
                }],
                truncated: false,
            }),
        });
        click(window, cx, "file-browser-open");
        let request = sent.try_iter().find_map(|msg| match msg {
            ClientMsg::ResolveFile { request, .. } => Some(request),
            _ => None,
        }).unwrap();
        click(window, cx, "chat-input");
        assert!(focus.is_focused(window));

        event(view, window, cx, ServerEvent::FileResolved {
            request, result: Ok(ResolvedFile { absolute: None, deck: None }),
        });
        assert!(focus.is_focused(window), "file resolution took focus from the composer");
        let id = view.read(cx).source.active_id().unwrap();
        view.update(cx, |view, cx| view.complete_source_load_for_shot(id, "fn fixture() {}", window, cx));
        draw(window, cx);
        assert!(focus.is_focused(window), "loading the resolved file took focus from the composer");
    });
}

#[test]
fn dialog_and_attachment_popup_restore_the_composer() {
    run_workbench(|view, handle, cx| {
        let focus = composer(view, cx);
        handle.update(cx, |_, window, cx| {
            click(window, cx, "attach-file");
            assert!(!focus.is_focused(window), "attachment menu did not take focus");
        }).unwrap();
        handle.update(cx, |_, window, cx| key(window, cx, "escape")).unwrap();
        handle.update(cx, |_, window, cx| {
            draw(window, cx);
            assert!(focus.is_focused(window), "attachment menu did not restore focus");

            window.open_dialog(cx, |dialog, _, _| dialog.title("Fixture dialog"));
            draw(window, cx);
            assert!(!focus.is_focused(window), "modal dialog did not take focus");
            window.close_dialog(cx);
            draw(window, cx);
            assert!(focus.is_focused(window), "dialog did not restore focus");
        }).unwrap();
    });
}
