use std::{io::Read, path::Component};

use fuzukue_cards::{course, FOLLOW_RELS};
use sha2::{Digest, Sha256};

use super::*;

pub(super) const MAX_CARDS: usize = 4;
const MAX_MESSAGE_BYTES: usize = 32 * 1024;
const CRITERIA: &str = include_str!("criteria.md");
const SKILL_REVISION: &str = "123eb23502ff597222cb3c31bd49e48e84e4c04c";
const INSTRUCTIONS: &str = include_str!("response-format.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Part {
    Statement,
    Proof,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Statement => "statement",
            Self::Proof => "proof",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Selection {
    Card { card: String, part: Part },
    Material { key: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Allowed {
    pub(super) lines: [usize; 2],
    pub(super) source: Selection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Target,
    Premise,
    Material,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Excerpt {
    pub(super) label: String,
    pub(super) card: Option<String>,
    pub(super) part: Option<Part>,
    pub(super) lines: [usize; 2],
    pub(super) role: Role,
    pub(super) file: PathBuf,
    sha256: String,
    pub(super) text: String,
    license: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Skill {
    names: Vec<String>,
    revision: String,
    sha256: String,
    text: String,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PartSnapshot {
    pub(super) card: String,
    pub(super) part: Part,
    pub(super) file: PathBuf,
    pub(super) first_line: usize,
    pub(super) lines: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Packet {
    skill: Skill,
    pub(super) excerpts: Vec<Excerpt>,
    pub(super) message: String,
    pub(super) fingerprints: BTreeMap<String, String>,
    // Hash unpublished lines to verify the changed range without sending their contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) target_parts: Vec<PartSnapshot>,
    relevant: BTreeMap<String, BTreeSet<String>>,
    original_source_acquisition: String,
    original_source_check: String,
}

impl Packet {
    pub(super) fn model_input(&self, scope: &Scope) -> crate::isolated_model::Input {
        use crate::isolated_model::{Input, Material};

        Input::legacy(
            vec![
                INSTRUCTIONS.into(),
                self.skill.text.clone(),
                scope_header(scope),
            ],
            self.excerpts
                .iter()
                .map(|excerpt| Material {
                    label: excerpt.label.clone(),
                    text: excerpt.text.clone(),
                })
                .collect(),
            self.message.clone(),
        )
    }

    pub(super) fn is_relevant(&self, target: &str, excerpt: &Excerpt) -> bool {
        excerpt.role == Role::Material
            || excerpt
                .card
                .as_ref()
                .is_some_and(|id| self.relevant[target].contains(id))
    }
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(super) fn exact_lines(text: &str, first: usize, last: usize) -> Option<&str> {
    if first == 0 || first > last {
        return None;
    }
    let mut offset = 0;
    let mut start = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let number = index + 1;
        if number == first {
            start = offset;
        }
        offset += line.len();
        if number == last {
            return Some(&text[start..offset]);
        }
    }
    None
}

/// Require a real workspace file at the graph's logical location, with no path aliases.
fn regular_source(project: &Path, path: &Path) -> Result<PathBuf> {
    let mut logical = PathBuf::new();
    for part in path.strip_prefix(project)?.components() {
        match part {
            Component::Normal(part) => logical.push(part),
            Component::ParentDir => {
                ensure!(logical.pop(), "Review source is outside the workspace")
            }
            _ => bail!("Invalid review source path"),
        }
    }
    let resolved = workspace_paths::resolve(project, project, &logical)?;
    ensure!(
        resolved == project.join(&logical) && fs::symlink_metadata(&resolved)?.is_file(),
        "Review source must be a regular file at its declared location"
    );
    Ok(logical)
}

fn card_text(
    project: &Path,
    root: &Path,
    card: &Card,
    part: Part,
    lines: [usize; 2],
) -> Result<(PathBuf, String, PartSnapshot)> {
    let field = match part {
        Part::Statement => &card.statement,
        Part::Proof => &card.proof,
    };
    let field = field
        .as_ref()
        .with_context(|| format!("Card {} has no {}", card.id, part.name()))?;
    let tex_name = format!("{}.tex", part.name());
    let tex = card.tex.iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name == tex_name.as_str())
    });
    let file = regular_source(project, &root.join(tex.unwrap_or(&card.path)))?;
    let mut raw = String::new();
    crate::mcp::workspace_read::open_regular_source(&project.join(&file))?
        .read_to_string(&mut raw)?;
    let first = usize::try_from(field.line)?;
    let last = first + field.text.split_inclusive('\n').count().saturating_sub(1);
    let captured = exact_lines(&raw, first, last).context("Card part changed during capture")?;
    ensure!(
        captured.trim() == field.text.trim(),
        "Card part changed during capture"
    );
    ensure!(
        first <= lines[0] && lines[1] <= last,
        "Excerpt runs outside the selected card part"
    );
    let text = exact_lines(&raw, lines[0], lines[1]).context("Invalid card excerpt line range")?;
    ensure!(!text.trim().is_empty(), "Card excerpt is empty");
    let snapshot = PartSnapshot {
        card: card.id.clone(),
        part,
        file: file.clone(),
        first_line: first,
        lines: captured
            .split_inclusive('\n')
            .map(|line| sha256(line.as_bytes()))
            .collect(),
    };
    Ok((file, text.to_owned(), snapshot))
}

fn section_order(project: &Path, root: &Path, cfg: &DeckConfig, key: &str) -> Result<Vec<String>> {
    if let Some(course) = course::Course::read(root)? {
        let section = course
            .sections
            .into_iter()
            .find(|section| section.key == key)
            .with_context(|| format!("Unknown section {key}"))?;
        ensure!(
            matches!(
                section.status,
                course::Status::Written | course::Status::Done
            ),
            "Course section has not been committed"
        );
        return Ok(section.cards);
    }
    let structure = regular_source(project, &root.join(&cfg.structure))?;
    course::book_sections_from(&project.join(structure))?
        .into_iter()
        .find(|section| section.key == key)
        .map(|section| section.cards)
        .with_context(|| format!("Unknown section {key}"))
}

fn dependencies(graph: &Graph, targets: &[&Card]) -> Result<BTreeMap<String, BTreeSet<String>>> {
    targets
        .iter()
        .map(|card| {
            let mut direct = BTreeSet::new();
            for link in &card.links {
                if FOLLOW_RELS.contains(&link.rel.as_str()) {
                    ensure!(
                        graph.cards.contains_key(&link.to),
                        "Required premise {} is missing",
                        link.to
                    );
                    direct.insert(link.to.clone());
                }
            }
            Ok((card.id.clone(), direct))
        })
        .collect()
}

pub(super) fn capture(
    project: &Path,
    deck: &str,
    key: &str,
    ids: &[String],
    allow: &[Allowed],
) -> Result<(Scope, Packet)> {
    ensure!(
        ids.len() <= MAX_CARDS,
        "Blind review allows at most {MAX_CARDS} target cards"
    );
    ensure!(!allow.is_empty(), "Provide at least one permitted excerpt");
    let root = project.join(deck);
    let cfg = DeckConfig::read_in_workspace(&root, project)?;
    for (_, external) in cfg.external_roots(project, &root) {
        relative(project, &external?)?;
    }
    let graph = Graph::load_deck(&root, &cfg, project)?;
    let targets = targets(&graph, ids)?;
    let order = section_order(project, &root, &cfg, key)?;
    ensure!(
        order.iter().filter(|id| ids.contains(id)).eq(ids),
        "Blind review targets must belong to the selected section and follow publication order"
    );
    let mut dependencies = dependencies(&graph, &targets)?;
    let mut excerpts: Vec<Excerpt> = Vec::new();
    let mut excerpt_bytes = 0;
    let mut target_parts = BTreeMap::new();
    for (index, allowed) in allow.iter().enumerate() {
        let lines = allowed.lines;
        let label = format!("excerpt-{}", index + 1);
        let (card, part, role, file, text, license) = match &allowed.source {
            Selection::Card { card, part } => {
                let role = if ids.contains(card) {
                    Role::Target
                } else {
                    ensure!(
                        *part == Part::Statement,
                        "Card {card} is outside the targets and their direct premise statements"
                    );
                    Role::Premise
                };
                let selected = graph
                    .cards
                    .get(card)
                    .with_context(|| format!("Unknown review card {card}"))?;
                let (file, text, snapshot) = card_text(project, &root, selected, *part, lines)
                    .with_context(|| format!("Cannot capture {card} {}", part.name()))?;
                if role == Role::Target {
                    match target_parts.entry((card, part)) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(snapshot);
                        }
                        std::collections::btree_map::Entry::Occupied(entry) => {
                            ensure!(*entry.get() == snapshot, "Card part changed during capture");
                        }
                    }
                }
                (Some(card.clone()), Some(*part), role, file, text, None)
            }
            Selection::Material { key } => {
                let (file, text, license) = crate::references::ledger::blind_text(project, key)?;
                let text = exact_lines(&text, lines[0], lines[1])
                    .context("Invalid material excerpt line range")?;
                ensure!(!text.trim().is_empty(), "Material excerpt is empty");
                (
                    None,
                    None,
                    Role::Material,
                    file,
                    text.to_owned(),
                    Some(license),
                )
            }
        };
        ensure!(
            !excerpts.iter().any(|item| item.file == file
                && lines[0] <= item.lines[1]
                && item.lines[0] <= lines[1]),
            "Overlapping excerpts: {label}"
        );
        excerpt_bytes += text.len();
        ensure!(
            excerpt_bytes <= MAX_MESSAGE_BYTES,
            "Blind review message exceeds 32 KiB. Split the request"
        );
        excerpts.push(Excerpt {
            label,
            card,
            part,
            lines,
            role,
            file,
            sha256: sha256(text.as_bytes()),
            text,
            license,
        });
    }
    // Resolve labels only against the explicit excerpts before checking required premises.
    add_tex_dependencies(&excerpts, &graph, &mut dependencies)?;
    let has_part = |id: &str, part| {
        excerpts
            .iter()
            .any(|e| e.card.as_deref() == Some(id) && e.part == Some(part))
    };
    for target in &targets {
        ensure!(
            has_part(&target.id, Part::Statement),
            "Missing target statement for {}",
            target.id
        );
        if target.proof.is_some()
            || matches!(
                target.kind.as_str(),
                "theorem" | "lemma" | "proposition" | "corollary"
            )
        {
            ensure!(
                has_part(&target.id, Part::Proof),
                "Missing target proof for {}",
                target.id
            );
        }
        for premise in &dependencies[&target.id] {
            ensure!(
                has_part(premise, Part::Statement),
                "Missing required premise excerpt for {premise}"
            );
        }
    }
    let mut fingerprints = BTreeMap::new();
    let mut relevant_by_target = BTreeMap::new();
    for target in &targets {
        let mut relevant = BTreeSet::from([target.id.clone()]);
        let mut pending = vec![target.id.clone()];
        while let Some(id) = pending.pop() {
            for dependency in dependencies.get(&id).into_iter().flatten() {
                if relevant.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
        let evidence = excerpts
            .iter()
            .filter(|e| {
                e.role == Role::Material || e.card.as_ref().is_some_and(|id| relevant.contains(id))
            })
            .map(|e| (&e.card, &e.part, &e.file, e.lines, &e.sha256, &e.license))
            .collect::<Vec<_>>();
        fingerprints.insert(
            target.id.clone(),
            input_hash(&(SKILL_REVISION, CRITERIA, INSTRUCTIONS, &relevant, evidence))?,
        );
        relevant_by_target.insert(target.id.clone(), relevant);
    }
    for excerpt in excerpts
        .iter()
        .filter(|excerpt| excerpt.role == Role::Premise)
    {
        let card = excerpt.card.as_deref().expect("premise card");
        ensure!(
            relevant_by_target
                .values()
                .any(|relevant| relevant.contains(card)),
            "Card {card} is outside the targets and their premises"
        );
    }
    let reviewed_version = input_hash(&(
        key,
        ids,
        excerpts
            .iter()
            .filter(|e| e.role == Role::Target)
            .map(|e| (&e.card, &e.part, e.lines, &e.sha256))
            .collect::<Vec<_>>(),
    ))?;
    let scope = Scope {
        section: Some(key.to_owned()),
        cards: ids.to_vec(),
        reviewed_version,
    };
    let skill = Skill {
        names: vec!["reading-book-math-review".into(), "math-verifier".into()],
        revision: SKILL_REVISION.into(),
        sha256: sha256(CRITERIA.as_bytes()),
        text: CRITERIA.into(),
    };
    let message = message(&scope, &skill, &excerpts)?;
    let original_source = if targets.iter().any(|card| {
        card.source
            .as_deref()
            .and_then(|source| source.split_once('#'))
            .is_some_and(|(file, _)| file.starts_with("books/") && !root.join(file).is_file())
    }) {
        "unacquired"
    } else {
        "unverified"
    };
    Ok((
        scope,
        Packet {
            skill,
            excerpts,
            message,
            fingerprints,
            target_parts: target_parts.into_values().collect(),
            relevant: relevant_by_target,
            original_source_acquisition: original_source.into(),
            original_source_check: "unchecked".into(),
        },
    ))
}

fn add_tex_dependencies(
    excerpts: &[Excerpt],
    graph: &Graph,
    dependencies: &mut BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let mut labels: BTreeMap<String, BTreeSet<Option<&str>>> = BTreeMap::new();
    let mut references = Vec::new();
    for excerpt in excerpts {
        let mut symbols = crate::graph::review_symbols(&excerpt.file, &excerpt.text);
        ensure!(
            symbols.includes.is_empty(),
            "An excerpt contains a TeX input that is outside the packet"
        );
        let card = excerpt.card.as_deref();
        if let Some(id) = card.filter(|_| excerpt.part == Some(Part::Statement)) {
            symbols.labels.insert(format!("card:{id}"));
            symbols.labels.extend(graph.cards[id].label.iter().cloned());
        }
        for label in symbols.labels {
            labels.entry(label).or_default().insert(card);
        }
        references.extend(symbols.references.into_iter().map(|label| (card, label)));
    }
    for (card, label) in references {
        let owners = labels.get(&label).with_context(|| {
            format!("Required TeX reference {label} is missing from the packet")
        })?;
        ensure!(
            owners.len() == 1,
            "Ambiguous TeX reference {label} in the packet"
        );
        if let (Some(from), Some(&Some(to))) = (card, owners.first()) {
            if from != to {
                dependencies
                    .entry(from.to_owned())
                    .or_default()
                    .insert(to.to_owned());
            }
        }
    }
    Ok(())
}

fn scope_header(scope: &Scope) -> String {
    format!(
        "Scope: {}",
        serde_json::json!({
            "cards": scope.cards, "reviewed_version": scope.reviewed_version,
        })
    )
}

fn message(scope: &Scope, skill: &Skill, excerpts: &[Excerpt]) -> Result<String> {
    use std::fmt::Write as _;
    let mut message = format!("{INSTRUCTIONS}\n{}\n{}\n", skill.text, scope_header(scope));
    // Field order is fixed independently of serde_json's map features.
    #[derive(Serialize)]
    struct Header<'a> {
        bytes: usize,
        card: Option<&'a str>,
        label: &'a str,
        lines: [usize; 2],
        part: Option<Part>,
        role: Role,
    }
    for excerpt in excerpts {
        let header = Header {
            bytes: excerpt.text.len(),
            card: excerpt.card.as_deref(),
            label: &excerpt.label,
            lines: excerpt.lines,
            part: excerpt.part,
            role: excerpt.role,
        };
        writeln!(message, "\nExcerpt: {}", serde_json::to_string(&header)?)?;
        message.push_str(&excerpt.text);
        message.push('\n');
    }
    ensure!(
        message.len() <= MAX_MESSAGE_BYTES,
        "Blind review message exceeds 32 KiB. Split the request"
    );
    Ok(message)
}

pub(super) fn affected(
    app: &Workspace,
    first: &Receipt,
    findings: &[&report::Finding],
    declared: &[String],
) -> Result<workflow::Affected> {
    let project = app.project_dir.canonicalize()?;
    let root = project.join(&first.deck);
    let cfg = DeckConfig::read_in_workspace(&root, &project)?;
    let graph = Graph::load_deck(&root, &cfg, &project)?;
    let order: BTreeSet<_> = section_order(
        &project,
        &root,
        &cfg,
        first.scope.section.as_deref().context("Missing section")?,
    )?
    .into_iter()
    .collect();
    let mut users = BTreeMap::<&str, BTreeSet<&str>>::new();
    let mut labels = BTreeMap::<String, BTreeSet<&str>>::new();
    let mut references = BTreeMap::<&str, BTreeSet<String>>::new();
    for card in graph.cards.values() {
        for link in &card.links {
            if FOLLOW_RELS.contains(&link.rel.as_str()) {
                users.entry(&link.to).or_default().insert(&card.id);
            }
        }
        if let Some(label) = &card.label {
            labels.entry(label.clone()).or_default().insert(&card.id);
        }
        for (part, field) in [
            ("statement.tex", &card.statement),
            ("proof.tex", &card.proof),
        ] {
            if let Some(field) = field {
                let symbols = crate::graph::review_symbols(Path::new(part), &field.text);
                for label in symbols.labels {
                    labels.entry(label).or_default().insert(&card.id);
                }
                references
                    .entry(&card.id)
                    .or_default()
                    .extend(symbols.references);
            }
        }
    }
    for (user, references) in references {
        for dependency in references
            .iter()
            .filter_map(|label| labels.get(label))
            .flatten()
        {
            users.entry(dependency).or_default().insert(user);
        }
    }
    let mut affected = workflow::Affected {
        by_finding: BTreeMap::new(),
        outside_section: BTreeMap::new(),
    };
    for finding in findings {
        let mut uses = BTreeSet::from([finding.card.as_str()]);
        let mut pending = vec![finding.card.as_str()];
        for use_id in finding.impacts.iter().chain(declared) {
            ensure!(
                !use_id.trim().is_empty(),
                "Affected use requires a card or a location"
            );
            if uses.insert(use_id.as_str()) {
                pending.push(use_id);
            }
        }
        while let Some(card) = pending.pop() {
            for &user in users.get(card).into_iter().flatten() {
                if uses.insert(user) {
                    pending.push(user);
                }
            }
        }
        let mut within = BTreeSet::new();
        for card in uses {
            if order.contains(card) && graph.cards.get(card).is_some_and(|card| !card.external) {
                within.insert(card.to_owned());
            } else {
                let path = graph
                    .cards
                    .get(card)
                    .filter(|card| !card.external)
                    .map(|card| Path::new(&first.deck).join(&card.path));
                affected.outside_section.insert(card.to_owned(), path);
            }
        }
        ensure!(
            within.contains(&finding.card),
            "The accepted finding's card is no longer in its section"
        );
        affected.by_finding.insert(finding.id.clone(), within);
    }
    Ok(affected)
}
