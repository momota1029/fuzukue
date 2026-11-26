use fuzukue_proto::{ApprovalDisplayAction, ApprovalOwner};
use serde_json::json;

use super::*;

fn item(id: &str, thread: &str, job: &str, risk: ApprovalRisk) -> ApprovalSummary {
    ApprovalSummary {
        approval_id: id.into(),
        workspace: "workspace".into(),
        owner: ApprovalOwner { thread: thread.into(), job: job.into(), requester: "worker".into() },
        risk,
        unattended: ApprovalUnattended::DeferAndContinue,
        reason: "Review a generated file".into(),
        created_at: 10,
        expires_at: 60_000,
        updated_at: 10,
        state: ApprovalState::Pending,
        grant_scope: None,
        consumed_at: None,
        automatic: false,
        tool_item: None,
        anchor_item: None,
        action: ApprovalDisplayAction {
            tool: "fs_write".into(),
            args: json!({"path": format!("notes/{id}.md")}),
            scope: json!({"access": "write", "root": "notes"}),
        },
    }
}

#[test]
fn preserves_arrival_order_within_each_job_and_counts_by_thread() {
    let mut dock = ApprovalDock { fetching: Some(1), ..ApprovalDock::default() };
    dock.snapshot(1, Ok(vec![
        item("z", "thread", "later-name", ApprovalRisk::Low),
        item("a", "thread", "earlier-name", ApprovalRisk::Mid),
        item("other", "another-thread", "later-name", ApprovalRisk::High),
        item("b", "thread", "later-name", ApprovalRisk::Low),
    ]));
    assert!(dock.pending_change(item("c", "thread", "earlier-name", ApprovalRisk::Low)));
    assert!(!dock.pending_change(item("a", "thread", "earlier-name", ApprovalRisk::Mid)));
    assert_eq!(
        dock.groups("thread").iter().map(|group| group.iter().map(|item| item.approval_id.as_str())
            .collect::<Vec<_>>()).collect::<Vec<_>>(),
        [vec!["z", "b"], vec!["a", "c"]]
    );
    assert_eq!(dock.thread("thread").count(), 4);
    assert_eq!(dock.thread("another-thread").count(), 1);
}

#[test]
fn bulk_requires_matching_kind_scope_and_thread_and_excludes_high_risk() {
    let first = item("first", "thread", "a", ApprovalRisk::Low);
    let second = item("second", "thread", "b", ApprovalRisk::Mid);
    let mut unattended = item("unattended", "thread", "b", ApprovalRisk::Low);
    unattended.unattended = ApprovalUnattended::NeverUnattended;
    let mut other_kind = second.clone();
    other_kind.action.tool = "fs_edit".into();
    let mut other_scope = second.clone();
    other_scope.action.scope = json!({"access": "write", "root": "src"});
    assert!(same_batch(&first, &second));
    for excluded in [
        unattended,
        item("high", "thread", "a", ApprovalRisk::High),
        item("other-thread", "other", "a", ApprovalRisk::Low),
        other_kind,
        other_scope,
    ] {
        assert!(!same_batch(&first, &excluded), "{excluded:?}");
    }

    let mut dock = ApprovalDock { pending: vec![first.clone(), second.clone()], ..ApprovalDock::default() };
    let reviewed = vec![first.clone(), second];
    assert_eq!(dock.decision_candidates("thread", &reviewed), ["first", "second"]);
    let mut resolved = first;
    resolved.state = ApprovalState::Denied;
    resolved.updated_at = 20;
    assert!(dock.resolve(resolved.clone()));
    assert!(!dock.resolve(resolved));
    dock.pending_change(item("third", "thread", "a", ApprovalRisk::Low));
    assert_eq!(dock.decision_candidates("thread", &reviewed), ["second"]);
}

#[test]
fn fresh_snapshot_reconciles_after_disconnect_and_stale_replies_are_ignored() {
    let mut dock = ApprovalDock { available: true, fetching: Some(1), ..ApprovalDock::default() };
    dock.snapshot(1, Ok(vec![item("offline-decision", "a", "job", ApprovalRisk::Low)]));
    assert!(dock.ready());
    dock.submitted = Some(Submission {
        thread: "a".into(),
        outstanding: [2, 3].into(),
        feedback: Feedback { applied: 1, bulk: true, ..Feedback::default() },
    });
    dock.disconnect();
    assert!(!dock.available);
    assert!(!dock.ready());
    assert_eq!(dock.thread("a").count(), 0);
    assert_eq!(dock.feedback["a"].interrupted, 2);
    dock.available = true;
    dock.fetching = Some(4);
    dock.snapshot(1, Ok(vec![item("stale", "a", "job", ApprovalRisk::Low)]));
    assert!(!dock.ready());
    dock.snapshot(4, Ok(vec![item("new", "a", "job", ApprovalRisk::Mid)]));
    dock.pending_change(item("live", "a", "job", ApprovalRisk::High));
    assert_eq!(
        dock.thread("a").map(|item| item.approval_id.as_str()).collect::<Vec<_>>(),
        ["new", "live"]
    );
}

#[test]
fn each_risk_has_a_distinct_text_label() {
    let en = Locale::supported("en").unwrap();
    for (risk, key, english) in [
        (ApprovalRisk::Low, "approval-dock-risk-low", "Low risk"),
        (ApprovalRisk::Mid, "approval-dock-risk-mid", "Medium risk"),
        (ApprovalRisk::High, "approval-dock-risk-high", "High risk"),
    ] {
        assert_eq!(risk_label(risk, Locale::JAPANESE), Locale::JAPANESE.text(key));
        assert_eq!(risk_label(risk, en), english);
    }
}
