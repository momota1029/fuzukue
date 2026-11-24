//! Backend-independent worker definitions from built-ins and project rule layers.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{AgentBackend, AgentCaller, AgentInfo, AgentPolicy, AgentPolicyOverride, AgentSelection, AgentSettings, ServerEvent};
use serde::Deserialize;

use crate::rules::{self, Rules};

pub(crate) struct Agent {
    pub info: AgentInfo,
    pub skill: Option<rules::Skills>,
}

impl Agent {
    fn markdown(text: &str, path: Option<&Path>) -> Result<Self> {
        let (front, body) =
            fuzukue_cards::split_frontmatter(text).context("missing agent front matter")?;
        let mut info: AgentInfo = serde_yaml::from_str(front)?;
        ensure!(
            !info.name.is_empty()
                && info
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid agent name {:?}",
            info.name
        );
        ensure!(
            !info.description.trim().is_empty() && info.description.lines().count() == 1,
            "agent description must be one line"
        );
        ensure!(!info.model.trim().is_empty(), "agent model is empty");
        validate_hint(&info.policy.when)?;
        info.prompt = body.trim().into();
        info.path = path.map(|p| p.display().to_string());
        Ok(Self { info, skill: None })
    }
}

#[derive(Deserialize)]
struct SkillFront {
    tier: Option<fuzukue_proto::Tier>,
    name: Option<String>,
    description: String,
    #[serde(flatten)]
    policy: AgentPolicy,
}

fn defaults() -> Result<BTreeMap<String, Agent>> {
    let mut agents = BTreeMap::new();
    for text in [
        harness_text!(include_str!("../../../agents/prose.md"), include_str!("../prompts/ja/agents/prose.md")),
        harness_text!(include_str!("../../../agents/proof.md"), include_str!("../prompts/ja/agents/proof.md")),
        harness_text!(include_str!("../../../agents/lean.md"), include_str!("../prompts/ja/agents/lean.md")),
        harness_text!(include_str!("../../../agents/literature.md"), include_str!("../prompts/ja/agents/literature.md")),
        harness_text!(include_str!("../../../agents/book.md"), include_str!("../prompts/ja/agents/book.md")),
        harness_text!(include_str!("../../../agents/git.md"), include_str!("../prompts/ja/agents/git.md")),
        harness_text!(include_str!("../../../agents/planner.md"), include_str!("../prompts/ja/agents/planner.md")),
    ] {
        let agent = Agent::markdown(text, None)?;
        agents.insert(agent.info.name.clone(), agent);
    }
    Ok(agents)
}

pub(crate) fn load(project: &Path, rules: &Rules) -> Result<BTreeMap<String, Agent>> {
    let mut agents = defaults()?;
    for kind in crate::reviewers::Kind::ALL {
        let info = kind.agent_info(project, rules)?;
        agents.insert(info.name.clone(), Agent { info, skill: None });
    }
    for root in rules.layers(project) {
        for (folder, dir) in rules::skill_dirs(&root)? {
            let path = dir.join("SKILL.md");
            let text = fs::read_to_string(&path)?;
            let Some(front) = fuzukue_cards::split_frontmatter(&text)
                .and_then(|(front, _)| serde_yaml::from_str::<SkillFront>(front).ok())
            else {
                tracing::warn!("missing or invalid skill metadata in {}", path.display());
                continue;
            };
            validate_hint(&front.policy.when).with_context(|| format!("reading {}", path.display()))?;
            let name = front
                .name
                .unwrap_or_else(|| folder.to_string_lossy().into_owned());
            let description = front
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let default = AgentSelection::skill_default();
            let info = AgentInfo {
                name: name.clone(),
                description,
                backend: default.backend,
                model: default.model.unwrap_or_default(),
                effort: default.effort,
                tools: Vec::new(),
                prompt: text,
                path: Some(path.display().to_string()),
                tier: front.tier,
                policy: front.policy,
            };
            agents.insert(
                name,
                Agent {
                    info,
                    skill: Some(rules::Skills::single(folder, dir)),
                },
            );
        }
        for path in fuzukue_cards::rule_files(&root, "agents", "md")? {
            let agent = Agent::markdown(&fs::read_to_string(&path)?, Some(&path))
                .with_context(|| format!("reading {}", path.display()))?;
            agents.insert(agent.info.name.clone(), agent);
        }
    }
    Ok(agents)
}

