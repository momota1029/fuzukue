//! 研究リポジトリの Issue 一覧と取り込みの承認。

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _}, h_flex, scroll::ScrollableElement as _,
    text::TextView, v_flex, ActiveTheme as _, IconName, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, Context, MouseButton, SharedString};
use fuzukue_proto::{ClientMsg, ResearchIssueState as State, ResearchIssueView};

use crate::{chat::approval_buttons, pill, RightPane, Workbench};

#[derive(Default)]
pub(super) struct Workshop {
    pub repo: String,
    pub issues: Vec<ResearchIssueView>,
    pub error: Option<String>,
    pub loaded: bool,
    pub approvals: Vec<(String, crate::i18n::Message)>,
    pub pending_navigation: Option<(String, Target)>,
    pub report_open: Option<(u64, bool)>,
}

pub(super) enum Target {
    Deck(String),
    Card(fuzukue_proto::ResearchOrigin),
    Thread(String),
}

fn state_label(state: State) -> &'static str {
    match state {
        State::Ready => tr!("workshop-ready"),
        State::Running => tr!("workshop-in-progress"),
        State::NeedsReview => tr!("workshop-awaiting-review"),
        State::Incomplete => tr!("workshop-incomplete"),
        State::Done => tr!("workshop-done"),
    }
}

impl Workbench {
    pub(super) fn show_research_issue(&mut self, number: u64, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.right = RightPane::ResearchIssue(number);
        self.github_view();
        cx.notify();
    }

    pub(super) fn navigate_workshop(
        &mut self,
        workspace: String,
        target: Target,
        cx: &mut Context<Self>,
    ) {
        if self.workspaces.active.as_ref() == Some(&workspace) {
            self.open_workshop_target(target, cx);
        } else if !self.workspaces.pending {
            self.workshop.pending_navigation = Some((workspace.clone(), target));
            self.switch_workspace(workspace, cx);
        }
    }

    pub(super) fn open_workshop_target(&mut self, target: Target, cx: &mut Context<Self>) {
        match target {
            Target::Deck(deck) => self.open_attachment(&deck, cx),
            Target::Card(origin) => {
                if self.deck == origin.deck && self.cards.iter().any(|card| card.id == origin.card)
                {
                    self.select(origin.card, cx);
                } else if self
                    .out
                    .send(ClientMsg::SelectDeck {
                        deck: origin.deck.clone(),
                    })
                    .is_ok()
                {
                    self.pending_research_origin = Some(origin);
                }
            }
            Target::Thread(session) => self.select_thread(session, cx),
        }
    }

