use super::*;
use crate::ConnEvent;
use fuzukue_proto::Actor;
use gpui_kit::{component::theme::Theme, Pixels, Point};

const SESSION: &str = "scroll-growth-fixture";
const ROW_COUNT: usize = 48;
const REPLAY_ROWS: usize = 300;
const REPLAY_WORKSPACE: &str = "replay-fixture";
const SAVED_REPLAY_ROW: usize = 137;

#[derive(Debug)]
struct ReplayFrame {
    count: usize,
    first: Option<usize>,
    anchor: gpui_kit::ListOffset,
    offset: Pixels,
    max: Pixels,
    following: bool,
}

#[derive(Debug)]
struct ScrollbarSample {
    thumb: Bounds<Pixels>,
    offset: Pixels,
    max: Pixels,
    content_height: Pixels,
}

fn sample(
    window: &Window,
    cx: &App,
    viewport: Bounds<Pixels>,
    offset: Pixels,
    max: Pixels,
) -> Result<ScrollbarSample> {
    let tokens = &Theme::global(cx).tokens;
    let scale = window.scale_factor();
    let thumb = window
        .painted_quads()
        .into_iter()
        .find_map(|quad| {
            let bounds = Bounds::new(
                point(
                    px(quad.bounds.origin.x.0 / scale),
                    px(quad.bounds.origin.y.0 / scale),
                ),
                size(
                    px(quad.bounds.size.width.0 / scale),
                    px(quad.bounds.size.height.0 / scale),
                ),
            );
            ((quad.background == tokens.scrollbar_thumb.into()
                || quad.background == tokens.scrollbar_thumb_hover.into())
                && (bounds.right() - viewport.right() + px(4.)).abs() < px(1.)
                && bounds.intersect(&viewport).size.height > px(0.))
            .then_some(bounds)
        })
        .with_context(|| {
            format!("scrollbar thumb vanished (viewport={viewport:?}, offset={offset}, max={max})")
        })?;
    Ok(ScrollbarSample {
        thumb,
        offset,
        max,
        content_height: viewport.size.height + max,
    })
}

