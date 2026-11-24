use super::*;
use crate::pane_resize::PANE_INSET;

fn source_view(runner: &Runner, cx: &mut AsyncApp, settings: &Path) -> Result<()> {
    runner.update(cx, |view, window, cx| {
        view.pane_resize = crate::pane_resize::PaneResize::load(Some(settings.to_owned()), cx);
        seed_thread_switch(view, cx);
        view.open_source_file("experiment.rs".into(), None, window, cx);
        view.complete_source_load_for_shot(
            view.source.active_id().unwrap(),
            include_str!("../../../../tests/shots/fixtures/editor/experiment.rs"),
            window,
            cx,
        );
    })?;
    runner.frame(cx)
}

fn new_runner(handle: AnyWindowHandle, view: Entity<Workbench>) -> Runner {
    Runner {
        handle,
        view,
        target: None,
        output: PathBuf::new(),
        read_only: false,
        seen: HashMap::new(),
    }
}

fn double_click(runner: &Runner, cx: &mut AsyncApp, position: gpui_kit::Point<gpui_kit::Pixels>) -> Result<()> {
    cx.update_window(runner.handle, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }),
            cx,
        );
        for click_count in [1, 2] {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent { position, click_count, ..Default::default() }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent { position, click_count, ..Default::default() }),
                cx,
            );
        }
    })?;
    runner.frame(cx)
}

