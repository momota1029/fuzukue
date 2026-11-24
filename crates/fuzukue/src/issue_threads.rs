use std::collections::{HashMap, HashSet};

use fuzukue_proto::{
    ClientMsg, GitHubActivity, GitHubActivityKind as Kind, GitHubAuthor, GitHubItem, GitHubPolicy,
    IssueThreadAction as Action, ThreadView, ISSUE_RESPOND_PROTOCOL, ISSUE_THREADS_PROTOCOL,
};
use gpui_kit::base::text::TextView;
use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, Context, Entity, Focusable, SharedString};

use crate::{RightPane, Workbench};

mod intake;

#[derive(Default)]
pub(super) struct GitHub {
    pub repo: String,
    pub intake_repos: Vec<String>,
    pub items: Vec<GitHubItem>,
    pub error: Option<String>,
    pub retry_at: Option<i64>,
    link: Option<(String, Entity<InputState>)>,
    replies: HashMap<(String, String), Reply>,
    expanded: HashSet<(String, String)>,
    reply_open: HashSet<(String, String)>,
    pub(super) unread: HashMap<String, Vec<String>>,
}

struct Reply {
    input: Entity<TextareaState>,
    pending: bool,
    result: Option<Result<(), String>>,
}

fn policy_label(policy: GitHubPolicy) -> &'static str {
    match policy {
        GitHubPolicy::Notify => tr!("github-policy-notify"),
        GitHubPolicy::Ask => tr!("github-policy-ask"),
        GitHubPolicy::Allow => tr!("github-policy-allow"),
    }
}

fn author_label(author: GitHubAuthor) -> &'static str {
    match author {
        GitHubAuthor::Owner => tr!("github-author-owner"),
        GitHubAuthor::Agent => tr!("github-author-agent"),
        GitHubAuthor::Other => tr!("github-author-other"),
    }
}

fn report_preview(body: &str) -> (&str, bool) {
    let lines = body
        .split_inclusive('\n')
        .take(6)
        .map(str::len)
        .sum::<usize>();
    let chars = body
        .char_indices()
        .nth(480)
        .map_or(body.len(), |(end, _)| end);
    let end = lines.min(chars);
    (body[..end].trim_end(), end < body.len())
}

fn card_markdown(source: &str) -> String {
    use markdown::mdast::Node;
    // GitHub renders comments as inert HTML and proxies images. Keep both out of
    // the desktop renderer, which otherwise resolves image URLs on the client.
    fn excluded(node: &Node, ranges: &mut Vec<std::ops::Range<usize>>) {
        if matches!(
            node,
            Node::Html(_) | Node::Image(_) | Node::ImageReference(_)
        ) {
            if let Some(position) = node.position() {
                ranges.push(position.start.offset..position.end.offset);
            }
        } else if let Some(children) = node.children() {
            for child in children {
                excluded(child, ranges);
            }
        }
    }
    let mut sanitized = source.to_owned();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        let mut ranges = Vec::new();
        excluded(&root, &mut ranges);
        for range in ranges.into_iter().rev() {
            sanitized.replace_range(range, "");
        }
    }
    sanitized
}

fn state_label(state: &str) -> &'static str {
    match state {
        "open" => tr!("github-open"),
        "closed" => tr!("github-closed"),
        "merged" => tr!("github-merged"),
        _ => tr!("workbench-loading"),
    }
}

impl Workbench {
    pub(super) fn github_view(&self) {
        if self.protocol < ISSUE_THREADS_PROTOCOL
            || matches!(
                self.status,
                crate::ConnectionStatus::Connecting
                    | crate::ConnectionStatus::Reconnecting(_)
                    | crate::ConnectionStatus::Switching
                    | crate::ConnectionStatus::ServerSwap
            )
        {
            return;
        }
        let item = match self.right {
            RightPane::Issue(number) | RightPane::ResearchIssue(number) => Some(number),
            _ => None,
        };
        let session = (!self.active.is_empty()
            && !self.settings_open
            && !matches!(
                self.right,
                RightPane::Library
                    | RightPane::ResearchIssues
                    | RightPane::ResearchIssue(_)
                    | RightPane::Trash
                    | RightPane::BranchReview
                    | RightPane::Extension
            ))
        .then(|| self.active.clone());
        if session.is_some() || item.is_some() {
            let _ = self.out.send(ClientMsg::IssueThreads {
                action: Action::View { session, item },
            });
        }
    }

