//! Current thread's session, artifacts, and context usage.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use fuzukue_i18n::STATUS_SEPARATOR;
use gpui_kit::base::{Disableable as _, Selectable as _, StyledExt as _};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    menu::{DropdownMenu as _, PopupMenuItem},
    h_flex, v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, px, relative, ClipboardItem, Context, Entity, Focusable, SharedString, Window};
use fuzukue_proto::{ClientMsg, GoalPace, GoalParallelLimits, GoalProgress, Plan, PlanStatus, ServerEvent, ThreadArtifact, FolderGrant, FolderGrantExpiry, FolderGrantRead};

use crate::{appearance::UI_LINE_HEIGHT, chat::{permission_label, AgentTarget}, icons::Symbol, Workbench};
use crate::chat::chat_activity::{status_row, status_speech};

#[derive(Default)]
pub(super) struct ThreadInfo {
    pub plans: HashMap<String, (Plan, Option<GoalProgress>)>,
    blockers: HashMap<String, BlockerAttention>,
    blocker_expiry: Option<(Instant, gpui_kit::Task<()>)>,
    pub parallel_limits: GoalParallelLimits,
    pub artifacts: HashMap<String, Vec<ThreadArtifact>>,
    pub compaction: HashMap<String, (String, String)>,
    artifacts_open: bool,
    plan_history_open: Option<String>,
    tools_open: bool,
    pub agents: Vec<fuzukue_proto::AgentInfo>,
    agent_open: Option<String>,
    pub git: crate::workspace_git::WorkspaceGit,
    pub folders: HashMap<String, FolderDetails>,
    folder_draft: Option<(String, Entity<InputState>)>,
    folder_audit_open: Option<String>,
}

pub(super) struct FolderDetails {
    pub grants: Vec<FolderGrant>,
    pub audit: Vec<FolderGrantRead>,
    pub audit_before: Option<u64>,
}

pub(super) const FOLDER_EXPIRIES: [FolderGrantExpiry; 5] = [
    FolderGrantExpiry::ThisTurn, FolderGrantExpiry::OneHour, FolderGrantExpiry::Today,
    FolderGrantExpiry::UntilThreadEnds, FolderGrantExpiry::NoExpiry,
];

pub(super) fn folder_expiry_label(expiry: FolderGrantExpiry) -> &'static str {
    match expiry {
        FolderGrantExpiry::ThisTurn => tr!("folder-grant-this-turn"),
        FolderGrantExpiry::OneHour => tr!("folder-grant-one-hour"),
        FolderGrantExpiry::Today => tr!("folder-grant-today"),
        FolderGrantExpiry::UntilThreadEnds => tr!("folder-grant-until-thread-ends"),
        FolderGrantExpiry::NoExpiry => tr!("folder-grant-no-expiry"),
    }
}