    pub(super) fn render_research_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = match self.right {
            RightPane::ResearchIssue(number) => Some(number),
            _ => None,
        };
        v_flex()
            .id("research-issues-view")
            .size_full()
            .min_w_0()
            .child(
                h_flex().px(crate::pane_resize::PANE_INSET).py_3().gap_3().flex_wrap()
                    .border_b_1().border_color(theme.sidebar_border)
                    .child(div().text_lg().child(tr!("workshop-research-issues")))
                    .when(!self.workshop.repo.is_empty(), |row| row.child(
                        div().text_xs().text_color(theme.muted_foreground).child(self.workshop.repo.clone())
                    ))
                    .child(div().flex_1())
                    .when(selected.is_some(), |row| row.child(
                        pill("research-issues-back", tr!("workshop-back-to-issues"), false, cx)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::ResearchIssues;
                                cx.notify();
                            }))
                    ))
                    .child(
                        pill("refresh-research-issues", tr!("workshop-refresh"), false, cx)
                            .on_click(cx.listener(|this, _, _, _| {
                                let _ = this.out.send(ClientMsg::RefreshIssues);
                            }))
                    )
                    .child(self.pane_close_button("research-issues-close", cx)),
            )
            .child(
                div().id(SharedString::from(format!("research-issues-scroll-{selected:?}")))
                    .flex_1().min_h_0().overflow_y_scrollbar()
                    .child(
                        v_flex().w_full().max_w(px(880.)).mx_auto().p_5().gap_3()
                            .children(self.workshop.error.as_ref().map(|error| {
                                div().text_sm().text_color(theme.danger).whitespace_normal()
                                    .child(if self.workshop.repo.is_empty() {
                                        tr!("workshop-repository-unavailable", detail = error.to_owned())
                                    } else {
                                        error.clone()
                                    })
                            }))
                            .map(|pane| match selected {
                                Some(number) => pane.child(self.render_research_issue(number, cx)),
                                None => pane.child(self.render_research_issues(cx)),
                            }),
                    ),
            )
    }

    fn render_research_issues(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex().id("research-issue-list").gap_2()
            .children((self.workshop.issues.is_empty() && self.workshop.error.is_none()).then(|| {
                div().text_sm().text_color(theme.muted_foreground).child(
                    if self.workshop.loaded { tr!("workshop-no-issues") } else { tr!("workshop-loading-issues") }
                )
            }))
            .children(self.workshop.issues.iter().map(|issue| {
                let number = issue.number;
                v_flex().min_w_0().gap_1().p_3().rounded_md().border_1().border_color(theme.border)
                    .child(
                        v_flex().id(SharedString::from(format!("research-issue-{number}")))
                            .min_w_0().gap_1().cursor_pointer().hover(|row| row.text_color(theme.link))
                            .child(div().text_base().child(format!("#{number} {}", issue.title)))
                            .child(div().text_sm().text_color(theme.muted_foreground)
                                .child(format!("{} · {}", state_label(issue.state), issue.assignee)))
                            .child(div().text_xs().text_color(theme.muted_foreground).whitespace_normal()
                                .child(issue.cards.join(" · ")))
                            .on_click(cx.listener(move |this, _, _, cx| this.show_research_issue(number, cx))),
                    )
                    .child(self.research_issue_actions(issue, false, cx))
            }))
    }

    fn research_issue_actions(&self, issue: &ResearchIssueView, detail: bool, cx: &mut Context<Self>) -> gpui_kit::Div {
        let number = issue.number;
        let action_id = |action| {
            if detail { format!("research-issue-{action}") } else { format!("research-issue-{number}-{action}") }
        };
        let mut actions = h_flex().gap_2().flex_wrap();
        if issue.state == State::Ready
            || issue.state == State::Incomplete && issue.assignee != "dots"
        {
            actions = actions.child(
                pill(
                    action_id("start"),
                    if issue.assignee == "dots" {
                        tr!("workshop-send-to-dots")
                    } else {
                        tr!("workshop-start-work")
                    },
                    true,
                    cx,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.show_research_issue(number, cx);
                        let _ = this.out.send(ClientMsg::StartResearchIssue { number });
                    }),
                ),
            );
        }
        if let (Some(workspace), Some(session)) = (&issue.workspace, &issue.session) {
            let (workspace, session) = (workspace.clone(), session.clone());
            actions = actions.child(
                pill(action_id("thread"), tr!("workshop-open-thread"), false, cx).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.navigate_workshop(
                            workspace.clone(),
                            Target::Thread(session.clone()),
                            cx,
                        );
                    }),
                ),
            );
        }
        if let Some(entry) = self.workspaces.entries.iter().find(|entry| detail && Some(&entry.id) == issue.workspace.as_ref()) {
            let path = format!("{}/", entry.path);
            actions = actions.child(
                pill(action_id("workspace"), tr!("workshop-open-working-folder"), false, cx)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                        this.open_workspace_browser(&path, window, cx);
                    })),
            );
        }
        if issue.state == State::NeedsReview && self.workshop.approvals.is_empty() {
            actions = actions.child(
                pill(action_id("accept"), tr!("workshop-merge"), true, cx).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.show_research_issue(number, cx);
                        let _ = this.out.send(ClientMsg::AcceptResearchIssue { number });
                    }),
                ),
            );
        }
        let url = issue.url.clone();
        actions.when(detail, |actions| actions.child(
            pill(action_id("github"), tr!("workshop-open-on-github"), false, cx)
                .on_mouse_down(MouseButton::Left, move |_, _, cx| cx.open_url(&url)),
        ))
    }

    fn render_research_issue(&self, number: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pane = v_flex().id("research-issue-detail").gap_3().min_w_0();
        let Some(issue) = self.workshop.issues.iter().find(|issue| issue.number == number) else {
            let url = format!("https://github.com/{}/issues/{number}", self.workshop.repo);
            return pane.items_start()
                .child(tr!("workshop-research-issue", number = number.to_string()))
                .when(!self.workshop.repo.is_empty(), |pane| pane.child(
                    pill("research-issue-fallback", tr!("workshop-open-on-github"), false, cx)
                        .on_click(move |_, _, cx| cx.open_url(&url))
                ));
        };
        let actions = self.research_issue_actions(issue, true, cx);
        let card_links = issue
            .cards
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                let row = div()
                    .id(SharedString::from(format!("research-issue-card-{index}")))
                    .child(reference.clone());
                let location = issue
                    .card_workspaces
                    .get(index)
                    .and_then(|workspace| workspace.clone())
                    .zip(
                        reference
                            .split_once(':')
                            .and_then(|(_, path)| path.rsplit_once('#')),
                    );
                if let Some((workspace, (deck, card))) = location {
                    let origin = fuzukue_proto::ResearchOrigin {
                        deck: crate::wire_deck(deck).into(),
                        card: card.into(),
                    };
                    row.text_color(theme.link).cursor_pointer().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.navigate_workshop(
                                workspace.clone(),
                                Target::Card(origin.clone()),
                                cx,
                            )
                        }),
                    )
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();
        let entity = cx.entity();
        pane.child(div().text_lg().child(format!("#{number} {}", issue.title)))
            .child(div().text_sm().text_color(theme.muted_foreground)
                .child(format!("{} · {}", state_label(issue.state), issue.assignee)))
            .child(actions)
            .child(self.github_linked_threads(number, cx))
            .children(self.github_watch_status(cx))
            .children(self.workshop.approvals.iter().map(|(id, summary)| {
                v_flex().gap_2().p_3().rounded_md().bg(theme.secondary).items_start()
                    .child(div().text_sm().whitespace_normal().child(summary.text().into_owned()))
                    .child(approval_buttons(id, &entity))
            }))
            .child(v_flex().gap_1().text_sm().children(card_links))
            .child(div().font_bold().text_sm().child(if issue.details.is_some() {
                tr!("workshop-issue-body")
            } else {
                tr!("workshop-acceptance-criteria")
            }))
            .child(
                div().id("research-issue-body").min_w_0().text_sm()
                    .child(TextView::markdown(
                        SharedString::from(format!("research-issue-body-{number}")),
                        issue.details.as_ref().map_or_else(|| issue.acceptance.clone(), |details| details.body.clone()),
                    ))
            )
            .children(issue.details.as_ref().map(|details| {
                v_flex().id("research-issue-history").gap_2().border_t_1().border_color(theme.sidebar_border).pt_3()
                    .child(div().font_bold().text_sm().child(tr!("workshop-issue-history")))
                    .children(details.created_at.as_ref().map(|date| {
                        div().text_xs().text_color(theme.muted_foreground)
                            .child(tr!("workshop-issue-created", date = date.to_owned()))
                    }))
                    .children(details.comments.iter().enumerate().map(|(index, comment)| {
                        let author = comment.author.clone().unwrap_or_else(|| tr!("workshop-unknown-author").to_owned());
                        let metadata = comment.created_at.as_ref()
                            .map_or(author.clone(), |date| format!("{author} · {date}"));
                        v_flex().id(SharedString::from(format!("research-issue-comment-{index}")))
                            .gap_1().text_sm().p_3().rounded_md().bg(theme.secondary)
                            .child(div().text_xs().text_color(theme.muted_foreground).child(metadata))
                            .child(TextView::markdown(
                                SharedString::from(format!("research-issue-{number}-comment-{index}")),
                                comment.body.clone(),
                            ))
                    }))
                    .children(details.closed_at.as_ref().map(|date| {
                        div().text_xs().text_color(theme.muted_foreground)
                            .child(tr!("workshop-issue-closed", date = date.to_owned()))
                    }))
            }))
            .children(issue.summary.as_ref().map(|report| {
                let open = self.workshop.report_open.filter(|(issue, _)| *issue == number)
                    .map_or(self.appearance.settings.show_worker_messages, |(_, open)| open);
                let action = if open { tr!("chat-collapse") } else { tr!("chat-expand") };
                let label = tr!("workshop-work-report");
                let tooltip = tr!("chat-toggle-details", label = label.to_string(), action = action.to_string());
                v_flex().id("research-issue-report").gap_1()
                    .child(Button::new("research-issue-report-toggle")
                        .text().small()
                        .icon(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                        .label(label)
                        .accessibility_label(tooltip.clone())
                        .tooltip(tooltip)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workshop.report_open = Some((number, !open));
                            cx.notify();
                        })))
                    .children(open.then(|| div().text_sm().whitespace_normal().child(report.clone())))
            }))
    }

    pub(super) fn research_issue_chips(
        &self,
        card: &fuzukue_proto::CardView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let repo = self.workshop.repo.rsplit('/').next().unwrap_or("");
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(card.issues.iter().filter_map(|issue| {
                let (owner, number) = issue.rsplit_once('#')?;
                if owner != repo && owner != self.workshop.repo {
                    return None;
                }
                let number = number.parse::<u64>().ok()?;
                Some(
                    pill(
                        format!("card-issue-{}-{number}", card.id),
                        tr!("workshop-issue-link", issue = issue.to_owned()),
                        false,
                        cx,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.show_research_issue(number, cx)),
                    ),
                )
            }))
    }
}