#[test]
fn thread_info_close_stays_inside_pane_without_shifting_chat_header() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.spawn(async move |cx| {
                let outcome = async {
                    for width in [1360., 800.] {
                        let (handle, view) =
                            cx.update(|cx| open_workbench_at_size(cx, width, 700.))?;
                        let mut runner = new_runner(handle, view);
                        let config = tempfile::tempdir()?;
                        runner.update(cx, |view, _, _| {
                            view.appearance.isolate_shot_settings(config.path().join("settings.toml"));
                        })?;
                        runner.step(Step::SeedThreadSwitch, cx).await?;
                        runner.step(Step::AssertThreadInfoLayout { open: false }, cx).await?;
                        for close in ["close-thread-details", "thread-info-toggle", "Escape"] {
                            let toggle = ClickTarget::Element { id: "thread-info-toggle".into() };
                            runner.click(cx, toggle)?;
                            runner.step(Step::AssertThreadInfoLayout { open: true }, cx).await?;
                            runner.update(cx, |_, window, _| {
                                let pane = window.shot_element_bounds("workbench-right-pane").unwrap();
                                let content = window.shot_element_bounds("thread-tests").unwrap();
                                assert_eq!(content.left() - pane.left(), PANE_INSET);
                            })?;
                            if close == "Escape" {
                                runner.key(cx, close)?;
                            } else {
                                runner.click(cx, ClickTarget::Element { id: close.into() })?;
                            }
                            runner.step(Step::AssertThreadInfoLayout { open: false }, cx).await?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn pane_headers_keep_close_buttons_inside_their_panes_at_both_widths() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.spawn(async move |cx| {
                let outcome = async {
                    for width in [1360., 800.] {
                        let (handle, view) =
                            cx.update(|cx| open_workbench_at_size(cx, width, 700.))?;
                        let mut runner = new_runner(handle, view);
                        let theme = if width == 800. { ShotTheme::Dark } else { ShotTheme::Light };
                        runner.step(Step::Theme { theme }, cx).await?;
                        runner.step(Step::SeedThreadSwitch, cx).await?;
                        runner.update(cx, |view, _, cx| {
                            view.on_event(ServerEvent::Cards {
                                deck: view.deck.clone(),
                                cards: vec![serde_json::from_value(serde_json::json!({
                                    "id": "layout-card", "kind": "design", "title": "Layout card",
                                    "status": "", "path": "cards/layout-card.md", "body": "Card body",
                                    "links": [], "backlinks": [], "files": [], "external": false,
                                })).unwrap()],
                                diagnostics: vec![],
                            }, cx);
                        })?;
                        runner.frame(cx)?;
                        let closed_header_y = runner.update(cx, |_, window, _| {
                            window.shot_element_bounds("thread-header").unwrap().top()
                        })?;

                        let click = |runner: &Runner, cx: &mut AsyncApp, id: &str| {
                            runner.click(cx, ClickTarget::Element { id: id.into() })?;
                            runner.frame(cx)
                        };
                        let gear = crate::menu_bar::SETTINGS_BUTTON;
                        click(&runner, cx, gear)?;
                        runner.update(cx, |view, _, _| assert!(view.settings_open))?;
                        click(&runner, cx, "settings-agents")?;
                        click(&runner, cx, "settings-agent-history")?;
                        runner.update(cx, |view, _, _| assert!(view.settings_open))?;
                        click(&runner, cx, gear)?;
                        runner.update(cx, |view, _, _| assert!(!view.settings_open))?;
                        for open in [true, false] {
                            runner.click_menu(cx, "menu-help", "tool-settings")?;
                            runner.frame(cx)?;
                            runner.update(cx, |view, _, _| assert_eq!(view.settings_open, open))?;
                        }
                        for open in [true, false] {
                            runner.key(cx, "Ctrl+O")?;
                            runner.frame(cx)?;
                            runner.update(cx, |view, _, _| assert_eq!(view.right == crate::RightPane::Files, open))?;
                        }

                        for (header_id, close_id, content_id, editing) in [
                            ("settings-pane-header", "close-settings", "settings-pane-title", false),
                            ("file-browser-header", "close-file-browser", "file-browser-breadcrumb", false),
                            ("workbench-pane-header", "close-pane", "card-file-breadcrumb", false),
                            ("workbench-pane-header", "close-pane", "card-file-breadcrumb", true),
                            ("paper-pane-header", "close-paper", "paper-file-breadcrumb", false),
                            ("source-pane-header", "close-source", "source-tabs", false),
                            ("history-pane-header", "history-close", "history-pane-title", false),
                        ] {
                            for escape in [false, true] {
                                runner.update(cx, |view, window, cx| match close_id {
                                    "close-settings" => view.show_sidebar_settings(false, cx),
                                    "close-file-browser" => view.open_file_browser(window, cx),
                                    "close-pane" => {
                                        view.select("layout-card".into(), cx);
                                        if editing {
                                            view.install_card_file_editor(
                                                "cards/layout-card.md".into(), "Card body".into(), window, cx,
                                            );
                                        }
                                    }
                                    "close-paper" => {
                                        view.open_paper_deck(view.deck.clone(), cx);
                                        view.files.paper_file = Some((view.deck.clone(), "papers/reader.tex".into()));
                                    }
                                    "close-source" => {
                                        view.open_source_file("experiment.rs".into(), None, window, cx);
                                        view.complete_source_load_for_shot(
                                            view.source.active_id().unwrap(),
                                            "fn main() {}",
                                            window,
                                            cx,
                                        );
                                    }
                                    "history-close" => {
                                        view.open_history(fuzukue_proto::HistoryScope::Global);
                                        cx.notify();
                                    }
                                    _ => unreachable!(),
                                })?;
                                runner.frame(cx)?;
                                runner.update(cx, |_, window, _| -> Result<()> {
                                    let bounds = |id: &str| window.shot_element_bounds(id)
                                        .with_context(|| format!("{id} is missing at {width}px ({close_id})"));
                                    let chat = bounds("thread-header")?;
                                    let pane = bounds(match close_id {
                                        "history-close" => "history-panel",
                                        "close-paper" => "paper-centre-column",
                                        _ => "workbench-right-pane",
                                    })?;
                                    let header = bounds(header_id)?;
                                    let close = bounds(close_id)?;
                                    let content = bounds(content_id)?;
                                    ensure!(chat.top() == closed_header_y,
                                        "{header_id} moved the chat header at {width}px");
                                    ensure!(header.left() >= pane.left() && header.right() <= pane.right()
                                        && header.top() >= pane.top() && header.bottom() <= pane.bottom(),
                                        "{header_id} extends beyond its pane at {width}px");
                                    ensure!(close.left() >= header.left() && close.right() <= header.right()
                                        && close.top() >= header.top() && close.bottom() <= header.bottom(),
                                        "{close_id} extends beyond its header at {width}px");
                                    ensure!(content.left() - pane.left() == PANE_INSET,
                                        "{content_id} inset at {width}px is {}", content.left() - pane.left());
                                    ensure!(content.top() < close.bottom() && content.bottom() > close.top()
                                        && content.right() <= close.left(),
                                        "{close_id} does not share the content row at {width}px");
                                    if close_id != "history-close" {
                                        let split = bounds("right-pane-splitter")?;
                                        let divider = bounds("right-pane-divider")?;
                                        ensure!(split.size.width == px(8.) && divider.size.width == px(1.)
                                            && divider.left() == split.left() && divider.size.height == split.size.height,
                                            "splitter has unexpected hit area or divider at {width}px");
                                        ensure!(split.top() == chat.top() && pane.top() == chat.top(),
                                            "a row sits above the split at {width}px ({close_id})");
                                        let (left, right) = if close_id == "close-paper" {
                                            (header, chat)
                                        } else {
                                            (chat, header)
                                        };
                                        ensure!(left.right() <= divider.left() && right.left() >= divider.right(),
                                            "{header_id} spans the chat and pane columns at {width}px");
                                        let flush = if close_id == "close-paper" {
                                            pane.right() == divider.left()
                                        } else {
                                            pane.left() == divider.right()
                                        };
                                        ensure!(flush, "divider did not meet the pane edge at {width}px ({close_id})");
                                    }
                                    Ok(())
                                })??;
                                if escape {
                                    runner.key(cx, "Escape")?;
                                } else {
                                    runner.click(cx, ClickTarget::Element { id: close_id.into() })?;
                                }
                                runner.frame(cx)?;
                                if editing {
                                    runner.update(cx, |view, window, _| {
                                        assert!(view.editing.is_none());
                                        assert!(window.shot_element_bounds("card-file-breadcrumb").is_some());
                                    })?;
                                    runner.click(cx, ClickTarget::Element { id: close_id.into() })?;
                                    runner.frame(cx)?;
                                }
                                runner.update(cx, |_, window, _| -> Result<()> {
                                    ensure!(window.shot_element_bounds(close_id).is_none()
                                        && window.shot_element_bounds("workbench-right-pane").is_none()
                                        && window.shot_element_bounds("history-panel").is_none(),
                                        "{close_id} did not close its pane at {width}px (escape={escape})");
                                    ensure!(window.shot_element_bounds("thread-header").unwrap().top() == closed_header_y,
                                        "closing {header_id} moved the chat header at {width}px");
                                    Ok(())
                                })??;
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn pane_resize_drags_keeps_chat_anchor_resets_and_restores_in_new_windows() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            cx.spawn(async move |cx| {
                let outcome = (|| -> Result<()> {
                    let dir = tempfile::tempdir()?;
                    let settings = dir.path().join("settings.toml");
                    let state_path = settings.with_extension("state.toml");
                    std::fs::write(&settings, "# user settings\n")?;

                    let mut runner = new_runner(handle, view);
                    source_view(&runner, cx, &settings)?;
                    runner.update(cx, |view, _, _| {
                        view.appearance.isolate_shot_settings(settings.clone());
                    })?;
                    runner.frame(cx)?;
                    let sidebar = |runner: &Runner, cx: &mut AsyncApp| {
                        runner.update(cx, |_, window, _| {
                            let sidebar = window.shot_element_bounds("workbench-sidebar").unwrap();
                            let hit = window.shot_element_bounds("sidebar-splitter").unwrap();
                            let line = window.shot_element_bounds("sidebar-divider").unwrap();
                            assert_eq!(hit.size.width, px(8.));
                            assert_eq!(line.size.width, px(1.));
                            assert_eq!(line.left(), sidebar.right());
                            assert_eq!(line.left(), hit.left());
                            assert_eq!(line.size.height, hit.size.height);
                            (sidebar.size.width, point(hit.right() - px(1.), hit.center().y))
                        })
                    };
                    let default_sidebar = px(fuzukue_view::appearance::AppearanceSettings::default().sidebar_width);
                    let (initial, start) = sidebar(&runner, cx)?;
                    runner.drag(cx, start, start + point(px(32.), px(0.)))?;
                    runner.frame(cx)?;
                    assert_eq!(sidebar(&runner, cx)?.0, initial + px(32.));
                    runner.key(cx, "left")?;
                    runner.frame(cx)?;
                    let (resized, hit) = sidebar(&runner, cx)?;
                    assert_eq!(resized, initial + px(16.));
                    let saved_settings = crate::settings_file::read(&settings)?;
                    assert_eq!(saved_settings["sidebar_width"].as_float(), Some(f64::from(f32::from(resized))));
                    double_click(&runner, cx, hit)?;
                    assert_eq!(sidebar(&runner, cx)?.0, default_sidebar);
                    std::fs::write(&settings, "# user settings\n")?;

                    let width = |runner: &Runner, cx: &mut AsyncApp| {
                        runner.update(cx, |_, window, _| {
                            let pane = window.shot_element_bounds("workbench-right-pane").unwrap();
                            let editor = window.shot_element_bounds("source-pane").unwrap();
                            assert!((pane.size.width - editor.size.width).abs() <= px(1.));
                            pane.size.width
                        })
                    };
                    let handle_center = |runner: &Runner, cx: &mut AsyncApp| {
                        runner.update(cx, |_, window, _| {
                            let bounds = window.shot_element_bounds("right-pane-splitter").unwrap();
                            assert_eq!(bounds.size.width, px(8.));
                            bounds.center()
                        })
                    };

                    let anchor = gpui_kit::ListOffset {
                        item_ix: 137,
                        offset_in_item: px(9.),
                    };
                    runner.update(cx, |view, _, cx| {
                        view.chats[&view.active].scroller.scroll_to(anchor);
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    let default_width = width(&runner, cx)?;
                    let start = handle_center(&runner, cx)?;
                    runner.drag(cx, start, start - point(px(96.), px(0.)))?;
                    runner.frame(cx)?;
                    let resized = width(&runner, cx)?;
                    assert!((resized - default_width - px(96.)).abs() <= px(1.));
                    assert_eq!(
                        runner.update(cx, |view, _, _| view.chats[&view.active]
                            .scroller
                            .logical_scroll_top())?,
                        anchor
                    );
                    assert!(
                        (px(crate::settings_file::read(&state_path)?["right_pane_width"]
                            .as_float()
                            .unwrap() as f32)
                            - resized)
                            .abs()
                            <= px(1.)
                    );
                    assert_eq!(std::fs::read_to_string(&settings)?, "# user settings\n");

                    for (key, adjustment) in [("right", -16.), ("shift-left", 64.)] {
                        let previous = width(&runner, cx)?;
                        runner.key(cx, key)?;
                        runner.frame(cx)?;
                        assert!((width(&runner, cx)? - previous - px(adjustment)).abs() <= px(1.));
                    }
                    let saved = width(&runner, cx)?;

                    for window_width in [980., 1440.] {
                        let (handle, view) =
                            cx.update(|cx| open_workbench_at_size(cx, window_width, 900.))?;
                        runner.update(cx, |_, window, _| window.remove_window())?;
                        runner = new_runner(handle, view);
                        source_view(&runner, cx, &settings)?;
                        let max = runner.update(cx, |_, window, _| {
                            let sidebar = window
                                .shot_element_bounds("workbench-sidebar")
                                .unwrap()
                                .size
                                .width;
                            window.viewport_size().width - sidebar - px(360. + 2.)
                        })?;
                        assert!((width(&runner, cx)? - saved.min(max)).abs() <= px(1.));
                    }

                    let position = handle_center(&runner, cx)?;
                    double_click(&runner, cx, position)?;
                    assert!((width(&runner, cx)? - default_width).abs() <= px(1.));
                    assert!(
                        !crate::settings_file::read(&state_path)?.contains_key("right_pane_width")
                    );
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}
