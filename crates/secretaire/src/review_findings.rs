//! Per-deck findings recorded by hand, with independent evidence and lifecycle history.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{
    ReviewFinding, ReviewFindingAssessment as Assessment, ReviewFindingChange as Change,
    ReviewFindingChangeReference as ChangeReference, ReviewFindingContent as FindingContent,
    ReviewFindingLocation as Location, ReviewFindingStatus as Status,
    ReviewFindingTransition as Transition, ReviewFindingTransitionAction as TransitionAction,
    ReviewFindingsOutcome, ReviewFindingsSnapshot, ServerEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{references::write_atomic, workspace, Workspace};

pub(crate) const FILE: &str = "review-findings.toml";
const VERSION: u32 = 1;
const UUID_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
static UUID: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(UUID_PATTERN).expect("valid UUID pattern"));

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    #[serde(default)]
    findings: Vec<Finding>,
}

#[derive(Deserialize, Serialize)]
struct Finding {
    id: String,
    #[serde(flatten)]
    content: FindingContent,
    transitions: Vec<Transition>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListArgs {
    deck: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordArgs {
    pub(crate) deck: Option<String>,
    pub(crate) id: String,
    pub(crate) actor: String,
    pub(crate) finding: FindingContent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateArgs {
    pub(crate) deck: Option<String>,
    pub(crate) id: String,
    pub(crate) expected_revision: String,
    pub(crate) finding: Option<FindingContent>,
    pub(crate) transition: Option<TransitionAction>,
}

pub(crate) enum Request {
    List(ListArgs),
    Record(RecordArgs),
    Update(UpdateArgs),
}

impl Request {
    fn deck(&self) -> Option<&str> {
        match self {
            Self::List(args) => args.deck.as_deref(),
            Self::Record(args) => args.deck.as_deref(),
            Self::Update(args) => args.deck.as_deref(),
        }
    }
}

pub(crate) async fn run(app: Arc<Workspace>, request: Request) -> Result<Value> {
    let deck = request
        .deck()
        .map(str::to_owned)
        .unwrap_or_else(|| app.current_deck());
    let guard = match &request {
        Request::List(_) => None,
        _ => Some(app.file_save.clone().lock_owned().await),
    };
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        run_locked(&app, &deck, request)
    })
    .await?
}

// The section workflow holds file_save across receipt verification and the ledger mutation.
pub(crate) fn run_locked(app: &Workspace, deck: &str, request: Request) -> Result<Value> {
    let store = Store::open(&app.project_dir, deck)?;
    let mut ledger = store.read()?;
    let result = match request {
        Request::List(_) => return store.list(&ledger),
        Request::Record(args) => store.record(&mut ledger, args),
        Request::Update(args) => store.update(&mut ledger, args),
    }?;
    app.hub
        .publish_snapshot(ServerEvent::FileChanged { path: store.path });
    Ok(result)
}

pub(crate) fn read_one(
    app: &Workspace,
    deck: &str,
    id: &str,
) -> Result<Option<(ReviewFinding, String)>> {
    let store = Store::open(&app.project_dir, deck)?;
    let ledger = store.read()?;
    ledger
        .findings
        .iter()
        .find(|finding| finding.id == id)
        .map(|finding| Ok((finding.desktop_view(), finding.revision()?)))
        .transpose()
}

pub(crate) fn read_result(app: &Workspace, deck: &str, id: &str) -> Result<Value> {
    let store = Store::open(&app.project_dir, deck)?;
    let ledger = store.read()?;
    store.result(
        ledger
            .findings
            .iter()
            .find(|finding| finding.id == id)
            .context("Finding disappeared")?,
    )
}

pub(crate) async fn snapshot(project: &Path, deck: &str) -> ServerEvent {
    let deck = deck.replace('\\', "/");
    let requested = deck.clone();
    let project = project.to_owned();
    let read = tokio::task::spawn_blocking(move || {
        let ledger = Store::open(&project, &requested)?.read()?;
        let (open_count, card_open_counts) = ledger.open_counts();
        Ok::<_, anyhow::Error>(ReviewFindingsSnapshot {
            open_count,
            card_open_counts,
            findings: ledger
                .ordered()
                .into_iter()
                .map(Finding::desktop_view)
                .collect(),
        })
    })
    .await;
    let outcome = match read
        .context("Could not load review findings")
        .and_then(|result| result)
    {
        Ok(snapshot) => ReviewFindingsOutcome::Loaded { snapshot },
        Err(error) => ReviewFindingsOutcome::Error {
            message: format!("{error:#}"),
        },
    };
    ServerEvent::ReviewFindings { deck, outcome }
}

pub(crate) fn matches_deck_file(deck: &str, path: &str) -> bool {
    match path.rsplit_once('/') {
        Some((parent, name)) => name == FILE && parent == deck,
        None => path == FILE && (deck.is_empty() || deck == "."),
    }
}

impl Ledger {
    fn ordered(&self) -> Vec<&Finding> {
        let mut findings: Vec<_> = self.findings.iter().collect();
        findings.sort_by_key(|finding| (finding.content.severity, &finding.id));
        findings
    }