impl Runner {
    pub(super) fn assert_thread_history_paging(&self, cx: &mut AsyncApp) -> Result<()> {
        const WORKSPACE: &str = "history-paging";
        const A: &str = "thread-history-a";
        const B: &str = "thread-history-b";
        const WORKER: &str = "delegate:history-worker";

        let (out, sent) = std::sync::mpsc::channel();
        let worker = || ServerEvent::ToolCall {
            session: A.into(), item: WORKER.into(), actor: Actor::Claude, name: "delegate".into(),
            input: serde_json::json!({"model": "codex", "started_at_ms": 1}),
        };
        let message = |session: &str, id, text: String| ServerEvent::TextDelta {
            session: session.into(), item: format!("user:{id}"), actor: Actor::User, text,
        };
        self.update(cx, |view, window, cx| {
            view.out = out.into();
            view.protocol = fuzukue_proto::THREAD_HISTORY_PROTOCOL;
            view.on_conn(ConnEvent::Connected, window, cx);
            replay_event(view, window, cx, None, ServerEvent::WorkspaceSelected { id: WORKSPACE.into(), replay: true });
            let mut push = |event| replay_event(view, window, cx, Some(WORKSPACE), event);
            push(ServerEvent::ThreadHistoryStart { session: A.into(), before: 900, workers: vec![WORKER.into()] });
            push(worker());
            for row in 10..50 {
                push(message(A, row, format!("Loaded message {row}")));
            }
            push(ServerEvent::TurnActivity { session: A.into(), running: true });
            push(message(B, 1, "Complete short tail".into()));
            let threads = [A, B].into_iter().map(|id| serde_json::from_value(serde_json::json!({
                "id": id, "title": id, "updated": 1
            })).unwrap()).collect();
            push(ServerEvent::Threads { threads });
            view.select_thread(B.into(), cx);
        })?;
        let requests = || -> Vec<(String, String, u64, u64)> {
            sent.try_iter().filter_map(|msg| match msg {
                ClientMsg::InWorkspace { id, request } => match *request {
                    ClientMsg::GetThreadHistory { session, request, before, generation: None } => Some((id, session, request, before)),
                    _ => None,
                },
                ClientMsg::GetThreadHistory { .. } => panic!("history request lost its workspace scope"),
                _ => None,
            }).collect()
        };
        self.frame(cx)?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        ensure!(requests().is_empty(), "complete tail requested history");

        self.update(cx, |view, _, cx| view.select_thread(A.into(), cx))?;
        self.frame(cx)?;
        self.dispatch_transcript_wheel(cx, -10_000.)?;
        self.frame(cx)?;
        let header_y = |cx: &mut AsyncApp| self.update(cx, |_, window, _| {
            window.shot_element_bounds(&format!("delegate-block-{WORKER}")).map(|bounds| bounds.top())
        })?.context("anchor worker is not visible");
        let y = header_y(cx)?;
        for _ in 0..4 {
            self.dispatch_transcript_wheel(cx, -30.)?;
        }
        let [first] = requests().try_into().map_err(|requests: Vec<_>| anyhow!("expected one pending request, got {requests:?}"))?;
        ensure!((first.0.as_str(), first.1.as_str(), first.3) == (WORKSPACE, A, 900), "wrong first request: {first:?}");

        let reply = |workspace, session: &str, request, events, next_before, cx: &mut AsyncApp| {
            self.update(cx, |view, window, cx| replay_event(view, window, cx, Some(workspace),
                ServerEvent::ThreadHistoryPage { session: session.into(), request, events, next_before, generation: None, through: None }))
        };
        reply("other-workspace", A, first.2, vec![], None, cx)?;
        reply(WORKSPACE, A, first.2 + 99, vec![], None, cx)?;
        reply(WORKSPACE, B, first.2, vec![], None, cx)?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        ensure!(requests().is_empty(), "unmatched reply cleared the pending request");
        reply(WORKSPACE, A, first.2, vec![], Some(600), cx)?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        let [second] = requests().try_into().map_err(|requests: Vec<_>| anyhow!("empty page did not advance: {requests:?}"))?;
        ensure!(second.2 != first.2 && second.3 == 600, "empty page did not advance: {second:?}");

        self.update(cx, |view, _, cx| view.select_thread(B.into(), cx))?;
        self.frame(cx)?;
        let page = vec![
            message(A, 7, "Early short message".into()),
            message(A, 8, format!("Earlier long message\n\n{}", "Variable height **Markdown**. ".repeat(26))),
            worker(),
            message(A, 9, "Immediately before the attached tail".into()),
            ServerEvent::TurnEnded { session: A.into(), actor: Actor::Claude },
        ];
        reply(WORKSPACE, A, second.2, page, None, cx)?;
        self.update(cx, |view, _, cx| {
            ensure!(view.active == B && view.chats[B].scroller.item_count() == 1, "A's reply changed B");
            ensure!(view.chats[A].scroller.item_count() == 44 && view.chats[A].busy
                && view.chats[A].active_worker() == Some("codex"), "history duplicated a worker or changed live activity");
            view.select_thread(A.into(), cx);
            Ok::<_, anyhow::Error>(())
        })??;
        self.frame(cx)?;
        ensure!((header_y(cx)? - y).abs() <= px(1.), "the same worker moved on screen after prepend");
        let position = self.update(cx, |view, _, _| view.chats[A].scroller.logical_scroll_top())?;
        ensure!(position.item_ix == 2, "worker is out of chronological order: {position:?}");

        self.update(cx, |view, _, cx| view.select_thread(B.into(), cx))?;
        self.frame(cx)?;
        self.update(cx, |view, _, cx| view.select_thread(A.into(), cx))?;
        self.frame(cx)?;
        ensure!((header_y(cx)? - y).abs() <= px(1.) && requests().is_empty(), "switching lost A's position or requested more history");
        for _ in 0..3 {
            self.dispatch_transcript_wheel(cx, -10_000.)?;
            self.frame(cx)?;
        }
        ensure!(requests().is_empty(), "exhausted cursor requested another page");
        self.update(cx, |view, _, _| {
            let list = &view.chats[A].scroller;
            let short = list.bounds_for_item(0).context("missing short older row")?;
            let long = list.bounds_for_item(1).context("missing long older row")?;
            ensure!(long.size.height > short.size.height, "older rows were not independently measured");
            Ok::<_, anyhow::Error>(())
        })??;

        self.update(cx, |view, window, cx| {
            view.select_thread(B.into(), cx);
            replay_event(view, window, cx, Some(WORKSPACE),
                ServerEvent::ThreadHistoryStart { session: B.into(), before: 1, workers: vec![] });
        })?;
        self.frame(cx)?;
        self.update(cx, |view, _, _| {
            assert_eq!(view.chats[B].scroller.max_offset_for_scrollbar().y, px(0.));
        })?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        let [short_request] = requests().try_into().map_err(|requests: Vec<_>| anyhow!("short tail did not request history: {requests:?}"))?;
        self.update(cx, |view, window, cx| {
            view.on_conn(ConnEvent::Disconnected("fixture".into()), window, cx);
            view.on_conn(ConnEvent::Connected, window, cx);
            view.receiving_backlog = false;
        })?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        let [retry] = requests().try_into().map_err(|requests: Vec<_>| anyhow!("disconnect did not clear the request: {requests:?}"))?;
        ensure!(retry.2 != short_request.2 && retry.3 == short_request.3, "incorrect reconnect retry: {retry:?}");
        self.update(cx, |view, window, cx| replay_event(view, window, cx, Some(WORKSPACE),
            ServerEvent::ThreadHistoryFailed { session: B.into(), request: retry.2, reason: None }))?;
        self.dispatch_transcript_wheel(cx, -30.)?;
        ensure!(requests().is_empty(), "rejected cursor requested another page");
        self.update(cx, |view, _, cx| view.select_thread(A.into(), cx))?;
        self.frame(cx)
    }

