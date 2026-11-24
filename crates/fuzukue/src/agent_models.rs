use std::path::PathBuf;

use fuzukue_proto::{
    AgentModelRole, AgentSelection, ChatSettings, ClientMsg, CodexModel, Tier,
    AGENT_MODELS_PROTOCOL,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants as _},
        h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _,
    },
    div,
    prelude::*,
    AnyElement, Context,
};

use crate::{
    chat::{effort_label, model_label, AgentTarget},
    settings_file, Workbench,
};

const SONNET: &str = "claude-sonnet-5-5";
const SOL: &str = "gpt-6.2-sol";

#[derive(Default)]
pub(super) struct ModelSettings {
    path: Option<PathBuf>,
    backends: Option<(bool, bool)>,
    pending: bool,
    offered: bool,
    reopened: bool,
    awaiting: Option<ChatSettings>,
    pub(super) draft: Option<AgentSelection>,
}

impl ModelSettings {
    pub(super) fn load(path: Option<PathBuf>) -> Self {
        let pending = path.as_deref().is_some_and(|path| {
            settings_file::initialize_model_setup(path).unwrap_or_else(|error| {
                eprintln!("{error:#}");
                false
            })
        });
        Self {
            path,
            pending,
            ..Self::default()
        }
    }

    pub(super) fn connected(&mut self, claude: bool, codex: bool, already_configured: bool) {
        self.backends = Some((claude, codex));
        if (self.pending || self.offered) && already_configured && self.awaiting.is_none() {
            self.close();
        }
    }

    pub(super) fn disconnect(&mut self) {
        self.backends = None;
        self.awaiting = None;
        self.draft = None;
    }

    pub(super) fn partner_saved(&mut self, settings: &ChatSettings) {
        if self.awaiting.as_ref() == Some(settings) {
            self.close();
        }
    }

    pub(super) fn save_failed(&mut self) {
        self.awaiting = None;
    }

    fn consume_pending(&mut self) -> bool {
        if !std::mem::take(&mut self.pending) {
            return false;
        }
        self.path.as_deref().is_some_and(|path| {
            settings_file::consume_model_setup(path).unwrap_or_else(|error| {
                eprintln!("{error:#}");
                false
            })
        })
    }

    fn close(&mut self) {
        self.consume_pending();
        self.offered = false;
        self.reopened = false;
        self.awaiting = None;
        self.draft = None;
    }
}

fn recommendation(claude: Option<&[String]>, codex: &[CodexModel]) -> Option<AgentSelection> {
    let (model, levels) = if let Some(levels) = claude {
        (SONNET, levels)
    } else {
        let model = codex
            .iter()
            .find(|model| model.slug == SOL)
            .or_else(|| codex.first())?;
        (model.slug.as_str(), model.effort_levels.as_slice())
    };
    let effort = levels.iter().find(|level| level.as_str() == "low").cloned();
    Some(AgentSelection::from_model(Some(model.into()), effort))
}

impl Workbench {
    pub(super) fn model_backends(&self) -> (bool, bool) {
        if self.protocol < AGENT_MODELS_PROTOCOL {
            return (true, !self.codex_models.is_empty());
        }
        self.agent_models.backends.unwrap_or_default()
    }

    fn model_setup_recommendation(&self) -> Option<AgentSelection> {
        let (claude, codex) = self.agent_models.backends?;
        let capabilities = self.chat_capabilities.as_ref()?;
        self.partner_default.as_ref()?;
        recommendation(
            claude.then_some(&capabilities.effort_levels[..]),
            if codex { &self.codex_models } else { &[] },
        )
    }

    pub(super) fn offer_model_setup(&mut self) {
        if self.agent_models.pending && self.model_setup_recommendation().is_some() {
            // Claim the offer once even if the user closes the app without choosing.
            self.agent_models.offered = self.agent_models.consume_pending();
        }
    }

