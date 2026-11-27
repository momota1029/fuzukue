use gpui_kit::{
    component::{h_flex, scroll::Scrollbar, v_flex, ActiveTheme as _, Theme},
    div, prelude::*, px, AnyElement, Context, Div, FontWeight, Pixels, Role, ScrollHandle, SharedString,
    Stateful, Window,
};

use super::*;
use crate::{appearance::ThemeSurfaces, chat::chat_column};

const ROW_BUTTONS: [&str; 4] = ["details", "select", "allow", "deny"];
const DOCK_BUTTONS: [&str; 8] = [
    "toggle", "retry", "bulk-allow", "bulk-deny", "back", "confirm", "dismiss", "cancel-selection",
];

#[derive(Clone, Copy)]
enum Tone {
    Default,
    Primary,
}

fn risk_badge(risk: ApprovalRisk, theme: &Theme) -> Div {
    let color = match risk {
        ApprovalRisk::Low => theme.muted_foreground,
        ApprovalRisk::Mid => theme.warning,
        ApprovalRisk::High => theme.danger,
    };
    div().flex_none().px_1().rounded_md().bg(theme.secondary)
        .text_xs().text_color(color).child(risk_label(risk, crate::i18n::current()))
}

fn field(title: &str, value: String, theme: &Theme) -> Div {
    div().min_w_0().text_sm().whitespace_normal().child(
        div().text_color(theme.muted_foreground).child(title.to_owned()),
    ).child(value)
}

/// Lines shown on a card before its details are opened.
const LISTED: usize = 5;

/// A titled list with one entry per line. `unlisted` counts entries missing from `lines`.
fn list(title: String, lines: &[String], limit: usize, unlisted: usize, theme: &Theme) -> Div {
    let hidden = lines.len().saturating_sub(limit) + unlisted;
    v_flex().w_full().min_w_0().text_sm()
        .child(div().text_color(theme.muted_foreground).child(title))
        .children(lines.iter().take(limit).map(|line| div().min_w_0().whitespace_normal().pl_2().child(line.clone())))
        .children((hidden > 0).then(|| div().pl_2().text_color(theme.muted_foreground)
            .child(tr!("approval-dock-more", count = hidden))))
}

fn file_list(card: &IntegrateCard, limit: usize, theme: &Theme) -> Div {
    list(tr!("approval-dock-files", count = card.files.len()), &card.files, limit, 0, theme)
}

fn flagged_list(card: &IntegrateCard, limit: usize, theme: &Theme) -> Option<Div> {
    (!card.flagged.is_empty()).then(|| list(
        tr!("approval-dock-flagged-lines").to_owned(), &card.flagged, limit, card.unlisted, theme,
    ))
}