fn folder_time(time: i64) -> String {
    chrono::DateTime::from_timestamp_millis(time)
        .map(|at| at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

const BLOCKER_ATTENTION_AGE: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct BlockerAttention {
    // Plan snapshots have no timestamps. Preserve the first local observation of each reason.
    since: HashMap<String, Instant>,
    last_user_prompt: Option<Instant>,
}

impl BlockerAttention {
    fn update(&mut self, plan: &Plan, now: Instant) {
        let reasons: HashSet<_> = plan.next.iter()
            .filter_map(|step| step.blocker.as_ref())
            .map(|blocker| blocker.reason.trim())
            .filter(|reason| !reason.is_empty() && plan.status != PlanStatus::Done)
            .collect();
        self.since.retain(|reason, _| reasons.contains(reason.as_str()));
        for reason in reasons {
            if !self.since.contains_key(reason) {
                self.since.insert(reason.to_owned(), now);
            }
        }
    }

    fn deadlines(&self) -> impl Iterator<Item = Option<Instant>> + '_ {
        self.since.values().map(|&since| {
            self.last_user_prompt.filter(|&prompt| prompt > since)
                .map(|_| since + BLOCKER_ATTENTION_AGE)
        })
    }

    fn count(&self, now: Instant) -> usize {
        self.deadlines().filter(|deadline| deadline.is_none_or(|at| at > now)).count()
    }
}

impl ThreadInfo {
    pub(super) fn record_user_prompt(&mut self, session: &str) {
        self.blockers.entry(session.to_owned()).or_default().last_user_prompt = Some(Instant::now());
    }

    pub(super) fn update_plan(&mut self, session: String, plan: Plan, progress: Option<GoalProgress>) {
        self.blockers.entry(session.clone()).or_default().update(&plan, Instant::now());
        self.plans.insert(session, (plan, progress));
    }
}

impl Workbench {
    pub(super) fn sync_blocker_expiry(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let deadline = self.thread_info.blockers.get(&self.active).and_then(|blockers| {
            blockers.deadlines().flatten().filter(|&at| at > now).min()
        });
        if self.thread_info.blocker_expiry.as_ref().map(|(at, _)| *at) != deadline {
            self.thread_info.blocker_expiry = deadline.map(|at| (at, cx.spawn(async move |this, cx| {
                cx.background_executor().timer(at.saturating_duration_since(Instant::now())).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })));
        }
    }

    pub(super) fn thread_info_visible(&self) -> bool {
        self.protocol >= 6 && !self.active.is_empty() && self.appearance.thread_info_open()
    }

    pub(super) fn request_thread_artifacts(&self) {
        if self.thread_info_visible() {
            let _ = self.out.send(ClientMsg::GetThreadArtifacts {
                session: self.active.clone(),
            });
            self.request_folder_grants(None);
            if self.protocol >= 14 && self.thread_info.agents.is_empty() {
                let _ = self.out.send(ClientMsg::ListAgents);
            }
        }
    }

    pub(super) fn request_folder_grants(&self, audit_before: Option<u64>) {
        if self.thread_info_visible() && self.protocol >= fuzukue_proto::FOLDER_GRANTS_PROTOCOL {
            let _ = self.out.send(ClientMsg::ListFolderGrants { session: self.active.clone(), audit_before });
        }
    }

    fn render_thread_folders(&self, archived: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let details = self.thread_info.folders.get(&self.active);
        let audit_open = self.thread_info.folder_audit_open.as_ref() == Some(&self.active);
        let draft = self.thread_info.folder_draft.as_ref()
            .filter(|(session, _)| session == &self.active).map(|(_, input)| input.clone());
        let muted = cx.theme().muted_foreground;
        let now = chrono::Utc::now().timestamp_millis();
        let grants = details.into_iter().flat_map(|details| details.grants.iter())
            .filter(|grant| grant.expires_at.is_none_or(|at| at > now));
        let no_grants = grants.clone().next().is_none();
        v_flex().id("thread-folders").gap_2()
            .child(div().font_bold().child(tr!("folder-grant-heading")))
            .children(no_grants.then(|| status_row(tr!("folder-grant-empty"), muted)))
            .children(grants.map(|grant| {
                let grant_id = grant.id.clone();
                let session = grant.session.clone();
                let until = grant.expires_at.map(folder_time);
                v_flex().gap_1().min_w_0()
                    .child(div().w_full().text_sm().whitespace_normal().child(grant.path.clone()))
                    .child(status_row(&tr!("folder-grant-duration", expiry = folder_expiry_label(grant.expiry)), muted))
                    .children(until.map(|at| status_row(&tr!("folder-grant-expires-at", time = at), muted)))
                    .child(status_row(&tr!("folder-grant-granted-by", principal = grant.granted_by.clone()), muted))
                    .child(Button::new(SharedString::from(format!("revoke-folder-{grant_id}")))
                        .ghost().xsmall().self_start().label(tr!("folder-grant-revoke"))
                        .on_click(cx.listener(move |this, _, _, _| {
                            let _ = this.out.send(ClientMsg::RevokeFolderGrant {
                                session: session.clone(), grant_id: grant_id.clone(),
                            });
                        })))
            }))
            .when(!archived && draft.is_none(), |section| section.child(
                Button::new("add-folder-grant").secondary().xsmall().self_start().label(tr!("folder-grant-add"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("folder-grant-path")));
                        input.read(cx).focus_handle(cx).focus(window, cx);
                        this.thread_info.folder_draft = Some((this.active.clone(), input));
                        cx.notify();
                    })),
            ))
            .children(draft.map(|input| {
                v_flex().gap_1()
                    .child(Input::new(&input))
                    .child(div().text_sm().child(tr!("folder-grant-allow-for")))
                    .child(h_flex().flex_wrap().gap_1()
                        .children(FOLDER_EXPIRIES.into_iter()
                            .filter(|expiry| *expiry != FolderGrantExpiry::ThisTurn || self.chats[&self.active].busy)
                            .enumerate().map(|(index, expiry)| {
                                let input = input.clone();
                                Button::new(("add-folder-expiry", index)).secondary().xsmall()
                                    .label(folder_expiry_label(expiry))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let path = input.read(cx).value().trim().to_owned();
                                        if path.is_empty() {
                                            return;
                                        }
                                        let _ = this.out.send(ClientMsg::AddFolderGrant {
                                            session: this.active.clone(), path, expiry,
                                        });
                                        this.thread_info.folder_draft = None;
                                        cx.notify();
                                    }))
                            })))
                    .child(Button::new("cancel-folder-grant").ghost().xsmall().self_start().label(tr!("folder-grant-cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.thread_info.folder_draft = None;
                            cx.notify();
                        })))
            }))
            .child(disclosure("folder-grant-audit", tr!("folder-grant-audit").into(), audit_open)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.thread_info.folder_audit_open = (!audit_open).then(|| this.active.clone());
                    if !audit_open {
                        this.request_folder_grants(None);
                    }
                    cx.notify();
                })))
            .when(audit_open, |section| {
                let audit = details.map_or(&[][..], |details| details.audit.as_slice());
                section.children(audit.is_empty().then(|| status_row(tr!("folder-grant-audit-empty"), muted)))
                    .children(audit.iter().map(|entry| v_flex().min_w_0()
                        .child(div().w_full().whitespace_normal().child(entry.path.clone()))
                        .child(status_row(&folder_time(entry.time), muted))))
                    .child(h_flex().flex_wrap().gap_1()
                        .child(Button::new("folder-audit-refresh").ghost().xsmall().label(tr!("folder-grant-audit-latest"))
                            .on_click(cx.listener(|this, _, _, _| this.request_folder_grants(None))))
                        .children(details.and_then(|details| details.audit_before).map(|before| {
                            Button::new("folder-audit-older").ghost().xsmall().label(tr!("folder-grant-audit-older"))
                                .on_click(cx.listener(move |this, _, _, _| this.request_folder_grants(Some(before))))
                        })))
            })
    }

    pub(super) fn thread_info_toggle(&self, cx: &mut Context<Self>) -> Button {
        let open = self.thread_info_visible();
        let label = if open {
            tr!("thread-details-close-thread-details")
        } else {
            tr!("thread-details-open-thread-details")
        };
        let count = self.thread_info.blockers.get(&self.active)
            .map_or(0, |blockers| blockers.count(Instant::now()));
        let waiting = (count > 0).then(|| tr!("thread-details-goal-attention-blocked", count = count));
        let accessibility_label = waiting.as_ref().map_or_else(
            || label.to_owned(),
            |waiting| format!("{label}{STATUS_SEPARATOR}{waiting}"),
        );
        Button::new("thread-info-toggle")
            .relative()
            .ghost()
            .xsmall()
            .flex_none()
            .icon(Symbol::LayoutList.icon())
            .selected(open)
            .children(waiting.as_ref().map(|_| div().id("thread-info-blocker-dot")
                .absolute().top(px(2.)).right(px(2.)).size(px(5.)).rounded_full()
                .bg(cx.theme().warning)))
            .tooltip(waiting.unwrap_or_else(|| label.to_owned()))
            .accessibility_label(accessibility_label)
            .on_click(cx.listener(|this, _, _, cx| {
                if let Err(error) = this.appearance.toggle_thread_info() {
                    this.notice = Some(tr!("thread-details-cannot-save-thread-details-visibility", error = error.to_string()).into());
                }
                this.request_thread_artifacts();
                if this.protocol >= 14 && this.thread_info_visible() && !this.thread_info.agents.is_empty() {
                    let _ = this.out.send(ClientMsg::ListAgents);
                }
                cx.notify();
            }))
    }

    fn open_thread_artifact(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(card) = self
            .cards
            .iter()
            .find(|card| card.path == path || card.tex.contains(&path))
        {
            self.select(card.id.clone(), cx);
        } else if path.ends_with(".pdf") {
            self.open_pdf(path, Some(window), cx);
        } else if path.ends_with(".md") {
            self.open_note(path, cx);
        } else {
            self.open_source_file(path, None, window, cx);
        }
    }

    fn render_thread_plan(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let (plan, progress) = self.thread_info.plans.get(&self.active)?;
        let session = self.active.clone();
        let entity = cx.weak_entity();
        let pace = plan.pace;
        let limits = self.thread_info.parallel_limits;
        let muted = cx.theme().muted_foreground;
        let status = if plan.paused_by_user {
            tr!("chat-goal-paused").to_owned()
        } else if plan.verification.is_some() {
            tr!("thread-details-goal-verification").to_owned()
        } else {
            match (plan.status, progress) {
                (PlanStatus::Done, _) => tr!("thread-details-complete"),
                (PlanStatus::WaitingForUser, _) => tr!("thread-details-goal-attention-waiting"),
                (_, Some(GoalProgress::Paused { .. })) => tr!("thread-details-goal-waiting"),
                _ => tr!("thread-details-goal-ready"),
            }.to_owned()
        };
        let (goal, done_condition) = plan_summary_text(plan, crate::i18n::current());
        let done = plan.done.len();
        let total = done + plan.next.len();
        let (older_done, recent_done) = plan.done.split_at(done.saturating_sub(12));
        let (older_rejected, recent_rejected) = plan.rejected.split_at(plan.rejected.len().saturating_sub(8));
        let history_open = self.thread_info.plan_history_open.as_deref() == Some(self.active.as_str());
        let heading = |label| div().font_semibold().child(label);
        let rejected = |entry: &fuzukue_proto::RejectedApproach| v_flex()
            .child(entry.approach.clone()).child(div().text_color(muted).child(entry.reason.clone()));
        let history = (!older_done.is_empty() || !older_rejected.is_empty()).then(|| v_flex().gap_1()
            .child(disclosure("thread-plan-history", tr!("thread-details-plan-history").into(), history_open)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.thread_info.plan_history_open = (!history_open).then(|| this.active.clone());
                    cx.notify();
                })))
            .children((history_open && !older_done.is_empty()).then(|| v_flex().gap_1()
                .child(heading(tr!("thread-details-completed")))
                .children(older_done.iter().map(|step| div().child(step.clone())))))
            .children((history_open && !older_rejected.is_empty()).then(|| v_flex().gap_1()
                .child(heading(tr!("thread-details-unsuccessful-approaches")))
                .children(older_rejected.iter().map(rejected)))));
        Some(v_flex().id("thread-plan").gap_2()
            .child(div().flex().items_center().justify_between().flex_wrap().gap_1()
                .child(heading(tr!("thread-details-plan")))
                .child(Button::new("thread-replan").ghost().xsmall().label(tr!("thread-details-review-plan"))
                    .on_click(cx.listener(|this, _, _, _| {
                        let _ = this.out.send(ClientMsg::Replan {
                            session: this.active.clone(), reason: "情報欄からの見直し依頼".into(),
                        });
                    }))))
            .child(div().id("thread-goal-title").min_w_0().truncate()
                .tooltip({
                    let goal = goal.clone();
                    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(goal.clone()).build(window, cx)
                })
                .child(goal))
            .children((total > 0).then(|| div().child(tr!("thread-details-goal-steps", done = done, total = total))))
            .child(Button::new("goal-pace").ghost().xsmall().self_start().dropdown_caret(true)
                .label(tr!("thread-details-goal-pace", pace = pace.map_or_else(|| tr!("thread-details-goal-pace-unset").to_owned(), |pace| pace_label(pace, limits))))
                .tooltip(tr!("thread-details-goal-pace-tooltip"))
                .dropdown_menu(move |mut menu, _, _| {
                    for (value, label) in GoalPace::ALL.map(|pace| (Some(pace), pace_label(pace, limits)))
                        .into_iter().chain([(None, tr!("thread-details-clear-long-term-goal").to_owned())]) {
                        let (entity, session) = (entity.clone(), session.clone());
                        menu = menu.item(PopupMenuItem::new(label).checked(pace == value)
                            .on_click(move |_, _, cx| {
                                let _ = entity.update(cx, |this, _| {
                                    let _ = this.out.send(ClientMsg::SetGoalPace { session: session.clone(), pace: value });
                                });
                            }));
                    }
                    menu
                }))
            .child(div().child(tr!("thread-details-goal-status", state = status)))
            .children(plan.question.as_deref().map(str::trim).filter(|question| !question.is_empty())
                .map(|question| v_flex().id("thread-goal-question").gap_1()
                    .child(heading(tr!("thread-details-goal-attention-question")))
                    .child(question.to_owned())))
            .children(plan.verification.as_ref().map(|verification| v_flex().gap_1()
                .child(heading(tr!("thread-details-to-verify"))).child(verification.clone())))
            .children((self.protocol >= fuzukue_proto::GOAL_PAUSE_PROTOCOL && plan.status == PlanStatus::Active).then(|| {
                let session = self.active.clone();
                let (command, label) = if plan.paused_by_user {
                    (ClientMsg::ResumeNow { session }, tr!("chat-resume-now"))
                } else {
                    (ClientMsg::PauseGoal { session }, tr!("thread-details-pause-goal"))
                };
                let out = self.out.clone();
                Button::new("thread-goal-pause").ghost().xsmall().self_start().label(label)
                    .on_click(move |_, _, _| { let _ = out.send(command.clone()); })
            }))
            .children((*progress).and_then(|progress| match progress {
                GoalProgress::Ready => None,
                GoalProgress::Slowed => Some(tr!("thread-details-throttled").to_owned()),
                GoalProgress::Paused { resume_at: Some(time) } => {
                    Some(tr!("thread-details-paused-until", time = crate::until_text(time)))
                }
                GoalProgress::Paused { resume_at: None } => {
                    Some(tr!("thread-details-paused-resume-time-unknown").to_owned())
                }
            }).map(|detail| {
                status_row(&detail, muted).text_color(muted)
            }))
            .child(v_flex().id("thread-goal-done-condition").gap_1()
                .child(heading(tr!("thread-details-completion-criteria"))).child(done_condition.to_owned()))
            .children((!plan.done.is_empty()).then(|| v_flex().gap_1().child(heading(tr!("thread-details-completed")))
                .children(recent_done.iter().map(|step| div().child(step.clone())))))
            .children((!plan.next.is_empty()).then(|| v_flex().gap_1().child(heading(tr!("thread-details-next-steps")))
                .children(plan.next.iter().map(|step| {
                    let worker = step.agent.as_deref().unwrap_or(tr!("thread-details-default-worker"));
                    v_flex().child(step.task.clone())
                        .children(step.blocker.as_ref().map(|blocker| div().text_color(muted)
                            .child(tr!("thread-details-step-blocked", reason = blocker.reason.as_str()))))
                        .children((!step.reason.is_empty()).then(|| status_row(
                            &format!("{worker}{STATUS_SEPARATOR}{}", step.reason), muted,
                        ).text_color(muted)))
                        .children(step.routing.tier.zip(step.tier_reason.as_deref()).map(|(tier, reason)| {
                            status_row(&tr!("tiers-decision", tier = crate::chat::tier_label(tier), reason = reason), muted).text_color(muted)
                        }))
                }))))
            .children((!recent_rejected.is_empty()).then(|| v_flex().gap_1().child(heading(tr!("thread-details-unsuccessful-approaches")))
                .children(recent_rejected.iter().map(rejected))))
            .children(history))
    }

    pub(super) fn render_thread_info(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme().clone();
        let thread = self.threads.iter().find(|thread| thread.id == self.active);
        let panel = v_flex().id("thread-info").size_full().min_h_0().min_w_0()
            .child(
                h_flex().id("thread-info-header").flex_none().justify_between().gap_2()
                    .px(crate::pane_resize::PANE_INSET).pt_4()
                    .when(thread.is_some() && self.protocol >= fuzukue_proto::TESTS_PROTOCOL, |row| {
                        row.child(Button::new("thread-tests").small().ghost().label(tr!("thread-details-tests"))
                            .flex_1().min_w_0().overflow_hidden()
                            .icon(Symbol::Check.icon())
                            .on_click(cx.listener(|this, _, _, cx| this.show_tests(cx))))
                    })
                    .when(thread.is_none() || self.protocol < fuzukue_proto::TESTS_PROTOCOL, |row| {
                        row.child(div().text_sm().child(tr!("sidebar-thread-details")))
                    })
                    .child(Button::new("close-thread-details").ghost().xsmall().flex_none()
                        .icon(Symbol::X.icon())
                        .tooltip(tr!("thread-details-close-thread-details"))
                        .accessibility_label(tr!("thread-details-close-thread-details"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.close_thread_info();
                            cx.notify();
                        }))),
            );
        let Some(thread) = thread else {
            return panel.children(self.render_update_notice(cx)).into_any_element();
        };
        let chat = &self.chats[&self.active];
        let claude = self.stats.get(&(self.active.clone(), "claude".into()));
        let codex = self.stats.get(&(self.active.clone(), "codex".into()));
        let has_claude = !chat.codex_selected();
        let session_id = if has_claude {
            thread.claude_session.as_ref().map(|s| s.id.clone())
        } else {
            chat.codex_thread_id.clone()
        };
        let (connected, mcp_tools) = match if has_claude { claude } else { codex } {
            Some(ServerEvent::SessionStats { connected, mcp_tools, .. }) => (*connected, mcp_tools.as_deref()),
            _ => (None, None),
        };
        let mcp_tools = mcp_tools.or_else(|| {
            self.chat_capabilities
                .as_ref()
                .map(|cap| cap.mcp_tools.as_slice())
        });
        let heading = |text: &'static str| div().font_bold().child(text);
        let muted = |text: String| status_row(&text, theme.muted_foreground).text_color(theme.muted_foreground);
        let model = chat.settings.model.as_deref().unwrap_or(tr!("thread-details-default-model"));
        let mut partner = v_flex().gap_1().child(heading(tr!("thread-details-partner")))
            .child(status_row(&tr!("thread-details-advisor-model", model = model), theme.muted_foreground))
            .child(h_flex().gap_2().children(self.chat_model_menu(cx)).children(self.chat_effort_menu(cx)));
        let status = if self.thread_usage_paused(&thread.id) {
            tr!("thread-details-paused")
        } else if chat.active_worker().is_some() || self.scoped_worker(&thread.id).is_some() {
            tr!("thread-details-waiting-for-worker-report")
        } else if chat.busy {
            tr!("thread-details-running")
        } else if connected == Some(true) {
            tr!("thread-details-waiting")
        } else if session_id.is_some() {
            tr!("thread-details-saved")
        } else {
            tr!("thread-details-not-started")
        };
        let reasoning = chat.settings.effort.as_deref();
        partner = partner.child(muted(tr!("thread-details-reasoning-effort",
            effort = reasoning.map(|effort| crate::chat::effort_label(Some(effort)))
                .unwrap_or(tr!("thread-details-default"))
        )));
        {
            let mail_restricted = self
                .chat_capabilities
                .as_ref()
                .is_some_and(|cap| cap.mail_restricted);
            partner = partner.child(muted(tr!("thread-details-approval-status", mode = (permission_label(chat.settings.permission_mode.effective(mail_restricted))).to_string(), status = status.to_string())));
        }
        if let Some(id) = session_id {
            partner = partner.child(
                Button::new("info-copy-session")
                    .text()
                    .xsmall()
                    .self_start()
                    .label(format!("ID {}", id.chars().take(8).collect::<String>()))
                    .tooltip(tr!("thread-details-session-copy-tooltip", id = id.to_string()))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))
                    }),
            );
        }

        if let Some(model) = chat.active_worker() {
            partner = partner.child(muted(tr!("thread-details-worker-running", model = model.to_string())));
        }
        if let Some(picker) = self.thread_worker_menu(thread, cx) {
            partner = partner.child(v_flex().items_start().gap_1().child(tr!("thread-details-worker")).child(picker));
        }

        let agents = v_flex()
            .gap_1()
            .child(heading(tr!("thread-details-subagent")))
            .children(self.thread_info.agents.iter().enumerate().map(|(index, agent)| {
                let name = agent.name.clone();
                let open = self.thread_info.agent_open.as_ref() == Some(&name);
                let default = self.agent_settings.overrides.get(&name).cloned()
                    .unwrap_or_else(|| agent.default_selection());
                let backend = agent.backend.label();
                let target = AgentTarget::Thread(thread.id.clone(), name.clone());
                v_flex()
                    .gap_1()
                    .child(Button::new(SharedString::from(format!("agent-detail-{index}")))
                        .ghost().xsmall().self_start().label(if name == "planner" { tr!("thread-details-planner").to_owned() } else { name.clone() }).selected(open)
                        .tooltip(agent.description.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.thread_info.agent_open = (!open).then(|| name.clone());
                            cx.notify();
                        })))
                    .child(muted(agent.description.clone()))
                    .children(open.then(|| v_flex().gap_1()
                        .child(muted(format!("{backend}{STATUS_SEPARATOR}{}", if agent.model.is_empty() { tr!("thread-details-cli-default") } else { &agent.model })))
                        .child(muted(tr!("thread-details-settings-for-this-thread").into()))
                        .child(self.agent_override_menu(format!("thread-agent-model-{}", agent.name).into(),
                            target, default, thread.agent_overrides.get(&agent.name), cx))
                        .children(agent.effort.as_ref().map(|effort| muted(tr!("thread-details-reasoning", effort = crate::chat::effort_label(Some(effort))))))
                        .children(agent.path.as_ref().map(|path| muted(path.clone())))
                        .children((!agent.tools.is_empty()).then(|| muted(agent.tools.join(", "))))
                        .child(div().whitespace_normal().child(agent.prompt.clone()))))
            }));

        let attachments = v_flex()
            .gap_1()
            .items_start()
            .child(heading(tr!("thread-details-attached-papers-and-research")))
            .children(thread.attached.is_empty().then(|| muted(tr!("thread-details-none").into())))
            .children(thread.attached.iter().enumerate().map(|(i, path)| {
                let path = path.clone();
                Button::new(SharedString::from(format!("info-attachment-{i}")))
                    .ghost()
                    .xsmall()
                    .line_height(UI_LINE_HEIGHT)
                    .max_w_full()
                    .truncate()
                    .label(self.deck_title(&path))
                    .tooltip(path.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.open_attachment(&path, cx)))
            }));

        let detail = self.thread_info.artifacts.get(&thread.id);
        let artifacts = v_flex()
            .gap_1()
            .items_start()
            .child(
                disclosure(
                    "thread-artifacts-toggle",
                    tr!("thread-details-artifact-count", count = thread.artifacts.len().to_string()),
                    self.thread_info.artifacts_open,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.thread_info.artifacts_open = !this.thread_info.artifacts_open;
                    this.request_thread_artifacts();
                    cx.notify();
                })),
            )
            .children(
                thread
                    .artifacts
                    .iter()
                    .enumerate()
                    .filter(|_| self.thread_info.artifacts_open)
                    .map(|(index, path)| {
                        let artifact =
                            detail.and_then(|items| items.iter().find(|item| &item.path == path));
                        let label = path.rsplit('/').next().unwrap_or(path);
                        let text = match artifact {
                            Some(item) if !item.exists => tr!("thread-details-deleted", label = label.to_string()),
                            Some(item) if item.git_status.is_some() => {
                                format!(
                                    "{label}{STATUS_SEPARATOR}{}",
                                    item.git_status.as_deref().unwrap_or_default()
                                )
                            }
                            _ => label.to_owned(),
                        };
                        let path = path.clone();
                        Button::new(SharedString::from(format!("thread-artifact-{index}")))
                            .ghost()
                            .xsmall()
                            .line_height(UI_LINE_HEIGHT)
                            .max_w_full()
                            .truncate()
                            .accessibility_label(status_speech(&text))
                            .child(status_row(&text, theme.muted_foreground).truncate())
                            .tooltip(path.clone())
                            .disabled(artifact.is_some_and(|item| !item.exists))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_thread_artifact(path.clone(), window, cx);
                            }))
                    }),
            );

        let mut context = v_flex().gap_2().child(heading(tr!("thread-details-context")));
        if claude.is_none() && codex.is_none() {
            context = context.child(muted(tr!("thread-details-no-usage-data-yet").into()));
        }
        for stats in claude.into_iter().chain(codex) {
            let ServerEvent::SessionStats {
                agent,
                context_tokens,
                context_window,
                context_breakdown,
                auto_compact_threshold,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cost_usd,
                turns,
                ..
            } = stats
            else {
                continue;
            };
            if claude.is_some() && codex.is_some() {
                context = context.child(div().font_bold().child(if agent == "claude" {
                    "Claude Code"
                } else {
                    "Codex"
                }));
            }
            if let (Some(used), Some(window)) =
                (*context_tokens, context_window.filter(|window| *window > 0))
            {
                let ratio = used as f32 / window as f32;
                let color = if ratio < 0.5 {
                    theme.success
                } else if ratio < 0.8 {
                    theme.warning
                } else {
                    theme.danger
                };
                context = context
                    .child(tr!("thread-details-context-percent", used = used.to_string(), window = window.to_string(), percent = format!("{:.0}", ratio * 100.)))
                    .child(
                        div()
                            .h(px(6.))
                            .w_full()
                            .rounded_full()
                            .bg(theme.secondary)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(color)
                                    .w(relative(ratio.clamp(0., 1.))),
                            ),
                    );
            } else {
                context = context.child(muted(match context_tokens {
                    Some(used) => tr!("thread-details-tokens-limit-unavailable", used = used.to_string()),
                    None => tr!("thread-details-current-usage-unavailable").into(),
                }));
            }
            if let Some(breakdown) = context_breakdown {
                for (label, tokens) in breakdown {
                    context = context.child(muted(format!("{label} {tokens}")));
                }
            }
            let cost = cost_usd
                .map(|cost| format!("{STATUS_SEPARATOR}${cost:.3}"))
                .unwrap_or_default();
            context = context.child(muted(tr!("thread-details-total-input-output-turns", input_tokens = input_tokens.to_string(), output_tokens = output_tokens.to_string(), turns = turns, cost = cost.to_string())));
            if cache_read_tokens.is_some() || cache_write_tokens.is_some() {
                let show = |value: &Option<u64>| {
                    value.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
                };
                context = context.child(muted(tr!("thread-details-input-cache", read = show(cache_read_tokens).to_string(), written = show(cache_write_tokens).to_string())));
            }
            if let (Some(used), Some(threshold)) = (context_tokens, auto_compact_threshold) {
                context = context.child(muted(tr!("thread-details-tokens-until-auto-compaction", count = threshold.saturating_sub(*used).to_string())));
            }
        }

        let compaction = self.thread_info.compaction.get(&thread.id);
        let can_compact = has_claude && thread.claude_session.is_some()
            && !chat.busy
            && !self.thread_usage_paused(&thread.id)
            && compaction.is_none_or(|(state, _)| state != "running");
        let compact = v_flex()
            .gap_1()
            .items_start()
            .child(
                Button::new("compact-session")
                    .outline()
                    .label(tr!("thread-details-compact-session"))
                    .disabled(!can_compact)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let session = this.active.clone();
                        if this
                            .out
                            .send(ClientMsg::CompactSession {
                                session: session.clone(),
                            })
                            .is_ok()
                        {
                            this.thread_info
                                .compaction
                                .insert(session, ("running".into(), tr!("thread-details-requesting-compaction").into()));
                            cx.notify();
                        }
                    })),
            )
            .children(compaction.map(|(_, message)| muted(fuzukue_view::truncate(message, 200))));
        let tools = v_flex()
            .gap_1()
            .items_start()
            .child(
                disclosure(
                    "thread-mcp-toggle",
                    tr!("thread-details-available-tools").into(),
                    self.thread_info.tools_open,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.thread_info.tools_open = !this.thread_info.tools_open;
                    cx.notify();
                })),
            )
            .children(
                mcp_tools
                    .into_iter()
                    .flatten()
                    .filter(|_| self.thread_info.tools_open)
                    .map(|name| muted(name.clone())),
            );

        panel
            .child(
                div().id("thread-info-scroll").flex_1().min_h_0().w_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex().w_full().px(crate::pane_resize::PANE_INSET).py_4().gap_4().text_sm()
                            .children(self.render_recent_history(cx))
                            .children((self.protocol >= 12).then(|| self.render_workspace_git(cx)))
                            .children(self.render_thread_plan(cx))
                            .children(self.render_update_notice(cx))
                            .child(partner)
                            .children(self.github_policy(thread))
                            .children((self.protocol >= 14).then_some(agents))
                            .child(attachments)
                            .child(artifacts)
            .children((self.protocol >= fuzukue_proto::FOLDER_GRANTS_PROTOCOL)
                .then(|| self.render_thread_folders(thread.archived, cx)))
                            .child(context)
                            .child(compact)
                            .child(tools),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn close_thread_info(&mut self) {
        if let Err(error) = self.appearance.toggle_thread_info() {
            self.notice = Some(tr!("pane-close-error", detail = error.to_string()).into());
        }
    }
}

