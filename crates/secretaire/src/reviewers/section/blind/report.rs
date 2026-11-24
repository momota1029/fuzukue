use super::sources::{exact_lines, Excerpt, Packet, Part, Role};
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Criterion {
    Quantifiers,
    Hypotheses,
    EdgeCases,
    LemmaApplicability,
    Derivation,
    Conclusion,
}

fn criteria_for(part: Part) -> [Criterion; 3] {
    use Criterion::*;
    match part {
        Part::Statement => [Quantifiers, Hypotheses, EdgeCases],
        Part::Proof => [LemmaApplicability, Derivation, Conclusion],
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Verdict {
    Pass,
    Findings,
    Partial,
    Unchecked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    pub(super) excerpt: String,
    pub(super) lines: [usize; 2],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Checked {
    pub(super) location: Location,
    pub(super) criteria: Vec<Criterion>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unchecked {
    pub(super) location: Location,
    pub(super) criteria: Vec<Criterion>,
    pub(super) reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    pub(super) card: String,
    pub(super) verdict: Verdict,
    #[serde(default)]
    pub(super) checked: Vec<Checked>,
    #[serde(default)]
    pub(super) unchecked: Vec<Unchecked>,
    #[serde(default)]
    pub(super) references: Vec<Location>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Severity {
    Critical,
    Major,
    Minor,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Finding {
    pub(super) id: String,
    pub(super) card: String,
    pub(super) location: Location,
    pub(super) quote: String,
    pub(super) criterion: Criterion,
    pub(super) severity: Severity,
    pub(super) reasoning: String,
    pub(super) repair: String,
    #[serde(default)]
    pub(super) impacts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    reviewed_version: String,
    #[serde(default)]
    targets: Vec<Target>,
    #[serde(default)]
    findings: Vec<Finding>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Coverage {
    pub(super) verdict: Verdict,
    pub(super) targets: Vec<Target>,
    pub(super) findings: Vec<Finding>,
}

pub(super) fn location<'a>(packet: &'a Packet, location: &Location) -> Result<&'a Excerpt> {
    let excerpt = packet
        .excerpts
        .iter()
        .find(|e| e.label == location.excerpt)
        .with_context(|| format!("Unknown reported excerpt {}", location.excerpt))?;
    let [first, last] = location.lines;
    ensure!(
        (excerpt.lines[0]..=excerpt.lines[1]).contains(&first)
            && (first..=excerpt.lines[1]).contains(&last),
        "Reported lines are outside {}",
        excerpt.label
    );
    Ok(excerpt)
}

fn target_location<'a>(
    packet: &'a Packet,
    card: &str,
    position: &Location,
    criteria: &[Criterion],
) -> Result<&'a Excerpt> {
    let excerpt = location(packet, position)?;
    ensure!(
        excerpt.card.as_deref() == Some(card) && excerpt.role == Role::Target,
        "Reported location does not belong to target {card}"
    );
    let available = criteria_for(excerpt.part.context("Target has no card part")?);
    ensure!(
        !criteria.is_empty()
            && criteria
                .iter()
                .all(|criterion| available.contains(criterion)),
        "Reported criteria do not match the card part for {card}"
    );
    Ok(excerpt)
}

fn report_lines<'a>(
    location: &'a Location,
    criteria: &'a [Criterion],
) -> impl Iterator<Item = (&'a str, usize, Criterion)> {
    (location.lines[0]..=location.lines[1]).flat_map(move |line| {
        criteria
            .iter()
            .map(move |&criterion| (location.excerpt.as_str(), line, criterion))
    })
}

fn validate_target(packet: &Packet, target: &Target) -> Result<()> {
    let mut checked = BTreeSet::new();
    for item in &target.checked {
        target_location(packet, &target.card, &item.location, &item.criteria)?;
        ensure!(
            report_lines(&item.location, &item.criteria).all(|line| checked.insert(line)),
            "Overlapping checked coverage for {}",
            target.card
        );
    }
    let mut unchecked = BTreeSet::new();
    for item in &target.unchecked {
        target_location(packet, &target.card, &item.location, &item.criteria)?;
        ensure!(
            !item.reason.trim().is_empty(),
            "Unchecked coverage requires a reason"
        );
        ensure!(
            report_lines(&item.location, &item.criteria)
                .all(|line| !checked.contains(&line) && unchecked.insert(line)),
            "Contradictory or overlapping coverage for {}",
            target.card
        );
    }
    for reference in &target.references {
        let excerpt = location(packet, reference)?;
        ensure!(
            excerpt.card.as_deref() != Some(&target.card),
            "A target must report its own excerpts as checked or unchecked"
        );
        ensure!(
            packet.is_relevant(&target.card, excerpt),
            "Reported premise does not belong to the target's dependencies"
        );
    }
    Ok(())
}

fn fill_omissions(packet: &Packet, target: &mut Target) {
    let reported: BTreeSet<_> = target
        .checked
        .iter()
        .map(|item| (&item.location, &item.criteria))
        .chain(
            target
                .unchecked
                .iter()
                .map(|item| (&item.location, &item.criteria)),
        )
        .flat_map(|(location, criteria)| report_lines(location, criteria))
        .collect();
    let mut omissions = Vec::new();
    for excerpt in packet
        .excerpts
        .iter()
        .filter(|e| e.card.as_deref() == Some(&target.card))
    {
        for criterion in criteria_for(excerpt.part.expect("target has a part")) {
            let [first, last] = excerpt.lines;
            let mut missing = (first..=last)
                .filter(|line| !reported.contains(&(excerpt.label.as_str(), *line, criterion)))
                .peekable();
            while let Some(start) = missing.next() {
                let mut end = start;
                while missing.next_if_eq(&(end + 1)).is_some() {
                    end += 1;
                }
                omissions.push(Unchecked {
                    location: Location {
                        excerpt: excerpt.label.clone(),
                        lines: [start, end],
                    },
                    criteria: vec![criterion],
                    reason: "Coverage omitted from response".into(),
                });
            }
        }
    }
    target.unchecked.extend(omissions);
}

fn validate_finding(
    packet: &Packet,
    finding: &Finding,
    targets: &BTreeMap<String, Target>,
) -> Result<()> {
    let excerpt = target_location(
        packet,
        &finding.card,
        &finding.location,
        &[finding.criterion],
    )?;
    let range = finding
        .location
        .lines
        .map(|line| line - excerpt.lines[0] + 1);
    let text =
        exact_lines(&excerpt.text, range[0], range[1]).context("Invalid finding location")?;
    ensure!(
        !finding.quote.trim().is_empty() && text.contains(&finding.quote),
        "Finding {} quote does not match the sent text",
        finding.id
    );
    ensure!(
        !finding.reasoning.trim().is_empty() && !finding.repair.trim().is_empty(),
        "Finding {} requires reasoning and a repair",
        finding.id
    );
    let covered = |line| {
        targets.get(&finding.card).is_some_and(|target| {
            target.checked.iter().any(|item| {
                item.location.excerpt == finding.location.excerpt
                    && item.criteria.contains(&finding.criterion)
                    && (item.location.lines[0]..=item.location.lines[1]).contains(&line)
            })
        })
    };
    ensure!(
        (finding.location.lines[0]..=finding.location.lines[1]).all(covered),
        "Finding {} is outside the reported checked coverage",
        finding.id
    );
    Ok(())
}

pub(super) fn validate(scope: &Scope, packet: &Packet, text: &str) -> Result<Coverage> {
    let response: Response = serde_json::from_str(text).context("Invalid section review JSON")?;
    ensure!(
        response.reviewed_version == scope.reviewed_version,
        "Response version does not match the sent version"
    );
    let mut targets = BTreeMap::new();
    for target in response.targets {
        ensure!(
            scope.cards.contains(&target.card),
            "Unknown reported target {}",
            target.card
        );
        validate_target(packet, &target)?;
        ensure!(
            targets.insert(target.card.clone(), target).is_none(),
            "Duplicate reported target"
        );
    }
    let mut ids = BTreeSet::new();
    for finding in &response.findings {
        ensure!(
            !finding.id.trim().is_empty() && ids.insert(&finding.id),
            "Missing or duplicate finding ID"
        );
        validate_finding(packet, finding, &targets)?;
    }
    let mut ordered = Vec::new();
    for id in &scope.cards {
        let mut target = targets.remove(id).unwrap_or_else(|| Target {
            card: id.clone(),
            verdict: Verdict::Unchecked,
            checked: Vec::new(),
            unchecked: Vec::new(),
            references: Vec::new(),
        });
        fill_omissions(packet, &mut target);
        let has_finding = response.findings.iter().any(|finding| finding.card == *id);
        target.verdict = if target.checked.is_empty() {
            Verdict::Unchecked
        } else if !target.unchecked.is_empty()
            || target.verdict
                != if has_finding {
                    Verdict::Findings
                } else {
                    Verdict::Pass
                }
        {
            Verdict::Partial
        } else {
            target.verdict
        };
        ordered.push(target);
    }
    let verdict = if ordered
        .iter()
        .all(|target| target.verdict == Verdict::Unchecked)
    {
        Verdict::Unchecked
    } else if ordered
        .iter()
        .any(|target| matches!(target.verdict, Verdict::Partial | Verdict::Unchecked))
    {
        Verdict::Partial
    } else if response.findings.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Findings
    };
    Ok(Coverage {
        verdict,
        targets: ordered,
        findings: response.findings,
    })
}