    fn open_counts(&self) -> (usize, BTreeMap<String, usize>) {
        let mut open_count = 0;
        let mut card_open_counts = BTreeMap::new();
        for finding in self
            .findings
            .iter()
            .filter(|finding| finding.status() == Status::Open)
        {
            open_count += 1;
            let cards: BTreeSet<_> = finding
                .content
                .locations
                .iter()
                .filter_map(|location| location.card.as_ref())
                .collect();
            for card in cards {
                *card_open_counts.entry(card.clone()).or_insert(0) += 1;
            }
        }
        (open_count, card_open_counts)
    }
}

struct Store {
    root: PathBuf,
    deck: String,
    path: String,
}

impl Store {
    fn open(project: &Path, requested: &str) -> Result<Self> {
        let requested = if requested.is_empty() { "." } else { requested };
        ensure!(
            requested == "." || relative_path(requested),
            "Deck must be a workspace-relative path: {requested}"
        );
        let (root, deck) = workspace::resolve_deck(project, Path::new(requested))?;
        let (deck, path) = if deck.is_empty() {
            (".".into(), FILE.into())
        } else {
            let path = format!("{deck}/{FILE}");
            (deck, path)
        };
        Ok(Self { root, deck, path })
    }

    fn read(&self) -> Result<Ledger> {
        let file = self.root.join(FILE);
        match fs::symlink_metadata(&file) {
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(Ledger {
                    version: VERSION,
                    findings: Vec::new(),
                });
            }
            result => ensure!(result?.is_file(), "{} must be a regular file", self.path),
        }
        let read = || -> Result<Ledger> {
            let ledger = toml::from_str::<Ledger>(&fs::read_to_string(file)?)?;
            ensure!(
                ledger.version == VERSION,
                "Unsupported ledger version: {}",
                ledger.version
            );
            let mut ids = BTreeSet::new();
            for finding in &ledger.findings {
                ensure!(
                    ids.insert(&finding.id),
                    "Duplicate finding ID: {}",
                    finding.id
                );
                finding
                    .validate()
                    .with_context(|| format!("Finding {}", finding.id))?;
            }
            Ok(ledger)
        };
        read().with_context(|| format!("Could not read {}", self.path))
    }

    fn save(&self, ledger: &Ledger) -> Result<()> {
        write_atomic(&self.root, FILE, toml::to_string_pretty(ledger)?.as_bytes())
            .with_context(|| format!("Could not save {}", self.path))
    }

