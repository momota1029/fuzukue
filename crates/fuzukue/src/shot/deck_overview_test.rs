use super::*;
use crate::ConnectionStatus;
use fuzukue_proto::{
    CardSaveOutcome, CardView, IssueView, TabView, ThreadView, TranslationCheck, TranslationRevision,
    TranslationStatus, ReviewFindingsOutcome,
};
use serde_json::json;

#[test]
fn deck_overview_opens_attached_cards_and_reports_issue_refresh() {
    const DECK: &str = "research/observable-diameter";
    const SESSION: &str = "deck-overview-thread";
    const CARD: &str = "card-idea-projection";

    let tab: TabView = serde_json::from_value(json!({
        "id": "observable-diameter", "title": "オブザーバブル直径の計算", "kind": "research",
        "summary": "観測直径の極限公式を調べる。", "status": "in_progress",
        "next": "原稿を参照し、片側連続性とパラメータ領域を照合する。",
        "deck": DECK, "papers": [], "notes": [], "lean": [], "related": [],
    }))
    .unwrap();
    let thread: ThreadView = serde_json::from_value(json!({
        "id": SESSION, "title": "観測直径の原稿", "updated": 1, "attached": [DECK],
    }))
    .unwrap();
    let card: CardView = serde_json::from_value(json!({
        "id": "idea-projection", "kind": "idea", "title": "射影の比較", "status": "",
        "path": format!("{DECK}/cards/idea-projection.md"), "body": "研究カードの本文",
        "links": [], "backlinks": [], "files": [], "external": false,
    }))
    .unwrap();
    let issue: IssueView = serde_json::from_value(json!({
        "number": 13, "title": "Check the proof", "url": "https://github.com/example/lab/issues/13",
        "updated_at": "", "labels": [], "body": "", "cards": [], "comments": 0,
    }))
    .unwrap();
    let (result, completed) = std::sync::mpsc::channel();
    gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        math::init(cx);
        let (handle, view) = open_workbench_at_size(cx, 1440., 900.).unwrap();
        cx.spawn(async move |cx| {
            let runner = Runner {
                handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
            };
            let outcome = (|| -> Result<()> {
                let config = tempfile::tempdir()?;
                runner.update(cx, |view, _, cx| {
                    view.appearance.isolate_shot_settings(config.path().join("settings.toml"));
                    view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                    view.on_event(ServerEvent::Workspace { tabs: vec![tab.clone()], deck: DECK.into() }, cx);
                    view.on_event(ServerEvent::Cards { deck: DECK.into(), cards: vec![card.clone()], diagnostics: vec![] }, cx);
                    view.on_event(ServerEvent::Threads { threads: vec![thread] }, cx);
                    view.on_event(ServerEvent::Issues { repo: "example/lab".into(), issues: vec![], error: None }, cx);
                })?;
                let click = |cx: &mut AsyncApp, id: &str| {
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: id.into() })
                };
                click(cx, "thread-info-toggle")?;
                click(cx, &format!("open-attachment-{DECK}"))?;
                let (out, messages) = std::sync::mpsc::channel();
                runner.update(cx, |view, _, _| {
                    assert_eq!(view.right, RightPane::Overview);
                    view.status = ConnectionStatus::Connected;
                    view.out = out.into();
                    view.out.select_workspace(Some("lab-fixture".into()));
                })?;

                click(cx, "tabdeck-observable-diameter")?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.right, RightPane::DeckCards);
                    assert_eq!(view.active, SESSION);
                    let list = window.shot_element_bounds("deck-card-list").unwrap();
                    assert!(list.contains(&window.shot_element_bounds(CARD).unwrap().center()));
                    assert!(window.shot_element_bounds("thread-info").is_some());
                })?;
                assert!(matches!(messages.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

                click(cx, "deck-cards-back")?;
                click(cx, "refresh-issues")?;
                assert!(matches!(messages.try_recv()?, ClientMsg::InWorkspace { id, request }
                    if id == "lab-fixture" && matches!(*request, ClientMsg::RefreshIssues)));
                runner.frame(cx)?;
                runner.update(cx, |view, window, cx| {
                    assert_eq!(view.deck_overview.issue_status(), Some(tr!("workbench-refreshing-issues")));
                    assert!(window.shot_element_bounds("issue-refresh-status").is_some());
                    view.on_event(ServerEvent::Issues { repo: "example/lab".into(), issues: vec![issue], error: None }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.deck_overview.issue_status(), Some(tr!("workbench-issues-refreshed")));
                    assert!(window.shot_element_bounds("issue-13").is_some());
                })?;

                click(cx, "refresh-issues")?;
                assert!(matches!(messages.try_recv()?, ClientMsg::InWorkspace { request, .. }
                    if matches!(*request, ClientMsg::RefreshIssues)));
                runner.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::Issues {
                        repo: "example/lab".into(), issues: view.issues.clone(), error: Some("Fixture refresh failed".into()),
                    }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.deck_overview.issue_status(), None);
                    assert!(window.shot_element_bounds("overview-issue-error").is_some());
                    view.status = ConnectionStatus::Reconnecting("Fixture disconnected".into());
                })?;
                click(cx, "refresh-issues")?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, cx| {
                    assert_eq!(view.deck_overview.issue_status(), Some(tr!("workbench-action-disconnected")));
                    assert!(window.shot_element_bounds("issue-refresh-status").is_some());
                    view.status = ConnectionStatus::Connected;
                    view.on_event(ServerEvent::Workspace { tabs: vec![tab.clone()], deck: "research/other".into() }, cx);
                    view.on_event(ServerEvent::Cards { deck: "research/other".into(), cards: vec![], diagnostics: vec![] }, cx);
                })?;
                assert!(matches!(messages.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

                click(cx, "tabdeck-observable-diameter")?;
                assert!(matches!(messages.try_recv()?, ClientMsg::InWorkspace { id, request }
                    if id == "lab-fixture" && matches!(request.as_ref(), ClientMsg::SelectDeck { deck } if deck == DECK)));
                runner.frame(cx)?;
                runner.update(cx, |view, window, cx| {
                    assert_eq!(view.right, RightPane::Overview);
                    assert!(window.shot_element_bounds("overview-deck-status").is_some());
                    view.on_event(ServerEvent::Workspace { tabs: vec![tab], deck: DECK.into() }, cx);
                    view.on_event(ServerEvent::Cards { deck: DECK.into(), cards: vec![card.clone()], diagnostics: vec![] }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.right, RightPane::DeckCards);
                    assert!(window.shot_element_bounds(CARD).is_some());
                    assert!(window.shot_element_bounds("translation-diagnostic-idea-projection").is_none());
                })?;
                let mut translated = card;
                translated.translation = Some(TranslationCheck {
                    status: TranslationStatus::Stale,
                    revision: Some(TranslationRevision {
                        base_hash: "card-revision".into(), source_fingerprint: "0123456789".into(),
                    }),
                });
                runner.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::Cards { deck: DECK.into(), cards: vec![translated.clone()], diagnostics: vec![] }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |_, window, _| {
                    assert!(window.shot_element_bounds("translation-diagnostic-idea-projection").is_some());
                })?;
                click(cx, CARD)?;
                runner.update(cx, |view, _, _| {
                    assert_eq!(view.right, RightPane::Card);
                    assert_eq!(view.selected.as_deref(), Some("idea-projection"));
                })?;
                click(cx, "record-source-fingerprint")?;
                let ClientMsg::InWorkspace { request, .. } = messages.try_recv()? else {
                    anyhow::bail!("expected a workspace request");
                };
                let ClientMsg::RecordSourceFingerprint { request, deck, card: id, revision } = *request else {
                    anyhow::bail!("expected a fingerprint request");
                };
                assert_eq!((deck.as_str(), id.as_str()), (DECK, "idea-projection"));
                assert_eq!(revision.source_fingerprint, "0123456789");
                translated.translation = Some(TranslationCheck { status: TranslationStatus::Current, revision: None });
                runner.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::CardSaveResult { request, outcome: CardSaveOutcome::Saved }, cx);
                    view.on_event(ServerEvent::Cards { deck: DECK.into(), cards: vec![translated], diagnostics: vec![] }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |_, window, _| {
                    assert!(window.shot_element_bounds("translation-detail-diagnostic").is_none());
                    assert!(window.shot_element_bounds("record-source-fingerprint").is_none());
                })?;

                let finding = |id: &str, locations| json!({
                    "id": id, "summary": "Check the estimate", "detail": "State the hypothesis.",
                    "severity": "major", "status": "open", "locations": locations,
                    "sources": [{"kind": "human"}],
                    "transitions": [{"time_ms": 1, "status": "open", "actor": "reviewer"}],
                });
                let outcome: ReviewFindingsOutcome = serde_json::from_value(json!({
                    "kind": "loaded",
                    "snapshot": {
                        "open_count": 2, "card_open_counts": {"idea-projection": 1},
                        "findings": [
                            finding("card-finding", json!([{"card": "idea-projection"}])),
                            finding("file-finding", json!([{"file": "Proof.lean"}])),
                        ],
                    },
                }))?;
                runner.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::ReviewFindings { deck: DECK.into(), outcome: outcome.clone() }, cx);
                })?;
                click(cx, "card-detail-review-findings")?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.right, RightPane::Findings);
                    assert!(window.shot_element_bounds("review-finding-card-finding").is_some());
                    assert!(window.shot_element_bounds("review-finding-file-finding").is_none());
                })?;
                click(cx, "review-finding-card-finding")?;
                click(cx, "review-findings-back-to-list")?;
                runner.frame(cx)?;
                runner.update(cx, |_, window, _| {
                    assert!(window.shot_element_bounds("review-finding-card-finding").is_some());
                    assert!(window.shot_element_bounds("review-finding-file-finding").is_none());
                })?;
                click(cx, "review-findings-close")?;
                runner.update(cx, |view, _, cx| view.open_overview_cards(DECK, cx))?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.right, RightPane::DeckCards);
                    assert!(window.shot_element_bounds("review-findings-row-idea-projection").is_some());
                })?;
                click(cx, "deck-review-findings")?;
                runner.frame(cx)?;
                runner.update(cx, |_, window, _| {
                    assert!(window.shot_element_bounds("review-finding-card-finding").is_some());
                    assert!(window.shot_element_bounds("review-finding-file-finding").is_some());
                })?;
                runner.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::Workspace { tabs: vec![], deck: "research/other".into() }, cx);
                    view.on_event(ServerEvent::ReviewFindings { deck: DECK.into(), outcome }, cx);
                })?;
                runner.frame(cx)?;
                runner.update(cx, |view, window, _| {
                    assert_eq!(view.right, RightPane::DeckCards);
                    assert!(window.shot_element_bounds("review-findings-row-idea-projection").is_none());
                })?;
                Ok(())
            })();
            result.send(outcome).unwrap();
            cx.update(|cx| cx.quit());
        }).detach();
    });
    completed.recv().unwrap().unwrap();
}
