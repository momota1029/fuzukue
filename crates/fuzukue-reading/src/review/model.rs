use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct Ledger {
    #[serde(rename = "_meta", default)]
    meta: Metadata,
    pub(super) steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default = "default_target")]
    target: String,
}

fn default_target() -> String {
    "T".into()
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            target: default_target(),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct Step {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) statement: String,
    pub(super) deps: Vec<String>,
    pub(super) loc: String,
    pub(super) origin: Origin,
    pub(super) tier: Tier,
    pub(super) role: String,
    pub(super) why_tier: Option<String>,
    pub(super) outside: Option<bool>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Tier {
    Key,
    Semi,
    Standard,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Origin {
    Inherited,
    Standard,
    Adapted,
    New,
}

pub(super) struct Graph<'a> {
    pub(super) steps: &'a [Step],
    pub(super) by_id: HashMap<&'a str, &'a Step>,
    pub(super) target: &'a str,
}

impl Ledger {
    pub(super) fn graph(&self) -> Result<Graph<'_>> {
        let mut errors = Vec::new();
        let mut by_id = HashMap::new();
        for step in &self.steps {
            if by_id.insert(step.id.as_str(), step).is_some() {
                errors.push(format!("duplicate id {}", step.id));
            }
            if step.tier == Tier::Key && step.why_tier.as_deref().unwrap_or("").is_empty() {
                errors.push(format!(
                    "{}: a key step needs why_tier (why it is both necessary and not in the literature)",
                    step.id
                ));
            }
        }

        let target = self.meta.target.as_str();
        if !by_id.contains_key(target) {
            errors.push(format!("target step '{target}' not found"));
        }

        let mut dependents = HashMap::<&str, Vec<&str>>::new();
        let mut indegrees = HashMap::new();
        for step in &self.steps {
            indegrees.insert(step.id.as_str(), step.deps.len());
            for dep in &step.deps {
                if !by_id.contains_key(dep.as_str()) {
                    errors.push(format!("{}: unknown dependency {dep}", step.id));
                }
                dependents.entry(dep).or_default().push(&step.id);
            }
        }
        if !errors.is_empty() {
            bail!("ledger errors:\n{}", errors.join("\n"));
        }

        // Remove zero-indegree steps to detect cycles, including cycles through the target.
        let mut ready: VecDeque<_> = indegrees
            .iter()
            .filter_map(|(&id, &degree)| (degree == 0).then_some(id))
            .collect();
        while let Some(id) = ready.pop_front() {
            for &dependent in dependents.get(id).into_iter().flatten() {
                let degree = indegrees.get_mut(dependent).expect("known dependent");
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(dependent);
                }
            }
        }
        let mut cyclic: Vec<_> = indegrees
            .iter()
            .filter_map(|(&id, &degree)| (degree != 0).then_some(id))
            .collect();
        if !cyclic.is_empty() {
            cyclic.sort_unstable();
            bail!(
                "ledger errors:\ndependency cycle among: {}",
                cyclic.join(", ")
            );
        }

        let mut needed = HashSet::new();
        let mut visit = vec![target];
        while let Some(id) = visit.pop() {
            if needed.insert(id) {
                visit.extend(by_id[id].deps.iter().map(String::as_str));
            }
        }
        let mut unreached: Vec<_> = self
            .steps
            .iter()
            .filter(|step| !step.outside.unwrap_or(false) && !needed.contains(step.id.as_str()))
            .map(|step| step.id.as_str())
            .collect();
        if !unreached.is_empty() {
            unreached.sort_unstable();
            bail!(
                "ledger errors:\nsteps the target does not depend on (mark them \"outside\": true if intentional): {}",
                unreached.join(", ")
            );
        }

        Ok(Graph {
            steps: &self.steps,
            by_id,
            target,
        })
    }
}

