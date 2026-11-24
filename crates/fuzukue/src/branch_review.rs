//! The workspace's branch review pane.

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, AnyElement, Context, Entity, SharedString};
use fuzukue_proto::{
    BranchDiff, BranchReviewOutcome as Outcome, BranchReviewRequest as Request, ClientMsg,
    ReviewBranch, TestState,
};

use crate::{unified_diff, RightPane, Workbench};

#[derive(Default)]
pub(super) struct BranchReview {
    next: u64,
    pending: Option<u64>,
    base: String,
    branches: Vec<ReviewBranch>,
    selected: Option<String>,
    diff: Option<BranchDiff>,
    file: usize,
    confirmation: Option<Confirmation>,
    result: Option<Outcome>,
}

enum Confirmation {
    Merge { cleanup: bool, override_lean: bool },
    Discard,
    SendBack(Entity<InputState>),
}

impl BranchReview {
    pub(super) fn reset(&mut self) {
        *self = Self {
            next: self.next,
            ..Self::default()
        };
    }

    pub(super) fn close(&mut self) {
        self.confirmation = None;
    }

    fn selected(&self) -> Option<&ReviewBranch> {
        self.branches
            .iter()
            .find(|branch| Some(&branch.name) == self.selected.as_ref())
    }
}

fn control(id: &'static str, label: &'static str) -> Button {
    Button::new(id).small().outline().label(label)
}

impl Workbench {
    pub(super) fn toggle_branch_review(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        if self.right == RightPane::BranchReview {
            self.branch_review.close();
            self.right = RightPane::None;
        } else {
            self.right = RightPane::BranchReview;
            if self.branch_review.pending.is_none() {
                self.branch_review.diff = None;
                self.send_branch_review(Request::List);
            }
        }
        cx.notify();
    }

    fn send_branch_review(&mut self, request: Request) {
        let view = &mut self.branch_review;
        view.next += 1;
        let id = view.next;
        if self
            .out
            .send(ClientMsg::BranchReview { id, request })
            .is_ok()
        {
            view.pending = Some(id);
        } else {
            view.result = Some(Outcome::Error {
                message: tr!("branch-review-disconnected").into(),
            });
        }
    }

    pub(super) fn branch_review_result(&mut self, id: u64, outcome: Outcome) {
        let view = &mut self.branch_review;
        if view.pending != Some(id) {
            return;
        }
        view.pending = None;
        match outcome {
            Outcome::Branches { base, branches } => {
                view.base = base;
                view.branches = branches;
                view.selected = view
                    .selected()
                    .or(view.branches.first())
                    .map(|branch| branch.name.clone());
                if let Some(branch) = view.selected.clone() {
                    self.load_branch_diff(branch);
                }
            }
            Outcome::Diff { diff } => {
                view.diff = Some(diff);
            }
            outcome @ (Outcome::Merged { .. }
            | Outcome::Discarded { .. }
            | Outcome::Tests { .. }) => {
                view.result = Some(outcome);
                view.diff = None;
                self.send_branch_review(Request::List);
            }
            outcome => view.result = Some(outcome),
        }
    }

    fn load_branch_diff(&mut self, branch: String) {
        let view = &mut self.branch_review;
        view.selected = Some(branch.clone());
        view.diff = None;
        view.file = 0;
        view.confirmation = None;
        self.send_branch_review(Request::Diff { branch });
    }

    fn confirm_branch_action(&mut self, cx: &mut Context<Self>) {
        let view = &mut self.branch_review;
        let Some(branch) = view.selected().cloned() else {
            return;
        };
        let request = match view.confirmation.take() {
            Some(Confirmation::Merge { cleanup, override_lean }) => {
                let Some(diff) = &view.diff else { return };
                let tip = diff.tip.clone();
                let base_tip = diff.base_tip.clone();
                if diff.lean.as_ref().is_some_and(|lean| lean.unconfirmed()) {
                    if !override_lean { return; }
                    Request::MergeWithUnconfirmedLean {
                        branch: branch.name, tip, base_tip, cleanup, confirm: true,
                    }
                } else {
                    Request::Merge { branch: branch.name, tip, base_tip, cleanup, confirm: true }
                }
            }
            Some(Confirmation::Discard) => Request::Discard {
                branch: branch.name,
                tip: view
                    .diff
                    .as_ref()
                    .map_or(branch.tip, |diff| diff.tip.clone()),
                confirm: true,
            },
            Some(Confirmation::SendBack(input)) => {
                let comment = input.read(cx).value().trim().to_string();
                if comment.is_empty() {
                    view.confirmation = Some(Confirmation::SendBack(input));
                    view.result = Some(Outcome::Error {
                        message: tr!("branch-review-empty-comment").into(),
                    });
                    cx.notify();
                    return;
                }
                Request::SendBack {
                    branch: branch.name,
                    comment,
                }
            }
            None => return,
        };
        view.result = None;
        self.send_branch_review(request);
        cx.notify();
    }

