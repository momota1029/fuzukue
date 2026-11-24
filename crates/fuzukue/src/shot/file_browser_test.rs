use super::*;
use fuzukue_proto::{Envelope, FileEntry, FileKind, FilePage, FILE_BROWSER_PROTOCOL};
use gpui_kit::component::Theme;

#[test]
fn file_browser_keeps_folder_labels_visible_at_narrow_widths() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 1250.).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, _| {
                view.protocol = FILE_BROWSER_PROTOCOL;
                view.out = out.into();
            });
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle,
                    view,
                    target: None,
                    output: PathBuf::new(),
                    read_only: false,
                    seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    runner.key(cx, "Ctrl+O")?;
                    let mut paths = vec![String::new()];
                    for name in [
                        "論文",
                        "下書き",
                        "章",
                        "節",
                        "長いフォルダー名の表示を確認する",
                    ] {
                        let dir = paths.last().unwrap();
                        let ClientMsg::ListFiles {
                            request,
                            dir: requested,
                            ..
                        } = messages.try_recv()?
                        else {
                            bail!("expected a file listing request for {dir:?}");
                        };
                        assert_eq!(&requested, dir);
                        let path = if dir.is_empty() {
                            name.into()
                        } else {
                            format!("{dir}/{name}")
                        };
                        let page = FilePage {
                            entries: vec![FileEntry {
                                path: path.clone(),
                                name: name.into(),
                                kind: FileKind::Directory,
                                size: 0,
                                modified_ms: None,
                            }],
                            truncated: false,
                        };
                        runner.update(cx, |view, window, cx| {
                            view.on_conn(
                                conn::ConnEvent::Event(Envelope {
                                    seq: 0,
                                    workspace: None,
                                    event: ServerEvent::FileList {
                                        request,
                                        result: Ok(page),
                                    },
                                }),
                                window,
                                cx,
                            );
                        })?;
                        runner.frame(cx)?;
                        runner.click(
                            cx,
                            ClickTarget::Element {
                                id: format!("file-tree-expand-{path}"),
                            },
                        )?;
                        paths.push(path);
                    }

                    // Keep the saved splitter widths isolated from the user's state.
                    let settings_dir = tempfile::tempdir()?;
                    let settings = settings_dir.path().join("settings.toml");
                    for (width, font_size, stacked, tree_width) in [
                        (600., 16., false, 208.),
                        (280., 16., false, 144.),
                        (280., 24., true, 280.),
                    ] {
                        std::fs::write(
                            settings.with_extension("state.toml"),
                            format!("right_pane_width = {width}"),
                        )?;
                        runner.update(cx, |view, _, cx| {
                            view.pane_resize =
                                crate::pane_resize::PaneResize::load(Some(settings.clone()), cx);
                            Theme::update(cx, |theme| theme.font_size = px(font_size));
                            cx.notify();
                        })?;
                        runner.frame(cx)?;
                        runner.assert_file_tree_labels(cx, &paths)?;
                        runner.update(cx, |_, window, _| {
                            assert_eq!(window.rem_size(), px(font_size));
                            let tree = window.shot_element_bounds("file-browser-tree").unwrap();
                            let list = window.shot_element_bounds("file-browser-list").unwrap();
                            assert!((tree.size.width - px(tree_width)).abs() <= px(1.));
                            if stacked {
                                assert!(list.top() >= tree.bottom());
                                assert!((list.size.width - tree.size.width).abs() <= px(1.));
                            } else {
                                assert!((list.left() - tree.right()).abs() <= px(1.));
                            }
                            assert!(window.shot_element_bounds("file-row-論文").is_some());
                        })?;
                    }
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}
