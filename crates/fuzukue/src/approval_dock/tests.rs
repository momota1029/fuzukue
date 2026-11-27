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
fn integrate_cards_list_files_and_flagged_lines_as_plain_text() {
    let mut request = item("integrate", "thread", "front_desk", ApprovalRisk::High);
    request.action.tool = "git_integrate_patch".into();
    request.action.args = json!({
        "paths": ["docs/a.md", "docs/b.md"], "privacy_findings": 3, "locations": ["docs/b.md:14"],
        "patch_sha256": "0123456789abcdef", "head": "fedcba9876543210",
    });
    request.action.scope = json!({"workspace": "handbook"});
    let card = integrate_card(&request).unwrap();
    assert_eq!(card.files, ["docs/a.md", "docs/b.md"]);
    assert_eq!(card.flagged, ["docs/b.md:14"]);
    assert_eq!(card.unlisted, 2);
    assert_eq!((card.workspace.as_str(), card.patch.as_str(), card.head.as_str()), ("handbook", "0123456789ab", "fedcba987654"));
    assert_eq!(card.explanation, tr!("approval-dock-integrate-explanation"));
    assert!(card.privacy.as_ref().unwrap().contains('3'));
    let shown = target(&request);
    assert!(shown.contains("docs/a.md") && !shown.contains(['[', '"']), "{shown}");

    assert_eq!(card.checkout, None);
    let mut on = request.clone();
    on.action.args["branch"] = json!("topic/notes");
    let shown_branch = integrate_card(&on).unwrap().checkout.unwrap();
    assert!(shown_branch.contains("topic/notes"), "{shown_branch}");
    on.action.args.as_object_mut().unwrap().remove("branch");
    on.action.args["detached_head"] = json!("fedcba9876543210");
    let detached = integrate_card(&on).unwrap().checkout.unwrap();
    assert!(detached.contains("fedcba987654") && !detached.contains("fedcba9876543"), "{detached}");
    // Other tools and redacted records keep the generic card.
    assert!(integrate_card(&item("other", "thread", "job", ApprovalRisk::Low)).is_none());
    request.action.args = json!("[redacted]");
    assert!(integrate_card(&request).is_none());
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
fn requests_without_a_deadline_and_the_integration_have_readable_labels() {
    let mut waiting = item("waiting", "thread", "front_desk", ApprovalRisk::High);
    waiting.expires_at = fuzukue_proto::APPROVAL_NO_EXPIRY;
    assert_eq!(waiting.deadline(), None);
    assert_eq!(expiry(&waiting), tr!("approval-dock-no-deadline"));
    let dated = item("dated", "thread", "job", ApprovalRisk::Low);
    assert_ne!(expiry(&dated), tr!("approval-dock-no-deadline"));
    assert_eq!(tool_title("git_integrate_patch"), tr!("approval-dock-tool-integrate"));
    assert_eq!(tool_title("fs_write"), "fs_write");
    for locale in [Locale::JAPANESE, Locale::supported("en").unwrap()] {
        for key in ["approval-dock-no-deadline", "approval-dock-tool-integrate", "approval-dock-question"] {
            assert_ne!(locale.text(key), key);
        }
    }
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
