//! Render proof-review deck JSON to Reader Markdown and a dependency diagram.

mod diagram;
mod labels;
mod markup;
mod model;
mod standalone;

use std::{collections::HashMap, fs, path::Path};

use anyhow::{ensure, Context, Result};
use chrono::Local;
use serde::de::DeserializeOwned;

pub use labels::Language;
use markup::{details, escape, markdown, present, table};
use model::{Body, Finding, Graph, Ledger, Review, Tier};

/// Source for a private Reader page and its SVG figure.
pub struct Rendered {
    pub markdown: String,
    pub diagram: String,
}

impl Language {
    /// Prefix that identifies generated Reader Markdown in this language.
    pub fn generation_notice(&self) -> String {
        let labels = self.labels();
        format!("> {}\n>\n> {} ", escape(labels.caution), labels.generated)
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("Cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("Invalid {}", path.display()))
}

/// Render a proof-review spec as Markdown and SVG for the private Reader.
pub fn deck(deck: &Path, slug: &str, language: Language) -> Result<Rendered> {
    ensure!(valid_slug(slug), "invalid Reader slug {slug:?}");
    with_page(deck, language, |page| {
        Ok(Rendered {
            markdown: page.markdown(slug)?,
            diagram: diagram::render(&page.graph, &page.by_step),
        })
    })
}

pub(crate) fn valid_slug(slug: &str) -> bool {
    slug.split('-').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// Render a review as a local single-file HTML page, including its optional demo.
pub fn standalone(
    deck: &Path,
    language: Language,
    katex: Option<&Path>,
    template: Option<&Path>,
) -> Result<String> {
    with_page(deck, language, |page| {
        standalone::render(page, deck, katex, template)
    })
}

/// Check the stated viability verdict against the evidence and the seven rules.
pub fn check_indicator(path: &Path) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Record {
        indicator: model::Indicator,
    }
    let record: Record = read_json(path)?;
    record.indicator.validate()?;
    Ok(())
}

pub(crate) fn card_ids(spec: &Path) -> Result<Vec<String>> {
    let ledger: Ledger = read_json(&spec.join("steps.json"))?;
    let graph = ledger.graph()?;
    let review: Review = read_json(&spec.join("review.json"))?;
    Ok(graph
        .steps
        .iter()
        .filter(|step| step.id != graph.target)
        .map(|step| format!("step-{}", step.id))
        .chain(
            review
                .findings
                .iter()
                .map(|finding| format!("find-{}", finding.id)),
        )
        .collect())
}

fn with_page<T>(
    deck: &Path,
    language: Language,
    render: impl FnOnce(Page<'_>) -> Result<T>,
) -> Result<T> {
    for name in ["steps.json", "bodies.json", "review.json"] {
        let path = deck.join(name);
        ensure!(
            fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()),
            "Expected a regular proof-review source file at {}",
            path.display()
        );
    }
    let ledger: Ledger = read_json(&deck.join("steps.json")).context("ledger errors")?;
    let bodies = read_json(&deck.join("bodies.json"))?;
    let review = read_json(&deck.join("review.json"))?;
    let page = Page::new(ledger.graph()?, &bodies, &review, language)?;
    render(page)
}

struct Page<'a> {
    graph: Graph<'a>,
    bodies: &'a HashMap<String, Body>,
    review: &'a Review,
    language: Language,
    by_step: HashMap<&'a str, Vec<&'a Finding>>,
}

impl<'a> Page<'a> {
    fn new(
        graph: Graph<'a>,
        bodies: &'a HashMap<String, Body>,
        review: &'a Review,
        language: Language,
    ) -> Result<Self> {
        let mut findings_by_id = std::collections::HashSet::new();
        let mut by_step = HashMap::<&str, Vec<&Finding>>::new();
        for finding in &review.findings {
            ensure!(
                findings_by_id.insert(&finding.id),
                "duplicate finding id {}",
                finding.id
            );
            for step in &finding.steps {
                ensure!(
                    graph.by_id.contains_key(step.as_str()),
                    "finding {} refers to unknown step {step}",
                    finding.id
                );
                by_step.entry(step).or_default().push(finding);
            }
        }
        Ok(Self {
            graph,
            bodies,
            review,
            language,
            by_step,
        })
    }

