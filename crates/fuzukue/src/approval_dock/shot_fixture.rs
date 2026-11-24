use fuzukue_proto::{ApprovalDisplayAction, ApprovalOwner, Envelope};
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
