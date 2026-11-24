//! Git controls in the thread information panel.

use gpui_kit::base::{Disableable as _, Selectable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, Context, Entity, Focusable, SharedString};
use fuzukue_proto::{ClientMsg, GitCommit, GitOutcome, GitRequest, GitStatus};

use crate::{appearance::UI_LINE_HEIGHT, thread_info::disclosure, Workbench};

struct Diff {
    path: String,
    staged: bool,
    id: Option<u64>,
    result: Option<Result<String, String>>,
}

#[derive(Default)]
pub(super) struct WorkspaceGit {
    open: bool,
    pub(super) status: Option<GitStatus>,
    pub(super) commits: Vec<GitCommit>,
    status_error: Option<String>,
    status_id: Option<u64>,
    refresh_again: bool,
    next_id: u64,
    diff: Option<Diff>,
    message: Option<Entity<InputState>>,
    operation_id: Option<u64>,
    approval_session: Option<String>,
    approval: Option<(String, crate::i18n::Message)>,
    operation: Option<Result<String, String>>,
}

impl WorkspaceGit {
    pub(super) fn close_top(&mut self) -> bool {
        if !self.open {
            return false;
        }
        if self.diff.take().is_none() {
            self.open = false;
        }
        true
    }

    pub(super) fn branch_indicator(&self) -> Option<String> {
        let status = self.status.as_ref()?;
        let branch = status.branch.as_deref().unwrap_or("HEAD");
        let dirty = if status.files.is_empty() { "" } else { " *" };
        Some(format!("{branch}{dirty}"))
    }

    pub(super) fn disconnect(&mut self) {
        self.status_id = None;
        self.refresh_again = false;
        if let Some(diff) = &mut self.diff {
            diff.id = None;
        }
        if self.operation_id.take().is_some() {
            self.operation = Some(Err(tr!("git-connection-lost-git-status-will-refresh-after-reconnection").into()));
        }
        self.approval_session = None;
        self.approval = None;
    }

    pub(super) fn show_approval(&mut self, session: &str, id: &str, summary: &crate::i18n::Message) {
        if self.approval_session.as_deref() == Some(session) {
            self.approval = Some((id.to_owned(), summary.clone()));
        }
    }

    pub(super) fn clear_approval(&mut self, id: &str) {
        if self
            .approval
            .as_ref()
            .is_some_and(|(pending, _)| pending == id)
        {
            self.approval = None;
        }
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn diff_contains(&self, text: &str) -> bool {
        self.diff
            .as_ref()
            .and_then(|diff| diff.result.as_ref())
            .and_then(|result| result.as_ref().ok())
            .is_some_and(|diff| diff.contains(text))
    }
}

impl Workbench {
    fn send_git(&mut self, request: GitRequest) -> Option<u64> {
        let git = &mut self.thread_info.git;
        git.next_id += 1;
        let id = git.next_id;
        self.out
            .send(ClientMsg::Git { id, request })
            .ok()
            .map(|_| id)
    }

    pub(super) fn refresh_workspace_git(&mut self) {
        if self.protocol < 12 || self.workspaces.active.is_none() {
            return;
        }
        if self.thread_info.git.status_id.is_some() {
            self.thread_info.git.refresh_again = true;
        } else {
            self.thread_info.git.status_id = self.send_git(GitRequest::Status);
        }
    }

    pub(super) fn workspace_git_result(&mut self, id: u64, outcome: GitOutcome) {
        let git = &mut self.thread_info.git;
        if git.status_id == Some(id) {
            git.status_id = None;
            match outcome {
                GitOutcome::Status { status, commits } => {
                    if git
                        .diff
                        .as_ref()
                        .is_some_and(|diff| !status.files.iter().any(|file| file.path == diff.path))
                    {
                        git.diff = None;
                    }
                    git.status = Some(status);
                    git.commits = commits;
                    git.status_error = None;
                }
                GitOutcome::Error { message } => {
                    git.status = None;
                    git.diff = None;
                    git.status_error = Some(message);
                }
                _ => {}
            }
            if std::mem::take(&mut git.refresh_again) {
                self.refresh_workspace_git();
            } else if let Some(diff) = git.diff.as_ref().filter(|_| git.open) {
                let (path, staged) = (diff.path.clone(), diff.staged);
                self.load_git_diff(path, staged, true);
            }
        } else if git.operation_id == Some(id) {
            git.operation_id = None;
            git.approval_session = None;
            git.approval = None;
            git.operation = match outcome {
                GitOutcome::Done { message } => {
                    git.message = None;
                    Some(Ok(message))
                }
                GitOutcome::Error { message } => Some(Err(message)),
                _ => None,
            };
            self.refresh_workspace_git();
        } else if let Some(diff) = git.diff.as_mut().filter(|diff| diff.id == Some(id)) {
            diff.id = None;
            diff.result = match outcome {
                GitOutcome::Diff { text } => Some(Ok(text)),
                GitOutcome::Error { message } => Some(Err(message)),
                _ => None,
            };
        }
    }

