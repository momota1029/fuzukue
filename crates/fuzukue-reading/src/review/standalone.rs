use std::fmt::Write as _;

use super::*;
use crate::html::{self, escape};
use model::{Severity, Step};

const TEMPLATE: &str = include_str!("../assets/review-page-template.html");
const TIER_KEYS: [&str; 3] = ["key", "semi", "standard"];
const TIER_CLASSES: [&str; 3] = ["t-keyc", "t-semic", ""];
const SEVERITY_CLASSES: [&str; 4] = ["sev-c", "sev-m", "sev-n", "sev-e"];
const LEVEL_CLASSES: [&str; 4] = ["l-unk", "l-weak", "l-mid", "l-strong"];

pub(super) fn render(
    page: Page<'_>,
    spec: &Path,
    katex: Option<&Path>,
    template: Option<&Path>,
) -> Result<String> {
    let labels = page.language.labels();
    let review = page.review;
    let assessment = review.indicator.assess()?;
    let mut sections = vec![
        (
            "summary",
            labels.h_summary,
            raw(&review.summary_html).into(),
        ),
        ("intro", labels.h_intro, raw(&review.intro_html).into()),
        ("spine", labels.h_spine, page.html_spine()?),
        ("compare", labels.h_compare, page.html_comparison()),
    ];
    let mut demo_script = String::new();
    let demo_path = spec.join("demo.html");
    if let Some(demo) = match fs::read_to_string(&demo_path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", demo_path.display())),
    } {
        let script = regex::Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>")?;
        demo_script = script
            .find_iter(&demo)
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        sections.push((
            "demo",
            labels.h_demo,
            script.replace_all(&demo, "").into_owned(),
        ));
    }
    sections.extend([
        ("review", labels.h_review, page.html_review()?),
        ("prior", labels.h_prior, page.html_prior()),
        (
            "indicator",
            labels.h_indicator,
            page.html_indicator(assessment),
        ),
    ]);
    if let Some(glossary) = review.glossary.as_ref().filter(|items| !items.is_empty()) {
        let terms: String = glossary
            .iter()
            .map(|(term, meaning)| format!("<dt>{}</dt><dd>{}</dd>", raw(term), raw(meaning)))
            .collect();
        sections.push(("glossary", labels.h_glossary, format!("<dl>{terms}</dl>")));
    }
    let mut body = format!(
        r#"<h1>{}</h1><p class="sub">{}<br><span class="lab">{}　{} {}</span></p><div class="card warn"><p>{}</p></div>"#,
        escape(&review.title),
        escape(raw(&review.subtitle)),
        labels.local,
        labels.generated,
        Local::now().date_naive(),
        escape(labels.caution),
    );
    for (number, (id, heading, content)) in sections.into_iter().enumerate() {
        write!(
            body,
            r#"<h2 id="{id}">{number}. {}</h2>{content}"#,
            escape(heading)
        )
        .unwrap();
    }
    let custom = template.map(fs::read_to_string).transpose()?;
    html::page(
        &body,
        &review.title,
        page.language,
        custom.as_deref().unwrap_or(TEMPLATE),
        katex,
        &demo_script,
    )
}

fn raw(text: &Option<String>) -> &str {
    text.as_deref().unwrap_or_default()
}

fn link(prefix: &str, id: &str, label: &str) -> String {
    format!(
        r##"<a href="#{prefix}{}">{}</a>"##,
        escape(id),
        escape(label)
    )
}

fn heading(title: &str) -> String {
    format!("<h3>{}</h3>", escape(title))
}

fn list(items: impl IntoIterator<Item = String>, tag: &str) -> String {
    let items = items
        .into_iter()
        .map(|text| format!("<li>{text}</li>"))
        .collect::<String>();
    format!("<{tag}>{items}</{tag}>")
}

fn list_section(title: &str, items: &Option<Vec<String>>) -> String {
    items
        .as_deref()
        .filter(|items| !items.is_empty())
        .map_or_else(String::new, |items| {
            heading(title) + &list(items.iter().map(|item| escape(item)), "ul")
        })
}