    pub(super) fn github_chips(&self, thread: &ThreadView, cx: &Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme();
        let mut row = h_flex().gap_2().flex_wrap();
        let linking = self.github.link.as_ref().filter(|(session, _)| session == &thread.id);
        let unread = self.github.unread.get(&thread.id).filter(|events| {
            self.protocol >= ISSUE_RESPOND_PROTOCOL && events.len() > 1
        });
        let intake = thread.github.intake_from.as_ref().filter(|id| {
            self.protocol >= fuzukue_proto::ISSUE_INTAKE_PROTOCOL
                && self.threads.iter().any(|thread| &thread.id == *id)
        });
        if self.protocol < ISSUE_THREADS_PROTOCOL
            || (thread.github.links.is_empty() && linking.is_none() && unread.is_none() && intake.is_none())
        {
            return None;
        }
        if let Some(intake) = intake.cloned() {
            row = row.child(Button::new("github-intake-back").ghost().small()
                .label(tr!("github-intake-back"))
                .on_click(cx.listener(move |this, _, _, cx| this.select_thread(intake.clone(), cx))));
        }
        if let Some(events) = unread {
            let events = events.clone();
            let session = thread.id.clone();
            row = row.child(
                Button::new("github-respond-all")
                    .small()
                    .primary()
                    .label(tr!("github-respond-all"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.github_respond(&session, &events, cx);
                    })),
            );
        }
        for link in &thread.github.links {
            let number = link.number;
            let own_repo = link.repo.eq_ignore_ascii_case(&self.repo)
                || link.repo.eq_ignore_ascii_case(&self.github.repo);
            let item = self.github.items.iter().find(|item| item.link == *link);
            let mut details = vec![
                format!("#{number}"),
                state_label(item.map_or("", |item| &item.state)).into(),
            ];
            if let Some(item) = item {
                if item.pull {
                    details[0] = format!("PR #{number}");
                }
                details.extend(item.labels.iter().cloned());
                if !item.checks.is_empty() {
                    let mut checks: Vec<_> = item
                        .checks
                        .iter()
                        .map(|check| check.status.as_str())
                        .collect();
                    checks.sort_unstable();
                    checks.dedup();
                    details.push(format!("CI {}", checks.join(", ")));
                }
            }
            let session = thread.id.clone();
            let url = format!("https://github.com/{}/issues/{number}", link.repo);
            row = row.child(
                h_flex()
                    .bg(theme.secondary)
                    .rounded_md()
                    .min_w_0()
                    .child(
                        Button::new(SharedString::from(format!("github-chip-{number}")))
                            .ghost()
                            .xsmall()
                            .label(details.join(" · "))
                            .tooltip(link.repo.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if own_repo {
                                    this.right = RightPane::Issue(number);
                                    this.github_view();
                                    cx.notify();
                                } else {
                                    cx.open_url(&url);
                                }
                            })),
                    )
                    .children(own_repo.then(|| {
                        Button::new(SharedString::from(format!("github-unlink-{number}")))
                            .ghost()
                            .xsmall()
                            .icon(crate::icons::Symbol::X.icon())
                            .tooltip(tr!("github-unlink"))
                            .on_click(cx.listener(move |this, _, _, _| {
                                let _ = this.out.send(ClientMsg::IssueThreads {
                                    action: Action::Unlink {
                                        session: session.clone(),
                                        number,
                                    },
                                });
                            }))
                    })),
            );
        }

        if let Some((session, input)) = linking {
            let session = session.clone();
            let input = input.clone();
            row = row.child(
                h_flex()
                    .gap_1()
                    .child(Input::new(&input).small())
                    .child(
                        Button::new("github-submit-link")
                            .small()
                            .label(tr!("github-link"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let reference = input.read(cx).value().trim().to_owned();
                                if !reference.is_empty() {
                                    let _ = this.out.send(ClientMsg::IssueThreads {
                                        action: Action::Link {
                                            session: session.clone(),
                                            reference,
                                        },
                                    });
                                    this.github.link = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("github-cancel-link")
                            .ghost()
                            .small()
                            .label(tr!("threads-cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github.link = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        Some(row)
    }

    pub(super) fn issue_link_menu(
        &self, thread: &ThreadView, view: gpui_kit::WeakEntity<Self>,
    ) -> impl Fn(PopupMenu, &mut gpui_kit::Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let session = thread.id.clone();
        let choices: Vec<_> = self.issues.iter().map(|issue| (issue.number, issue.title.clone()))
            .chain(self.workshop.issues.iter()
                .filter(|issue| !matches!(issue.state, fuzukue_proto::ResearchIssueState::Done))
                .map(|issue| (issue.number, issue.title.clone())))
            .filter(|(number, _)| !thread.github.links.iter().any(|link| link.number == *number))
            .collect();
        let out = self.out.clone();
        move |mut menu, _, _| {
            let session_for_input = session.clone();
            let view = view.clone();
            menu = menu.item(PopupMenuItem::new(tr!("github-enter-reference")).on_click(
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this.active != session_for_input {
                            this.select_thread(session_for_input.clone(), cx);
                        }
                        if matches!(this.right, RightPane::Library | RightPane::Handoff
                            | RightPane::BranchReview | RightPane::Trash | RightPane::Extension
                            | RightPane::ResearchIssues | RightPane::ResearchIssue(_))
                        {
                            this.close_right_pane(cx);
                        }
                        let input = cx.new(|cx| {
                            InputState::new(window, cx).placeholder(tr!("github-reference"))
                        });
                        input.read(cx).focus_handle(cx).focus(window, cx);
                        this.github.link = Some((session_for_input.clone(), input));
                        cx.notify();
                    });
                },
            ));
            for (number, title) in &choices {
                let reference = number.to_string();
                let out = out.clone();
                let session = session.clone();
                menu = menu.item(PopupMenuItem::new(format!("#{number} {title}")).on_click(
                    move |_, _, _| {
                        let _ = out.send(ClientMsg::IssueThreads {
                            action: Action::Link { session: session.clone(), reference: reference.clone() },
                        });
                    },
                ));
            }
            menu
        }
    }

    pub(super) fn github_policy(&self, thread: &ThreadView) -> Option<impl IntoElement> {
        if self.protocol < ISSUE_THREADS_PROTOCOL {
            return None;
        }
        let session = thread.id.clone();
        let out = self.out.clone();
        let authors = [
            GitHubAuthor::Owner,
            GitHubAuthor::Agent,
            GitHubAuthor::Other,
        ];
        let modern = self.protocol >= ISSUE_RESPOND_PROTOCOL;
        Some(
            v_flex()
                .gap_1()
                .child(div().font_bold().child(tr!("github-policy")))
                .children(
                    authors
                        .into_iter()
                        .enumerate()
                        .take(if modern { 3 } else { 1 })
                        .map(|(index, author)| {
                            let current = if modern {
                                thread.github.policies().get(author)
                            } else {
                                thread.github.policy
                            };
                            let session = session.clone();
                            let out = out.clone();
                            h_flex()
                                .gap_2()
                                .flex_wrap()
                                .children(
                                    modern.then(|| div().text_sm().child(author_label(author))),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "github-policy-{index}"
                                    )))
                                    .small()
                                    .label(policy_label(current))
                                    .dropdown_menu(
                                        move |mut menu, _, _| {
                                            for policy in [
                                                GitHubPolicy::Notify,
                                                GitHubPolicy::Ask,
                                                GitHubPolicy::Allow,
                                            ] {
                                                let session = session.clone();
                                                let out = out.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(policy_label(policy))
                                                        .on_click(move |_, _, _| {
                                                            let _ =
                                                                out.send(ClientMsg::IssueThreads {
                                                                    action: if modern {
                                                                        Action::AuthorPolicy {
                                                                            session: session
                                                                                .clone(),
                                                                            author,
                                                                            policy,
                                                                        }
                                                                    } else {
                                                                        Action::Policy {
                                                                            session: session
                                                                                .clone(),
                                                                            policy,
                                                                        }
                                                                    },
                                                                });
                                                        }),
                                                );
                                            }
                                            menu
                                        },
                                    ),
                                )
                        }),
                ),
        )
    }

    fn github_respond(&mut self, session: &str, events: &[String], cx: &mut Context<Self>) {
        if self
            .out
            .send(ClientMsg::IssueThreads {
                action: Action::Respond {
                    session: session.into(),
                    events: events.to_vec(),
                },
            })
            .is_ok()
        {
            if let Some(unread) = self.github.unread.get_mut(session) {
                unread.retain(|id| !events.contains(id));
            }
            cx.notify();
        }
    }

    pub(super) fn github_linked_threads(
        &self,
        number: u64,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex().gap_2().flex_wrap().children(
            self.threads
                .iter()
                .filter(|thread| {
                    thread.github.links.iter().any(|link| {
                        link.number == number && link.repo.eq_ignore_ascii_case(&self.repo)
                    })
                })
                .map(|thread| {
                    let session = thread.id.clone();
                    Button::new(SharedString::from(format!("github-thread-{session}")))
                        .small()
                        .label(tr!(
                            "github-linked-thread",
                            title = crate::thread_title(thread)
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_thread(session.clone(), cx)
                        }))
                }),
        )
    }

    pub(super) fn github_watch_status(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let text = if let Some(at) = self.github.retry_at {
            tr!("github-waiting", time = crate::until_text(at))
        } else {
            self.github.error.clone()?
        };
        Some(div().text_xs().text_color(cx.theme().warning).child(text))
    }

    pub(super) fn github_item_detail(
        &self,
        item: &GitHubItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let url = item.url.clone();
        v_flex()
            .gap_3()
            .w_full()
            .child(
                self.pane_heading(
                    div().text_lg().child(format!("#{} {}", item.link.number, item.title)), cx,
                ),
            )
            .child(div().text_xs().child(format!(
                "{} · {}",
                state_label(&item.state),
                item.labels.join(", ")
            )))
            .child(self.github_linked_threads(item.link.number, cx))
            .child(
                Button::new("github-open-item")
                    .small()
                    .label(tr!("workbench-open-on-github"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .children(item.checks.iter().map(|check| {
                div()
                    .text_sm()
                    .child(format!("{} · {}", check.name, check.status))
            }))
            .child(div().text_sm().child(item.body.clone()))
    }

    pub(super) fn github_card(
        &self,
        session: &str,
        row: &str,
        event: &GitHubActivity,
        entity: &Entity<Self>,
        cx: &gpui_kit::App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let action = match event.kind {
            Kind::Comment if event.intake && event.id.ends_with(":opened") => {
                if event.url.contains("/pull/") { tr!("github-intake-pr-opened") }
                else { tr!("github-intake-issue-opened") }
            }
            Kind::Comment => tr!("github-comment"),
            Kind::Review => tr!("github-review"),
            Kind::ReviewComment => tr!("github-review-comment"),
            Kind::Labeled => tr!("github-labeled"),
            Kind::Unlabeled => tr!("github-unlabeled"),
            Kind::Closed => tr!("github-closed"),
            Kind::Reopened => tr!("github-reopened"),
            Kind::Merged => tr!("github-merged"),
            Kind::Check => tr!("github-check"),
        };
        let url = event.url.clone();
        let item = event.link.number;
        let key = (session.to_owned(), event.id.clone());
        let can_reply = event.link.repo.eq_ignore_ascii_case(&self.github.repo);
        let actor = match event.author {
            GitHubAuthor::Owner | GitHubAuthor::Agent if event.intake && !event.actor.is_empty() =>
                format!("@{} · {}", event.actor, author_label(event.author)),
            GitHubAuthor::Owner | GitHubAuthor::Agent => author_label(event.author).to_owned(),
            GitHubAuthor::Other if event.kind == Kind::Check || event.actor.is_empty() => {
                event.actor.clone()
            }
            GitHubAuthor::Other => format!("@{}", event.actor),
        };
        let review_status = if event.kind == Kind::Review && event.body.is_some() {
            event
                .excerpt
                .split_once(" · ")
                .map(|(state, _)| state.to_owned())
        } else {
            None
        };
        let expanded = self.github.expanded.contains(&key);
        let opening = event.intake && event.id.ends_with(":opened");
        let (source, long) = if event.intake {
            (event.body.as_deref().unwrap_or(if opening { "" } else { &event.excerpt }), false)
        } else { event
            .body
            .as_deref()
            .map_or((event.excerpt.as_str(), false), report_preview) };
        let source = if expanded {
            event.body.as_deref().unwrap_or(source)
        } else {
            source
        };
        let mut card = v_flex()
            .id(SharedString::from(format!("github-card-{}", event.id)))
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .text_xs()
                    .child(format!("GitHub #{item} · {action}"))
                    .child(div().text_color(theme.muted_foreground).child(actor))
                    .children(review_status)
                    .child(
                        Button::new(SharedString::from(format!("github-event-url-{}", event.id)))
                            .ghost()
                            .xsmall()
                            .label(tr!("workbench-open-on-github"))
                            .on_click(move |_, _, cx| {
                                if url.starts_with("https://") {
                                    cx.open_url(&url);
                                }
                            }),
                    ),
            )
            .children(opening.then(|| div().text_sm().child(event.excerpt.clone())))
            .children((event.intake && !source.is_empty()).then(|| {
                // External text is rendered literally, with no Markdown links or images.
                v_flex().px_2().border_l_1().border_color(theme.border).text_sm()
                    .children(source.lines().map(|line| div().child(line.to_owned())))
            }))
            .children((!event.intake && !source.is_empty()).then(|| {
                div()
                    .id(SharedString::from(format!("github-body-{}", event.id)))
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .child(
                        TextView::markdown(
                            if expanded { "full" } else { "preview" },
                            card_markdown(source),
                        )
                        .markdown_raw_html(false)
                        .image_source(|_| {
                            gpui_kit::ImageSource::Custom(std::sync::Arc::new(|_, _| None))
                        })
                        .on_link_click(|url, _, _, cx| {
                            if url.starts_with("https://") || url.starts_with("http://") {
                                cx.open_url(url);
                            }
                        }),
                    )
            }));
        let view = entity.clone();
        let expand_key = key.clone();
        let respond_key = key.clone();
        let reply_key = key.clone();
        let expand_view = entity.clone();
        let reply_view = entity.clone();
        let modern = self.protocol >= ISSUE_RESPOND_PROTOCOL;
        let reply_open = self.github.reply_open.contains(&key);
        card = card.child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(self.intake_open_button(session, event))
                .children(modern.then(|| {
                    Button::new(SharedString::from(format!("github-respond-{}", event.id)))
                        .primary()
                        .small()
                        .label(tr!("github-respond"))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.github_respond(
                                    &respond_key.0,
                                    std::slice::from_ref(&respond_key.1),
                                    cx,
                                );
                            })
                        })
                }))
                .children(long.then(|| {
                    Button::new(SharedString::from(format!("github-full-{}", event.id)))
                        .ghost()
                        .small()
                        .label(if expanded {
                            tr!("chat-collapse")
                        } else {
                            tr!("github-full")
                        })
                        .on_click(move |_, _, cx| {
                            expand_view.update(cx, |this, cx| {
                                if !this.github.expanded.remove(&expand_key) {
                                    this.github.expanded.insert(expand_key.clone());
                                }
                                this.chats[&expand_key.0].remeasure_github(&expand_key.1);
                                cx.notify();
                            })
                        })
                }))
                .children((modern && can_reply).then(|| {
                    Button::new(SharedString::from(format!("github-more-{}", event.id)))
                        .ghost()
                        .small()
                        .icon(if reply_open {
                            crate::icons::Symbol::ChevronDown.icon()
                        } else {
                            crate::icons::Symbol::ChevronRight.icon()
                        })
                        .label(tr!("github-more"))
                        .on_click(move |_, _, cx| {
                            reply_view.update(cx, |this, cx| {
                                if !this.github.reply_open.remove(&reply_key) {
                                    this.github.reply_open.insert(reply_key.clone());
                                }
                                this.chats[&reply_key.0].remeasure_github(&reply_key.1);
                                cx.notify();
                            })
                        })
                })),
        );
        if !can_reply || (modern && !reply_open) {
            return card;
        }
        let view = entity.clone();
        let row = row.to_owned();
        if let Some(reply) = self.github.replies.get(&key) {
            if matches!(reply.result, Some(Ok(()))) {
                return card.child(div().text_xs().child(tr!("github-reply-posted")));
            }
            let input = reply.input.clone();
            let error = reply
                .result
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .cloned();
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(div().id("github-reply-box").child(Textarea::new(&input)))
                    .child(
                        Button::new("github-send-reply")
                            .small()
                            .disabled(reply.pending)
                            .label(if reply.pending {
                                tr!("github-posting")
                            } else {
                                tr!("github-reply")
                            })
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    let body = input.read(cx).value().trim().to_owned();
                                    if body.is_empty() {
                                        return;
                                    }
                                    let (session, event) = key.clone();
                                    if this
                                        .out
                                        .send(ClientMsg::IssueThreads {
                                            action: Action::Reply {
                                                session,
                                                event,
                                                body,
                                            },
                                        })
                                        .is_ok()
                                    {
                                        if let Some(reply) = this.github.replies.get_mut(&key) {
                                            reply.pending = true;
                                            reply.result = None;
                                        }
                                    }
                                    this.chats[&key.0]
                                        .remeasure_history([row.as_str()].into_iter());
                                    cx.notify();
                                })
                            }),
                    )
                    .children(
                        error.map(|error| div().text_xs().text_color(theme.danger).child(error)),
                    ),
            );
        } else {
            card = card.child(
                Button::new("github-open-reply")
                    .ghost()
                    .small()
                    .label(tr!("github-reply"))
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| {
                            let input = cx.new(|cx| {
                                TextareaState::new(window, cx)
                                    .auto_grow(2, 5)
                                    .placeholder(tr!("github-reply"))
                            });
                            input.read(cx).focus_handle(cx).focus(window, cx);
                            this.github.replies.insert(
                                key.clone(),
                                Reply {
                                    input,
                                    pending: false,
                                    result: None,
                                },
                            );
                            this.chats[&key.0].remeasure_history([row.as_str()].into_iter());
                            cx.notify();
                        })
                    }),
            );
        }
        card
    }

    pub(super) fn github_disconnected(&mut self, cx: &mut Context<Self>) {
        for ((session, event), reply) in &mut self.github.replies {
            if reply.pending {
                reply.pending = false;
                reply.result = Some(Err(tr!("github-reply-disconnected").into()));
                if let Some(chat) = self.chats.get(session) {
                    chat.remeasure_github(event);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn github_reply_result(
        &mut self,
        session: &str,
        event: String,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(reply) = self
            .github
            .replies
            .get_mut(&(session.to_owned(), event.clone()))
        {
            reply.pending = false;
            reply.result = Some(error.map_or(Ok(()), Err));
            if let Some(chat) = self.chats.get(session) {
                chat.remeasure_github(&event);
            }
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{card_markdown, report_preview};
    use markdown::mdast::Node;

    #[test]
    fn report_preview_keeps_markdown_headings_and_links() {
        let report = "### Findings\n\n[Example](https://example.com)\n\n<!-- dot -->\n\n### Remaining work\n\nDetails";
        let (excerpt, truncated) = report_preview(report);
        assert!(truncated);
        let source = card_markdown(excerpt);
        assert!(!source.contains("<!-- dot -->"));
        assert!(!source.contains("Remaining work"));
        let root = markdown::to_mdast(&source, &markdown::ParseOptions::gfm()).unwrap();
        let blocks = root.children().unwrap();
        assert!(matches!(&blocks[0], Node::Heading(heading) if heading.depth == 3));
        let inline = blocks[1].children().unwrap();
        assert!(matches!(&inline[0], Node::Link(link) if link.url == "https://example.com"));
        assert_eq!(card_markdown("![alt](file:///local.png)"), "");
    }
}
