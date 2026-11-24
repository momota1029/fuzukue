use gpui_kit::base::StyledExt as _;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use fuzukue_i18n::STATUS_SEPARATOR;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    switch::Switch,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, AnyElement, Context, Entity, SharedString, Window};
use fuzukue_proto::{
    AgentCaller, AgentInfo, AgentInvocation, AgentOutcome, AgentPolicyOverride, AgentStats,
    ClientMsg,
};

use crate::{chat::chat_activity::status_row, Workbench};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    #[default]
    General,
    Agents,
    History,
}

#[derive(Default)]
pub(super) struct AgentManagement {
    pub tab: Tab,
    pub stats: Option<BTreeMap<String, AgentStats>>,
    pub history: Option<Vec<AgentInvocation>>,
    pub agent: Option<String>,
    pub session: Option<String>,
    hints: HashMap<String, (String, Entity<InputState>)>,
}

fn caller_label(caller: AgentCaller) -> &'static str {
    match caller {
        AgentCaller::FrontDesk => tr!("agents-front-desk"),
        AgentCaller::Planner => tr!("agents-planner"),
        AgentCaller::Worker => tr!("agents-worker"),
    }
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y/%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

fn stats_label(stats: &AgentStats) -> String {
    let rate = stats
        .days_30
        .success_rate
        .map_or_else(|| "—".into(), |rate| format!("{:.0}%", rate * 100.));
    let last = stats.last_used_at_ms.map_or_else(|| tr!("agents-never-used").into(), date);
    tr!("agents-usage-summary", weekly = stats.days_7.uses.to_string(), monthly = stats.days_30.uses.to_string(), rate = rate.to_string(), last = last.to_string())
}

fn stats_detail(stats: &AgentStats) -> impl Iterator<Item = String> + '_ {
    [(tr!("agents-7-days"), &stats.days_7), (tr!("agents-30-days"), &stats.days_30)]
        .into_iter()
        .flat_map(|(label, period)| {
            let median = period
                .median_duration_ms
                .map_or_else(|| "—".into(), |ms| tr!("agents-duration", seconds = format!("{:.1}", ms as f64 / 1000.)));
            let mut lines = vec![tr!("agents-usage-period", label = label.to_string(), median = median.to_string(), tokens = period.total_tokens.to_string())];
            for (tier, stats) in &period.tiers {
                lines.push(tier_stats_line(*tier, stats));
            }
            lines
        })
}

fn tier_stats_line(tier: fuzukue_proto::Tier, stats: &fuzukue_proto::TierStats) -> String {
    let rate = format!("{:.0}%", 100. * stats.passes as f64 / stats.attempts.max(1) as f64);
    let cost = stats.cost_usd.map_or_else(|| "—".into(), |cost| format!("${cost:.4}"));
    let weighted = stats.weighted_tokens.map_or_else(|| "—".into(), |cost| format!("{cost:.0}"));
    tr!("tiers-stats", tier = crate::chat::tier_label(tier), count = stats.attempts, rate = rate, cost = cost, weighted = weighted)
}

impl Workbench {
    pub(super) fn render_tier_totals(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let stats = self.agent_management.stats.as_ref()?;
        let mut totals = BTreeMap::<_, fuzukue_proto::TierStats>::new();
        for (tier, stat) in stats.values().flat_map(|agent| &agent.days_30.tiers) {
            let total = totals.entry(*tier).or_default();
            total.attempts += stat.attempts;
            total.passes += stat.passes;
            if let Some(cost) = stat.cost_usd {
                *total.cost_usd.get_or_insert(0.0) += cost;
            }
            if let Some(weighted) = stat.weighted_tokens {
                *total.weighted_tokens.get_or_insert(0.0) += weighted;
            }
        }
        (!totals.is_empty()).then(|| {
            v_flex().id("tier-usage-totals").gap_1().text_sm()
                .child(status_row(tr!("tiers-usage-totals"), cx.theme().muted_foreground).font_semibold())
                .children(totals.iter().map(|(tier, stats)| {
                    let muted = cx.theme().muted_foreground;
                    status_row(&tier_stats_line(*tier, stats), muted).text_color(muted)
                }))
        })
    }

    pub(super) fn open_agent_management(
        &mut self,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agent_management.tab = tab;
        if tab == Tab::Agents {
            self.sync_agent_hints(window, cx);
            let _ = self.out.send(ClientMsg::ListAgents);
        }
        self.refresh_agent_usage();
        cx.notify();
    }

    pub(super) fn refresh_agent_usage(&self) {
        if !self.settings_open
            || self.sidebar.server_settings
            || self.protocol < fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL
        {
            return;
        }
        match self.agent_management.tab {
            Tab::General => {}
            Tab::Agents => {
                let _ = self.out.send(ClientMsg::GetAgentStats);
            }
            Tab::History => {
                let _ = self.out.send(ClientMsg::GetAgentHistory {
                    agent: self.agent_management.agent.clone(),
                    session: self.agent_management.session.clone(),
                });
            }
        }
    }

