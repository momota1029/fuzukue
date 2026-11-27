use super::*;
use fuzukue_proto::{GitHubReportAction, GitHubReportUpdate};

#[test]
fn github_report_preview_sends_the_edited_fields_only_after_a_click() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1200., 760.).unwrap();
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
                    let (sender, messages) = std::sync::mpsc::channel();
                    runner.update(cx, |view, _, cx| {
                        view.out = sender.into();
                        seed_github_report(view, Some("reporter"), cx);
                    })?;
                    runner.click_menu(cx, "menu-help", "menu-report-bug")?;
                    runner.frame(cx)?;
                    assert!(messages.try_recv().is_err(), "opening must not submit");
                    let click = |cx: &mut AsyncApp, id: &str| {
                        runner.click(cx, ClickTarget::Element { id: id.into() })
                    };
                    click(cx, "github-report-title-input")?;
                    runner.key(cx, "Ctrl+a")?;
                    runner.type_text(cx, "Edited title")?;
                    click(cx, "github-report-body-input")?;
                    runner.key(cx, "Ctrl+a")?;
                    runner.type_text(cx, "Edited report")?;
                    runner.frame(cx)?;
                    assert!(messages.try_recv().is_err(), "editing must not submit");
                    click(cx, "github-report-send")?;
                    let ClientMsg::GitHubReport { request, action } = messages.try_recv()? else {
                        anyhow::bail!("expected a report submission");
                    };
                    assert_eq!(
                        action,
                        GitHubReportAction::Submit {
                            repo: "example/project".into(),
                            login: "reporter".into(),
                            title: "Edited title".into(),
                            body: "Edited report".into(),
                        }
                    );
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::GitHubReportResult {
                                request,
                                result: Ok(GitHubReportUpdate::Submitted {
                                    url: "https://github.com/example/project/issues/42".into(),
                                }),
                            },
                            cx,
                        )
                    })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window
                        .shot_element_bounds("github-report-issue-url")
                        .is_some())?);
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn screenshot_button_opens_the_report_and_shows_unavailable_states() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1200., 850.).unwrap();
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
                    let (sender, messages) = std::sync::mpsc::channel();
                    let png = screenshot_fixture_png()?;
                    runner.update(cx, |view, _, cx| {
                        view.out = sender.into();
                        seed_github_report(view, Some("reporter"), cx);
                    })?;
                    let visible = |cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| {
                            window.shot_element_bounds(id).is_some_and(|bounds| {
                                window.visual_viewport_bounds().contains(&bounds.center())
                            })
                        })
                    };
                    let click = |cx: &mut AsyncApp, id: &str| {
                        runner.click(cx, ClickTarget::Element { id: id.into() })
                    };
                    let capture = |cx: &mut AsyncApp, result: Result<Vec<u8>>| {
                        runner.update(cx, |view, _, _| view.stub_screenshot_capture(result))?;
                        runner.frame(cx)?;
                        runner.key(cx, "Ctrl+Shift+S")?;
                        runner.frame(cx)
                    };

                    capture(cx, Ok(png.clone()))?;
                    ensure!(
                        visible(cx, "screenshot-preview")?,
                        "capture preview missing"
                    );
                    click(cx, "screenshot-note")?;
                    let note = "The workbench stopped updating after switching threads.";
                    runner.type_text(cx, note)?;
                    click(cx, "screenshot-report")?;
                    runner.frame(cx)?;
                    ensure!(!visible(cx, "screenshot-preview")?, "preview must close");
                    for id in [
                        "github-report-destination",
                        "github-report-title-input",
                        "github-report-body-input",
                        "github-report-screenshot",
                        "github-report-save-screenshot",
                        "github-report-browser",
                    ] {
                        ensure!(visible(cx, id)?, "report element missing: {id}");
                    }
                    ensure!(messages.try_recv().is_err(), "opening must send no request");
                    click(cx, "github-report-send")?;
                    let ClientMsg::GitHubReport {
                        request,
                        action:
                            GitHubReportAction::Submit {
                                repo,
                                login,
                                title,
                                body,
                            },
                    } = messages.try_recv()?
                    else {
                        bail!("expected the OAuth report submission");
                    };
                    ensure!(
                        repo == "example/project" && login == "reporter",
                        "wrong report destination or sender"
                    );
                    ensure!(
                        fuzukue_i18n::Locale::available().any(|locale| {
                            title
                                == format!(
                                    "{} {}",
                                    locale.text("github-report-title-prefix"),
                                    locale.text("github-report-screenshot-title")
                                )
                        }),
                        "screenshot title was not prefilled: {title}"
                    );
                    ensure!(
                        body.starts_with(note) && body.contains(env!("CARGO_PKG_VERSION")),
                        "screenshot note or diagnostics missing: {body}"
                    );
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::GitHubReportResult {
                                request,
                                result: Ok(GitHubReportUpdate::Submitted {
                                    url: "https://github.com/example/project/issues/42".into(),
                                }),
                            },
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    ensure!(
                        visible(cx, "github-report-issue-url")?
                            && visible(cx, "github-report-save-screenshot")?,
                        "the created issue and screenshot must both remain available"
                    );
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;

                    runner.update(cx, |view, _, cx| seed_github_report(view, None, cx))?;
                    capture(cx, Ok(png.clone()))?;
                    click(cx, "screenshot-report")?;
                    runner.frame(cx)?;
                    ensure!(
                        visible(cx, "github-report-account-disconnected")?
                            && visible(cx, "github-connect")?
                            && visible(cx, "github-report-browser")?
                            && !visible(cx, "github-report-send")?,
                        "a missing token must show account and browser options"
                    );
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;

                    runner.update(cx, |view, _, cx| {
                        view.status = crate::ConnectionStatus::Connecting;
                        view.github_report_disconnected(cx);
                    })?;
                    capture(cx, Ok(png))?;
                    click(cx, "screenshot-report")?;
                    runner.frame(cx)?;
                    ensure!(
                        visible(cx, "github-report-server-required")?
                            && visible(cx, "github-report-screenshot")?
                            && !visible(cx, "github-report-send")?,
                        "offline reporting must show why app submission is unavailable"
                    );
                    runner.key(cx, "escape")?;

                    capture(cx, Err(anyhow!("simulated screenshot failure")))?;
                    ensure!(
                        visible(cx, "screenshot-feedback")?
                            && !visible(cx, "screenshot-preview")?
                            && !visible(cx, "github-report-dialog")?,
                        "a failed capture must show feedback"
                    );
                    ensure!(
                        messages.try_recv().is_err(),
                        "unavailable states must send no request"
                    );
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn title_bar_controls_fit_narrow_windows() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.spawn(async move |cx| {
                let outcome: Result<()> = async {
                    for (width, canary) in [(360., false), (360., true), (800., true)] {
                        let (handle, view) =
                            cx.update(|cx| open_workbench_at_size(cx, width, 600.))?;
                        let mut runner = Runner {
                            handle,
                            view,
                            target: None,
                            output: PathBuf::new(),
                            read_only: false,
                            seen: HashMap::new(),
                        };
                        if canary {
                            let preview = Step::SelfUpdatePreview {
                                canary: true,
                                waiting: false,
                            };
                            runner.step(preview, cx).await?;
                        }
                        runner.frame(cx)?;
                        runner.click(
                            cx,
                            ClickTarget::Element {
                                id: crate::menu_bar::MENU_BUTTON.into(),
                            },
                        )?;
                        runner.frame(cx)?;
                        let bounds = |cx: &mut AsyncApp, id: &str| {
                            runner
                                .update(cx, |_, window, _| window.shot_element_bounds(id))?
                                .with_context(|| format!("missing {id} at {width} px, canary: {canary}"))
                        };
                        let menu = bounds(cx, "workbench-menu-bar")?;
                        let search = bounds(cx, crate::menu_bar::SEARCH_BUTTON)?;
                        let github = bounds(cx, "github-title-button")?;
                        let gear = bounds(cx, crate::menu_bar::SETTINGS_BUTTON)?;
                        ensure!(
                            menu.right() <= search.left()
                                && search.right() <= github.left()
                                && github.right() <= gear.left()
                                && gear.right() <= px(width),
                            "title controls overlap at {width} px (canary: {canary}): \
                             menu {menu:?}, search {search:?}, GitHub {github:?}, gear {gear:?}"
                        );
                        runner.update(cx, |_, window, _| window.remove_window())?;
                    }
                    Ok(())
                }
                .await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}

#[test]
fn github_title_menu_and_chat_card_share_device_actions_and_account_state() {
    use base64::Engine as _;
    use fuzukue_proto::{GitHubAvatar, GitHubConnectState as State, GITHUB_CONNECT_PROTOCOL};
    use gpui_kit::ShotClipboard;

    const BUTTON: &str = "github-title-button";
    const CODE: &str = "ABCD-1234";
    const URL: &str = "https://github.com/login/device";

    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.set_global(ShotClipboard::default());
            let (handle, view) = open_workbench_at_size(cx, 600., 850.).unwrap();
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
                    let (sender, messages) = std::sync::mpsc::channel();
                    let session = "github-fixture".to_owned();
                    runner.update(cx, |view, _, cx| {
                        view.out = sender.into();
                        view.switch_session(session.clone(), cx);
                        seed_github_report(view, None, cx);
                        view.protocol = GITHUB_CONNECT_PROTOCOL;
                        view.on_event(
                            ServerEvent::GitHubAccountChanged {
                                state: State::Disconnected,
                            },
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    let bounds = |cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id))
                    };
                    let click = |cx: &mut AsyncApp, id: &str| {
                        runner.click(cx, ClickTarget::Element { id: id.into() })
                    };
                    let github = bounds(cx, BUTTON)?.context("GitHub title button")?;
                    let gear = bounds(cx, crate::menu_bar::SETTINGS_BUTTON)?
                        .context("Settings title button")?;
                    ensure!(
                        github.size == gear.size,
                        "title controls must use the same size"
                    );
                    ensure!(
                        (gear.left() - github.right() - px(8.)).abs() < px(1.),
                        "GitHub must sit immediately to the left of Settings with an 8 px gap"
                    );
                    let search = bounds(cx, crate::menu_bar::SEARCH_BUTTON)?
                        .context("Search title button")?;
                    ensure!(
                        search.size == gear.size
                            && (github.left() - search.right() - px(8.)).abs() < px(1.),
                        "Search must sit immediately to the left of GitHub with the same size and gap"
                    );
                    assert!(bounds(cx, "github-title-mark")?.is_some());
                    assert!(bounds(cx, "github-title-disconnected")?.is_some());
                    click(cx, BUTTON)?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-menu-mark")?.is_some());
                    assert!(bounds(cx, "github-menu-report")?.is_none());
                    click(cx, "github-connect")?;
                    let ClientMsg::GitHubReport {
                        request,
                        action: GitHubReportAction::Connect,
                    } = messages.try_recv()?
                    else {
                        bail!("expected a device authorization request");
                    };

                    let pending = State::Pending {
                        code: CODE.into(),
                        url: URL.into(),
                        expires_at: chrono::Utc::now().timestamp() + 120,
                    };
                    let set_state = |cx: &mut AsyncApp, state: State| {
                        runner.update(cx, |view, _, cx| {
                            view.on_event(
                                ServerEvent::GitHubAccountChanged {
                                    state: state.clone(),
                                },
                                cx,
                            );
                            view.on_event(
                                ServerEvent::GitHubConnectCard {
                                    session: session.clone(),
                                    attempt: "fixture".into(),
                                    state,
                                },
                                cx,
                            );
                        })
                    };
                    set_state(cx, pending)?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-title-connecting")?.is_some());
                    assert!(bounds(cx, "github-account-menu-pending")?.is_some());
                    assert!(bounds(cx, "github-account-menu-open")?.is_some());
                    let copy = |cx: &mut AsyncApp, id: &str, expected: &str| -> Result<()> {
                        click(cx, id)?;
                        let value = runner.update(cx, |_, _, cx| {
                            cx.read_from_clipboard()
                                .and_then(|clipboard| clipboard.text())
                        })?;
                        ensure!(
                            value.as_deref() == Some(expected),
                            "unexpected value copied by {id}"
                        );
                        Ok(())
                    };
                    copy(cx, "github-account-menu-copy-code", CODE)?;
                    copy(cx, "github-account-menu-copy-url", URL)?;
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-chat-fixture-pending")?.is_some());
                    assert!(bounds(cx, "github-chat-fixture-open")?.is_some());
                    copy(cx, "github-chat-fixture-copy-code", CODE)?;
                    copy(cx, "github-chat-fixture-copy-url", URL)?;

                    set_state(
                        cx,
                        State::Connected {
                            login: "reporter".into(),
                        },
                    )?;
                    runner.update(cx, |view, _, cx| {
                        use fuzukue_proto::{GitHubReportStatus, WorkspaceKind};
                        view.on_event(
                            ServerEvent::GitHubReportResult {
                                request,
                                result: Ok(GitHubReportUpdate::Status {
                                    status: GitHubReportStatus {
                                        enabled: true,
                                        login: Some("reporter".into()),
                                        report_repo: Some("example/project".into()),
                                        workspace_kind: WorkspaceKind::Software,
                                    },
                                }),
                            },
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-chat-connected")?.is_some());
                    assert!(bounds(cx, "github-chat-fixture-copy-code")?.is_none());
                    assert!(bounds(cx, "github-title-connected")?.is_some());
                    let png =
                        base64::engine::general_purpose::STANDARD.encode(screenshot_fixture_png()?);
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::GitHubAvatar {
                                avatar: GitHubAvatar {
                                    login: "reporter".into(),
                                    etag: "fixture".into(),
                                    png,
                                },
                            },
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-title-mark-avatar")?.is_some());
                    assert!(bounds(cx, "github-title-mark")?.is_none());
                    click(cx, BUTTON)?;
                    runner.frame(cx)?;
                    for id in [
                        "github-menu-mark-avatar",
                        "github-menu-report",
                        "github-menu-switch",
                        "github-disconnect",
                    ] {
                        assert!(
                            bounds(cx, id)?.is_some(),
                            "missing connected menu element {id}"
                        );
                    }
                    click(cx, "github-disconnect")?;
                    runner.frame(cx)?;
                    assert!(
                        messages.try_recv().is_err(),
                        "disconnect must wait for confirmation"
                    );
                    click(cx, "github-disconnect-cancel")?;
                    runner.frame(cx)?;
                    click(cx, "github-menu-switch")?;
                    runner.frame(cx)?;
                    assert!(
                        messages.try_recv().is_err(),
                        "switching must also wait for confirmation"
                    );
                    click(cx, "github-disconnect-cancel")?;
                    runner.frame(cx)?;
                    click(cx, "github-menu-report")?;
                    runner.frame(cx)?;
                    assert!(bounds(cx, "github-account-menu-copy-code")?.is_none());
                    assert!(bounds(cx, "github-report-dialog")?.is_some());
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}