fn pace_label(pace: GoalPace, limits: GoalParallelLimits) -> String {
    match pace {
        GoalPace::Full => tr!("thread-details-pace-full").into(),
        GoalPace::Normal => tr!("thread-details-pace-normal", limit = limits.normal),
        GoalPace::Economy => tr!("thread-details-pace-economy", limit = limits.economy),
    }
}

pub(super) fn active_goal_title(plan: &Plan) -> Option<&str> {
    let title = plan.goal.trim();
    (plan.status != PlanStatus::Done && !title.is_empty()).then_some(title)
}

fn plan_summary_text(plan: &Plan, locale: fuzukue_i18n::Locale) -> (String, &str) {
    let mut args = fuzukue_i18n::FluentArgs::new();
    args.set("title", plan.goal.as_str());
    (locale.format("thread-details-goal-title", &args), &plan.done_condition)
}

#[cfg(test)]
mod goal_tests {
    use super::*;

    #[test]
    fn goal_blockers_expire_individually_after_a_newer_user_prompt() {
        let mut plan = Plan {
            goal: "引用を確認する".into(), done_condition: String::new(),
            status: PlanStatus::Active, pace: None, done: vec![], next: vec![],
            rejected: vec![], question: None, verification: None, paused_by_user: false,
        };
        let blocked = |reason: &str| fuzukue_proto::PlanStep {
            task: String::new(), agent: None, reason: String::new(), routing: Default::default(),
            tier_reason: None, blocker: Some(fuzukue_proto::StepBlocker {
                reason: reason.into(), needs: None,
            }),
        };
        let start = Instant::now();
        let ten_minutes = Duration::from_secs(600);
        let expiry = start + BLOCKER_ATTENTION_AGE;
        let mut blockers = BlockerAttention::default();

        assert_eq!(active_goal_title(&plan), Some("引用を確認する"));
        plan.status = PlanStatus::WaitingForUser;
        plan.question = Some("どの版を使う？".into());
        blockers.update(&plan, start);
        assert_eq!(blockers.count(start), 0);

        plan.next = vec![blocked("Check the edition"), blocked(" Check the edition "), blocked(" ")];
        blockers.update(&plan, start);
        assert_eq!(blockers.count(expiry + ten_minutes), 1);

        blockers.last_user_prompt = Some(start + ten_minutes);
        plan.next.push(blocked("Check the device"));
        blockers.update(&plan, start + ten_minutes * 2);
        assert_eq!(blockers.count(expiry - Duration::from_secs(1)), 2);
        assert_eq!(blockers.count(expiry), 1);
        assert_eq!(blockers.count(expiry + ten_minutes * 3), 1);

        plan.next.clear();
        blockers.update(&plan, expiry);
        plan.next.push(blocked("Check the edition"));
        blockers.update(&plan, expiry + ten_minutes);
        assert_eq!(blockers.count(expiry + ten_minutes * 5), 1);

        plan.status = PlanStatus::Done;
        blockers.update(&plan, expiry + ten_minutes);
        assert_eq!(blockers.count(expiry + ten_minutes), 0);
        assert_eq!(active_goal_title(&plan), None);
    }

    #[test]
    fn plan_summary_keeps_received_goal_and_completion_criteria_in_either_ui_locale() {
        use fuzukue_i18n::Locale;

        let plan = Plan {
            goal: "原稿の ‘Theorem 1’ の引用を確認する".into(),
            done_condition: "引用と出典が一致する。\n使った版を記録する。".into(),
            status: PlanStatus::Active, pace: None, done: vec![], next: vec![], rejected: vec![], question: None,
            verification: None, paused_by_user: false,
        };
        for (locale, label) in [(Locale::JAPANESE, "目標"), (Locale::supported("en").unwrap(), "Goal")] {
            let (goal, done_condition) = plan_summary_text(&plan, locale);
            assert_eq!(goal, format!("{label}: {}", plan.goal));
            assert_eq!(done_condition, plan.done_condition);
        }
    }
}

pub(super) fn disclosure(id: &'static str, label: String, open: bool) -> Button {
    Button::new(id).ghost().small().label(label).icon(if open {
        Symbol::ChevronDown.icon()
    } else {
        Symbol::ChevronRight.icon()
    })
}
