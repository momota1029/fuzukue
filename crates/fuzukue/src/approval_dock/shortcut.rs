//! Alt+Y (Option+Y on macOS) allows the first pending approval of the active
//! thread once, as a click on its allow button would.
//!
//! The shortcut only approves a card that the last frame drew with its allow
//! button in reach. It consumes the key whenever such a card is the target, so
//! that Option+Y types nothing into the composer, and leaves the key alone when
//! there is nothing it could act on.

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::{point, px, Bounds, Context, Keystroke, Pixels, Window};

use super::*;
use crate::{chat::PendingLiveApproval, ConnectionStatus};

pub(super) const SHORTCUT: &str = if cfg!(target_os = "macos") { "⌥Y" } else { "Alt+Y" };
/// A card that has just taken the first place ignores the shortcut this long.
pub(super) const SETTLE: Duration = Duration::from_millis(500);
/// A press without a seen release counts as held until this long after its last
/// key down. It has to exceed the longest delay before a key starts repeating.
const HELD_EXPIRY: Duration = Duration::from_secs(3);

/// The card at the top of the dock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Card {
    Question,
    Live(String),
    /// A queued request and the revision the card shows.
    Queued(String, i64),
}

/// What the current frame drew.
#[derive(Clone, Copy, Default)]
pub(super) struct Painted {
    pub(super) dock: bool,
    pub(super) card: Option<Bounds<Pixels>>,
    pub(super) allow: Option<Bounds<Pixels>>,
}

#[derive(Default)]
pub(super) struct Shortcut {
    /// The thread and first card last drawn, and when it took that place.
    first: Option<(String, Card, Instant)>,
    pub(super) painted: Rc<Cell<Painted>>,
    /// The pressed key: whether it was consumed and when it last arrived.
    held: Option<(bool, Instant)>,
    /// A live approval already allowed here and still waiting for its resolution.
    sent_live: Option<String>,
    /// The card that refused the shortcut.
    refused: Option<(String, Card)>,
}

impl Shortcut {
    /// Call before each frame. A card that the last frame did not draw starts
    /// its waiting time again when it appears.
    pub(super) fn begin_frame(&mut self) {
        if self.painted.take().allow.is_none() {
            self.first = None;
        }
    }

    pub(super) fn observe(&mut self, thread: &str, card: Option<Card>) {
        let unchanged = matches!((&self.first, &card),
            (Some((shown, previous, _)), Some(card)) if shown == thread && previous == card);
        if !unchanged {
            self.first = card.map(|card| (thread.to_owned(), card, Instant::now()));
        }
    }

    pub(super) fn refused(&self, thread: &str) -> Option<&Card> {
        let (refused, card) = self.refused.as_ref()?;
        let (shown, first, _) = self.first.as_ref()?;
        (refused == thread && shown == thread && first == card).then_some(card)
    }

    pub(super) fn disconnect(&mut self) {
        self.first = None;
        self.sent_live = None;
        self.refused = None;
    }

    pub(super) fn key_up(&mut self, key: &Keystroke) {
        if key.key == "y" {
            self.held = None;
        }
    }
}

pub(super) fn first_card(
    live: &[PendingLiveApproval], question: bool, queued: Option<&ApprovalSummary>,
) -> Option<Card> {
    live.first().map(|approval| Card::Live(approval.request_id.clone()))
        .or_else(|| question.then_some(Card::Question))
        .or_else(|| queued.map(|item| Card::Queued(item.approval_id.clone(), item.updated_at)))
}

/// Why a live approval needs a click. Live requests carry no risk level, so Git
/// operations are held back by their kind. Folder access has no single "allow
/// once" button.
pub(super) fn live_click_reason(id: &str) -> Option<&'static str> {
    if id.starts_with(fuzukue_proto::FOLDER_ACCESS_REQUEST) {
        Some("approval-dock-shortcut-folder")
    } else if id.starts_with(fuzukue_proto::GIT_APPROVAL) {
        Some("approval-dock-shortcut-git")
    } else {
        None
    }
}

pub(super) fn queued_click_reason(item: &ApprovalSummary) -> Option<&'static str> {
    (item.risk == ApprovalRisk::High).then_some("approval-dock-shortcut-high-risk")
}

/// How far to move the dock's content so that the first card can be read from its
/// top and its allow button is in view. `None` when both already are.
fn reveal_offset(view: Bounds<Pixels>, card: Bounds<Pixels>, allow: Bounds<Pixels>) -> Option<Pixels> {
    let slack = px(1.);
    let reachable = allow.top() >= view.top() - slack && allow.bottom() <= view.bottom() + slack;
    // A card taller than the dock cannot be shown whole.
    let top_in_view = card.top() >= view.top() - slack || card.size.height > view.size.height;
    if reachable && top_in_view {
        return None;
    }
    let to_top = view.top() - card.top();
    Some(to_top - (allow.bottom() + to_top - view.bottom()).max(px(0.)))
}

