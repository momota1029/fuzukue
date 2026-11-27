use super::*;
use fuzukue_proto::{Actor, QuestionOutcome, SECRET_ANSWER_MASK};
use serde_json::json;

fn question(id: &str) -> Result<ServerEvent> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/shots/fixtures/question-card.json"
    ))?;
    let questions: Vec<_> = fixture["questions"].as_array().unwrap().iter().enumerate().map(|(i, question)| {
        let options: Vec<_> = question["options"].as_array().unwrap().iter().enumerate().map(|(i, option)| {
            json!({"id": i.to_string(), "label": option["label"], "description": option["description"]})
        }).collect();
        json!({"id": i.to_string(), "header": question["header"], "question": question["question"],
            "options": options, "allow_other": true, "multi_select": question["multiSelect"] == true,
            "secret": i == 2})
    }).collect();
    Ok(serde_json::from_value(json!({
        "type": "question_asked", "session": "questions", "request_id": id,
        "actor": Actor::Claude, "questions": questions,
    }))?)
}

#[test]
fn question_card_wraps_preserves_drafts_and_supports_keyboard_submission() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1120., 1050.).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.out = out.into();
                view.switch_session("questions".into(), cx);
                view.on_event(question("first").unwrap(), cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle,
                    view,
                    target: None,
                    output: PathBuf::new(),
                    read_only: false,
                    seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click {
                        id: Some(id.into()),
                        x: None,
                        y: None,
                    };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };
                    let answers = || {
                        messages
                            .try_iter()
                            .filter_map(|message| match message {
                                ClientMsg::AnswerQuestion {
                                    request_id,
                                    answers,
                                    dismiss,
                                } => Some((request_id, answers, dismiss)),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    };
                    runner.step(wait("question-send"), cx).await?;
                    // The open card is pinned in the dock above the composer.
                    runner.update(cx, |_, window, _| {
                        let dock = window.shot_element_bounds("approval-dock").unwrap();
                        let card = window.shot_element_bounds("question-card").unwrap();
                        let composer = window.shot_element_bounds("chat-input").unwrap();
                        assert!(card.top() >= dock.top() && dock.bottom() <= composer.top());
                        // Long option labels wrap inside the card instead of widening it.
                        let label = window.shot_element_bounds("question-option-0-0-label").unwrap();
                        assert!(label.right() <= card.right() && card.right() <= dock.right());
                        assert!(window.shot_element_bounds("question-in-dock-0").is_some()
                            || window.shot_element_bounds("question-in-dock-1").is_some());
                    })?;
                    let height = |runner: &Runner, cx: &mut AsyncApp| {
                        runner.update(cx, |_, window, _| {
                            window
                                .shot_element_bounds("question-option-0-0-label")
                                .unwrap()
                                .size
                                .height
                        })
                    };
                    assert!(height(&runner, cx)? > px(0.));
                    let settings = |runner: &Runner, cx: &mut AsyncApp, open| -> Result<()> {
                        runner.update(cx, |view, _, cx| {
                            view.settings_open = open;
                            cx.notify();
                        })?;
                        runner.frame(cx)
                    };
                    runner.step(click("question-option-0-0"), cx).await?;
                    runner.step(click("question-send"), cx).await?;
                    runner.key(cx, "Enter")?;
                    assert!(answers().is_empty());
                    runner.step(click("question-option-1-0"), cx).await?;
                    runner.step(click("question-option-1-1"), cx).await?;
                    runner.step(click("question-other-2"), cx).await?;
                    runner.step(wait("question-other-input-2"), cx).await?;
                    runner.type_text(cx, "private answer")?;
                    runner.update(cx, |view, _, cx| {
                        view.switch_session("elsewhere".into(), cx);
                        // Rebuild the transcript as a full replay would.
                        view.chats.remove("questions");
                        view.on_event(question("first").unwrap(), cx);
                        view.switch_session("questions".into(), cx);
                    })?;
                    settings(&runner, cx, true)?;
                    runner.step(wait("question-other-input-2"), cx).await?;
                    runner.step(click("question-other-input-2"), cx).await?;
                    runner.key(cx, "Enter")?;
                    let submitted = answers();
                    assert_eq!(submitted.len(), 1);
                    let (id, submitted, dismiss) = &submitted[0];
                    assert_eq!(id, "first");
                    assert!(!dismiss);
                    assert_eq!(submitted["0"].selected, ["0"]);
                    assert_eq!(submitted["1"].selected, ["0", "1"]);
                    assert_eq!(submitted["2"].text.as_deref(), Some("private answer"));
                    runner.update(cx, |view, _, cx| {
                        view.on_event(
                            ServerEvent::QuestionResolved {
                                session: "questions".into(),
                                request_id: "first".into(),
                                outcome: QuestionOutcome::Answered,
                                answers: [("2".into(), vec![SECRET_ANSWER_MASK.into()])]
                                    .into_iter()
                                    .collect(),
                            },
                            cx,
                        );
                    })?;
                    runner.step(wait("question-summary"), cx).await?;
                    assert!(runner.update(cx, |_, window, _| window
                        .shot_element_bounds("question-send")
                        .is_none())?);

                    settings(&runner, cx, false)?;
                    runner.update(cx, |view, _, cx| {
                        view.on_event(question("second").unwrap(), cx)
                    })?;
                    runner.step(wait("question-dismiss"), cx).await?;
                    runner.step(click("chat-input"), cx).await?;
                    runner.key(cx, "Escape")?;
                    assert!(answers().is_empty());
                    runner.step(click("question-option-0-0"), cx).await?;
                    runner.key(cx, "Escape")?;
                    let dismissed = answers();
                    assert_eq!(dismissed.len(), 1);
                    assert_eq!(dismissed[0].0, "second");
                    assert!(dismissed[0].1.is_empty() && dismissed[0].2);
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
