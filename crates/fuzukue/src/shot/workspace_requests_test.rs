use fuzukue_proto::{
    WorkspaceRequestAction, WorkspaceRequestDestination, WorkspaceRequestDraft,
    WorkspaceRequestItem, WorkspaceRequestUpdate, WorkspaceRequestUpdateKind,
    WorkspaceRequestsView,
};

use super::*;

#[test]
fn workspace_requests_headless_review_queue_and_reply() {
    let (finished, result) = std::sync::mpsc::channel();
    gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        math::init(cx);
        let (handle, view) = open_workbench_at_size(cx, 1100., 900.).unwrap();
        let (out, messages) = std::sync::mpsc::channel();
        view.update(cx, |view, cx| {
            view.protocol = fuzukue_proto::WORKSPACE_REQUESTS_PROTOCOL;
            view.status = crate::ConnectionStatus::Connected;
            view.out = out.into();
            view.switch_session("research-thread".into(), cx);
        });
        cx.spawn(async move |cx| {
            let mut runner = Runner {
                handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
            };
            let outcome: Result<()> = async {
                let wait = |id: &str| Step::WaitElement { id: id.into() };
                let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                let preview = |revision: &str, body: &str| ServerEvent::WorkspaceRequests {
                    view: Some(WorkspaceRequestsView {
                        drafts: vec![WorkspaceRequestDraft {
                            session: "research-thread".into(), revision: revision.into(),
                            destination: WorkspaceRequestDestination::Workspace { id: "target".into(), title: "fuzukue".into() },
                            title: "Empty list clears the selection".into(), body: body.into(), reply: None,
                        }],
                        ..Default::default()
                    }),
                };

                runner.update(cx, |view, _, cx| {
                    view.on_event(preview("first", "A synthetic bug report"), cx);
                    cx.notify();
                })?;
                for id in ["workspace-request-to", "workspace-request-title", "workspace-request-body",
                    "workspace-request-no-attachments", "accept-workspace-request", "decline-workspace-request"] {
                    runner.step(wait(id), cx).await?;
                }
                assert!(!messages.try_iter().any(|message| matches!(message, ClientMsg::WorkspaceRequest { .. })));
                runner.step(click("decline-workspace-request"), cx).await?;
                assert!(matches!(messages.try_recv()?,
                    ClientMsg::WorkspaceRequest { action: WorkspaceRequestAction::Decide { revision, send: false } }
                    if revision == "first"));

                runner.update(cx, |view, _, cx| {
                    view.on_event(preview("edited", "Observed: lost selection.\nExpected: keep selection."), cx);
                    cx.notify();
                })?;
                runner.frame(cx)?;
                runner.step(click("accept-workspace-request"), cx).await?;
                assert!(matches!(messages.try_recv()?,
                    ClientMsg::WorkspaceRequest { action: WorkspaceRequestAction::Decide { revision, send: true } }
                    if revision == "edited"));

                runner.update(cx, |view, _, cx| {
                    view.right = RightPane::Inbox;
                    view.switch_session("other-thread".into(), cx);
                    view.on_event(ServerEvent::WorkspaceRequests {
                        view: Some(WorkspaceRequestsView {
                            inbox: vec![WorkspaceRequestItem {
                                id: "receiver-handle".into(), title: "Empty list clears the selection".into(),
                                body: "Observed: lost selection.\nExpected: keep selection.".into(),
                                session: None, starting: false, failed: false, complete: false,
                            }],
                            updates: vec![WorkspaceRequestUpdate {
                                session: "research-thread".into(), title: "Empty list clears the selection".into(),
                                kind: WorkspaceRequestUpdateKind::Result, body: Some("Fixed.".into()), external: true,
                            }],
                            ..Default::default()
                        }),
                    }, cx);
                    cx.notify();
                })?;
                runner.step(wait("workspace-request-inbox-0"), cx).await?;
                runner.step(click("workspace-request-inbox-0"), cx).await?;
                runner.step(wait("workspace-request-inbox-body"), cx).await?;
                assert!(!messages.try_iter().any(|message| matches!(message, ClientMsg::WorkspaceRequest { .. })));
                runner.step(click("workspace-request-start"), cx).await?;
                assert!(matches!(messages.try_recv()?,
                    ClientMsg::WorkspaceRequest { action: WorkspaceRequestAction::Start { id } } if id == "receiver-handle"));
                runner.update(cx, |view, window, cx| {
                    assert!(window.shot_element_bounds("workspace-request-update-result").is_none());
                    view.switch_session("research-thread".into(), cx);
                    cx.notify();
                })?;
                runner.step(wait("workspace-request-update-result"), cx).await?;
                Ok(())
            }.await;
            let _ = finished.send(outcome.map_err(|error| format!("{error:#}")));
            cx.update(|cx| cx.quit());
        }).detach();
    });
    result
        .recv_timeout(Duration::from_secs(45))
        .unwrap()
        .unwrap();
}