impl Workbench {
    /// Handle a key down. Returns whether the key was used here.
    pub(crate) fn approval_shortcut_key(
        &mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>,
    ) -> bool {
        // AltGr arrives as Control+Alt, so it never matches.
        if key.key != "y" || !key.modifiers.alt || key.modifiers.number_of_modifiers() != 1 {
            return false;
        }
        let now = Instant::now();
        if let Some((consumed, last)) = self.approvals.shortcut.held {
            if now.duration_since(last) < HELD_EXPIRY {
                // Key repeat. X11 and macOS do not mark it, so the release is tracked.
                self.approvals.shortcut.held = Some((consumed, now));
                return consumed;
            }
        }
        let consumed = self.press_approval_shortcut(now, window, cx);
        self.approvals.shortcut.held = Some((consumed, now));
        // The release reaches the workbench only when focus is inside it.
        if consumed && !self.view_focus.contains_focused(window, cx) {
            self.view_focus.focus(window, cx);
        }
        consumed
    }

    fn press_approval_shortcut(&mut self, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let covered = window.has_active_dialog(cx)
            || window.has_active_sheet(cx)
            || self.palette.is_some()
            || self.workspace_browser_open()
            || self.history.panel.is_some()
            || self.screenshots.preview.is_some()
            || self.app_menu.is_expanded();
        if covered
            || !matches!(self.status, ConnectionStatus::Connected)
            || self.workspaces.pending
            || self.approvals.review.is_some()
            || crate::focused_composition(&self.input, window, cx)
            || crate::focused_composition(&self.comment_input, window, cx)
            || self.editing.as_ref().is_some_and(|editor| crate::focused_composition(&editor.input, window, cx))
        {
            return false;
        }
        let painted = self.approvals.shortcut.painted.get();
        if !painted.dock {
            return false;
        }
        let (live, question) = self.chats.get(&self.active)
            .map(|chat| (chat.pending_live_approvals(), !chat.pending_questions(cx).is_empty()))
            .unwrap_or_default();
        let queued = self.approvals.thread(&self.active).next();
        let Some(card) = first_card(&live, question, queued) else { return false };
        let reason = match &card {
            Card::Question => return false,
            Card::Live(id) => live_click_reason(id),
            Card::Queued(..) => queued.and_then(queued_click_reason),
        };
        if self.approvals.collapsed {
            self.approvals.collapsed = false;
            cx.notify();
            return true;
        }
        let since = self.approvals.shortcut.first.as_ref()
            .filter(|(thread, shown, _)| thread == &self.active && shown == &card)
            .map(|(_, _, since)| *since);
        // A card that has not been drawn yet is never allowed.
        let (Some(since), Some(card_bounds), Some(allow)) = (since, painted.card, painted.allow) else {
            cx.notify();
            return true;
        };
        let scroll = &self.approvals.scroll;
        if let Some(delta) = reveal_offset(scroll.bounds(), card_bounds, allow) {
            let mut offset = scroll.offset();
            offset.y += delta;
            scroll.set_offset(point(offset.x, offset.y));
            self.approvals.shortcut.first = Some((self.active.clone(), card, now));
            cx.notify();
            return true;
        }
        if reason.is_some() {
            self.approvals.shortcut.refused = Some((self.active.clone(), card));
            cx.notify();
            return true;
        }
        if now.duration_since(since) < SETTLE {
            return true;
        }
        match card {
            Card::Question => {}
            Card::Live(id) => {
                if self.approvals.shortcut.sent_live.as_ref() != Some(&id) {
                    let message = ClientMsg::Resolve { request_id: id.clone(), decision: "accept".into() };
                    if self.out.send(message).is_ok() {
                        self.approvals.shortcut.sent_live = Some(id);
                    }
                }
            }
            Card::Queued(id, _) => self.decide_one(&id, ALLOW),
        }
        cx.notify();
        true
    }

    /// Reset what the shortcut knows about the screen. Call once per frame.
    pub(crate) fn begin_approval_frame(&mut self) {
        self.approvals.shortcut.begin_frame();
    }

    pub(crate) fn approval_shortcut_released(&mut self, key: &Keystroke) {
        self.approvals.shortcut.key_up(key);
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::size;

    use super::*;

    fn rect(top: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(top)), size(px(100.), px(height)))
    }

    #[test]
    fn a_card_must_be_in_view_before_it_can_be_allowed() {
        let view = rect(100., 200.);
        assert_eq!(reveal_offset(view, rect(100., 120.), rect(180., 20.)), None);
        // Scrolled past: bring the top of the card back.
        assert_eq!(reveal_offset(view, rect(-100., 120.), rect(-20., 20.)), Some(px(200.)));
        // The top is hidden although the button shows.
        assert_eq!(reveal_offset(view, rect(60., 120.), rect(140., 20.)), Some(px(40.)));
        // A card taller than the dock only needs its button, which wins over its top.
        assert_eq!(reveal_offset(view, rect(0., 400.), rect(200., 20.)), None);
        assert_eq!(reveal_offset(view, rect(100., 400.), rect(350., 20.)), Some(px(-70.)));
    }

    #[test]
    fn a_new_first_card_restarts_the_wait() {
        let mut shortcut = Shortcut::default();
        shortcut.observe("thread", Some(Card::Live("a".into())));
        let since = shortcut.first.as_ref().unwrap().2;
        std::thread::sleep(Duration::from_millis(2));
        shortcut.observe("thread", Some(Card::Live("a".into())));
        assert_eq!(shortcut.first.as_ref().unwrap().2, since);
        shortcut.observe("other", Some(Card::Live("a".into())));
        assert!(shortcut.first.as_ref().unwrap().2 > since);
        // Nothing was drawn, so the same card counts as new next time.
        shortcut.begin_frame();
        assert!(shortcut.first.is_none());
    }
}