    pub(super) fn render_model_setup(
        &self,
        in_settings: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let setup = &self.agent_models;
        if if in_settings {
            !setup.reopened
        } else {
            !setup.offered || setup.reopened
        } {
            return None;
        }
        let recommended = self.model_setup_recommendation()?;
        let model = recommended.model.as_deref().unwrap_or_default();
        let label = self
            .codex_models
            .iter()
            .find(|entry| entry.slug == model)
            .map_or_else(|| model_label(model), |entry| &entry.label);
        let recommendation = tr!(
            "model-setup-recommendation",
            model = label.to_owned(),
            effort = effort_label(recommended.effort.as_deref()).to_owned()
        );
        let theme = cx.theme();
        Some(
            v_flex()
                .id("model-setup")
                .gap_2()
                .p_3()
                .flex_none()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(div().font_semibold().child(tr!("model-setup-heading")))
                .child(div().text_sm().child(recommendation))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("model-usage-cost")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("model-setup-later-settings")),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .child(self.agent_override_menu(
                            "model-setup-picker".into(),
                            AgentTarget::Setup,
                            recommended,
                            setup.draft.as_ref(),
                            cx,
                        ))
                        .child(
                            Button::new("model-setup-apply")
                                .small()
                                .primary()
                                .label(tr!("model-setup-apply"))
                                .disabled(setup.awaiting.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let choice = this
                                        .agent_models
                                        .draft
                                        .clone()
                                        .or_else(|| this.model_setup_recommendation());
                                    if let Some(choice) = choice {
                                        let settings = choice.chat_settings();
                                        if this
                                            .out
                                            .send(ClientMsg::SetPartnerDefault {
                                                settings: settings.clone(),
                                            })
                                            .is_ok()
                                        {
                                            this.agent_models.awaiting = Some(settings);
                                            cx.notify();
                                        }
                                    }
                                })),
                        )
                        .child(
                            Button::new("model-setup-later")
                                .small()
                                .ghost()
                                .label(tr!("model-setup-later"))
                                .disabled(setup.awaiting.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.agent_models.close();
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_model_roles(&self, cx: &Context<Self>) -> impl IntoElement {
        let role =
            |id: &'static str, label, picker: Option<AnyElement>, detail: Option<&'static str>| {
                v_flex()
                    .id(id)
                    .items_start()
                    .gap_1()
                    .child(div().font_semibold().child(label))
                    .children(picker)
                    .children(detail.map(|text| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(text)
                    }))
            };
        let usage = |name| {
            self.agent_management
                .stats
                .as_ref()
                .and_then(|stats| stats.get(name))
                .map(|stats| weekly_usage(stats.days_7.uses, stats.days_7.total_tokens, cx))
        };
        let picker = |id: &str, target, default, selected| {
            Some(self.agent_override_menu(id.to_owned().into(), target, default, selected, cx))
        };
        let partner = self.partner_default.as_ref().map(|settings| {
            AgentSelection::from_model(settings.model.clone(), settings.effort.clone())
        });
        let worker = self.delegate_settings.as_ref().map(|settings| {
            AgentSelection::from_model(
                Some(settings.model.clone()),
                (!settings.effort.is_empty()).then(|| settings.effort.clone()),
            )
        });
        let planner = self
            .thread_info
            .agents
            .iter()
            .find(|agent| agent.name == "planner");
        let planner_model = self
            .agent_settings
            .overrides
            .get("planner")
            .cloned()
            .or_else(|| {
                self.agent_settings
                    .tiers
                    .models
                    .get(&Tier::Heavy)
                    .and_then(|models| models.first())
                    .map(|entry| entry.selection.clone())
            })
            .or_else(|| planner.map(|agent| agent.default_selection()));
        let summary = self.agent_settings.summary_model();

        v_flex()
            .id("model-usage-settings")
            .gap_3()
            .child(div().text_lg().child(tr!("model-usage-heading")))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("model-usage-cost")),
            )
            .children(self.render_model_setup(true, cx))
            .child(
                role(
                    "model-role-front-desk",
                    tr!("model-role-front-desk"),
                    partner.as_ref().and_then(|selection| {
                        picker(
                            "model-front-desk-picker",
                            AgentTarget::PartnerDefault,
                            AgentSelection::from_model(None, None),
                            Some(selection),
                        )
                    }),
                    Some(tr!("model-role-front-desk-timing")),
                )
                .children(usage("front_desk"))
                .children(
                    (!self.agent_models.reopened && self.model_setup_recommendation().is_some())
                        .then(|| {
                            Button::new("model-setup-reopen")
                                .small()
                                .ghost()
                                .label(tr!("model-setup-reopen"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.agent_models.reopened = true;
                                    cx.notify();
                                }))
                        }),
                ),
            )
            .children((self.protocol >= fuzukue_proto::LIVE_MODELS_PROTOCOL).then(|| {
                let default = self.agent_settings.consult.clone()
                    .or_else(|| self.agent_settings.tier_models.get(&Tier::Heavy).cloned())
                    .or_else(|| planner_model.clone());
                role(
                    "model-role-escalation",
                    tr!("model-role-escalation"),
                    default.and_then(|default| picker(
                        "model-escalation-picker",
                        AgentTarget::Role(AgentModelRole::Escalation),
                        default,
                        self.agent_settings.escalation.as_ref(),
                    )),
                    Some(tr!("model-role-escalation-detail")),
                )
            }))
            .child(
                role(
                    "model-role-worker-digest",
                    tr!("model-role-worker-digest"),
                    partner.as_ref().and_then(|selection| {
                        picker(
                            "model-worker-digest-picker",
                            AgentTarget::PartnerDefault,
                            AgentSelection::from_model(None, None),
                            Some(selection),
                        )
                    }),
                    Some(tr!("model-role-worker-digest-shared")),
                )
                .children(usage("worker_digest")),
            )
            .child(
                role(
                    "model-role-consult",
                    tr!("model-role-consult"),
                    planner_model.and_then(|default| {
                        picker(
                            "model-consult-picker",
                            AgentTarget::Role(AgentModelRole::Consult),
                            default,
                            self.agent_settings.consult.as_ref(),
                        )
                    }),
                    Some(tr!("model-role-consult-inherited")),
                )
                .children(usage("consult")),
            )
            .child(
                role(
                    "model-role-worker",
                    tr!("model-role-worker"),
                    worker.as_ref().and_then(|selection| {
                        picker(
                            "model-worker-picker",
                            AgentTarget::WorkerDefault,
                            selection.clone(),
                            Some(selection),
                        )
                    }),
                    Some(tr!("model-role-worker-timing")),
                )
                .children(usage("worker")),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("model-role-tier-detail")),
                    )
                    .children(Tier::ALL.map(|tier| {
                        let id = format!("model-tier-{}", tier.key());
                        let default = self
                            .agent_settings
                            .tiers
                            .models
                            .get(&tier)
                            .and_then(|models| models.first())
                            .map(|model| model.selection.clone())
                            .or_else(|| worker.clone());
                        let selected = self.agent_settings.tier_models.get(&tier);
                        let totals = self.agent_management.stats.as_ref().map(|stats| {
                            stats
                                .iter()
                                .filter(|(name, _)| !matches!(name.as_str(), "planner" | "consult"))
                                .filter_map(|(_, stats)| stats.days_7.tiers.get(&tier))
                                .fold((0usize, 0u64), |(uses, tokens), stats| {
                                    (
                                        uses.saturating_add(stats.attempts),
                                        tokens.saturating_add(stats.total_tokens),
                                    )
                                })
                        });
                        v_flex()
                            .id(id.clone())
                            .items_start()
                            .gap_1()
                            .child(div().font_semibold().child(crate::chat::tier_label(tier)))
                            .children(default.and_then(|default| {
                                picker(
                                    &format!("{id}-picker"),
                                    AgentTarget::Role(AgentModelRole::WorkerTier(tier)),
                                    default,
                                    selected,
                                )
                            }))
                            .children(totals.map(|(uses, tokens)| weekly_usage(uses, tokens, cx)))
                    })),
            )
            .child(
                role(
                    "model-role-summary",
                    tr!("model-role-summary"),
                    picker(
                        "summary-model-picker",
                        AgentTarget::Summary,
                        AgentSelection::summary_default(),
                        self.agent_settings.summary.as_ref(),
                    ),
                    None,
                )
                .children(usage("summary")),
            )
            .child(
                role(
                    "model-role-thread-title",
                    tr!("model-role-thread-title"),
                    picker(
                        "model-thread-title-picker",
                        AgentTarget::Role(AgentModelRole::ThreadTitle),
                        summary,
                        self.agent_settings.thread_title.as_ref(),
                    ),
                    None,
                )
                .children(usage("thread_title")),
            )
    }
}

fn weekly_usage(uses: usize, tokens: u64, cx: &Context<Workbench>) -> gpui_kit::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(tr!(
            "model-usage-weekly",
            uses = uses.to_string(),
            tokens = tokens.to_string()
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_uses_only_the_available_backend_and_effort() {
        let codex = [CodexModel {
            slug: SOL.into(),
            label: "Sol".into(),
            effort_levels: vec!["medium".into()],
            default_effort: Some("medium".into()),
        }];
        assert!(recommendation(None, &[]).is_none());
        assert_eq!(
            recommendation(None, &codex),
            Some(AgentSelection::from_model(Some(SOL.into()), None))
        );
        assert_eq!(
            recommendation(Some(&["low".into()]), &codex),
            Some(AgentSelection::from_model(
                Some(SONNET.into()),
                Some("low".into())
            ))
        );
        assert_eq!(
            recommendation(Some(&[]), &[]),
            Some(AgentSelection::from_model(Some(SONNET.into()), None))
        );
    }
}