#[derive(Default, Deserialize)]
pub(super) struct Body {
    pub(super) l1: Option<String>,
    pub(super) l2: Option<String>,
    pub(super) l3: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Review {
    pub(super) title: String,
    pub(super) subtitle: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(super) summary_html: Option<String>,
    pub(super) intro_html: Option<String>,
    pub(super) key_ideas_html: Option<String>,
    pub(super) groups: Option<Vec<(String, Vec<String>)>>,
    #[serde(default)]
    pub(super) step_review: HashMap<String, StepReview>,
    pub(super) compare_note_html: Option<String>,
    pub(super) review_conclusion: String,
    pub(super) panel: Vec<Panel>,
    pub(super) my_checks: Option<Vec<String>>,
    pub(super) findings: Vec<Finding>,
    pub(super) prior: Vec<Prior>,
    pub(super) prior_hole: Option<String>,
    pub(super) uncited: Option<Vec<String>>,
    pub(super) admitted: Option<Vec<String>>,
    pub(super) indicator: Indicator,
    pub(super) glossary: Option<Vec<(Option<String>, Option<String>)>>,
}

#[derive(Deserialize)]
pub(super) struct StepReview {
    #[serde(default = "empty_cell")]
    pub(super) risk: String,
    #[serde(default = "empty_cell")]
    pub(super) evidence: String,
}

fn empty_cell() -> String {
    "—".into()
}

#[derive(Deserialize)]
pub(super) struct Panel {
    pub(super) name: String,
    pub(super) who: String,
    pub(super) scope: String,
    pub(super) result: String,
}

#[derive(Deserialize)]
pub(super) struct Finding {
    pub(super) id: String,
    pub(super) sev: Severity,
    pub(super) status: Option<Status>,
    pub(super) status_note: Option<String>,
    #[serde(default)]
    pub(super) steps: Vec<String>,
    pub(super) from: Vec<String>,
    pub(super) short: String,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(super) detail: Option<String>,
    pub(super) evidence: Option<String>,
    pub(super) ruling: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Severity {
    Fatal,
    Major,
    Minor,
    Form,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Confirmed,
    Suspected,
    Unchecked,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct Prior {
    pub(super) name: String,
    pub(super) op: String,
    pub(super) hyp: String,
    pub(super) overlap: String,
    pub(super) state: String,
}

const AXES: [&str; 6] = [
    "correctness",
    "novelty",
    "delta",
    "strength",
    "significance",
    "literature",
];
const LEVELS: [&str; 4] = ["unknown", "weak", "mid", "strong"];
const VERDICTS: [&str; 4] = ["none", "needs_work", "short_or_preprint", "journal"];
const MID: usize = 2;
const STRONG: usize = 3;

#[derive(Deserialize)]
pub(super) struct Indicator {
    axes: Vec<Axis>,
    #[serde(default)]
    facts: Facts,
    verdict: Option<String>,
    pub(super) verdict_why: Option<String>,
    pub(super) changers: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Axis {
    key: String,
    level: String,
    why: Option<String>,
}

#[derive(Default, Deserialize)]
struct Facts {
    #[serde(default)]
    review_done: bool,
    #[serde(default)]
    fatal_confirmed: f64,
    #[serde(default)]
    expert_or_formal_check: bool,
    #[serde(default)]
    closest_prior_unread: bool,
    sources_limited: Option<bool>,
    #[serde(default)]
    delta_stated: bool,
    #[serde(default)]
    attack_survived: bool,
}

pub(super) struct Assessment<'a> {
    pub(super) axes: Vec<(usize, usize, &'a str)>,
    pub(super) verdict: usize,
}

fn rank(order: &[&str], name: &str) -> Option<usize> {
    order.iter().position(|&item| item == name)
}

impl Indicator {
    pub(super) fn assess(&self) -> Result<Assessment<'_>> {
        let assessment = self.validate()?;
        anyhow::ensure!(self.verdict_why.is_some(), "indicator needs verdict_why");
        for axis in &self.axes {
            anyhow::ensure!(axis.why.is_some(), "indicator axis {} needs why", axis.key);
        }
        Ok(assessment)
    }

    pub(super) fn validate(&self) -> Result<Assessment<'_>> {
        // The last occurrence of each axis determines the allowed verdict.
        let levels: HashMap<_, _> = self
            .axes
            .iter()
            .map(|axis| (axis.key.as_str(), axis.level.as_str()))
            .collect();
        let mut errors = Vec::new();
        let mut ranks = [0; AXES.len()];
        for (index, key) in AXES.into_iter().enumerate() {
            match levels.get(key) {
                None => errors.push(format!("axis {key} missing")),
                Some(level) => match rank(&LEVELS, level) {
                    Some(level) => ranks[index] = level,
                    None => errors.push(format!(
                        "axis {key}: level must be one of ('unknown', 'weak', 'mid', 'strong')"
                    )),
                },
            }
        }
        if !errors.is_empty() {
            bail!("indicator violates the rules:\n{}", errors.join("\n"));
        }

        let [correctness, novelty, _, strength, _, literature] = ranks;
        let facts = &self.facts;
        if !facts.review_done && correctness != 0 {
            errors.push("correctness must be 'unknown' when the review is not done".into());
        }
        if !facts.expert_or_formal_check && correctness > MID {
            errors.push("correctness above 'mid' needs an expert check or formal proof".into());
        }
        if facts.closest_prior_unread && novelty > MID {
            errors.push("novelty above 'mid' while the closest prior work is unread".into());
        }
        if facts.sources_limited.unwrap_or(true) && literature > MID {
            errors.push("literature above 'mid' with a limited source list".into());
        }

        let allowed = if !facts.review_done {
            0
        } else if facts.fatal_confirmed > 0.0 || correctness < MID || !facts.delta_stated {
            1
        } else if novelty == STRONG && strength >= MID && facts.attack_survived {
            3
        } else {
            2
        };

        let verdict = self
            .verdict
            .as_deref()
            .and_then(|name| rank(&VERDICTS, name));
        match verdict {
            None => errors.push(
                "verdict must be one of ('none', 'needs_work', 'short_or_preprint', 'journal')"
                    .into(),
            ),
            Some(verdict) if verdict > allowed => errors.push(format!(
                "verdict '{}' exceeds what the rules allow ('{}')",
                VERDICTS[verdict], VERDICTS[allowed]
            )),
            _ => {}
        }

        if self.changers.as_deref().unwrap_or_default().is_empty() {
            errors.push("list what would change the verdict (changers)".into());
        }

        let mut axes = Vec::new();
        for axis in &self.axes {
            match (rank(&AXES, &axis.key), rank(&LEVELS, &axis.level)) {
                (Some(key), Some(level)) => {
                    axes.push((key, level, axis.why.as_deref().unwrap_or_default()));
                }
                _ => errors.push(format!(
                    "invalid axis {} with level {}",
                    axis.key, axis.level
                )),
            }
        }
        if !errors.is_empty() {
            bail!("indicator violates the rules:\n{}", errors.join("\n"));
        }
        Ok(Assessment {
            axes,
            verdict: verdict.expect("validated verdict"),
        })
    }
}
