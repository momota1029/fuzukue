use super::*;
use fuzukue_proto::ISSUE_INTAKE_PROTOCOL;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{px, WeakEntity, Window};

#[derive(Debug, PartialEq, Eq)]
enum MenuAction<'a> {
    Set(&'a str),
    Remove(&'a str),
}

fn menu_actions<'a>(
    protocol: u32,
    thread: &'a ThreadView,
    repos: &'a [String],
) -> Vec<MenuAction<'a>> {
    if protocol < ISSUE_INTAKE_PROTOCOL {
        return Vec::new();
    }
    if let Some(repo) = thread.github.intake_repo.as_deref() {
        return vec![MenuAction::Remove(repo)];
    }
    if thread.archived {
        return Vec::new();
    }
    repos.iter().map(|repo| MenuAction::Set(repo)).collect()
}

fn badge_repo(protocol: u32, thread: &ThreadView) -> Option<&str> {
    (protocol >= ISSUE_INTAKE_PROTOCOL)
        .then_some(thread.github.intake_repo.as_deref())
        .flatten()
}

impl Workbench {
    pub(crate) fn intake_menu(&self, mut menu: PopupMenu, thread: &ThreadView) -> PopupMenu {
        for action in menu_actions(self.protocol, thread, &self.github.intake_repos) {
            let session = thread.id.clone();
            let (label, action) = match action {
                MenuAction::Set(repo) => (
                    tr!("github-intake-set", repo = repo),
                    Action::SetIntake {
                        session,
                        repo: repo.into(),
                        move_from: None,
                    },
                ),
                MenuAction::Remove(repo) => (
                    tr!("github-intake-remove", repo = repo),
                    Action::RemoveIntake {
                        session,
                        repo: repo.into(),
                    },
                ),
            };
            let out = self.out.clone();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, _| {
                let _ = out.send(ClientMsg::IssueThreads {
                    action: action.clone(),
                });
            }));
        }
        menu
    }

    pub(crate) fn intake_badge(
        &self,
        thread: &ThreadView,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        let repo = badge_repo(self.protocol, thread)?;
        let theme = cx.theme();
        Some(
            div()
                .id(SharedString::from(format!(
                    "github-intake-badge-{}",
                    thread.id
                )))
                .flex_none()
                .px_1()
                .rounded_md()
                .bg(theme.secondary)
                .text_xs()
                .aria_label(tr!("github-intake-badge-repo", repo = repo))
                .child(tr!("github-intake-badge")),
        )
    }

    pub(crate) fn confirm_intake_move(
        &self,
        action: Action,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Action::SetIntake { repo, .. } = &action else {
            return;
        };
        let question = tr!("github-intake-move-question", repo = repo, title = title);
        let view: WeakEntity<Self> = cx.weak_entity();
        let epoch = self.connection_epoch;
        let workspace = self.workspaces.active.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let action = action.clone();
            let view = view.clone();
            let workspace = workspace.clone();
            dialog.title(tr!("github-intake-move")).w(px(440.)).child(
                v_flex().gap_4().child(question.clone()).child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("github-intake-cancel")
                                .label(tr!("trash-cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("github-intake-move-confirm")
                                .primary()
                                .label(tr!("github-intake-move"))
                                .on_click(move |_, window, cx| {
                                    if let Some(view) = view.upgrade() {
                                        let this = view.read(cx);
                                        if this.connection_epoch == epoch
                                            && this.workspaces.active == workspace
                                        {
                                            let _ = this.out.send(ClientMsg::IssueThreads {
                                                action: action.clone(),
                                            });
                                        }
                                    }
                                    window.close_dialog(cx);
                                }),
                        ),
                ),
            )
        });
    }

    pub(super) fn intake_open_button(
        &self,
        session: &str,
        event: &GitHubActivity,
    ) -> Option<Button> {
        if self.protocol < ISSUE_INTAKE_PROTOCOL
            || !event.intake
            || !self.threads.iter().any(|thread| {
                thread.id == session
                    && !thread.archived
                    && thread.github.intake_repo.as_ref() == Some(&event.link.repo)
            })
        {
            return None;
        }
        let action = Action::OpenIntakeItem {
            session: session.into(),
            event: event.id.clone(),
        };
        let out = self.out.clone();
        Some(
            Button::new(SharedString::from(format!(
                "github-open-intake-{}",
                event.id
            )))
            .small()
            .ghost()
            .label(tr!("github-intake-open"))
            .on_click(move |_, _, _| {
                let _ = out.send(ClientMsg::IssueThreads {
                    action: action.clone(),
                });
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intake_menu_and_badge_follow_thread_state_and_protocol() {
        let mut thread: ThreadView = serde_json::from_value(serde_json::json!({
            "id": "thread-a", "title": "Inbox", "updated": 0, "archived": false,
            "attached": [], "math_macros": []
        }))
        .unwrap();
        let repos = ["owner/primary".into(), "owner/secondary".into()];
        assert!(menu_actions(ISSUE_INTAKE_PROTOCOL - 1, &thread, &repos).is_empty());
        assert_eq!(
            menu_actions(ISSUE_INTAKE_PROTOCOL, &thread, &repos),
            [
                MenuAction::Set("owner/primary"),
                MenuAction::Set("owner/secondary")
            ]
        );
        assert_eq!(badge_repo(ISSUE_INTAKE_PROTOCOL, &thread), None);
        thread.github.intake_repo = Some("owner/secondary".into());
        assert_eq!(
            menu_actions(ISSUE_INTAKE_PROTOCOL, &thread, &repos),
            [MenuAction::Remove("owner/secondary")]
        );
        assert_eq!(
            badge_repo(ISSUE_INTAKE_PROTOCOL, &thread),
            Some("owner/secondary")
        );
        assert_eq!(badge_repo(ISSUE_INTAKE_PROTOCOL - 1, &thread), None);
    }
}