    fn list(&self, ledger: &Ledger) -> Result<Value> {
        let (open_count, card_open_counts) = ledger.open_counts();
        let findings = ledger
            .ordered()
            .into_iter()
            .map(Finding::view)
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "deck": self.deck, "path": self.path, "open_count": open_count,
            "card_open_counts": card_open_counts, "findings": findings,
        }))
    }

    fn record(&self, ledger: &mut Ledger, args: RecordArgs) -> Result<Value> {
        let RecordArgs {
            id,
            actor,
            finding: content,
            ..
        } = args;
        if let Some(existing) = ledger.findings.iter().find(|finding| finding.id == id) {
            ensure!(
                existing.content == content && existing.transitions[0].action.actor == actor,
                "Finding {id} already exists with different content or recorder"
            );
            return self.result(existing);
        }
        let action = TransitionAction {
            status: Status::Open,
            actor,
            reason: None,
            changes: Vec::new(),
        };
        let finding = Finding {
            id,
            content,
            transitions: vec![new_transition(action)],
        };
        finding.validate()?;
        let result = self.result(&finding)?;
        ledger.findings.push(finding);
        self.save(ledger)?;
        Ok(result)
    }

    fn update(&self, ledger: &mut Ledger, args: UpdateArgs) -> Result<Value> {
        let UpdateArgs {
            id,
            expected_revision,
            finding: content,
            transition,
            ..
        } = args;
        ensure!(
            content.is_some() || transition.is_some(),
            "Supply a finding or a transition"
        );
        let current = ledger
            .findings
            .iter_mut()
            .find(|finding| finding.id == id)
            .with_context(|| format!("Unknown finding ID: {id}"))?;
        ensure!(
            current.revision()? == expected_revision,
            "Finding {id} changed. List the findings and try again with its current revision."
        );
        if let Some(content) = content {
            current.content = content;
        }
        if let Some(action) = transition {
            current.transitions.push(new_transition(action));
        }
        current.validate()?;
        let result = self.result(current)?;
        self.save(ledger)?;
        Ok(result)
    }

    fn result(&self, finding: &Finding) -> Result<Value> {
        Ok(json!({ "deck": self.deck, "path": self.path, "finding": finding.view()? }))
    }
}

impl Finding {
    fn desktop_view(&self) -> ReviewFinding {
        ReviewFinding {
            id: self.id.clone(),
            content: self.content.clone(),
            status: self.status(),
            transitions: self.transitions.clone(),
        }
    }

    fn status(&self) -> Status {
        self.transitions
            .last()
            .expect("validated history")
            .action
            .status
    }

    fn revision(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }

    fn view(&self) -> Result<Value> {
        let mut view = serde_json::to_value(self)?;
        view["status"] = json!(self.status());
        view["revision"] = json!(self.revision()?);
        Ok(view)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            UUID.is_match(&self.id),
            "Finding ID must be a lowercase UUID"
        );
        let content = &self.content;
        nonblank(&content.summary, "Summary")?;
        nonblank(&content.detail, "Detail")?;
        validate_locations(&content.locations)?;
        ensure!(!content.sources.is_empty(), "Supply at least one source");
        for source in &content.sources {
            ensure!(
                source.finding.is_none() || source.review.as_deref().is_some_and(present),
                "A source finding ID requires its review identifier"
            );
        }
        ensure!(
            content.assessment != Some(Assessment::Confirmed)
                || content.evidence.as_deref().is_some_and(present),
            "Confirmed assessment requires evidence"
        );
        ensure!(!self.transitions.is_empty(), "Finding history is empty");
        let mut previous = None;
        for transition in &self.transitions {
            validate_transition(&transition.action, previous)?;
            previous = Some(transition.action.status);
        }
        Ok(())
    }
}

fn new_transition(action: TransitionAction) -> Transition {
    Transition {
        time_ms: chrono::Utc::now().timestamp_millis(),
        action,
    }
}

fn validate_transition(action: &TransitionAction, previous: Option<Status>) -> Result<()> {
    nonblank(&action.actor, "Recorder")?;
    match (previous, action.status) {
        (None, Status::Open) => {}
        (Some(Status::Open), Status::Fixed) => {
            ensure!(
                !action.changes.is_empty(),
                "Fixed requires a commit or card-edit reference"
            );
            for change in &action.changes {
                validate_change(change)?;
            }
        }
        (Some(Status::Open), Status::Deferred)
        | (Some(Status::Fixed | Status::Deferred), Status::Open) => {
            ensure!(
                action.reason.as_deref().is_some_and(present),
                "Deferring or reopening requires a reason"
            );
        }
        _ => bail!("Invalid finding status transition"),
    }
    ensure!(
        action.status == Status::Fixed || action.changes.is_empty(),
        "Closing changes belong on the fixed transition"
    );
    Ok(())
}