    fn load_git_diff(&mut self, path: String, staged: bool, refresh: bool) {
        let id = self.send_git(GitRequest::Diff {
            path: path.clone(),
            staged,
        });
        let git = &mut self.thread_info.git;
        let result = if id.is_none() {
            Some(Err(tr!("git-cannot-request-diff").into()))
        } else if refresh {
            git.diff.take().and_then(|diff| diff.result)
        } else {
            None
        };
        git.diff = Some(Diff {
            path,
            staged,
            id,
            result,
        });
    }

    fn run_git_operation(&mut self, request: GitRequest, cx: &mut Context<Self>) {
        if self.thread_info.git.operation_id.is_some() {
            return;
        }
        let session = match &request {
            GitRequest::Push { session } | GitRequest::Pull { session } => Some(session.clone()),
            _ => None,
        };
        let id = self.send_git(request);
        let git = &mut self.thread_info.git;
        git.operation_id = id;
        git.approval_session = session;
        git.operation = id.is_none().then(|| Err(tr!("git-cannot-request-git-operation").into()));
        cx.notify();
    }

    pub(super) fn render_workspace_git(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let git = &self.thread_info.git;
        let theme = cx.theme().clone();
        let mut section = v_flex().min_w_0().gap_2().child(
            disclosure("git-toggle", "Git".into(), git.open)
                .self_start()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.thread_info.git.open = !this.thread_info.git.open;
                    if this.thread_info.git.open {
                        this.refresh_workspace_git();
                    }
                    cx.notify();
                })),
        );
        if !git.open {
            return section;
        }
        if self.protocol >= 11 {
            let path = self.workspaces.entries.iter()
                .find(|entry| Some(&entry.id) == self.workspaces.active.as_ref())
                .map_or("~/".to_owned(), |entry| format!("{}/", entry.path));
            section = section.child(
                Button::new("git-browse-worktree")
                    .ghost().small().self_start()
                    .icon(crate::icons::Symbol::FolderGit.icon())
                    .label(tr!("git-open-working-tree"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_workspace_browser(&path, window, cx);
                    })),
            );
        }
        let Some(status) = &git.status else {
            return section.child(
                div().text_color(theme.muted_foreground).child(
                    git.status_error
                        .as_deref()
                        .unwrap_or(tr!("git-loading-git-status"))
                        .to_owned(),
                ),
            );
        };
        section = section.child(
            v_flex()
                .child(
                    div().font_bold().child(
                        status
                            .branch
                            .clone()
                            .unwrap_or_else(|| tr!("git-head-detached").into()),
                    ),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(
                    status.upstream.as_ref().map_or_else(
                        || tr!("git-no-upstream").into(),
                        |upstream| format!("{upstream} · ↑{} ↓{}", status.ahead, status.behind),
                    ),
                )),
        );
        let busy = git.operation_id.is_some();
        if status.read_only {
            section = section.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(tr!("git-read-only")),
            );
        } else {
            section = section.child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        Button::new("git-commit-open")
                            .outline()
                            .xsmall()
                            .label(tr!("git-commit"))
                            .disabled(busy || status.files.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let input = cx.new(|cx| {
                                    InputState::new(window, cx).placeholder(tr!("git-commit-message"))
                                });
                                input.read(cx).focus_handle(cx).focus(window, cx);
                                this.thread_info.git.message = Some(input);
                                this.thread_info.git.operation = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("git-push")
                            .outline()
                            .xsmall()
                            .label(tr!("git-push"))
                            .disabled(busy || status.upstream.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_git_operation(
                                    GitRequest::Push {
                                        session: this.active.clone(),
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("git-pull")
                            .outline()
                            .xsmall()
                            .label(tr!("git-merge"))
                            .tooltip(tr!("git-fast-forward-only"))
                            .disabled(busy || status.upstream.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_git_operation(
                                    GitRequest::Pull {
                                        session: this.active.clone(),
                                    },
                                    cx,
                                );
                            })),
                    ),
            );
        }
        if let Some(input) = git.message.as_ref() {
            section = section.child(
                v_flex().gap_1().child(Input::new(input).small()).child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("git-commit-confirm")
                                .primary()
                                .xsmall()
                                .label(tr!("git-commit-all"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let Some(input) = this.thread_info.git.message.as_ref() else {
                                        return;
                                    };
                                    let message = input.read(cx).value().to_string();
                                    if message.trim().is_empty() {
                                        this.thread_info.git.operation =
                                            Some(Err(tr!("git-enter-a-commit-message").into()));
                                        cx.notify();
                                    } else {
                                        this.run_git_operation(GitRequest::Commit { message }, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new("git-commit-cancel")
                                .ghost()
                                .xsmall()
                                .label(tr!("git-cancel"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.thread_info.git.message = None;
                                    cx.notify();
                                })),
                        ),
                ),
            );
        }
        if let Some((request_id, summary)) = &git.approval {
            section =
                section.child(
                    v_flex()
                        .gap_1()
                        .child(summary.text().into_owned())
                        .child(h_flex().gap_1().children(
                            [("accept", tr!("git-approve")), ("deny", tr!("git-deny"))].map(|(decision, label)| {
                                let request_id = request_id.clone();
                                Button::new(SharedString::from(format!("git-approval-{decision}")))
                                    .outline()
                                    .xsmall()
                                    .label(label)
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        let _ = this.out.send(ClientMsg::Resolve {
                                            request_id: request_id.clone(),
                                            decision: decision.into(),
                                        });
                                    }))
                            }),
                        )),
                );
        } else if busy {
            let text = if git.approval_session.is_some() {
                tr!("git-waiting-for-approval-or-completion")
            } else {
                tr!("git-committing")
            };
            section = section.child(div().text_color(theme.muted_foreground).child(text));
        } else if let Some(result) = &git.operation {
            let (color, text) = match result {
                Ok(text) => (theme.success, text),
                Err(text) => (theme.danger, text),
            };
            section = section.child(
                div()
                    .id("git-operation-result")
                    .max_h(px(100.))
                    .overflow_y_scrollbar()
                    .text_color(color)
                    .child(text.clone()),
            );
        }
        section = section.child(
            div()
                .font_bold()
                .child(tr!("git-changed-files", count = status.files.len().to_string())),
        );
        if status.files.is_empty() {
            section = section.child(div().text_color(theme.muted_foreground).child(tr!("git-no-changes")));
        } else {
            section = section.child(
                div()
                    .id("git-changed-files")
                    .max_h(px(165.))
                    .overflow_y_scrollbar()
                    .child(
                        v_flex().children(status.files.iter().enumerate().map(|(i, file)| {
                            let label = file.old_path.as_ref().map_or_else(
                                || file.path.clone(),
                                |old| format!("{old} → {}", file.path),
                            );
                            let staged =
                                !file.status.starts_with([' ', '?']) && file.status.ends_with(' ');
                            let path = file.path.clone();
                            Button::new(SharedString::from(format!("git-file-{i}")))
                                .ghost()
                                .xsmall()
                                .line_height(UI_LINE_HEIGHT)
                                .justify_start()
                                .w_full()
                                .truncate()
                                .label(format!("{}  {label}", file.status.trim()))
                                .tooltip(label)
                                .selected(git.diff.as_ref().is_some_and(|diff| diff.path == path))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.load_git_diff(path.clone(), staged, false);
                                    cx.notify();
                                }))
                        })),
                    ),
            );
        }
        if let Some(diff) = &git.diff {
            section = section.child(self.render_git_diff(diff, cx));
        }
        section.child(
            v_flex()
                .gap_1()
                .child(div().font_bold().child(tr!("git-recent-commits")))
                .children(git.commits.is_empty().then(|| tr!("git-no-commits")))
                .children(git.commits.iter().map(|commit| {
                    let date = commit.date.split('T').next().unwrap_or(&commit.date);
                    v_flex()
                        .min_w_0()
                        .child(div().truncate().child(commit.subject.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{} · {date}",
                                    commit.hash.chars().take(8).collect::<String>()
                                )),
                        )
                })),
        )
    }

    fn render_git_diff(&self, diff: &Diff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let toggle = |id, label, staged| {
            let path = diff.path.clone();
            Button::new(id)
                .outline()
                .xsmall()
                .label(label)
                .selected(diff.staged == staged)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.load_git_diff(path.clone(), staged, false);
                    cx.notify();
                }))
        };
        let result = match &diff.result {
            Some(Ok(text)) if text.is_empty() => div().child(tr!("git-no-differences")).into_any_element(),
            Some(Ok(text)) => div()
                .id("git-diff-scroll")
                .max_h(px(220.))
                .w_full()
                .overflow_scroll()
                .bg(theme.secondary)
                .p_2()
                .child(
                    v_flex()
                        .items_start()
                        .text_xs()
                        .font(crate::appearance::code_font(cx))
                        .children(text.lines().map(|line| {
                            let color = if line.starts_with('+') {
                                theme.success
                            } else if line.starts_with('-') {
                                theme.danger
                            } else {
                                theme.foreground
                            };
                            div()
                                .whitespace_nowrap()
                                .text_color(color)
                                .child(line.to_owned())
                        })),
                )
                .into_any_element(),
            Some(Err(error)) => div()
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element(),
            None => div().child(tr!("git-loading-diff")).into_any_element(),
        };
        v_flex()
            .gap_1()
            .min_w_0()
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(toggle("git-diff-unstaged", tr!("git-unstaged"), false))
                    .child(toggle("git-diff-staged", tr!("git-staged"), true))
                    .child(
                        Button::new("git-diff-close")
                            .ghost()
                            .xsmall()
                            .label(tr!("git-close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.thread_info.git.diff = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(result)
    }
}
