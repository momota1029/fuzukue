use fuzukue_proto::Actor;
use gpui_kit::base::TextSelection;

use super::*;

const CHAT: &str = "あなたに確認したいこと\n\n`~/research` の処理を見た\n\n次の段落";
const CHAT_LAST: &str = "OKです．";
const CARD: &str =
    "`client-a` のクライアントから `~/research` のサーバーへ\n\n1. スレッドの題が見える\n2. カードの最後";
const CHAT_COPY: &str = "あなたに確認したいこと\n~/research の処理を見た\n次の段落\n\nOKです．";
const CARD_COPY: &str =
    "client-a のクライアントから ~/research のサーバーへ\nスレッドの題が見える\nカードの最後";

impl Runner {
    pub(super) async fn assert_pane_text_selection(&self, cx: &mut AsyncApp) -> Result<()> {
        const BODY: &str = "body-selection-fixture-reader";
        const COMPOSER: &str = "入力中の文";
        self.update(cx, |view, window, cx| {
            let session = "selection-fixture".to_owned();
            view.switch_session(session.clone(), cx);
            for (item, actor, text) in [
                ("reply", Actor::Claude, CHAT),
                ("user", Actor::User, CHAT_LAST),
            ] {
                view.on_event(
                    ServerEvent::TextDelta {
                        session: session.clone(),
                        item: item.into(),
                        actor,
                        text: text.into(),
                    },
                    cx,
                );
            }
            view.chats[&session].scroll_to_end();
            view.deck = "selection-fixture".into();
            view.cards = vec![serde_json::from_value(serde_json::json!({
                "id": "reader", "kind": "note", "title": "Selection fixture", "status": "draft",
                "path": "cards/reader.md", "links": [["see", "another-card"]], "backlinks": [],
                "files": [], "body": CARD, "external": true,
            }))
            .expect("valid card fixture")];
            view.select("reader".into(), cx);
            view.input
                .update(cx, |input, cx| input.set_value(COMPOSER, window, cx));
        })?;
        self.wait_for(cx, "chat and card text", TIMEOUT_MS, |_, window| {
            window
                .shot_element_bounds("chat-message-bubble-1")
                .is_some()
                && window
                    .shot_element_bounds(BODY)
                    .is_some_and(|bounds| bounds.size.height > px(0.))
        })
        .await?;
        let (chat_start, card_start, card_target, card_end) =
            self.update(cx, |_, window, _| {
                let chat = window
                    .shot_element_bounds("chat-message-bubble-0")
                    .context("chat message bounds")?;
                let card = window
                    .shot_element_bounds(BODY)
                    .context("card body bounds")?;
                let divider = window
                    .shot_element_bounds("right-pane-divider")
                    .context("pane divider bounds")?;
                ensure!(
                    chat.right() < divider.right() && divider.right() < card.left(),
                    "fixture panes are not side by side"
                );
                Ok::<_, anyhow::Error>((
                    point(chat.left() + px(1.), chat.top() + px(8.)),
                    point(card.left() + px(5.), card.top() + px(8.)),
                    point(card.left() + px(36.), card.top() + px(8.)),
                    point(card.right() - px(2.), card.bottom() - px(8.)),
                ))
            })??;

        let check_copy =
            |cx: &mut AsyncApp, expected: &str, inactive_region: &'static str| -> Result<()> {
                self.frame(cx)?;
                self.update(cx, |_, window, cx| {
                    let copied = TextSelection::clipboard_text(window, cx);
                    ensure!(
                        copied == expected,
                        "copy returned {copied:?}, expected {expected:?}"
                    );
                    let selected = TextSelection::selected_text(window, cx);
                    ensure!(
                        selected.split_whitespace().eq(expected.split_whitespace()),
                        "painted selection was {selected:?}"
                    );
                    ensure!(
                        TextSelection::quote_text(&inactive_region.into(), window, cx).is_empty(),
                        "selection leaked into {inactive_region}"
                    );
                    Ok(())
                })?
            };

        self.drag(cx, chat_start, card_target)?;
        check_copy(cx, CHAT_COPY, "card-detail")?;
        self.drag(cx, card_end, chat_start)?;
        check_copy(cx, CARD_COPY, "chat-selection-fixture")?;
        self.key(cx, "ctrl-a")?;
        check_copy(cx, CARD_COPY, "chat-selection-fixture")?;

        self.drag(cx, card_start, card_end)?;
        check_copy(cx, CARD_COPY, "chat-selection-fixture")?;
        cx.update_window(self.handle, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: chat_start,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    position: chat_start,
                    click_count: 3,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    position: chat_start,
                    click_count: 3,
                    ..Default::default()
                }),
                cx,
            );
        })?;
        check_copy(cx, "あなたに確認したいこと", "card-detail")?;

        self.click(
            cx,
            ClickTarget::Element {
                id: "chat-input".into(),
            },
        )?;
        self.frame(cx)?;
        self.key(cx, "ctrl-a")?;
        self.update(cx, |view, window, cx| {
            ensure!(
                !TextSelection::has_selection(window, cx),
                "clicking the composer kept the transcript selection"
            );
            ensure!(
                view.input.read(cx).selected_range() == (0..COMPOSER.len()),
                "Ctrl+A should select the composer draft"
            );
            Ok::<_, anyhow::Error>(())
        })??;
        self.drag(cx, chat_start, card_target)?;
        check_copy(cx, CHAT_COPY, "card-detail")
    }
}

#[cfg(test)]
#[test]
fn drag_stays_in_the_pane_where_selection_started() {
    let scenario: Scenario = serde_json::from_str(include_str!(
        "../../../../tests/shots/selection-region.json"
    ))
    .unwrap();
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle,
                    view,
                    target: None,
                    output: PathBuf::new(),
                    read_only: false,
                    seen: HashMap::new(),
                };
                let outcome = async {
                    for step in scenario.steps {
                        if !matches!(step, Step::Screenshot { .. }) {
                            runner.step(step, cx).await?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    completed.recv().unwrap().unwrap();
}