pub(crate) async fn catalog(project: PathBuf) -> Result<BTreeMap<String, Agent>> {
    tokio::task::spawn_blocking(move || load(&project, &rules::resolve(&project)?)).await?
}

pub(crate) fn callable(info: &AgentInfo, settings: &AgentSettings, caller: AgentCaller) -> bool {
    info.name != crate::planner::ROLE
        && info.policy.with_override(settings.policies.get(&info.name)).allows(caller)
}

#[derive(serde::Serialize)]
pub(crate) struct Listing<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<fuzukue_proto::Tier>,
    #[serde(skip_serializing_if = "String::is_empty")]
    when: String,
}

pub(crate) fn listings<'a>(agents: &'a BTreeMap<String, Agent>, settings: &'a AgentSettings, caller: AgentCaller) -> impl Iterator<Item = Listing<'a>> + 'a {
    agents.values().filter_map(move |agent| {
        let info = &agent.info;
        callable(info, settings, caller).then(|| Listing {
            name: &info.name,
            description: &info.description,
            tier: info.tier,
            when: info.policy.with_override(settings.policies.get(&info.name)).when,
        })
    })
}

pub(crate) fn directory(agents: &BTreeMap<String, Agent>, settings: &AgentSettings, caller: AgentCaller) -> String {
    let mut prompt = String::from(harness_text!("\n\nWorkers (names, roles, and when to use them)", "\n\n作業役の一覧（名前と役目、使う場面）"));
    for agent in listings(agents, settings, caller) {
        prompt.push_str(&format!("\n- {}: {}", agent.name, agent.description));
        if let Some(tier) = agent.tier {
            prompt.push_str(&harness_format!(" (Tier hint: {})", "（段階の目安: {}）", tier.key()));
        }
        if !agent.when.is_empty() {
            prompt.push_str(&harness_format!(" (Use when: {})", "（使う場面: {}）", agent.when));
        }
    }
    prompt
}

fn validate_hint(hint: &str) -> Result<()> {
    ensure!(!hint.contains(['\r', '\n']) && hint.chars().count() <= 160, "使う場面は160文字以内の一行で指定する");
    Ok(())
}

pub(crate) async fn validate(app: &crate::Workspace, selection: &AgentSelection) -> Result<()> {
    let model = selection.model.as_deref();
    let effort = selection.effort.as_deref();
    let levels = match selection.backend {
        AgentBackend::Claude => {
            ensure!(
                model.is_none_or(|model| fuzukue_proto::CLAUDE_MODELS.contains(&model)),
                "Claude のモデルを指定する"
            );
            crate::claude::effort_levels().to_vec()
        }
        AgentBackend::Codex => match model {
            Some(model) => {
                app.codex_models()
                    .await
                    .into_iter()
                    .find(|entry| entry.slug == model)
                    .context("Codex のモデルが見つからない")?
                    .effort_levels
            }
            None => Vec::new(),
        },
    };
    ensure!(
        effort.is_none_or(|effort| levels.iter().any(|level| level == effort)),
        "このモデルでは指定した思考レベルを使えない"
    );
    Ok(())
}

impl crate::Workspace {
    pub(crate) async fn agent_catalog(&self) -> Result<BTreeMap<String, Agent>> {
        let mut agents = catalog(self.project_dir.clone()).await?;
        let review_settings = self.review_settings();
        for kind in crate::reviewers::Kind::ALL {
            if let Some(agent) = agents
                .get_mut(&kind.agent_name())
                .filter(|agent| agent.info.path.is_none())
            {
                let selection = kind.selection(&review_settings);
                agent.info.backend = selection.backend;
                agent.info.model = selection.model.unwrap_or_default();
                agent.info.effort = selection.effort;
            }
        }
        Ok(agents)
    }

