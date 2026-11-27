use fuzukue_proto::{ApprovalDisplayAction, ApprovalOwner, Envelope, ServerEvent};
use serde_json::json;

use super::*;
use crate::conn::ConnEvent;

impl Workbench {
    pub(crate) fn seed_approval_dock_fixture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspaces.active.clone().expect("select a fixture workspace");
        let thread = self.active.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let items = (0..12).map(|index| ApprovalSummary {
            approval_id: format!("fixture-{index}"),
            workspace: workspace.clone(),
            owner: ApprovalOwner {
                thread: thread.clone(),
                job: ["Source notes", "Index refresh", "Release review"][index / 4].into(),
                requester: "docs worker".into(),
            },
            risk: [ApprovalRisk::Low, ApprovalRisk::Mid, ApprovalRisk::High][index % 3],
            unattended: ApprovalUnattended::DeferAndContinue,
            reason: "The requested file is outside the job's approved scope.".into(),
            created_at: now + index as i64,
            expires_at: now + 900_000,
            updated_at: now + index as i64,
            state: ApprovalState::Pending,
            grant_scope: None,
            consumed_at: None,
            automatic: false,
            tool_item: None,
            anchor_item: None,
            action: ApprovalDisplayAction {
                tool: "fs_write".into(),
                args: json!({
                    "path": format!("docs/handbook/section-{:02}.md", index + 1),
                    "diff": "- Draft description\n+ Reviewed description"
                }),
                scope: json!({"access": "write", "root": "docs/handbook"}),
            },
        }).collect();
        let request = self.approvals.request();
        self.approvals.available = true;
        self.approvals.fetching = Some(request);
        self.on_conn(ConnEvent::Event(Envelope {
            workspace: Some(workspace),
            seq: 0,
            event: ServerEvent::ApprovalQueue { request, result: Ok(items) },
        }), window, cx);
    }

    /// A deferred privacy-override integration that waits without a deadline.
    pub(crate) fn seed_integrate_approval_fixture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspaces.active.clone().expect("select a fixture workspace");
        let now = chrono::Utc::now().timestamp_millis();
        let approval = ApprovalSummary {
            approval_id: "integrate-fixture".into(),
            workspace: workspace.clone(),
            owner: ApprovalOwner { thread: self.active.clone(), job: "front_desk".into(), requester: "front_desk".into() },
            risk: ApprovalRisk::High,
            unattended: ApprovalUnattended::NeverUnattended,
            reason: "Integrate Git patch 3f2a9c1d7e40 into 2 files with a privacy override (1 flagged lines). Approve only if the flagged lines are intended.".into(),
            created_at: now - 3 * 24 * 60 * 60 * 1000,
            expires_at: fuzukue_proto::APPROVAL_NO_EXPIRY,
            updated_at: now,
            state: ApprovalState::Pending,
            grant_scope: None,
            consumed_at: None,
            automatic: false,
            tool_item: None,
            anchor_item: None,
            action: ApprovalDisplayAction {
                tool: "git_integrate_patch".into(),
                args: json!({
                    "paths": ["docs/handbook/section-01.md", "docs/handbook/section-02.md"],
                    "override_privacy": true, "privacy_findings": 1,
                    "locations": ["docs/handbook/section-02.md:14"],
                    "patch_sha256": "3f2a9c1d7e405b8a", "head": "9d41c07be2a1f6c3", "push": false,
                }),
                scope: json!({"workspace": "handbook"}),
            },
        };
        self.on_conn(ConnEvent::Event(Envelope {
            workspace: Some(workspace),
            seq: 0,
            event: ServerEvent::ApprovalQueueChanged { approval },
        }), window, cx);
    }

    pub(crate) fn resolve_approval_dock_fixture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items: Vec<_> = self.approvals.thread(&self.active).cloned().collect();
        for mut item in items {
            item.state = if item.risk == ApprovalRisk::High { ApprovalState::Denied } else { ApprovalState::Granted };
            item.updated_at = chrono::Utc::now().timestamp_millis();
            self.on_conn(ConnEvent::Event(Envelope {
                workspace: Some(item.workspace.clone()),
                seq: 0,
                event: ServerEvent::ApprovalQueueChanged { approval: item },
            }), window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn approval_fixture_result(&self, id: &str) -> ApprovalSummary {
        let mut item = self.approvals.thread(&self.active).find(|item| item.approval_id == id).unwrap().clone();
        item.updated_at = chrono::Utc::now().timestamp_millis();
        item.state = ApprovalState::Granted;
        item.grant_scope = Some(ApprovalGrantScope::Once);
        item
    }

    pub(crate) fn approval_dock_scrolled(&self) -> bool {
        self.approvals.scroll.offset().y < px(0.)
    }
}