fn table<const N: usize>(
    headers: [&str; N],
    rows: impl IntoIterator<Item = (String, [String; N])>,
) -> String {
    let header: String = headers
        .into_iter()
        .map(|label| format!("<th>{}</th>", escape(label)))
        .collect();
    let mut html = format!("<table><thead><tr>{header}</tr></thead><tbody>");
    for (class, cells) in rows {
        write!(html, r#"<tr class="{class}">"#).unwrap();
        for cell in cells {
            write!(html, "<td>{cell}</td>").unwrap();
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");
    html
}

impl Page<'_> {
    fn html_link(&self, id: &str) -> String {
        if id == self.graph.target {
            link("", "intro", id)
        } else {
            link("step-", id, id)
        }
    }

    fn html_links(&self, ids: &[String], empty: &str) -> String {
        if ids.is_empty() {
            return escape(empty);
        }
        ids.iter()
            .map(|id| self.html_link(id))
            .collect::<Vec<_>>()
            .join("、")
    }

    fn tier_tag(&self, tier: Tier) -> String {
        let i = tier as usize;
        format!(
            r#"<span class="tag t-{}">{}</span>"#,
            TIER_KEYS[i],
            self.language.labels().tiers[i]
        )
    }

    fn html_spine(&self) -> Result<String> {
        let labels = self.language.labels();
        let diagram = diagram::render(&self.graph, &self.by_step);
        let mut body = String::new();
        if let Some(idea) = present(&self.review.key_ideas_html) {
            write!(body, r#"<div class="card key">{idea}</div>"#).unwrap();
        }
        write!(
            body,
            r#"<h3>{}</h3><p class="lab">{}</p><div class="flow2">{diagram}</div><div class="legend"><i style="background:#c2410c"></i>{}<i style="background:#6b4fbb"></i>{}<i style="background:#5b6677"></i>{}</div><p class="lab">{}</p>"#,
            labels.h_diagram,
            escape(labels.diagram_hint),
            labels.legend_key,
            labels.legend_semi,
            labels.legend_standard,
            escape(labels.spine_lead)
        )
        .unwrap();
        let mut grouped = std::collections::HashSet::new();
        for (title, ids) in self.review.groups.as_deref().unwrap_or_default() {
            if !title.is_empty() {
                body.push_str(&heading(title));
            }
            for id in ids {
                let step = self
                    .graph
                    .by_id
                    .get(id.as_str())
                    .with_context(|| format!("group refers to unknown step {id}"))?;
                ensure!(
                    grouped.insert(id.as_str()),
                    "step {id} appears in more than one group entry"
                );
                if id != self.graph.target {
                    body.push_str(&self.html_step(step));
                }
            }
        }
        for step in self.graph.steps {
            if step.id != self.graph.target && !grouped.contains(step.id.as_str()) {
                body.push_str(&self.html_step(step));
            }
        }
        Ok(body)
    }

    fn html_step(&self, step: &Step) -> String {
        let labels = self.language.labels();
        let id = escape(&step.id);
        let empty = Body::default();
        let body = self.bodies.get(&step.id).unwrap_or(&empty);
        let fallback = escape(&step.statement);
        let statement = body.l1.as_deref().unwrap_or(&fallback);
        let class = TIER_CLASSES[step.tier as usize];
        let open = if step.tier == Tier::Key { " open" } else { "" };
        let findings = self
            .by_step
            .get(step.id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let flags: String = findings
            .iter()
            .map(|finding| {
                format!(
                    r##" <a class="tag {}" href="#find-{}">{}</a>"##,
                    SEVERITY_CLASSES[finding.sev as usize],
                    escape(&finding.id),
                    escape(&finding.id)
                )
            })
            .collect();
        let mut html = format!(
            r#"<details class="step {class}" id="step-{id}"{open}><summary>{}<b>{id}</b>　{}{flags}</summary><div class="meta">{}: {}　/　{}: {}　/　{}: {}</div><div class="l1">{statement}</div>"#,
            self.tier_tag(step.tier),
            escape(&step.title),
            labels.meta_loc,
            escape(&step.loc),
            labels.meta_origin,
            labels.origins[step.origin as usize],
            labels.meta_deps,
            self.html_links(&step.deps, labels.none),
        );
        if let Some(reason) = present(&step.why_tier) {
            write!(
                html,
                r#"<div class="meta">{}: {}</div>"#,
                labels.why_tier,
                escape(reason)
            )
            .unwrap();
        }
        for (class, open, title, fragment) in [
            ("l2", open, labels.l2, &body.l2),
            ("l3", "", labels.l3, &body.l3),
        ] {
            if let Some(fragment) = present(fragment) {
                write!(html, r#"<details class="{class}"{open}><summary>{title}</summary>{fragment}</details>"#).unwrap();
            }
        }
        if !findings.is_empty() {
            let items = list(
                findings.iter().map(|finding| {
                    format!(
                        "{}　{}",
                        link("find-", &finding.id, &finding.id),
                        escape(&finding.short)
                    )
                }),
                "ul",
            );
            write!(
                html,
                r#"<details class="l3"><summary>{}</summary>{items}</details>"#,
                self.language.step_findings(findings.len())
            )
            .unwrap();
        }
        html.push_str("</details>");
        html
    }

    fn html_comparison(&self) -> String {
        let l = self.language.labels();
        let rows = [Tier::Key, Tier::Semi, Tier::Standard]
            .into_iter()
            .flat_map(|tier| {
                self.graph
                    .steps
                    .iter()
                    .filter(move |s| s.id != self.graph.target && s.tier == tier)
                    .map(move |s| {
                        let notes = self.review.step_review.get(&s.id);
                        (
                            ["k", "s", ""][tier as usize].into(),
                            [
                                self.tier_tag(tier) + &self.html_link(&s.id),
                                escape(&s.title),
                                escape(&s.role),
                                l.origins[s.origin as usize].into(),
                                escape(notes.map_or("—", |r| &r.risk)),
                                escape(notes.map_or("—", |r| &r.evidence)),
                            ],
                        )
                    })
            });
        format!(
            "<p>{}</p>{}{}",
            escape(l.compare_lead),
            table(
                [
                    l.th_class,
                    l.th_content,
                    l.th_role,
                    l.th_origin,
                    l.th_risk,
                    l.th_evidence
                ],
                rows
            ),
            raw(&self.review.compare_note_html)
        )
    }

    fn html_review(&self) -> Result<String> {
        let l = self.language.labels();
        let mut counts = [0; 4];
        for finding in &self.review.findings {
            counts[finding.sev as usize] += 1;
        }
        let panel = table(
            [l.th_view, l.th_who, l.th_scope, l.th_result],
            self.review.panel.iter().map(|p| {
                (
                    String::new(),
                    [&p.name, &p.who, &p.scope, &p.result].map(|s| escape(s)),
                )
            }),
        );
        let class = if counts[Severity::Fatal as usize] == 0 {
            "good"
        } else {
            "bad"
        };
        let mut html = format!(
            r#"<div class="card {class}"><p><b>{}</b>　{}</p><p>{}</p></div><div class="card warn"><p><b>{}</b>　{}</p></div>{heading}{panel}"#,
            l.review_conclusion,
            escape(&self.review.review_conclusion),
            escape(
                &self
                    .language
                    .findings_count(self.review.findings.len(), counts)
            ),
            l.review_reading,
            escape(l.review_reading_text),
            heading = heading(l.h_panel)
        );
        html.push_str(&list_section(l.h_mychecks, &self.review.my_checks));
        write!(
            html,
            r#"{}<p class="lab">{}</p>"#,
            heading(l.h_findings),
            escape(l.findings_hint)
        )
        .unwrap();
        let mut findings: Vec<_> = self.review.findings.iter().collect();
        findings.sort_by_key(|f| (f.sev as usize, f.id.as_str()));
        for finding in findings {
            let id = escape(&finding.id);
            let severity = finding.sev as usize;
            let status = present(&finding.status_note)
                .or_else(|| {
                    finding
                        .status
                        .and_then(|s| l.statuses.get(s as usize).copied())
                })
                .with_context(|| format!("finding {} needs a status or status_note", finding.id))?;
            write!(html, r#"<details class="step" id="find-{id}"><summary><span class="tag {}">{}</span><span class="tag t-info">{}</span><b>{id}</b>　{}</summary><div class="meta">{}: {}　/　{}: {}</div><div class="l1">{}</div>"#,
                SEVERITY_CLASSES[severity], l.severities[severity], escape(status), escape(&finding.short),
                l.f_target, self.html_links(&finding.steps, l.f_all), l.f_from, escape(&finding.from.join("、")), raw(&finding.detail)).unwrap();
            for (label, text) in [
                (l.f_evidence, &finding.evidence),
                (l.f_ruling, &finding.ruling),
            ] {
                if let Some(text) = present(text) {
                    write!(html, r#"<div class="meta">{label}: {}</div>"#, escape(text)).unwrap();
                }
            }
            html.push_str("</details>");
        }
        Ok(html)
    }

    fn html_prior(&self) -> String {
        let l = self.language.labels();
        let mut html = table(
            [l.th_prior, l.th_op, l.th_hyp, l.th_overlap, l.th_state],
            self.review.prior.iter().map(|p| {
                (
                    String::new(),
                    [&p.name, &p.op, &p.hyp, &p.overlap, &p.state].map(|s| escape(s)),
                )
            }),
        );
        if let Some(hole) = present(&self.review.prior_hole) {
            write!(
                html,
                r#"<div class="card warn"><p><b>{}</b>　{}</p></div>"#,
                l.prior_hole,
                escape(hole)
            )
            .unwrap();
        }
        html + &list_section(l.prior_uncited, &self.review.uncited)
            + &list_section(l.prior_admitted, &self.review.admitted)
    }

    fn html_indicator(&self, assessment: model::Assessment<'_>) -> String {
        let l = self.language.labels();
        let indicator = &self.review.indicator;
        let mut html = format!(
            r#"<div class="card key"><p class="verdict">{}: {}</p><p>{}</p></div>{}"#,
            l.verdict,
            l.verdicts[assessment.verdict],
            escape(raw(&indicator.verdict_why)),
            heading(l.axes_h)
        );
        for (axis, level, reason) in assessment.axes {
            write!(html, r#"<div class="axis"><div><b>{}</b></div><div><span class="lvl {}">{}</span></div><div>{}</div></div>"#,
                l.axis_names[axis], LEVEL_CLASSES[level], l.levels[level], escape(reason)).unwrap();
        }
        html + &heading(l.rules_h)
            + &list(l.rules.iter().map(|rule| escape(rule)), "ul")
            + &heading(l.changers_h)
            + &list(indicator.changers.iter().flatten().map(|s| escape(s)), "ol")
            + &format!(
                r#"<div class="card warn"><p>{}</p></div>"#,
                escape(l.indicator_note)
            )
    }
}