    pub(crate) async fn agent_catalog_event(&self) -> ServerEvent {
        match self.agent_catalog().await {
            Ok(agents) => ServerEvent::Agents {
                agents: agents.into_values().map(|agent| agent.info).collect(),
            },
            Err(error) => ServerEvent::Error {
                message: format!("作業役の一覧: {error:#}"),
            },
        }
    }

    pub(crate) fn agent_settings(&self) -> AgentSettings {
        self.agent_settings.lock().unwrap().clone()
    }

    pub(crate) fn agent_selection(
        &self,
        session: Option<&str>,
        name: &str,
        default: AgentSelection,
    ) -> AgentSelection {
        session
            .and_then(|session| self.hub.agent_override(session, name))
            .or_else(|| {
                self.agent_settings
                    .lock()
                    .unwrap()
                    .overrides
                    .get(name)
                    .cloned()
            })
            .unwrap_or(default)
    }

    pub(crate) async fn set_agent_override(
        &self,
        session: Option<&str>,
        name: String,
        selection: Option<AgentSelection>,
    ) -> Result<()> {
        if let Some(selection) = &selection {
            ensure!(
                self.agent_catalog().await?.contains_key(&name),
                "作業役 {name} が見つからない"
            );
            validate(self, selection).await?;
        }
        if let Some(session) = session {
            self.hub
                .edit_threads(|store| store.set_agent_override(session, name, selection))
        } else {
            self.edit_agent_settings(move |settings| {
                if let Some(selection) = selection {
                    settings.overrides.insert(name, selection);
                } else {
                    settings.overrides.remove(&name);
                }
            })
        }
    }

    pub(crate) async fn set_agent_policy(&self, name: String, policy: Option<AgentPolicyOverride>) -> Result<()> {
        if let Some(policy) = &policy {
            ensure!(self.agent_catalog().await?.contains_key(&name), "作業役 {name} が見つからない");
            if let Some(hint) = &policy.when {
                validate_hint(hint)?;
            }
        }
        self.edit_agent_settings(|settings| match policy {
            Some(policy) => {
                let current = settings.policies.entry(name).or_default();
                if policy.enabled.is_some() { current.enabled = policy.enabled; }
                if policy.callers.is_some() { current.callers = policy.callers; }
                if policy.when.is_some() { current.when = policy.when; }
            }
            None => { settings.policies.remove(&name); }
        })
    }

    pub(crate) async fn set_agent_caller(&self, name: String, caller: AgentCaller, allowed: bool) -> Result<()> {
        let agent = self.agent_catalog().await?.remove(&name).with_context(|| format!("作業役 {name} が見つからない"))?;
        self.edit_agent_settings(|settings| {
            let current = settings.policies.entry(name).or_default();
            let callers = current.callers.get_or_insert(agent.info.policy.callers);
            callers.retain(|entry| *entry != caller);
            if allowed {
                callers.push(caller);
            }
        })
    }

    pub(crate) async fn effective_agent_policy(&self, name: &str) -> Result<AgentPolicy> {
        let agent = self.agent_catalog().await?.remove(name).with_context(|| format!("作業役 {name} が見つからない"))?;
        let settings = self.agent_settings.lock().unwrap();
        Ok(agent.info.policy.with_override(settings.policies.get(name)))
    }

    pub(crate) async fn set_summary_model(&self, selection: Option<AgentSelection>) -> Result<()> {
        if let Some(selection) = &selection {
            validate(self, selection).await?;
        }
        self.edit_agent_settings(move |settings| settings.summary = selection)
    }

