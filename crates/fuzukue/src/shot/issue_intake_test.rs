use super::*;
use fuzukue_proto::{ClientMsg, IssueThreadAction as Action, ThreadView, ISSUE_INTAKE_PROTOCOL};

#[test]
fn intake_thread_menu_sends_designation_move_and_removal_and_shows_badge() {
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(),
                    read_only: false, seen: HashMap::new(),
                };
                let outcome = async {
                    let repo = "fixture/project";
                    let id = "thread-intake";
                    let mut thread: ThreadView = serde_json::from_value(serde_json::json!({
                        "id": id, "title": "Intake fixture", "updated": 0
                    }))?;
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.protocol = ISSUE_INTAKE_PROTOCOL;
                        view.receiving_backlog = false;
                        view.on_event(ServerEvent::IssueIntakeRepos { repos: vec![repo.into()] }, cx);
                        view.on_event(ServerEvent::Threads { threads: vec![thread.clone()] }, cx);
                        view.select_thread(id.into(), cx);
                    })?;
                    let menu_action = |runner: &Runner, cx: &mut AsyncApp| -> Result<()> {
                        runner.click(cx, ClickTarget::Element { id: format!("thread-menu-{id}") })?;
                        runner.frame(cx)?;
                        for _ in 0..4 { runner.key(cx, "down")?; }
                        runner.key(cx, "enter")
                    };
                    let sent = || messages.try_iter().find_map(|msg| match msg {
                        ClientMsg::IssueThreads { action: action @ (Action::SetIntake { .. } | Action::RemoveIntake { .. }) } =>
                            Some(action),
                        _ => None,
                    });
                    runner.frame(cx)?;
                    menu_action(&runner, cx)?;
                    assert!(matches!(sent(), Some(Action::SetIntake { session, repo: target, move_from: None })
                        if session == id && target == repo));
                    runner.update(cx, |view, window, cx| view.on_conn(
                        conn::ConnEvent::Event(fuzukue_proto::Envelope {
                            workspace: None, seq: 0,
                            event: ServerEvent::IssueIntakeMove {
                                session: id.into(), repo: repo.into(),
                                from: "thread-previous".into(), title: "Previous intake".into(),
                            },
                        }), window, cx,
                    ))?;
                    assert!(sent().is_none());
                    runner.step(Step::WaitElement { id: "github-intake-move-confirm".into() }, cx).await?;
                    runner.click(cx, ClickTarget::Element { id: "github-intake-move-confirm".into() })?;
                    assert!(matches!(sent(), Some(Action::SetIntake { session, repo: target, move_from: Some(from) })
                        if session == id && target == repo && from == "thread-previous"));
                    thread.github.intake_repo = Some(repo.into());
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::Threads { threads: vec![thread] }, cx);
                    })?;
                    runner.step(Step::WaitElement { id: format!("github-intake-badge-{id}") }, cx).await?;
                    menu_action(&runner, cx)?;
                    assert!(matches!(sent(), Some(Action::RemoveIntake { session, repo: target })
                        if session == id && target == repo));
                    Ok::<(), anyhow::Error>(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
    completed.recv().unwrap().unwrap();
}