    fn step_links(&self, steps: &[String], fallback: &str) -> String {
        if steps.is_empty() {
            return fallback.into();
        }
        steps
            .iter()
            .map(|id| {
                let label = escape(id);
                if id == self.graph.target {
                    label
                } else {
                    format!("[{label}](#step-{id})")
                }
            })
            .collect::<Vec<_>>()
            .join("、")
    }

    fn proof_spine(&self) -> Result<Vec<String>> {
        let labels = self.language.labels();
        let mut parts = Vec::new();
        if let Some(fragment) = present(&self.review.key_ideas_html) {
            parts.push(
                markdown(Some(fragment))
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        let title = labels.h_diagram;
        parts.push(format!(
            "### {title}\n\n{}\n\n![{title}](asset:diagram.svg)",
            escape(without_last_sentence(labels.diagram_hint))
        ));
        parts.push(escape(without_last_sentence(labels.spine_lead)));

        let groups = self.review.groups.as_deref().unwrap_or_default();
        let grouped: std::collections::HashSet<_> =
            groups.iter().flat_map(|(_, steps)| steps).collect();
        for (title, ids) in groups {
            if !title.is_empty() {
                parts.push(format!("### {}", escape(title)));
            }
            for id in ids {
                parts.push(self.step(id)?);
            }
        }
        for step in self.graph.steps {
            if step.id != self.graph.target && !grouped.contains(&step.id) {
                parts.push(self.step(&step.id)?);
            }
        }
        Ok(parts)
    }

    fn step(&self, id: &str) -> Result<String> {
        let labels = self.language.labels();
        let step = self
            .graph
            .by_id
            .get(id)
            .with_context(|| format!("group refers to unknown step {id}"))?;
        let empty_body = Body::default();
        let body = self.bodies.get(id).unwrap_or(&empty_body);
        let findings = self.by_step.get(id).map(Vec::as_slice).unwrap_or_default();
        let flags = findings
            .iter()
            .map(|finding| format!(" ［{}］", finding.id))
            .collect::<String>();
        let origin = labels.origins[step.origin as usize];
        let tier = labels.tiers[step.tier as usize];
        let deps = self.step_links(&step.deps, labels.none);
        let statement = markdown(body.l1.as_deref());
        let mut parts = vec![
            format!(
                "{}: {}　/　{}: {origin}　/　{}: {deps}",
                labels.meta_loc,
                escape(&step.loc),
                labels.meta_origin,
                labels.meta_deps
            ),
            if statement.is_empty() {
                escape(&step.statement)
            } else {
                statement
            },
        ];
        if let Some(reason) = present(&step.why_tier) {
            parts.push(format!("{}: {}", labels.why_tier, escape(reason)));
        }
        for (label, fragment) in [(labels.l2, &body.l2), (labels.l3, &body.l3)] {
            if let Some(fragment) = present(fragment) {
                parts.push(details(label, None, &markdown(Some(fragment))));
            }
        }
        if !findings.is_empty() {
            let items = findings
                .iter()
                .map(|finding| {
                    format!(
                        "- [{id}](#find-{id})　{}",
                        escape(&finding.short),
                        id = finding.id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(details(
                &self.language.step_findings(findings.len()),
                None,
                &items,
            ));
        }
        let summary = format!("［{tier}］{}　{}{flags}", escape(id), escape(&step.title));
        Ok(details(
            &summary,
            Some(&format!("step-{id}")),
            &parts.join("\n\n"),
        ))
    }

    fn comparison(&self) -> Vec<String> {
        let labels = self.language.labels();
        let rows = [Tier::Key, Tier::Semi, Tier::Standard]
            .into_iter()
            .flat_map(|tier| {
                self.graph
                    .steps
                    .iter()
                    .filter(move |step| step.id != self.graph.target && step.tier == tier)
                    .map(move |step| {
                        let notes = self.review.step_review.get(&step.id);
                        [
                            format!("{} {}", labels.tiers[tier as usize], step.id),
                            step.title.clone(),
                            step.role.clone(),
                            labels.origins[step.origin as usize].into(),
                            notes.map_or("—", |notes| &notes.risk).into(),
                            notes.map_or("—", |notes| &notes.evidence).into(),
                        ]
                    })
            });
        let mut parts = vec![
            escape(labels.compare_lead),
            table(
                [
                    labels.th_class,
                    labels.th_content,
                    labels.th_role,
                    labels.th_origin,
                    labels.th_risk,
                    labels.th_evidence,
                ],
                rows,
            ),
        ];
        if let Some(fragment) = present(&self.review.compare_note_html) {
            parts.push(markdown(Some(fragment)));
        }
        parts
    }

    fn review(&self) -> Result<Vec<String>> {
        let labels = self.language.labels();
        let mut counts = [0; 4];
        for finding in &self.review.findings {
            counts[finding.sev as usize] += 1;
        }
        let mut parts = vec![
            format!(
                "**{}**　{}",
                labels.review_conclusion,
                escape(&self.review.review_conclusion)
            ),
            escape(
                &self
                    .language
                    .findings_count(self.review.findings.len(), counts),
            ),
            format!(
                "**{}**　{}",
                labels.review_reading,
                escape(labels.review_reading_text)
            ),
            format!("### {}", labels.h_panel),
            table(
                [
                    labels.th_view,
                    labels.th_who,
                    labels.th_scope,
                    labels.th_result,
                ],
                self.review
                    .panel
                    .iter()
                    .map(|entry| [&entry.name, &entry.who, &entry.scope, &entry.result]),
            ),
        ];
        push_list(&mut parts, labels.h_mychecks, &self.review.my_checks);
        parts.push(format!(
            "### {}\n\n{}",
            labels.h_findings,
            escape(labels.findings_hint)
        ));
        let mut findings: Vec<_> = self.review.findings.iter().collect();
        findings.sort_by_key(|finding| (finding.sev as usize, finding.id.as_str()));
        for finding in findings {
            let target = self.step_links(&finding.steps, labels.f_all);
            let mut body = vec![
                format!(
                    "{}: {target}　/　{}: {}",
                    labels.f_target,
                    labels.f_from,
                    escape(&finding.from.join("、"))
                ),
                markdown(finding.detail.as_deref()),
            ];
            for (label, text) in [
                (labels.f_evidence, &finding.evidence),
                (labels.f_ruling, &finding.ruling),
            ] {
                if let Some(text) = present(text) {
                    body.push(format!("{label}: {}", escape(text)));
                }
            }
            let status = present(&finding.status_note)
                .or_else(|| {
                    finding
                        .status
                        .and_then(|status| labels.statuses.get(status as usize).copied())
                })
                .with_context(|| format!("finding {} needs a status or status_note", finding.id))?;
            let summary = format!(
                "［{}／{}］{}　{}",
                labels.severities[finding.sev as usize],
                escape(status),
                escape(&finding.id),
                escape(&finding.short)
            );
            parts.push(details(
                &summary,
                Some(&format!("find-{}", finding.id)),
                &body.join("\n\n"),
            ));
        }
        Ok(parts)
    }

    fn prior(&self) -> Vec<String> {
        let labels = self.language.labels();
        let mut parts = vec![table(
            [
                labels.th_prior,
                labels.th_op,
                labels.th_hyp,
                labels.th_overlap,
                labels.th_state,
            ],
            self.review.prior.iter().map(|entry| {
                [
                    &entry.name,
                    &entry.op,
                    &entry.hyp,
                    &entry.overlap,
                    &entry.state,
                ]
            }),
        )];
        if let Some(hole) = present(&self.review.prior_hole) {
            parts.push(format!("**{}**　{}", labels.prior_hole, escape(hole)));
        }
        push_list(&mut parts, labels.prior_uncited, &self.review.uncited);
        push_list(&mut parts, labels.prior_admitted, &self.review.admitted);
        parts
    }

    fn indicator(&self) -> Result<Vec<String>> {
        let labels = self.language.labels();
        let indicator = &self.review.indicator;
        let assessment = indicator.assess()?;
        let axes = assessment
            .axes
            .into_iter()
            .map(|(axis, level, why)| {
                format!(
                    "- **{}**（{}）{}",
                    labels.axis_names[axis],
                    labels.levels[level],
                    escape(why)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rules = labels
            .rules
            .iter()
            .map(|rule| format!("- {}", escape(rule)))
            .collect::<Vec<_>>()
            .join("\n");
        let changers = indicator
            .changers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, changer)| format!("{}. {}", index + 1, escape(changer)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(vec![
            format!(
                "**{}: {}**　{}",
                labels.verdict,
                labels.verdicts[assessment.verdict],
                escape(indicator.verdict_why.as_deref().unwrap_or_default())
            ),
            format!("### {}\n\n{axes}", labels.axes_h),
            format!("### {}\n\n{rules}", labels.rules_h),
            format!("### {}\n\n{changers}", labels.changers_h),
            format!("> {}", escape(labels.indicator_note)),
        ])
    }

    fn markdown(&self, slug: &str) -> Result<String> {
        let labels = self.language.labels();
        let title = self.review.title.replace('\\', "\\\\").replace('"', "\\\"");
        let lang = self.language.code();
        let front =
            format!("+++\ntitle = \"{title}\"\nslug = \"{slug}\"\nlanguage = \"{lang}\"\n+++\n\n");
        let date = Local::now().date_naive();
        let mut parts = vec![format!("{}{date}", self.language.generation_notice())];
        if let Some(subtitle) = present(&self.review.subtitle) {
            parts.push(escape(subtitle));
        }
        parts.push(section(
            labels.h_summary,
            &markdown(self.review.summary_html.as_deref()),
        ));
        if let Some(fragment) = present(&self.review.intro_html) {
            parts.push(section(labels.h_intro, &markdown(Some(fragment))));
        }
        parts.push(section(labels.h_spine, &self.proof_spine()?.join("\n\n")));
        parts.push(section(labels.h_compare, &self.comparison().join("\n\n")));
        parts.push(section(labels.h_review, &self.review()?.join("\n\n")));
        parts.push(section(labels.h_prior, &self.prior().join("\n\n")));
        parts.push(section(labels.h_indicator, &self.indicator()?.join("\n\n")));
        if let Some(glossary) = self
            .review
            .glossary
            .as_deref()
            .filter(|items| !items.is_empty())
        {
            let lines = glossary
                .iter()
                .map(|(term, definition)| {
                    format!(
                        "- **{}**：{}",
                        markdown(term.as_deref()),
                        markdown(definition.as_deref())
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(section(labels.h_glossary, &lines));
        }
        Ok(format!("{front}{}\n", parts.join("\n\n")))
    }
}

fn section(title: &str, body: &str) -> String {
    format!("## {title}\n\n{body}")
}

fn without_last_sentence(text: &str) -> &str {
    let end = text
        .trim_end_matches(['.', '。'])
        .rfind(['.', '。'])
        .expect("standalone guidance has more than one sentence");
    &text[..end + text[end..].chars().next().expect("punctuation").len_utf8()]
}

fn push_list(parts: &mut Vec<String>, title: &str, items: &Option<Vec<String>>) {
    if let Some(items) = items.as_deref().filter(|items| !items.is_empty()) {
        let lines = items
            .iter()
            .map(|item| format!("- {}", escape(item)))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("### {title}\n\n{lines}"));
    }
}