fn validate_change(change: &Change) -> Result<()> {
    nonblank(&change.summary, "Change summary")?;
    validate_locations(&change.locations)?;
    match &change.reference {
        ChangeReference::Commit { oid } => ensure!(
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Commit reference must be a full Git OID",
        ),
        ChangeReference::CardEdit { record, card } => {
            nonblank(record, "Card-edit record ID")?;
            nonblank(card, "Card ID")?;
        }
    }
    Ok(())
}

fn validate_locations(locations: &[Location]) -> Result<()> {
    ensure!(!locations.is_empty(), "Supply at least one location");
    for location in locations {
        ensure!(
            location.card.is_some() || location.file.is_some(),
            "Location requires a card or file"
        );
        if let Some(card) = &location.card {
            nonblank(card, "Card ID")?;
        }
        if let Some(file) = &location.file {
            ensure!(
                relative_path(file),
                "Finding file must be a workspace-relative path: {file}"
            );
        }
        ensure!(
            location.file.is_some() || (location.lines.is_none() && location.revision.is_none()),
            "Lines and revision require a file"
        );
        if let Some([start, end]) = location.lines {
            ensure!(
                start > 0 && start <= end,
                "Use a one-based, inclusive line range"
            );
        }
        if let Some(revision) = &location.revision {
            nonblank(revision, "File revision")?;
        }
    }
    Ok(())
}

fn relative_path(path: &str) -> bool {
    !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn present(value: &str) -> bool {
    !value.trim().is_empty()
}

fn nonblank(value: &str, field: &str) -> Result<()> {
    ensure!(present(value), "{field} must not be blank");
    Ok(())
}

pub(crate) fn id_schema() -> Value {
    json!({"type": "string", "pattern": UUID_PATTERN})
}

fn location_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false,
        "anyOf": [{"required": ["card"]}, {"required": ["file"]}],
        "properties": {
            "card": {"type": "string", "description": "Card ID in the owning deck."},
            "file": {"type": "string", "description": "Workspace-relative file path."},
            "lines": {"type": "array", "minItems": 2, "maxItems": 2,
                "items": {"type": "integer", "minimum": 1}, "description": "Inclusive start and end lines."},
            "revision": {"type": "string", "description": "File revision at the time of review."},
        }
    })
}

pub(crate) fn finding_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false,
        "required": ["summary", "detail", "severity", "locations", "sources"],
        "properties": {
            "summary": {"type": "string"},
            "detail": {"type": "string"},
            "severity": {"type": "string", "enum": ["fatal", "major", "minor", "form"]},
            "assessment": {"type": "string", "enum": ["confirmed", "suspected", "unchecked"]},
            "evidence": {"type": "string", "description": "Required for a confirmed assessment."},
            "locations": {"type": "array", "minItems": 1, "items": location_schema()},
            "sources": {"type": "array", "minItems": 1, "items": {
                "type": "object", "additionalProperties": false, "required": ["kind"],
                "properties": {
                    "kind": {"type": "string", "enum": ["human", "agent"]},
                    "reviewer": {"type": "string"}, "perspective": {"type": "string"},
                    "thread": {"type": "string"}, "review": {"type": "string"},
                    "finding": {"type": "string", "description": "Original finding ID within the named review."},
                }
            }},
        }
    })
}

pub(crate) fn transition_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["actor", "status"],
        "properties": {
            "actor": {"type": "string", "description": "Person or agent recording this transition."},
            "status": {"type": "string", "enum": ["open", "fixed", "deferred"]},
            "reason": {"type": "string", "description": "Required when deferring or reopening."},
            "changes": {"type": "array", "minItems": 1, "items": {
                "type": "object", "additionalProperties": false,
                "required": ["summary", "locations", "reference"],
                "properties": {
                    "summary": {"type": "string"},
                    "locations": {"type": "array", "minItems": 1, "items": location_schema()},
                    "reference": {"type": "object", "oneOf": [
                        {"additionalProperties": false, "required": ["kind", "oid"], "properties": {
                            "kind": {"const": "commit"},
                            "oid": {"type": "string", "pattern": "^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"}
                        }},
                        {"additionalProperties": false, "required": ["kind", "record", "card"], "properties": {
                            "kind": {"const": "card_edit"}, "record": {"type": "string"}, "card": {"type": "string"}
                        }}
                    ]}
                }
            }},
        }
    })
}