    pub(crate) async fn set_agent_role_model(
        &self, role: fuzukue_proto::AgentModelRole, selection: Option<AgentSelection>,
    ) -> Result<()> {
        use fuzukue_proto::AgentModelRole;

        if let Some(selection) = &selection {
            validate(self, selection).await?;
        }
        self.edit_agent_settings(|settings| match role {
            AgentModelRole::Consult => settings.consult = selection,
            AgentModelRole::ThreadTitle => settings.thread_title = selection,
            AgentModelRole::WorkerTier(tier) => match selection {
                Some(selection) => { settings.tier_models.insert(tier, selection); }
                None => { settings.tier_models.remove(&tier); }
            },
        })
    }

    pub(crate) async fn set_agent_fallback(&self, name: String, selection: Option<AgentSelection>) -> Result<()> {
        if let Some(selection) = &selection {
            ensure!(matches!(name.as_str(), "worker" | "summary" | "front_desk") || self.agent_catalog().await?.contains_key(&name),
                "{name} が見つからない");
            validate(self, selection).await?;
        }
        self.edit_agent_settings(|settings| {
            if let Some(selection) = selection {
                settings.fallbacks.insert(name, selection);
            } else {
                settings.fallbacks.remove(&name);
            }
        })
    }

    fn edit_agent_settings(&self, edit: impl FnOnce(&mut AgentSettings)) -> Result<()> {
        let mut settings = self.agent_settings.lock().unwrap();
        let mut next = settings.clone();
        edit(&mut next);
        crate::state::save_agent_settings(&self.state_dir, &next)?;
        *settings = next;
        self.hub
            .publish_snapshot(ServerEvent::AgentSettingsChanged {
                settings: settings.clone(),
            });
        Ok(())
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;
    use fuzukue_proto::{AgentModelRole, Tier};

    #[tokio::test]
    async fn model_role_overrides_round_trip_and_keep_existing_settings() -> Result<()> {
        let project = crate::test_support::TestDir::new();
        let state = crate::test_support::TestDir::new();
        let original = r#"{"summary":{"backend":"claude","model":"claude-opus-5-5","effort":null}}"#;
        state.write("agent-settings.json", original);
        let app = crate::Workspace::new(project.path().into(), "127.0.0.1:0".into(),
            state.path().into(), std::sync::Arc::new(crate::Hub::new()));
        assert_eq!(fs::read_to_string(state.path().join("agent-settings.json"))?, original);
        let before = app.agent_settings();
        assert_eq!(before.thread_title_model(), before.summary_model());
        assert_eq!(AgentSettings::default().thread_title_model(), AgentSelection::summary_default());

        let selection = AgentSelection::skill_default();
        let tier = Tier::Light;
        for role in [AgentModelRole::ThreadTitle, AgentModelRole::Consult, AgentModelRole::WorkerTier(tier)] {
            app.set_agent_role_model(role, Some(selection.clone())).await?;
        }
        let saved = crate::state::load_agent_settings(state.path());
        assert_eq!(saved.summary, before.summary);
        assert_eq!(saved.thread_title_model(), selection);
        assert_eq!(saved.consult.as_ref(), Some(&selection));
        assert_eq!(saved.tier_models.get(&tier), Some(&selection));

        let primary = AgentSelection::from_model(Some("gpt-6.2-sol".into()), Some("high".into()));
        let route = |pinned| crate::tiers::Route::new(&app, "fixture", "worker", primary.clone(), pinned,
            (fuzukue_proto::TierDecision { tier, reason: String::new() }, tier));
        assert_eq!(route(false).initial(&app, None).1.selection, selection);
        assert_eq!(route(true).initial(&app, None).1.selection, primary);

        app.set_agent_role_model(AgentModelRole::ThreadTitle, None).await?;
        app.set_agent_role_model(AgentModelRole::WorkerTier(tier), None).await?;
        assert_eq!(app.agent_settings().thread_title_model(), before.summary_model());
        assert_eq!(route(false).initial(&app, None).1.selection.effort.as_deref(), Some("low"));
        Ok(())
    }
}