    pub(super) fn sync_agent_hints(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_management.tab != Tab::Agents {
            return;
        }
        let hints = &mut self.agent_management.hints;
        hints.retain(|name, _| {
            self.thread_info
                .agents
                .iter()
                .any(|agent| agent.name == *name)
        });
        for agent in &self.thread_info.agents {
            let name = &agent.name;
            let hint = self
                .agent_settings
                .policies
                .get(name)
                .and_then(|policy| policy.when.as_ref())
                .unwrap_or(&agent.policy.when);
            if let Some((previous, input)) = hints.get_mut(name) {
                if previous != hint {
                    input.update(cx, |input, cx| input.set_value(hint.clone(), window, cx));
                    previous.clone_from(hint);
                }
                continue;
            }
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(hint.clone())
                    .placeholder(tr!("agents-when-to-use"))
            });
            let target = name.clone();
            cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_agent_hint(&target, cx);
                }
            })
            .detach();
            hints.insert(name.clone(), (hint.clone(), input));
        }
    }

    fn save_agent_hint(&self, name: &str, cx: &Context<Self>) {
        if let Some((_, input)) = self.agent_management.hints.get(name) {
            let hint = input.read(cx).value().trim().to_owned();
            let _ = self.out.send(ClientMsg::SetAgentPolicy {
                name: name.into(),
                policy: Some(AgentPolicyOverride {
                    when: Some(hint),
                    ..Default::default()
                }),
            });
        }
    }

    pub(super) fn render_agent_policy(
        &self,
        agent: &AgentInfo,
        cx: &Context<Self>,
    ) -> gpui_kit::Div {
        let theme = cx.theme();
        let name = &agent.name;
        let overrides = self.agent_settings.policies.get(name);
        let policy = agent.policy.with_override(overrides);
        let settings = self.out.clone();
        let target = name.clone();
        let enabled = Switch::new(SharedString::from(format!("agent-enabled-{name}")))
            .checked(policy.enabled)
            .label(tr!("agents-enabled"))
            .on_change(move |enabled, _, _| {
                let policy = AgentPolicyOverride {
                    enabled: Some(*enabled),
                    ..Default::default()
                };
                let _ = settings.send(ClientMsg::SetAgentPolicy {
                    name: target.clone(),
                    policy: Some(policy),
                });
            });
        let callers = h_flex()
            .gap_3()
            .flex_wrap()
            .children(AgentCaller::ALL.map(|caller| {
                let out = self.out.clone();
                let name = name.clone();
                Checkbox::new(SharedString::from(format!(
                    "agent-caller-{name}-{}",
                    caller.label()
                )))
                .label(caller_label(caller))
                .checked(policy.callers.contains(&caller))
                .on_change(move |checked, _, _| {
                    let _ = out.send(ClientMsg::SetAgentCaller {
                        name: name.clone(),
                        caller,
                        allowed: *checked,
                    });
                })
            }));
        let reset_target = name.clone();
        let stats = self
            .agent_management
            .stats
            .as_ref()
            .and_then(|stats| stats.get(name));
        v_flex()
            .w_full()
            .gap_2()
            .child(h_flex().gap_3().items_center().child(enabled).children(
                overrides.is_some().then(|| {
                    Button::new(SharedString::from(format!("agent-reset-{name}")))
                        .xsmall()
                        .ghost()
                        .label(tr!("agents-reset-to-agent-defaults"))
                        .on_click(cx.listener(move |this, _, _, _| {
                            let _ = this.out.send(ClientMsg::SetAgentPolicy {
                                name: reset_target.clone(),
                                policy: None,
                            });
                        }))
                }),
            ))
            .child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .child(tr!("agents-allowed-callers"))
                    .child(callers),
            )
            .children(self.agent_management.hints.get(name).map(|(_, input)| {
                let name = name.clone();
                v_flex().gap_1().text_sm().child(tr!("agents-when-to-use")).child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(Input::new(input)))
                        .child(
                            Button::new(SharedString::from(format!("agent-save-when-{name}")))
                                .small()
                                .label(tr!("agents-save"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.save_agent_hint(&name, cx)
                                })),
                        ),
                )
            }))
            .child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(status_row(
                        &stats.map_or_else(|| tr!("agents-loading-usage").into(), stats_label),
                        theme.muted_foreground,
                    ))
                    .children(
                        stats
                            .filter(|stats| stats.days_30.uses > 0)
                            .into_iter()
                            .flat_map(stats_detail)
                            .map(|line| status_row(&line, theme.muted_foreground)),
                    ),
            )
    }

    fn history_filter(
        &self,
        by_agent: bool,
        options: Vec<(String, String)>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let state = &self.agent_management;
        let (id, current, all) = if by_agent {
            ("agent-history-agent", &state.agent, tr!("agents-all-agents"))
        } else {
            ("agent-history-thread", &state.session, tr!("agents-all-threads"))
        };
        let current = current.clone();
        let label = current.as_ref().map_or_else(
            || all.into(),
            |key| {
                options
                    .iter()
                    .find(|(id, _)| id == key)
                    .map_or_else(|| key.clone(), |(_, title)| title.clone())
            },
        );
        let view = cx.weak_entity();
        Button::new(id)
            .small()
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, label) in std::iter::once((None, all.to_owned())).chain(
                    options
                        .iter()
                        .map(|(id, label)| (Some(id.clone()), label.clone())),
                ) {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(id == current).on_click(
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                let state = &mut this.agent_management;
                                let filter = if by_agent {
                                    &mut state.agent
                                } else {
                                    &mut state.session
                                };
                                filter.clone_from(&id);
                                state.history = None;
                                this.refresh_agent_usage();
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }

    pub(super) fn render_agent_history(&self, cx: &Context<Self>) -> gpui_kit::Div {
        let theme = cx.theme();
        let mut names: BTreeSet<_> = self
            .thread_info
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        names.extend(["worker".into(), "summary".into()]);
        if let Some(history) = &self.agent_management.history {
            names.extend(history.iter().map(|entry| entry.agent.clone()));
        }
        let agents = names.into_iter().map(|name| (name.clone(), name)).collect();
        let threads = self
            .threads
            .iter()
            .map(|thread| (thread.id.clone(), crate::thread_title(thread).to_owned()))
            .collect();
        let mut body = v_flex().gap_3().child(
            h_flex()
                .flex_wrap()
                .gap_2()
                .child(self.history_filter(true, agents, cx))
                .child(self.history_filter(false, threads, cx)),
        );
        let Some(history) = &self.agent_management.history else {
            return body.child(tr!("agents-loading-history"));
        };
        if history.is_empty() {
            return body.child(tr!("agents-no-matching-calls"));
        }
        for (index, entry) in history.iter().enumerate() {
            let thread = self
                .threads
                .iter()
                .find(|thread| thread.id == entry.session);
            let title = thread.map_or(entry.session.as_str(), crate::thread_title);
            let caller = entry.caller.map_or(tr!("agents-automatic"), caller_label);
            let outcome = match entry.outcome {
                AgentOutcome::Ok => tr!("agents-succeeded"),
                AgentOutcome::Failed => tr!("agents-failed"),
                AgentOutcome::Cancelled => tr!("agents-interrupted"),
                AgentOutcome::Stalled => tr!("tiers-stalled"),
            };
            let runtime = entry.selection.as_ref().map_or_else(
                || tr!("agents-before-execution").into(),
                |selection| {
                    let model = selection.model.as_deref().unwrap_or(tr!("agents-cli-default"));
                    let effort = selection
                        .effort
                        .as_deref()
                        .map_or(String::new(), |effort| format!("{STATUS_SEPARATOR}{}", crate::chat::effort_label(Some(effort))));
                    format!("{}{STATUS_SEPARATOR}{model}{effort}", selection.backend.label())
                },
            );
            let tokens = entry
                .tokens
                .map_or(String::new(), |tokens| tr!("agents-tokens", tokens = tokens.to_string()));
            let session = entry.session.clone();
            let job = entry.job.clone();
            let row = v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(
                    status_row(&format!("{}{STATUS_SEPARATOR}{outcome}", entry.agent), theme.muted_foreground)
                        .font_semibold(),
                )
                .child(div().text_sm().child(entry.title.clone()))
                .children(entry.attempts.iter().map(|attempt| {
                    status_row(&crate::chat::attempt_details(attempt), theme.muted_foreground).text_xs()
                }))
                .child(
                    status_row(&tr!("agents-history-entry", time = date(entry.started_at_ms).to_string(), caller = caller.to_string(), duration = format!("{:.1}", entry.duration_ms as f64 / 1000.), tokens = tokens.to_string()), theme.muted_foreground)
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    status_row(&runtime, theme.muted_foreground)
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(if thread.is_some() {
                            theme.link
                        } else {
                            theme.muted_foreground
                        })
                        .child(title.to_owned()),
                );
            let row = if thread.is_some() {
                Button::new(SharedString::from(format!("agent-history-entry-{index}")))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .p_0()
                    .accessibility_label(tr!("agents-open-history-artifact", title = title.to_string(), artifact = entry.agent.to_string()))
                    .child(row.w_full())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_thread(session.clone(), cx);
                        this.chats[&session].reveal_delegate(&job);
                    }))
                    .into_any_element()
            } else {
                row.into_any_element()
            };
            body = body.child(row);
        }
        body
    }
}