    pub(super) fn assert_chat_replay_load(&self, cx: &mut AsyncApp) -> Result<()> {
        ensure!(
            !self.read_only && self.update(cx, |view, _, _| view.protocol == 0)?,
            "transcript replay requires a local fixture without a server connection"
        );
        let mut failures = Vec::new();
        for (phase, workspace, session) in [
            ("cold attach", "cold-replay-fixture", "cold-replay-thread"),
            ("switch to unloaded thread", REPLAY_WORKSPACE, "replay-thread"),
            ("reconnect full attach", REPLAY_WORKSPACE, "replay-thread"),
        ] {
            let saved = if phase == "reconnect full attach" {
                self.update(cx, |view, _, cx| {
                    view.chats[session].scroller.scroll_to(gpui_kit::ListOffset {
                        item_ix: SAVED_REPLAY_ROW,
                        offset_in_item: px(9.),
                    });
                    cx.notify();
                })?;
                Some(self.replay_frame(cx, phase, session)?)
            } else {
                None
            };
            self.update(cx, |view, window, cx| {
                if saved.is_some() {
                    view.on_conn(ConnEvent::Disconnected("fixture interruption".into()), window, cx);
                }
                view.on_conn(ConnEvent::Connected, window, cx);
                replay_event(view, window, cx, None, ServerEvent::WorkspaceSelected {
                    id: workspace.into(),
                    replay: true,
                });
                if phase == "switch to unloaded thread" {
                    view.select_thread(session.into(), cx);
                }
            })?;
            let mut frames = vec![self.replay_frame(cx, phase, session)?];
            let mut start = 0;
            for end in [1, 2, 50, 100, 150, 200, 250, REPLAY_ROWS] {
                self.update(cx, |view, window, cx| {
                    for row in start..end {
                        // The first transport chunk contains one message taller than the viewport.
                        let text = if row == 0 {
                            "Opening message, with a long history excerpt.\n\n".repeat(40)
                        } else {
                            format!(
                                "Message {row}\n\n{}",
                                "Wrapped text with `code` and a [link](https://example.test). ".repeat(row % 4)
                            )
                        };
                        replay_event(view, window, cx, Some(workspace), ServerEvent::TextDelta {
                            session: session.into(),
                            item: format!("user:{row}"),
                            actor: Actor::User,
                            text,
                        });
                    }
                })?;
                start = end;
                for _ in 0..2 {
                    frames.push(self.replay_frame(cx, phase, session)?);
                }
                ensure!(frames.last().unwrap().count == end, "{phase}: missing replay rows");
            }
            self.update(cx, |view, window, cx| {
                let thread = serde_json::from_value(serde_json::json!({
                    "id": session, "title": "Replay fixture", "updated": 1
                })).unwrap();
                replay_event(view, window, cx, Some(workspace), ServerEvent::Threads {
                    threads: vec![thread],
                });
            })?;
            for _ in 0..3 {
                frames.push(self.replay_frame(cx, phase, session)?);
            }

            let mut previous = saved.as_ref();
            let mut movements = 0;
            for (index, frame) in frames.iter().enumerate() {
                if frame.first == Some(0) && frame.max > px(1.) && (frame.following || saved.is_some()) {
                    failures.push(format!("{phase} frame {index} painted the head: {frame:?}"));
                }
                if frame.first.is_some() {
                    if previous.is_some_and(|previous| {
                        previous.first != frame.first
                            || (previous.anchor.offset_in_item - frame.anchor.offset_in_item).abs() > px(1.)
                    }) {
                        movements += 1;
                    }
                    previous = Some(frame);
                }
            }
            let last = frames.last().unwrap();
            if movements > 1 {
                failures.push(format!("{phase} visibly scrolled {movements} times"));
            }
            if let Some(saved) = &saved {
                if last.first != saved.first || last.anchor != saved.anchor || last.following {
                    failures.push(format!("{phase} lost the saved position: {saved:?} -> {last:?}"));
                }
            } else if last.first.is_none() || !last.following || (last.max - last.offset).abs() > px(1.) {
                failures.push(format!("{phase} did not settle at the tail: {last:?}"));
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("\n"));
        Ok(())
    }

    fn replay_frame(&self, cx: &mut AsyncApp, phase: &str, session: &str) -> Result<ReplayFrame> {
        self.frame(cx)?;
        let frame = self.update(cx, |view, window, _| {
            let list = view.chats.get(session).map(|chat| &chat.scroller);
            let painted = view.active == session
                && window.shot_element_bounds(&format!("chat-{session}")).is_some();
            ReplayFrame {
                count: list.map_or(0, |list| list.item_count()),
                first: painted.then(|| list?.frame_stats().first_painted).flatten(),
                anchor: list.map_or_else(Default::default, |list| list.logical_scroll_top()),
                offset: list.map_or(px(0.), |list| -list.scroll_px_offset_for_scrollbar().y),
                max: list.map_or(px(0.), |list| list.max_offset_for_scrollbar().y),
                following: list.is_some_and(|list| list.is_following_tail()),
            }
        })?;
        eprintln!("{phase}: {frame:?}");
        Ok(frame)
    }

    pub(super) async fn assert_card_scrollbar_drag(&self, cx: &mut AsyncApp) -> Result<()> {
        let scroll = self
            .update(cx, |view, _, _| {
                if view.right != RightPane::Card {
                    return None;
                }
                view.shot_activity
                    .as_ref()?
                    .card_detail_scroll
                    .borrow()
                    .clone()
            })?
            .context("card detail is closed")?;
        self.wait_for(cx, "long card layout", TIMEOUT_MS, |_, _| {
            scroll.max_offset().y > scroll.bounds().size.height
        })
        .await?;
        ensure!(
            scroll.bounds().size.width <= px(440.),
            "card fixture must use a narrow pane"
        );
        let read = |cx: &mut AsyncApp| {
            self.update(cx, |_, window, cx| {
                sample(
                    window,
                    cx,
                    scroll.bounds(),
                    -scroll.offset().y,
                    scroll.max_offset().y,
                )
            })?
        };
        let initial = read(cx)?;
        eprintln!("card drag start: {initial:?}");
        let viewport = scroll.bounds();
        let logical_length = initial.thumb.size.height + px(8.);
        let expected_length =
            (viewport.size.height * (viewport.size.height / initial.content_height)).max(px(48.));
        ensure!(
            (logical_length - expected_length).abs() < px(2.),
            "thumb length is not proportional: {initial:?}"
        );
        let travel = viewport.size.height - logical_length;
        let start = initial.thumb.center();
        self.hold_scrollbar(cx, start)?;
        let mut previous = initial.offset;
        let mut pointer = start;
        for step in 1..=6 {
            let movement = travel * (step as f32 / 8.);
            pointer = start + point(px(0.), movement);
            self.move_scrollbar(cx, pointer)?;
            let current = read(cx)?;
            eprintln!("card drag frame {step}: pointer={} {current:?}", pointer.y);
            ensure!(
                (current.content_height - initial.content_height).abs() < px(1.),
                "card height changed"
            );
            ensure!(
                (current.max - initial.max).abs() < px(1.),
                "card scroll range changed"
            );
            ensure!(
                (current.thumb.size.height - initial.thumb.size.height).abs() < px(1.),
                "thumb resized"
            );
            ensure!(
                (current.thumb.top() - initial.thumb.top() - movement).abs() < px(2.),
                "thumb lost its grab point"
            );
            ensure!(current.offset > previous, "card offset did not advance");
            previous = current.offset;
        }
        self.release_scrollbar(cx, pointer)?;
        ensure!(
            (read(cx)?.offset - previous).abs() < px(1.),
            "card jumped on release"
        );
        Ok(())
    }

    pub(super) fn assert_transcript_scroll_growth(&self, cx: &mut AsyncApp) -> Result<()> {
        ensure!(
            !self.read_only && self.update(cx, |view, _, _| view.protocol == 0)?,
            "transcript growth requires a local fixture without a server connection"
        );
        self.update(cx, |view, _, cx| {
            view.switch_session(SESSION.into(), cx);
            append_messages(view, 0..ROW_COUNT, cx);
        })?;
        self.frame(cx)?;
        let list = self.update(cx, |view, _, _| view.chats[SESSION].scroller.clone())?;
        let read = |cx: &mut AsyncApp| {
            self.update(cx, |_, window, cx| {
                sample(
                    window,
                    cx,
                    list.viewport_bounds(),
                    -list.scroll_px_offset_for_scrollbar().y,
                    list.max_offset_for_scrollbar().y,
                )
            })?
        };

        self.step_scroll_fixture(cx, -10_000.)?;
        self.step_scroll_fixture(cx, 120.)?;
        let before = read(cx)?;
        let anchor = list.logical_scroll_top();
        ensure!(
            (1..4).contains(&anchor.item_ix),
            "expected a visible row near the top: {anchor:?}"
        );
        let bounds = list
            .bounds_for_item(anchor.item_ix)
            .context("first visible transcript row")?;
        let pointer = before.thumb.center();
        self.hold_scrollbar(cx, pointer)?;
        let before = read(cx)?;
        self.update(cx, |view, _, cx| {
            view.chats
                .get_mut(SESSION)
                .unwrap()
                .prepend_scroll_fixture(cx)
        })?;
        self.frame(cx)?;
        let during = read(cx)?;
        assert_held_thumb(&before, &during)?;
        let shifted = list.logical_scroll_top();
        let shifted_bounds = list
            .bounds_for_item(shifted.item_ix)
            .context("shifted transcript row")?;
        ensure!(shifted.item_ix == anchor.item_ix + 4 && (shifted_bounds.top() - bounds.top()).abs() < px(1.),
            "prepending moved the visible row from {anchor:?} / {bounds:?} to {shifted:?} / {shifted_bounds:?}");
        self.move_scrollbar(cx, pointer + point(px(0.), px(2.)))?;
        let moved = list.logical_scroll_top();
        ensure!(
            moved.item_ix >= shifted.item_ix && read(cx)?.offset > during.offset,
            "dragging after a prepend moved the viewport backward: {shifted:?} -> {moved:?}"
        );
        self.move_scrollbar(cx, pointer)?;
        self.release_scrollbar(cx, pointer)?;
        let after = read(cx)?;
        let released = list.logical_scroll_top();
        eprintln!("chat prepend released: {after:?}");
        ensure!(
            (after.offset - before.offset - (after.content_height - before.content_height)).abs()
                < px(2.)
                && after.offset > before.offset
                && released.item_ix == shifted.item_ix
                && (released.offset_in_item - shifted.offset_in_item).abs() < px(1.),
            "prepend offset did not account for added height: {before:?} -> {after:?}"
        );

        // A wheel event may arrive after an insertion and before the next paint.
        self.update(cx, |view, _, cx| {
            view.chats
                .get_mut(SESSION)
                .unwrap()
                .prepend_scroll_fixture(cx)
        })?;
        self.dispatch_transcript_wheel(cx, 8.)?;
        self.dispatch_transcript_wheel(cx, -3.)?;
        self.frame(cx)?;
        let wheel_anchor = list.logical_scroll_top();
        ensure!(
            wheel_anchor.item_ix == shifted.item_ix + 4
                && (wheel_anchor.offset_in_item - shifted.offset_in_item - px(5.)).abs() < px(1.),
            "wheel input used a stale anchor after prepending: {shifted:?} -> {wheel_anchor:?}"
        );

        self.step_scroll_fixture(cx, 10_000.)?;
        self.step_scroll_fixture(cx, -110.)?;
        let anchor = list.logical_scroll_top();
        let bounds = list
            .bounds_for_item(anchor.item_ix)
            .context("visible row near the tail")?;
        let pointer = read(cx)?.thumb.center();
        self.hold_scrollbar(cx, pointer)?;
        let before = read(cx)?;
        self.update(cx, |view, _, cx| {
            append_messages(view, ROW_COUNT..ROW_COUNT + 5, cx)
        })?;
        self.frame(cx)?;
        assert_held_thumb(&before, &read(cx)?)?;
        self.release_scrollbar(cx, pointer)?;
        let after = read(cx)?;
        eprintln!("chat append released: {after:?}");
        let after_bounds = list
            .bounds_for_item(anchor.item_ix)
            .context("shifted row near the tail")?;
        ensure!(
            after.max > before.max
                && (after.offset - before.offset).abs() < px(1.)
                && list.logical_scroll_top() == anchor
                && (after_bounds.top() - bounds.top()).abs() < px(1.),
            "appending moved the visible row: {before:?} -> {after:?}"
        );
        self.step_scroll_fixture(cx, 10_000.)?;
        ensure!(
            list.is_following_tail(),
            "scrolling to the bottom must resume following"
        );
        self.update(cx, |view, _, cx| {
            append_messages(view, ROW_COUNT + 5..ROW_COUNT + 7, cx)
        })?;
        self.frame(cx)?;
        let bottom = read(cx)?;
        ensure!(
            list.is_following_tail() && (bottom.max - bottom.offset).abs() < px(1.),
            "appended rows did not follow the tail: {bottom:?}"
        );
        Ok(())
    }

    fn step_scroll_fixture(&self, cx: &mut AsyncApp, pixels: f32) -> Result<()> {
        self.dispatch_transcript_wheel(cx, pixels)?;
        self.frame(cx)
    }

    fn dispatch_transcript_wheel(&self, cx: &mut AsyncApp, pixels: f32) -> Result<()> {
        let position = self.update(cx, |view, _, _| {
            view.chats[&view.active].scroller.viewport_bounds().center()
        })?;
        cx.update_window(self.handle, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::Scroll(gpui_kit::ScrollEvent {
                    position,
                    delta: gpui_kit::ScrollDelta::Pixels(point(px(0.), px(-pixels))),
                    ..Default::default()
                }),
                cx,
            );
        })
    }

    fn hold_scrollbar(&self, cx: &mut AsyncApp, position: Point<Pixels>) -> Result<()> {
        self.scrollbar_input(
            cx,
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                ..Default::default()
            }),
        )?;
        self.scrollbar_input(
            cx,
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                click_count: 1,
                ..Default::default()
            }),
        )
    }

    fn move_scrollbar(&self, cx: &mut AsyncApp, position: Point<Pixels>) -> Result<()> {
        self.scrollbar_input(
            cx,
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(gpui_kit::MouseButton::Left),
                ..Default::default()
            }),
        )
    }

    fn release_scrollbar(&self, cx: &mut AsyncApp, position: Point<Pixels>) -> Result<()> {
        self.scrollbar_input(
            cx,
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                click_count: 1,
                ..Default::default()
            }),
        )
    }

    fn scrollbar_input(&self, cx: &mut AsyncApp, input: PlatformInput) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            window.dispatch_event(input, cx);
        })?;
        self.frame(cx)
    }
}