    pub(super) fn render_branch_review(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = &self.branch_review;
        let busy = view.pending.is_some();
        v_flex()
            .id("branch-review-view")
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_3()
                    .gap_3()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().text_lg().child(tr!("branch-review-title")))
                    .when(!view.base.is_empty(), |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(tr!("branch-review-base", base = view.base.clone())),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        control("branch-review-refresh", tr!("branch-review-refresh"))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.branch_review.result = None;
                                this.branch_review.confirmation = None;
                                this.branch_review.diff = None;
                                this.send_branch_review(Request::List);
                                cx.notify();
                            })),
                    )
                    .child(self.pane_close_button("branch-review-close", cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        div()
                            .id("branch-review-list")
                            .h_full()
                            .w(px(235.))
                            .max_w_full()
                            .flex_none()
                            .overflow_y_scrollbar()
                            .border_r_1()
                            .border_color(theme.sidebar_border)
                            .child(
                                v_flex()
                                    .p_2()
                                    .gap_1()
                                    .when(
                                        !busy && view.branches.is_empty() && view.result.is_none(),
                                        |list| {
                                            list.child(
                                                div()
                                                    .p_2()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child(tr!("branch-review-empty")),
                                            )
                                        },
                                    )
                                    .children(view.branches.iter().enumerate().map(
                                        |(index, branch)| {
                                            let name = branch.name.clone();
                                            let selected = view.selected.as_ref() == Some(&name);
                                            v_flex()
                                                .id(SharedString::from(format!(
                                                    "branch-review-branch-{index}"
                                                )))
                                                .gap_1()
                                                .p_2()
                                                .rounded_md()
                                                .cursor_pointer()
                                                .when(selected, |row| row.bg(theme.accent))
                                                .hover(|row| row.bg(theme.accent))
                                                .child(
                                                    div().text_sm().truncate().child(name.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .truncate()
                                                        .text_color(theme.muted_foreground)
                                                        .child(branch.subject.clone()),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .flex_wrap()
                                                        .text_xs()
                                                        .child(
                                                            div()
                                                                .rounded_md()
                                                                .px_1()
                                                                .bg(theme.secondary)
                                                                .child(format!(
                                                                    "↑{} ↓{}",
                                                                    branch.ahead, branch.behind
                                                                )),
                                                        )
                                                        .when(branch.dirty, |row| {
                                                            row.child(
                                                                div()
                                                                    .text_color(theme.danger)
                                                                    .child(tr!(
                                                                        "branch-review-dirty"
                                                                    )),
                                                            )
                                                        })
                                                        .children(branch.issue.map(|number| {
                                                            div()
                                                                .rounded_md()
                                                                .px_1()
                                                                .bg(theme.secondary)
                                                                .child(format!("#{number}"))
                                                        })),
                                                )
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.branch_review.pending.is_none() {
                                                        this.branch_review.result = None;
                                                        this.load_branch_diff(name.clone());
                                                        cx.notify();
                                                    }
                                                }))
                                        },
                                    )),
                            ),
                    )
                    .child(self.render_branch_detail(cx)),
            )
    }

    fn render_branch_detail(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = &self.branch_review;
        let busy = view.pending.is_some();
        let mut detail = v_flex().h_full().flex_1().min_w_0().min_h_0().p_3().gap_2();
        if let Some(branch) = view.selected() {
            detail = detail.child(div().text_sm().truncate().child(branch.name.clone()));
            if let Some(path) = &branch.worktree {
                detail = detail.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(path.clone()),
                );
            }
        }
        if let Some(diff) = &view.diff {
            let commit_list = v_flex().gap_1().children(diff.commits.iter().map(|commit| {
                let short: String = commit.hash.chars().take(8).collect();
                div()
                    .whitespace_normal()
                    .text_xs()
                    .child(format!("{short}  {}  {}", commit.date, commit.subject))
            }));
            let files =
                v_flex()
                    .gap_1()
                    .children(diff.files.iter().enumerate().map(|(index, file)| {
                        let path = match &file.old_path {
                            Some(old) => format!("{old} → {}", file.path),
                            None => file.path.clone(),
                        };
                        h_flex()
                            .id(SharedString::from(format!("branch-review-file-{index}")))
                            .gap_2()
                            .px_1()
                            .cursor_pointer()
                            .rounded_md()
                            .text_xs()
                            .when(index == view.file, |row| row.bg(theme.accent))
                            .hover(|row| row.bg(theme.accent))
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(file.status.clone()),
                            )
                            .child(div().min_w_0().truncate().child(path))
                            .when(file.binary, |row| {
                                row.child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .child(tr!("branch-review-binary")),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.branch_review.file = index;
                                cx.notify();
                            }))
                    }));
            detail = detail
                .child(
                    h_flex()
                        .items_start()
                        .gap_3()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_sm().child(tr!("branch-review-commits")))
                                .child(
                                    div()
                                        .id("branch-review-commits")
                                        .max_h(px(92.))
                                        .overflow_y_scrollbar()
                                        .child(if diff.commits.is_empty() {
                                            div()
                                                .text_xs()
                                                .child(tr!("branch-review-no-commits"))
                                                .into_any_element()
                                        } else {
                                            commit_list.into_any_element()
                                        }),
                                ),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_sm().child(tr!("branch-review-files")))
                                .child(
                                    div()
                                        .id("branch-review-files")
                                        .max_h(px(92.))
                                        .overflow_y_scrollbar()
                                        .child(if diff.files.is_empty() {
                                            div()
                                                .text_xs()
                                                .child(tr!("branch-review-no-files"))
                                                .into_any_element()
                                        } else {
                                            files.into_any_element()
                                        }),
                                ),
                        ),
                )
                .when(!diff.stat.is_empty() || diff.truncated, |detail| {
                    detail.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(
                                diff.stat
                                    .lines()
                                    .last()
                                    .unwrap_or_default()
                                    .trim()
                                    .to_owned(),
                            )
                            .when(diff.truncated, |row| {
                                row.child(div().child(tr!("history-truncated")))
                            }),
                    )
                })
                .when_some(diff.lean.as_ref().filter(|_| self.protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL),
                    |detail, lean| detail.child(self.render_lean_claims(diff, lean, busy, cx)))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "branch-review-patch-{}-{}",
                            diff.tip, view.file
                        )))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_scroll()
                        .children(
                            diff.files
                                .get(view.file)
                                .filter(|file| !file.patch.is_empty())
                                .map(|file| {
                                    div()
                                        .id("branch-review-patch")
                                        .child(unified_diff::render(&file.patch, cx))
                                }),
                        ),
                );
        } else {
            detail = detail.child(div().flex_1());
        }
        detail = detail.children(
            view.result
                .as_ref()
                .map(|result| self.render_branch_result(result, cx)),
        );
        if busy {
            detail = detail.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(tr!("branch-review-working")),
            );
        }
        if let Some(branch) = view.selected() {
            let blocked = busy || view.confirmation.is_some();
            let branch_name = branch.name.clone();
            let tip = view
                .diff
                .as_ref()
                .map_or(&branch.tip, |diff| &diff.tip)
                .clone();
            detail = detail.child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        control("branch-review-test", tr!("branch-review-test"))
                            .disabled(blocked || branch.worktree.is_none())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.branch_review.result = None;
                                this.send_branch_review(Request::Test {
                                    branch: branch_name.clone(),
                                    tip: tip.clone(),
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("branch-review-merge")
                            .small()
                            .primary()
                            .label(tr!("branch-review-merge"))
                            .disabled(blocked || view.diff.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.branch_review.confirmation =
                                    Some(Confirmation::Merge { cleanup: true, override_lean: false });
                                cx.notify();
                            })),
                    )
                    .child(
                        control("branch-review-send-back", tr!("branch-review-send-back"))
                            .disabled(blocked || branch.issue.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let input = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder(tr!("branch-review-comment"))
                                });
                                input.update(cx, |state, cx| state.focus(window, cx));
                                this.branch_review.confirmation =
                                    Some(Confirmation::SendBack(input));
                                cx.notify();
                            })),
                    )
                    .child(
                        control("branch-review-discard", tr!("branch-review-discard"))
                            .disabled(blocked)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.branch_review.confirmation = Some(Confirmation::Discard);
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(confirmation) = &view.confirmation {
            detail = detail.child(self.render_branch_confirmation(confirmation, cx));
        }
        detail
    }

    fn render_lean_claims(
        &self, diff: &BranchDiff, review: &fuzukue_proto::LeanBranchReview, busy: bool, cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let branch = diff.branch.clone();
        let tip = diff.tip.clone();
        let base_tip = diff.base_tip.clone();
        v_flex().id("literate-lean-statement-review").flex_none().min_w_0().gap_1()
            .p_2().rounded_md().border_1().border_color(theme.border)
            .child(div().font_bold().text_sm().child(tr!("literate-lean-claims")))
            .when(review.proof_only > 0, |body| body.child(div().text_xs().child(
                tr!("literate-lean-proof-only", n = review.proof_only.to_string()))))
            .when(!review.incomplete.is_empty(), |body| body.child(div().text_xs().text_color(theme.danger)
                .child(tr!("literate-lean-incomplete"))).children(review.incomplete.iter().cloned()))
            .child(v_flex().id("literate-lean-claim-list").max_h(px(280.)).overflow_y_scrollbar().gap_2()
                .children(review.changes.iter().enumerate().map(|(i, change)| {
                    let named = change.card.clone();
                    v_flex().gap_1().p_2().border_1().border_color(theme.border).rounded_md()
                        .child(div().text_sm().child(format!("{} · {}", change.name, change.path)))
                        .when(change.stale_doc, |row| row.child(div().text_xs().text_color(theme.danger)
                            .child(tr!("literate-lean-stale-doc"))))
                        .when_some(named, |row, card| {
                            let deck = change.card_deck.clone().unwrap_or_else(|| self.deck.clone());
                            let label = self.cards.iter().find(|c| c.id == card)
                                .map_or_else(|| card.clone(), |c| c.title.clone());
                            row.child(Button::new(SharedString::from(format!("literate-lean-claim-card-{i}")))
                                .small().label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_paper_from_chat(deck.clone(), Some(card.clone()), cx);
                                })))
                        })
                        .child(h_flex().items_start().gap_3().children([
                            (tr!("literate-lean-before"), &change.before, &change.before_doc),
                            (tr!("literate-lean-after"), &change.after, &change.after_doc),
                        ].into_iter().enumerate().map(|(j, (label, claim, doc))| {
                            v_flex().flex_1().min_w_0().gap_1().text_xs()
                                .child(div().text_color(theme.muted_foreground).child(label))
                                .when(!doc.is_empty(), |col| col.child(crate::paper::paper_text(
                                    gpui_kit::component::text::TextView::markdown(
                                        SharedString::from(format!("literate-lean-claim-doc-{i}-{j}")), doc.clone()), cx)))
                                .child(v_flex().font(crate::appearance::code_font(cx))
                                    .children(claim.as_deref().unwrap_or(tr!("literate-lean-absent")).lines()
                                        .map(|line| div().whitespace_normal().child(line.to_owned()))))
                        })))
                })))
            .when_some(review.confirmation.as_ref(), |body, confirmed| body.child(div().text_xs().child(
                tr!("literate-lean-confirmed", who = confirmed.who.clone(), time = confirmed.time.clone(), tip = confirmed.tip.clone())
            )))
            .when(review.confirmation.is_none() && !review.changes.is_empty() && review.incomplete.is_empty(), |body| {
                body.child(control("literate-lean-confirm", tr!("literate-lean-confirm")).disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.branch_review.confirmation = None;
                        this.send_branch_review(Request::ConfirmLeanClaims {
                            branch: branch.clone(), tip: tip.clone(), base_tip: base_tip.clone(),
                        });
                        cx.notify();
                    })))
            })
    }

    fn render_branch_confirmation(
        &self,
        confirmation: &Confirmation,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let name = self
            .branch_review
            .selected
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        let mut box_el = v_flex()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .bg(theme.secondary);
        let warn_lean = self.branch_review.diff.as_ref().and_then(|d| d.lean.as_ref())
            .is_some_and(|lean| lean.unconfirmed());
        let must_override = warn_lean && matches!(confirmation, Confirmation::Merge { override_lean: false, .. });
        box_el = match confirmation {
            Confirmation::Merge { cleanup, override_lean } => {
                let entity = cx.entity().downgrade();
                box_el
                    .child(tr!("branch-review-confirm-merge", branch = name))
                    .when(warn_lean, |body| {
                        let entity = entity.clone();
                        body.child(div().text_color(theme.danger).child(tr!("literate-lean-merge-warning")))
                            .child(Checkbox::new("literate-lean-merge-override")
                                .label(tr!("literate-lean-merge-override"))
                                .checked(*override_lean)
                                .on_change(move |checked, _, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        if let Some(Confirmation::Merge { override_lean, .. }) = &mut this.branch_review.confirmation {
                                            *override_lean = *checked;
                                            cx.notify();
                                        }
                                    });
                                }))
                    })
                    .child(
                        Checkbox::new("branch-review-cleanup")
                            .label(tr!("branch-review-cleanup"))
                            .checked(*cleanup)
                            .on_change(move |checked, _, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    if let Some(Confirmation::Merge { cleanup, .. }) =
                                        &mut this.branch_review.confirmation
                                    {
                                        *cleanup = *checked;
                                        cx.notify();
                                    }
                                });
                            }),
                    )
            }
            Confirmation::Discard => {
                box_el.child(tr!("branch-review-confirm-discard", branch = name))
            }
            Confirmation::SendBack(input) => {
                box_el.child(Input::new(input).aria_label(tr!("branch-review-comment")))
            }
        };
        box_el.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("branch-review-confirm")
                        .small()
                        .primary()
                        .label(tr!("branch-review-confirm"))
                        .disabled(must_override)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_branch_action(cx))),
                )
                .child(
                    control("branch-review-cancel", tr!("branch-review-cancel")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.branch_review.confirmation = None;
                            cx.notify();
                        }),
                    ),
                ),
        )
    }

    fn render_branch_result(&self, result: &Outcome, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let body = v_flex()
            .gap_1()
            .p_2()
            .text_xs()
            .whitespace_normal()
            .bg(theme.secondary)
            .rounded_md();
        let body = match result {
            Outcome::Error { message } => body.text_color(theme.danger).child(message.clone()),
            Outcome::Conflicts { files } => body
                .child(
                    div()
                        .text_color(theme.danger)
                        .child(tr!("branch-review-conflicts")),
                )
                .children(files.iter().map(|file| div().child(file.clone()))),
            Outcome::Merged { commit, warnings } => body
                .child(
                    div()
                        .text_color(theme.success)
                        .child(tr!("branch-review-merged", commit = commit.clone())),
                )
                .children(
                    warnings
                        .iter()
                        .map(|warning| div().text_color(theme.danger).child(warning.clone())),
                ),
            Outcome::Discarded { tip } => {
                body.child(tr!("branch-review-discarded", tip = tip.clone()))
            }
            Outcome::SentBack => body.child(tr!("branch-review-sent-back")),
            Outcome::Tests { runs } => body
                .when(runs.is_empty(), |body| {
                    body.child(tr!("tests-no-tests-found"))
                })
                .children(runs.iter().map(|run| {
                    let state = match run.state {
                        TestState::Running => tr!("tests-running"),
                        TestState::Passed => tr!("tests-passed"),
                        TestState::Failed => tr!("tests-failed"),
                        TestState::TimedOut => tr!("tests-timed-out"),
                        TestState::Unavailable => tr!("tests-unavailable"),
                        TestState::Cancelled => tr!("branch-review-test-cancelled"),
                    };
                    v_flex()
                        .gap_1()
                        .child(div().child(format!(
                            "{} · {}",
                            run.suite,
                            tr!(
                                "tests-run-result",
                                state = state,
                                passed = run.passed,
                                failed = run.failed
                            )
                        )))
                        .children(run.failures.iter().map(|failure| {
                            let location = match (&failure.file, failure.line) {
                                (Some(file), Some(line)) => format!("{file}:{line} "),
                                (Some(file), None) => format!("{file} "),
                                _ => String::new(),
                            };
                            div()
                                .text_color(theme.danger)
                                .child(format!("{location}{}", failure.message))
                        }))
                        .when(!run.output.is_empty(), |item| {
                            item.child(
                                div()
                                    .font(crate::appearance::code_font(cx))
                                    .text_color(theme.muted_foreground)
                                    .child(run.output.clone()),
                            )
                        })
                })),
            Outcome::Branches { .. } | Outcome::Diff { .. } => body,
        };
        div()
            .id("branch-review-result")
            .max_h(px(145.))
            .overflow_y_scrollbar()
            .child(body)
            .into_any_element()
    }
}