fn feedback_text(feedback: &Feedback) -> Option<String> {
    let mut parts = Vec::new();
    if feedback.bulk {
        parts.push(tr!("approval-dock-applied", count = feedback.applied));
    }
    if feedback.conflicts > 0 {
        parts.push(tr!("approval-dock-conflicts", count = feedback.conflicts));
    }
    if feedback.interrupted > 0 {
        parts.push(tr!("approval-dock-interrupted", count = feedback.interrupted));
    }
    parts.extend(feedback.errors.iter().cloned());
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl Workbench {
    pub(crate) fn render_approval_dock(
        &mut self, window: &mut Window, cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.active.is_empty() {
            return None;
        }
        // Live approvals and questions of this thread share the dock with the queue.
        let (live, questions) = self.chats.get(&self.active)
            .map(|chat| (chat.pending_live_approvals(), chat.pending_questions(cx)))
            .unwrap_or_default();
        let live_count = live.len() + questions.len();
        if !self.approvals.available && live_count == 0 {
            return None;
        }
        let feedback = self.approvals.feedback.get(&self.active).and_then(feedback_text);
        let ids: Vec<_> = self.approvals.thread(&self.active).map(|item| item.approval_id.clone()).collect();
        if ids.is_empty() && live_count == 0 && self.approvals.queue_error.is_none() && self.approvals.review.is_none()
            && self.approvals.submitted.as_ref().is_none_or(|submission| submission.thread != self.active)
            && feedback.is_none()
        {
            return None;
        }

        for action in DOCK_BUTTONS {
            self.approval_focus("", action, cx);
        }
        for id in &ids {
            for action in ROW_BUTTONS {
                self.approval_focus(id, action, cx);
            }
        }

        self.approvals.last_focus = window.focused(cx);
        let groups = self.approvals.groups(&self.active);

        let entity = cx.entity();
        let theme = cx.theme();
        let total = ids.len() + live_count;
        let heading = tr!("approval-dock-pending", count = total);
        let first = live.first().map(|approval| approval.summary.text().lines().next().unwrap_or_default().to_owned())
            .or_else(|| (!questions.is_empty()).then(|| tr!("approval-dock-question").to_owned()))
            .or_else(|| self.approvals.thread(&self.active).next()
                .map(|item| format!("{} · {}", tool_title(&item.action.tool), target(item))));
        let expanded = !self.approvals.collapsed;
        let reviewing = self.approvals.review.is_some();
        let header = h_flex().w_full().min_w_0().gap_2().items_center().flex_wrap()
            .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_sm().child(heading))
            .children(first.map(|first| div().min_w_0().flex_1().truncate().text_sm()
                .text_color(theme.muted_foreground).child(first)))
            .when(total > 0 && !reviewing, |header| header.child(self.dock_button(
                "", "toggle", if expanded { tr!("approval-dock-collapse") } else { tr!("approval-dock-expand") },
                true, Tone::Default, cx,
                |this, _, cx| { this.approvals.collapsed = !this.approvals.collapsed; cx.notify(); },
            ).aria_expanded(expanded)));

        // A question form needs more room than a row of buttons. Half the window
        // still leaves the transcript and the composer visible.
        let body_height = if questions.is_empty() {
            (window.viewport_size().height * 0.34).min(px(252.))
        } else {
            window.viewport_size().height * 0.6
        };
        let mut dock = v_flex().id("approval-dock").w_full().min_w_0().gap_1().p_2().flex_none()
            .rounded_md().border_1().border_color(theme.border).bg(theme.secondary)
            .role(Role::Region).aria_label(tr!("approval-dock-title"))
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" || event.keystroke.modifiers.number_of_modifiers() != 0 {
                    return;
                }
                if !this.cancel_batch(window, cx) {
                    let Some(id) = this.approvals.detail.take() else { return };
                    let focus = this.approval_focus(&id, "details", cx);
                    focus.focus(window, cx);
                }
                cx.notify();
                cx.stop_propagation();
            }))
            .child(header);

        if let Some(review) = &self.approvals.review {
            dock = dock.child(self.render_review(review, body_height, cx));
        } else if expanded && total > 0 {
            let selected = self.approvals.selected_requests(&self.active);
            let mut rows = Self::dock_scroll(&self.approvals.scroll, body_height);
            let busy = self.chats.get(&self.active).is_some_and(|chat| chat.busy());
            for approval in &live {
                rows = rows.child(self.render_live_approval(approval, busy, &entity, cx));
            }
            for card in questions {
                rows = rows.child(div().w_full().min_w_0().p_2().rounded_md().border_1()
                    .border_color(theme.border).bg(theme.popover).child(card));
            }
            for group in groups {
                for (index, item) in group.into_iter().enumerate() {
                    rows = rows.child(self.render_request(item, index == 0, selected.first().copied(), cx));
                }
            }
            dock = dock
                .child(self.scrollbar(rows))
                .children((selected.len() >= 2).then(|| self.render_bulk_controls(selected.len(), cx)));
        }
        if let Some(error) = &self.approvals.queue_error {
            dock = dock.child(h_flex().gap_2().flex_wrap()
                .child(div().min_w_0().text_sm().text_color(theme.danger).child(error.clone()))
                .child(self.dock_button("", "retry", tr!("approval-dock-retry"), true, Tone::Default, cx,
                    |this, _, cx| { this.refresh_approvals(); cx.notify(); })));
        }
        if let Some(submission) = self.approvals.submitted.as_ref().filter(|item| item.thread == self.active) {
            let count = submission.outstanding.len();
            dock = dock.child(div().text_sm().text_color(theme.muted_foreground)
                .child(tr!("approval-dock-awaiting-result", count = count)));
        }
        if let Some(feedback) = feedback {
            dock = dock.child(h_flex().gap_2().items_start().flex_wrap()
                .child(div().min_w_0().text_sm().text_color(theme.muted_foreground).child(feedback))
                .child(self.dock_button("", "dismiss", tr!("approval-dock-dismiss"), true, Tone::Default, cx,
                    |this, _, cx| { this.approvals.feedback.remove(&this.active); cx.notify(); })));
        }
        Some(chat_column().pt_1().flex_none().child(dock).into_any_element())
    }

    fn dock_scroll(scroll: &ScrollHandle, height: Pixels) -> Stateful<Div> {
        v_flex().id("approval-dock-scroll").w_full().min_w_0().max_h(height)
            .overflow_y_scroll().track_scroll(scroll).gap_1().pr_3()
    }

    fn scrollbar(&self, rows: Stateful<Div>) -> Div {
        div().relative().w_full().min_w_0().child(rows)
            .child(Scrollbar::vertical(&self.approvals.scroll))
    }

    fn render_request(
        &self, item: &ApprovalSummary, first_in_job: bool, selected_kind: Option<&ApprovalSummary>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = &item.approval_id;
        let detail = self.approvals.detail.as_ref() == Some(id);
        let selected = self.approvals.selected.contains(id);
        let allow = id.clone();
        let deny = id.clone();
        let disclosure = id.clone();
        let select = id.clone();
        let scope = json(&item.action.scope);
        let surface = ThemeSurfaces::current(cx);

        let controls = h_flex().gap_1().items_center().flex_wrap()
            .child(self.dock_button(
                id, "details", tr!("approval-dock-details"), true, Tone::Default, cx,
                move |this, _, cx| {
                    this.approvals.detail = (this.approvals.detail.as_ref() != Some(&disclosure)).then(|| disclosure.clone());
                    cx.notify();
                },
            ).aria_expanded(detail))
            .children((same_batch(item, item) && selected_kind.is_none_or(|first| same_batch(first, item))).then(|| {
                self.dock_button(
                    id, "select", if selected { tr!("approval-dock-deselect") } else { tr!("approval-dock-select") },
                    self.can_decide(), Tone::Default, cx,
                    move |this, _, cx| { this.toggle_selection(&select); cx.notify(); },
                ).aria_selected(selected)
            }))
            .child(self.dock_button(
                id, "allow", decision_label(ALLOW), self.can_decide(), Tone::Primary, cx,
                move |this, _, cx| {
                    this.decide_one(&allow, ALLOW);
                    cx.notify();
                },
            ))
            .child(self.dock_button(
                id, "deny", decision_label(DENY), self.can_decide(), Tone::Default, cx,
                move |this, _, cx| {
                    this.decide_one(&deny, DENY);
                    cx.notify();
                },
            ));

        let args = &item.action.args;
        let card = integrate_card(item);
        let details = detail.then(|| {
            let details = v_flex().id("approval-request-details").w_full().gap_1().p_2().rounded_md().bg(surface.code)
                .child(field(tr!("approval-dock-requester"), item.owner.requester.clone(), theme))
                .child(field(tr!("approval-dock-grant-scope"), tr!("approval-dock-grant-once").to_owned(), theme));
            if let Some(card) = &card {
                return details
                    .child(file_list(card, usize::MAX, theme))
                    .children(flagged_list(card, usize::MAX, theme))
                    .child(div().text_sm().min_w_0().whitespace_normal().text_color(theme.muted_foreground)
                        .child(tr!("approval-dock-patch", patch = card.patch.as_str(), head = card.head.as_str())));
            }
            let diff = args.get("diff").or_else(|| item.action.scope.get("diff"));
            details
                .child(field(tr!("approval-dock-scope"), scope.clone(), theme))
                .child(field(tr!("approval-dock-arguments"), serde_json::to_string_pretty(args).unwrap_or_else(|_| json(args)), theme))
                .children(diff.map(|diff| field(tr!("approval-dock-diff"), json(diff), theme)))
        });
        let body = match &card {
            Some(card) => v_flex().w_full().min_w_0().gap_1()
                .child(div().text_sm().min_w_0().whitespace_normal().child(card.explanation))
                .children(card.privacy.clone().map(|privacy| div().text_sm().min_w_0().whitespace_normal()
                    .text_color(theme.warning).child(privacy)))
                // The open details list everything, so the summary lists are not repeated.
                .children(flagged_list(card, LISTED, theme).filter(|_| !detail)),
            None => v_flex().w_full().min_w_0().gap_1()
                .child(div().text_sm().min_w_0().whitespace_normal().child(target(item)))
                .child(div().text_xs().min_w_0().truncate().text_color(theme.muted_foreground)
                    .child(format!("{} · {scope}", tr!("approval-dock-scope"))))
                .child(div().text_sm().min_w_0().whitespace_normal().child(item.reason.clone())),
        };

        v_flex().w_full().min_w_0().gap_1()
            .children(first_in_job.then(|| div().text_sm().font_weight(FontWeight::SEMIBOLD).min_w_0().truncate()
                .text_color(theme.muted_foreground)
                .child(format!("{} · {}", tr!("approval-dock-job"), item.owner.job))))
            .child(
                v_flex().id(SharedString::from(format!("approval-row-{id}")))
                    .role(Role::Group).aria_label(format!("{} · {}", tool_title(&item.action.tool), target(item)))
                    .w_full().min_w_0().gap_1().p_2().rounded_md()
                    .border_1().border_color(if selected { theme.ring } else { theme.border }).bg(theme.popover)
                    .child(h_flex().min_w_0().justify_between().items_start().gap_2()
                        .child(div().font_weight(FontWeight::SEMIBOLD).min_w_0().truncate().child(tool_title(&item.action.tool)))
                        .child(risk_badge(item.risk, theme)))
                    .child(body)
                    .child(div().text_xs().text_color(theme.muted_foreground)
                        .child(match &card {
                            Some(card) => format!("{} · {} · {} · {}", tr!("approval-dock-scope"), card.workspace,
                                tr!("approval-dock-expires"), expiry(item)),
                            None => format!("{} · {}", tr!("approval-dock-expires"), expiry(item)),
                        }))
                    .child(controls)
                    // The file list follows the buttons so that they stay in reach in the dock.
                    .children(card.as_ref().filter(|_| !detail).map(|card| file_list(card, LISTED, theme)))
                    .children(details),
            )
    }

    /// A live approval: the same controls that used to sit in the transcript.
    fn render_live_approval(
        &self, approval: &crate::chat::PendingLiveApproval, busy: bool,
        entity: &gpui_kit::Entity<Workbench>, cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = approval.request_id.clone();
        let details = approval.summary.text();
        let preview: String = details.lines().take(3).collect::<Vec<_>>().join("\n").chars().take(180).collect();
        let clipped = preview.len() < details.trim_end_matches('\n').len();
        let open = self.approvals.live_details.contains(&id);
        let mut header = h_flex().flex_wrap().gap_2().items_center()
            .child(div().text_color(theme.muted_foreground).text_sm().child(tr!("chat-approval-waiting")))
            .child(if id.starts_with(fuzukue_proto::FOLDER_ACCESS_REQUEST) {
                crate::chat::folder_approval_buttons(&id, entity, busy).into_any_element()
            } else {
                crate::chat::approval_buttons(&id, entity).into_any_element()
            });
        if approval.remember_shell {
            let request_id = id.clone();
            let entity = entity.clone();
            use gpui_kit::component::button::{Button, ButtonVariants as _};
            header = header.child(Button::new(SharedString::from(format!("remember-{request_id}")))
                .label(tr!("chat-remember-shell").to_owned()).secondary()
                .on_click(move |_, _, cx| {
                    let _ = entity.read(cx).out.send(ClientMsg::RememberShellApproval {
                        request_id: request_id.clone(), prefix: None,
                    });
                }));
        }
        let toggle = id.clone();
        let code_font = self.appearance.code_font.clone();
        v_flex().id(SharedString::from(format!("approval-live-{id}")))
            .role(Role::Group).aria_label(preview.clone())
            .w_full().min_w_0().gap_2().p_2().rounded_md().border_1().border_color(theme.border).bg(theme.popover)
            .child(header)
            .children((approval.shell && !approval.remember_shell).then(|| {
                div().text_xs().text_color(theme.muted_foreground).whitespace_normal().child(tr!("chat-approval-shell-once"))
            }))
            .children(clipped.then(|| div().id(SharedString::from(format!("approval-details-{id}")))
                .role(Role::Button).aria_expanded(open).cursor_pointer()
                .text_sm().text_color(theme.muted_foreground)
                .child(format!("{} {}", if open { "▾" } else { "▸" }, tr!("chat-approval-details")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.approvals.live_details.remove(&toggle) {
                        this.approvals.live_details.insert(toggle.clone());
                    }
                    cx.notify();
                }))))
            .child(div()
                .id(SharedString::from(format!("approval-preview-{id}")))
                .w_full().min_w_0().text_sm().whitespace_normal()
                .when(approval.shell, |details| details.font(code_font))
                .max_h(px(if open { 240. } else { 66. })).overflow_y_scroll()
                .child(if clipped && !open { format!("{preview}…") } else { details.into_owned() }))
    }

    fn decide_one(&mut self, id: &str, decision: ApprovalDecision) {
        if self.approvals.thread(&self.active).any(|item| item.approval_id == id) {
            self.submit_approvals(vec![id.to_owned()], decision, Feedback::default());
        }
    }

    fn render_bulk_controls(&self, count: usize, cx: &Context<Self>) -> impl IntoElement {
        h_flex().gap_1().items_center().flex_wrap()
            .child(div().text_sm().child(tr!("approval-dock-selected", count = count)))
            .child(self.dock_button("", "bulk-allow", tr!("approval-dock-bulk-allow"), self.can_decide(),
                Tone::Default, cx, |this, window, cx| { this.review_batch(ALLOW, window, cx); cx.notify(); }))
            .child(self.dock_button("", "bulk-deny", tr!("approval-dock-bulk-deny"), self.can_decide(),
                Tone::Default, cx, |this, window, cx| { this.review_batch(DENY, window, cx); cx.notify(); }))
            .child(self.dock_button("", "cancel-selection", tr!("approval-dock-clear-selection"), true,
                Tone::Default, cx, |this, _, cx| { this.approvals.selected.clear(); cx.notify(); }))
    }

    fn render_review(&self, review: &Review, height: Pixels, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = Self::dock_scroll(&self.approvals.scroll, height).children(review.requests.iter().map(|item| {
            let still_pending = self.approvals.thread(&self.active).any(|current| current.approval_id == item.approval_id);
            v_flex().gap_1().p_2().rounded_md().bg(theme.popover)
                .child(field(tr!("approval-dock-target"), target(item), theme))
                .child(field(tr!("approval-dock-scope"), json(&item.action.scope), theme))
                .children((!still_pending).then(|| div().text_sm().text_color(theme.warning)
                    .child(tr!("approval-dock-already-resolved"))))
        }));
        v_flex().id("approval-bulk-review").gap_1()
            .child(div().font_weight(FontWeight::SEMIBOLD).text_sm().child(format!("{} · {} · {}",
                decision_label(review.decision), review.requests[0].action.tool,
                tr!("approval-dock-review-count", count = review.requests.len()))))
            .child(self.scrollbar(rows))
            .child(h_flex().gap_1().flex_wrap()
                .child(self.dock_button("", "back", tr!("approval-dock-back"), true, Tone::Default, cx,
                    |this, window, cx| { this.cancel_batch(window, cx); cx.notify(); }))
                .child(self.dock_button("", "confirm", tr!("approval-dock-confirm"), self.can_decide(),
                    Tone::Primary, cx, |this, window, cx| { this.confirm_batch(window, cx); cx.notify(); })))
    }

    fn dock_button(
        &self, item: &str, action: &'static str, title: &'static str, enabled: bool, tone: Tone,
        cx: &Context<Self>, on_click: impl Fn(&mut Workbench, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let id = if item.is_empty() { format!("approval-{action}") } else { format!("approval-{action}-{item}") };
        let (fill, text, hover) = match tone {
            Tone::Primary => (theme.button_primary, theme.button_primary_foreground, theme.button_primary_hover),
            Tone::Default => (theme.button, theme.button_foreground, theme.button_hover),
        };
        let focus = &self.approvals.focus[&(item.to_owned(), action)];
        let mut button = div();
        if enabled && !item.is_empty() && self.approvals.last_focus.as_ref() != Some(focus) {
            let focus = focus.clone();
            let scroll = self.approvals.scroll.clone();
            button = button.on_children_prepainted(move |children, window, cx| {
                if !focus.is_focused(window) {
                    return;
                }
                let [label] = children.as_slice() else { return };
                let bounds = scroll.bounds();
                let top = label.top() - px(4.);
                let bottom = label.bottom() + px(4.);
                let delta = (bounds.top() - top).max(px(0.)) + (bounds.bottom() - bottom).min(px(0.));
                if delta != px(0.) {
                    let scroll = scroll.clone();
                    window.defer(cx, move |window, _| {
                        let mut offset = scroll.offset();
                        offset.y += delta;
                        scroll.set_offset(offset);
                        window.refresh();
                    });
                }
            });
        }
        button.id(SharedString::from(id)).role(Role::Button).aria_label(title).flex_none()
            .rounded_md().border_1().border_color(theme.border).bg(fill).text_color(text)
            .px_2().py_0p5().text_xs().child(title)
            .when(!enabled, |button| button.opacity(0.5))
            .when(enabled || self.approvals.last_focus.as_ref() == Some(focus), |button| {
                let ring = theme.ring;
                button.track_focus(focus).tab_index(0).tab_stop(enabled)
                    .focus_visible(move |style| style.border_color(ring))
            })
            .when(enabled, |button| {
                button.cursor_pointer().hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
            })
    }
}