fn replay_event(
    view: &mut Workbench,
    window: &mut Window,
    cx: &mut Context<Workbench>,
    workspace: Option<&str>,
    event: ServerEvent,
) {
    view.on_conn(
        ConnEvent::Event(fuzukue_proto::Envelope {
            workspace: workspace.map(str::to_owned),
            seq: 0,
            event,
        }),
        window,
        cx,
    );
}

fn append_messages(
    view: &mut Workbench,
    rows: std::ops::Range<usize>,
    cx: &mut Context<Workbench>,
) {
    for row in rows {
        view.on_event(
            ServerEvent::TextDelta {
                session: SESSION.into(),
                item: format!("user:{row}"),
                actor: Actor::User,
                text: format!(
                    "Message {row}\n\n{}",
                    "Wrapped text with `code` and a [link](https://example.test). ".repeat(row % 4)
                ),
            },
            cx,
        );
    }
}

fn assert_held_thumb(before: &ScrollbarSample, during: &ScrollbarSample) -> Result<()> {
    eprintln!("chat growth while held: {before:?} -> {during:?}");
    ensure!(
        (during.thumb.top() - before.thumb.top()).abs() < px(1.)
            && (during.thumb.size.height - before.thumb.size.height).abs() < px(1.)
            && (during.offset - before.offset).abs() < px(1.)
            && (during.max - before.max).abs() < px(1.),
        "held transcript thumb moved during growth"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_scrollbar_stays_under_the_pointer() {
        let fixture = include_str!(
            "../../../../tests/shots/fixtures/card-markdown/papers/reader/cards/markdown-scroll.md"
        );
        let (_, body) = fuzukue_cards::split_frontmatter(fixture).unwrap();
        run_scenario(Step::AssertCardScrollbarDrag, move |view, cx| {
            let card = serde_json::from_value(serde_json::json!({
                "id": "markdown-scroll", "kind": "design", "title": "Scrollbar fixture", "status": "",
                "path": "papers/reader/cards/markdown-scroll.md", "body": body,
                "links": [["requires", "markdown-design"]], "backlinks": [], "files": [], "external": false
            })).unwrap();
            view.on_event(
                ServerEvent::Cards {
                    deck: String::new(),
                    cards: vec![card],
                    diagnostics: vec![],
                },
                cx,
            );
            view.select("markdown-scroll".into(), cx);
        });
    }

    #[test]
    fn transcript_scrollbar_and_visible_rows_stay_anchored_during_growth() {
        run_scenario(Step::AssertTranscriptScrollGrowth, |_, _| {});
    }

    #[test]
    fn thread_navigation_keys_follow_the_focused_control() {
        run_scenario(Step::AssertThreadNavigationKeys, |_, _| {});
    }

    #[test]
    fn transcript_pages_at_the_top_and_retains_the_reader_position() {
        run_scenario(Step::AssertThreadHistoryPaging, |_, _| {});
    }

    #[test]
    fn transcript_replay_paints_only_at_the_restored_position() {
        let scenario: Scenario = serde_json::from_str(include_str!(
            "../../../../tests/shots/chat-replay-load.json"
        ))
        .unwrap();
        let [step, Step::Screenshot { .. }]: [Step; 2] = scenario.steps.try_into().ok().unwrap() else {
            panic!("expected a replay assertion followed by a screenshot");
        };
        run_scenario(step, |_, _| {});
    }

    fn run_scenario(
        step: Step,
        setup: impl FnOnce(&mut Workbench, &mut Context<Workbench>) + 'static,
    ) {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                cx.set_reduce_motion(true);
                math::init(cx);
                let (handle, view) = open_workbench_at_size(cx, 820., 750.).unwrap();
                view.update(cx, setup);
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    result.send(runner.step(step, cx).await).unwrap();
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
        completed.recv().unwrap().unwrap();
    }
}
