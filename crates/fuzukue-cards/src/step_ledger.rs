//! Validate proof steps recorded as cards in one deck.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{DeckConfig, Graph};

/// Frontmatter fields specific to a proof-step card.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StepMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_tier: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    #[serde(skip)]
    pub(crate) declares_requires: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Config {
    target: Option<String>,
}

/// Validation errors and, for an acyclic ledger with valid inputs, graph details.
#[derive(Debug, Serialize)]
pub struct Report {
    pub errors: Vec<String>,
    pub info: Option<Info>,
}

/// Dependency order, transitive impact for each key step, and tier counts.
#[derive(Debug, Serialize)]
pub struct Info {
    pub order: Vec<String>,
    pub key_impact: BTreeMap<String, Vec<String>>,
    pub tiers: BTreeMap<String, usize>,
    pub target: String,
}

/// Check the deck's own step cards against its configured target (default `T`).
pub fn check(workspace: &Path, deck: &Path) -> Result<Report> {
    let cfg = DeckConfig::read_in_workspace(deck, workspace)?;
    let graph = Graph::load_cards_scoped(deck, &cfg.dirs, Some(workspace))?;
    let target = cfg.step_ledger.target.as_deref().unwrap_or("T");
    let mut report = Report {
        errors: graph
            .diagnostics
            .iter()
            .map(|diagnostic| {
                format!(
                    "{}: {}",
                    diagnostic.path.display(),
                    diagnostic.display_message()
                )
            })
            .collect(),
        info: None,
    };
    let steps: BTreeMap<_, _> = graph
        .cards
        .values()
        .filter_map(|card| {
            card.step
                .as_ref()
                .map(|step| (card.id.as_str(), (card, step)))
        })
        .collect();
    let mut deps = BTreeMap::new();

    for (&id, &(card, step)) in &steps {
        let requires: Vec<_> = card
            .links
            .iter()
            .filter(|link| link.rel == "requires")
            .map(|link| link.to.as_str())
            .collect();
        for (field, present) in [
            ("id", !id.trim().is_empty()),
            ("title", !card.title.trim().is_empty()),
            (
                "statement",
                has_text(card.statement.as_ref().map(|part| part.text.as_str())),
            ),
            ("requires", step.declares_requires || !requires.is_empty()),
            ("loc", has_text(step.loc.as_deref())),
            ("origin", step.origin.is_some()),
            ("tier", step.tier.is_some()),
            ("role", has_text(card.role.as_deref())),
        ] {
            if !present {
                report.errors.push(format!("{id}: missing field {field}"));
            }
        }
        if !matches!(step.tier.as_deref(), Some("key" | "semi" | "standard")) {
            report.errors.push(format!(
                "{id}: tier must be one of ('key', 'semi', 'standard')"
            ));
        }
        if !matches!(
            step.origin.as_deref(),
            Some("inherited" | "standard" | "adapted" | "new")
        ) {
            report.errors.push(format!(
                "{id}: origin must be one of ('inherited', 'standard', 'adapted', 'new')"
            ));
        }
        if step.tier.as_deref() == Some("key") && !has_text(step.why_tier.as_deref()) {
            report.errors.push(format!("{id}: a key step needs why_tier (why it is both necessary and not in the literature)"));
        }
        deps.insert(id, requires);
    }
    if !steps.contains_key(target) {
        report
            .errors
            .push(format!("target step '{target}' not found"));
    }
    for (&id, requires) in &deps {
        for &dep in requires {
            if !steps.contains_key(dep) {
                report
                    .errors
                    .push(format!("{id}: unknown dependency {dep}"));
            }
        }
    }
    if !report.errors.is_empty() {
        return Ok(report);
    }

    // Visit prerequisites before their dependents using the reference checker's FIFO order.
    let mut dependents = BTreeMap::<_, Vec<_>>::new();
    let mut remaining = BTreeMap::new();
    for (&id, requires) in &deps {
        remaining.insert(id, requires.len());
        for &dep in requires {
            dependents.entry(dep).or_default().push(id);
        }
    }
    let mut queue: VecDeque<_> = remaining
        .iter()
        .filter_map(|(&id, &n)| (n == 0).then_some(id))
        .collect();
    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        order.push(id.to_owned());
        for &next in dependents.get(id).into_iter().flatten() {
            let count = remaining.get_mut(next).expect("dependent is a step");
            *count -= 1;
            if *count == 0 {
                queue.push_back(next);
            }
        }
    }
    if order.len() != steps.len() {
        let blocked: Vec<_> = remaining
            .into_iter()
            .filter_map(|(id, count)| (count > 0).then_some(id))
            .collect();
        report
            .errors
            .push(format!("dependency cycle among: {}", blocked.join(", ")));
        return Ok(report);
    }

    let needed = closure(target, &deps);
    let unreached: Vec<_> = steps
        .iter()
        .filter_map(|(&id, &(_, step))| {
            (id != target && !needed.contains(id) && !step.outside).then_some(id)
        })
        .collect();
    if !unreached.is_empty() {
        report.errors.push(format!(
            "steps the target does not depend on (mark them \"outside\": true if intentional): {}",
            unreached.join(", ")
        ));
    }

    let mut tiers = BTreeMap::new();
    let mut key_impact = BTreeMap::new();
    for (&id, &(_, step)) in &steps {
        let tier = step.tier.as_deref().expect("tier was validated");
        if id != target {
            *tiers.entry(tier.to_owned()).or_default() += 1;
        }
        if tier == "key" {
            let affected = closure(id, &dependents)
                .into_iter()
                .map(str::to_owned)
                .collect();
            key_impact.insert(id.to_owned(), affected);
        }
    }
    report.info = Some(Info {
        order,
        key_impact,
        tiers,
        target: target.to_owned(),
    });
    Ok(report)
}

fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.trim().is_empty())
}

fn closure<'a>(start: &'a str, edges: &BTreeMap<&'a str, Vec<&'a str>>) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![start];
    while let Some(id) = pending.pop() {
        for &next in edges.get(id).into_iter().flatten() {
            if seen.insert(next) {
                pending.push(next);
            }
        }
    }
    seen
}
