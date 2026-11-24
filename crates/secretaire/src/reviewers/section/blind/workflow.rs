use fuzukue_proto::{
    ReviewFindingContent as Content, ReviewFindingLocation as Location, ReviewFindingSeverity,
    ReviewFindingSource as Source, ReviewFindingSourceKind, ReviewFindingStatus as Status,
};
use report::{Coverage, Finding, Severity};
use sources::Role;

use crate::review_findings::{self as ledger, RecordArgs, UpdateArgs};

use super::*;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Workflow {
    pub(super) decisions: BTreeMap<String, Decision>,
    #[serde(default)]
    pub(super) previous_decisions: Vec<(String, Decision)>,
    pub(super) repairs: BTreeMap<String, editor::Repair>,
    pub(super) stop: Option<editor::Stop>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Decision {
    pub(super) editor: String,
    pub(super) time: i64,
    pub(super) reason: String,
    pub(super) reviewed_version: String,
    pub(super) input_hash: String,
    pub(super) action: DecisionAction,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub(super) enum DecisionAction {
    Accept {
        id: String,
        content: Content,
        append: Option<Append>,
        registered: bool,
    },
    Reject,
    Hold,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Append {
    pub(super) id: String,
    pub(super) expected_revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Register {
    session: String,
    finding: String,
}

fn path(app: &Workspace, first: &Receipt) -> PathBuf {
    app.state_dir
        .join("section-review-workflows")
        .join(format!("{}.json", first.review.session))
}

impl Workflow {
    pub(super) fn read(app: &Workspace, first: &Receipt) -> Result<Self> {
        match fs::File::open(path(app, first)) {
            Ok(file) => Ok(serde_json::from_reader(BufReader::new(file))?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn save(&self, app: &Workspace, first: &Receipt) -> Result<()> {
        persist_json(&path(app, first), self, true)
    }
}

pub(super) fn initial(app: &Workspace, session: &str, origin: Option<&str>) -> Result<Receipt> {
    let first = Receipt::read(&app.state_dir, session)?.context("Initial review was not found")?;
    let run = first.blind.as_ref().context("Expected a blind review")?;
    ensure!(run.round == 1, "Use the initial review session");
    ensure!(
        origin.is_none_or(|origin| run.origin == origin),
        "This blind review belongs to another front desk thread"
    );
    settle_interrupted(app, first)
}

pub(super) fn recheck_session(first: &Receipt) -> String {
    format!("{}-recheck", first.review.session)
}

pub(super) fn recheck(app: &Workspace, first: &Receipt) -> Result<Option<Receipt>> {
    Receipt::read(&app.state_dir, &recheck_session(first))?
        .map(|receipt| settle_interrupted(app, receipt))
        .transpose()
}

pub(super) fn complete_coverage(receipt: &Receipt) -> Result<&Coverage> {
    let run = receipt.blind.as_ref().context("Expected a blind review")?;
    ensure!(
        run.stage == Stage::Reviewed && run.is_complete(),
        "Review requires a successful response with complete target coverage"
    );
    run.coverage.as_ref().context("The review has no coverage")
}

pub(super) fn candidate<'a>(first: &'a Receipt, id: &str) -> Result<&'a Finding> {
    first
        .blind
        .as_ref()
        .and_then(|run| run.coverage.as_ref())
        .context("No captured findings")?
        .findings
        .iter()
        .find(|finding| finding.id == id)
        .context("Finding was not in the captured response")
}

pub(super) fn ledger_location(
    card: &str,
    version: &str,
    excerpt: &sources::Excerpt,
    lines: [usize; 2],
) -> Result<Location> {
    Ok(Location {
        card: Some(card.into()),
        file: Some(
            excerpt
                .file
                .to_str()
                .context("Finding file is not UTF-8")?
                .into(),
        ),
        lines: Some([lines[0].try_into()?, lines[1].try_into()?]),
        revision: Some(version.into()),
    })
}

pub(super) fn content(first: &Receipt, finding: &Finding) -> Result<Content> {
    let run = first.blind.as_ref().context("Expected a blind review")?;
    let packet = run
        .packet
        .as_ref()
        .context("Review input was not captured")?;
    let excerpt = report::location(packet, &finding.location)?;
    let criterion = serde_json::to_value(finding.criterion)?;
    Ok(Content {
        summary: finding.repair.clone(),
        detail: format!("{}\n\nReviewed quote: {}", finding.reasoning, finding.quote),
        severity: match finding.severity {
            Severity::Critical => ReviewFindingSeverity::Fatal,
            Severity::Major => ReviewFindingSeverity::Major,
            Severity::Minor => ReviewFindingSeverity::Minor,
        },
        assessment: None,
        evidence: Some(finding.reasoning.clone()),
        locations: vec![ledger_location(
            &finding.card,
            &first.scope.reviewed_version,
            excerpt,
            finding.location.lines,
        )?],
        sources: vec![Source {
            kind: ReviewFindingSourceKind::Agent,
            reviewer: run
                .resolved_model
                .clone()
                .or_else(|| run.request.model.clone()),
            perspective: Some(format!(
                "reading-book-math-review / math-verifier / {}",
                criterion.as_str().expect("criterion name")
            )),
            thread: Some(run.origin.clone()),
            review: Some(first.review.session.clone()),
            finding: Some(finding.id.clone()),
        }],
    })
}

pub(super) fn finding_id(first: &Receipt, finding: &str) -> Result<String> {
    // Derive an RFC 9562 version 8 UUID from this review and candidate.
    let digest = input_hash(&(&first.review.session, finding))?;
    let hex = digest.strip_prefix("sha256:").expect("SHA-256 prefix");
    let variant = b"89ab"[usize::from(hex.as_bytes()[16] & 3)] as char;
    Ok(format!(
        "{}-{}-8{}-{variant}{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    ))
}

pub(super) fn same_content(actual: &Content, approved: &Content) -> bool {
    let mut actual = actual.clone();
    actual.sources.clone_from(&approved.sources);
    actual == *approved
}

pub(crate) async fn register(app: &Arc<Workspace>, origin: &str, args: Register) -> Result<Value> {
    let work = app.admit_work()?;
    let save = app.file_save.clone().lock_owned().await;
    let (app, origin) = (app.clone(), origin.to_owned());
    tokio::task::spawn_blocking(move || {
        let (_work, _save) = (work, save);
        let first = initial(&app, &args.session, Some(&origin))?;
        let mut workflow = Workflow::read(&app, &first)?;
        let decision = workflow
            .decisions
            .get_mut(&args.finding)
            .context("Finding requires explicit editor approval")?;
        let DecisionAction::Accept {
            id,
            content,
            append,
            registered,
        } = &mut decision.action
        else {
            bail!("Finding requires explicit editor approval");
        };
        ensure!(
            decision.reviewed_version == first.scope.reviewed_version
                && decision.input_hash == first.review.input_hash,
            "Approval does not match the captured receipt"
        );
        let source = content.sources.last().context("Missing approved source")?;
        let all = ledger::run_locked(
            &app,
            &first.deck,
            ledger::Request::List(ledger::ListArgs::default()),
        )?;
        let already_saved = all["findings"]
            .as_array()
            .context("Invalid ledger listing")?
            .iter()
            .find(|row| {
                row["sources"].as_array().is_some_and(|sources| {
                    sources.iter().any(|old| {
                        old["review"] == source.review.as_deref().unwrap_or_default()
                            && old["finding"] == args.finding
                    })
                })
            });
        let result = if let Some(row) = already_saved {
            ensure!(
                row["id"] == *id,
                "This source already belongs to another finding"
            );
            if !*registered {
                let (actual, _) =
                    ledger::read_one(&app, &first.deck, id)?.context("Finding disappeared")?;
                ensure!(
                    &actual.content == content
                        && actual.status == Status::Open
                        && (append.is_some()
                            || actual.transitions[0].action.actor == decision.editor),
                    "Existing registration differs from the approved write"
                );
            }
            json!({"deck": first.deck, "path": all["path"], "finding": row})
        } else {
            ensure!(
                !*registered,
                "The registered source changed in the findings ledger"
            );
            ensure!(workflow.stop.is_none(), "This review workflow has ended");
            complete_coverage(&first)?;
            candidate(&first, &args.finding)?;
            require_current(&app, &first)?;
            let request = match append {
                Some(append) => ledger::Request::Update(UpdateArgs {
                    deck: None,
                    id: id.clone(),
                    expected_revision: append.expected_revision.clone(),
                    finding: Some(content.clone()),
                    transition: None,
                }),
                None => ledger::Request::Record(RecordArgs {
                    deck: None,
                    id: id.clone(),
                    actor: decision.editor.clone(),
                    finding: content.clone(),
                }),
            };
            ledger::run_locked(&app, &first.deck, request)?
        };
        if !*registered {
            *registered = true;
            workflow.save(&app, &first)?;
        }
        Ok(result)
    })
    .await?
}

#[derive(Serialize, Deserialize)]
pub(super) struct Affected {
    pub(super) by_finding: BTreeMap<String, BTreeSet<String>>,
    pub(super) outside_section: BTreeMap<String, Option<PathBuf>>,
}

pub(super) fn prepare_recheck(
    app: &Workspace,
    first: &Receipt,
    request: &Request,
    link: &RecheckRequest,
) -> Result<Recheck> {
    let selection = request.selection()?;
    let run = first.blind.as_ref().expect("initial blind receipt");
    let (_, deck) = resolve_deck(&app.project_dir.canonicalize()?, Path::new(selection.deck))?;
    ensure!(
        first.deck == deck
            && first.scope.section.as_deref() == Some(selection.section)
            && run.request.backend == Some(selection.backend)
            && run.request.model.as_deref() == Some(selection.model),
        "Recheck must use the initial deck, section, backend and model"
    );
    ensure!(
        full_oid(&link.commit),
        "Recheck requires a full lowercase Git commit OID"
    );
    let workflow = Workflow::read(app, first)?;
    ensure!(workflow.stop.is_none(), "This review workflow has ended");
    let coverage = complete_coverage(first)?;
    let mut accepted = Vec::new();
    for finding in &coverage.findings {
        match workflow
            .decisions
            .get(&finding.id)
            .map(|decision| &decision.action)
        {
            Some(DecisionAction::Reject) => {}
            Some(DecisionAction::Accept {
                id,
                registered: true,
                content,
                ..
            }) => {
                let (actual, _) = ledger::read_one(app, &first.deck, id)?
                    .context("An approved finding is missing from the ledger")?;
                ensure!(
                    same_content(&actual.content, content)
                        && content
                            .sources
                            .iter()
                            .all(|source| actual.content.sources.contains(source)),
                    "An approved finding changed in the ledger"
                );
                if !workflow
                    .repairs
                    .get(&finding.id)
                    .is_some_and(|repair| repair.completed)
                {
                    ensure!(
                        actual.status == Status::Open,
                        "An approved finding is no longer open"
                    );
                    accepted.push(finding);
                }
            }
            _ => bail!(
                "Approval target unmet: decide each candidate and register each accepted finding"
            ),
        }
    }
    ensure!(
        !accepted.is_empty(),
        "There are no accepted findings to recheck"
    );
    let affected = sources::affected(app, first, &accepted, &link.affected_uses)?;
    let mut required: BTreeSet<_> = first.scope.cards.iter().collect();
    required.extend(affected.by_finding.values().flatten());
    ensure!(
        selection.cards.iter().collect::<BTreeSet<_>>() == required,
        "Recheck must include the initial targets and all affected uses within the section"
    );
    Ok(Recheck {
        initial_session: first.review.session.clone(),
        commit: link.commit.clone(),
        affected,
    })
}

fn full_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) async fn verify_commit(app: &Workspace, packet: &Packet, oid: &str) -> Result<()> {
    let git = crate::git::Repo::open(&app.project_dir).await?;
    ensure!(
        git.git(&["rev-parse", "--verify", "HEAD^{commit}"])
            .await?
            .trim_end()
            == oid,
        "The full recheck commit OID must match the current checkout"
    );
    let mut files = BTreeMap::<&Path, Vec<_>>::new();
    for excerpt in packet.excerpts.iter().filter(|e| e.role != Role::Material) {
        files.entry(&excerpt.file).or_default().push(excerpt);
    }
    const MAX_BLOB: usize = 4 * 1024 * 1024;
    for (path, excerpts) in files {
        let file = path.to_str().context("Review source path is not UTF-8")?;
        let object = format!("{oid}:{file}");
        let blob = crate::git::run(
            &app.project_dir,
            &["--no-replace-objects", "cat-file", "blob", &object],
            MAX_BLOB,
        )
        .await?;
        ensure!(
            blob.status.success(),
            "The recheck commit has no source blob for {file}"
        );
        ensure!(
            blob.stdout.len() <= MAX_BLOB,
            "Review source is too large to verify against Git"
        );
        if excerpts.iter().any(|excerpt| excerpt.role == Role::Target) {
            use std::io::Read as _;
            let source =
                crate::mcp::workspace_read::open_regular_source(&app.project_dir.join(path))?;
            let mut current = Vec::new();
            source
                .take((MAX_BLOB + 1) as u64)
                .read_to_end(&mut current)?;
            ensure!(
                current == blob.stdout,
                "Target source {file} has changes outside the recheck commit"
            );
        }
        let text = std::str::from_utf8(&blob.stdout)?;
        for excerpt in excerpts {
            ensure!(
                sources::exact_lines(text, excerpt.lines[0], excerpt.lines[1])
                    == Some(&excerpt.text),
                "Reviewed lines in {file} do not match the recheck commit"
            );
        }
    }
    Ok(())
}

pub(super) async fn verify_new_packet(
    app: &Workspace,
    first: &Receipt,
    scope: &Scope,
    packet: &Packet,
    recheck: &Recheck,
) -> Result<()> {
    let old = first
        .blind
        .as_ref()
        .and_then(|run| run.packet.as_ref())
        .context("Initial input is missing")?;
    ensure!(
        first.scope.reviewed_version != scope.reviewed_version,
        "Recheck requires a new target version"
    );
    for excerpt in old
        .excerpts
        .iter()
        .filter(|excerpt| excerpt.role == Role::Target)
    {
        ensure!(
            packet
                .target_parts
                .iter()
                .any(|part| excerpt.card.as_ref() == Some(&part.card)
                    && excerpt.part == Some(part.part)),
            "Recheck omits an initially reviewed target part"
        );
    }
    for part in &packet.target_parts {
        let previous = old
            .target_parts
            .iter()
            .find(|old| old.card == part.card && old.part == part.part);
        let covered = |line| {
            packet.excerpts.iter().any(|new| {
                new.card.as_ref() == Some(&part.card)
                    && new.part == Some(part.part)
                    && new.file == part.file
                    && (new.lines[0]..=new.lines[1]).contains(&line)
            })
        };
        let was_reviewed = |line| {
            old.excerpts.iter().any(|excerpt| {
                excerpt.card.as_ref() == Some(&part.card)
                    && excerpt.part == Some(part.part)
                    && (excerpt.lines[0]..=excerpt.lines[1]).contains(&line)
            })
        };
        let (prefix, suffix) = previous.map_or((0, 0), |old| {
            let prefix = old
                .lines
                .iter()
                .zip(&part.lines)
                .take_while(|(old, new)| old == new)
                .count();
            let suffix = old.lines[prefix..]
                .iter()
                .rev()
                .zip(part.lines[prefix..].iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            (prefix, suffix)
        });
        for idx in 0..part.lines.len() {
            let unchanged = previous.and_then(|old| {
                if idx < prefix {
                    Some(old.first_line + idx)
                } else if idx >= part.lines.len() - suffix {
                    Some(old.first_line + old.lines.len() - (part.lines.len() - idx))
                } else {
                    None
                }
            });
            let line = part.first_line + idx;
            ensure!(
                unchanged.is_some_and(|before| !was_reviewed(before)) || covered(line),
                "Recheck omits changed or initially reviewed line {line} in {}",
                part.file.display()
            );
        }
    }
    verify_commit(app, packet, &recheck.commit).await
}

pub(super) fn view(app: &Workspace, receipt: &Receipt, fresh_complete: bool) -> Result<Value> {
    let run = receipt.blind.as_ref().expect("blind receipt");
    let first = run
        .recheck
        .as_ref()
        .map(|link| initial(app, &link.initial_session, None))
        .transpose()?;
    let first = first.as_ref().unwrap_or(receipt);
    let linked = if run.round == 1 {
        recheck(app, first)?
    } else {
        None
    };
    let checked = if run.round == 2 {
        Some(receipt)
    } else {
        linked.as_ref()
    };
    let first_run = first.blind.as_ref().expect("initial receipt");
    let checked_run = checked.and_then(|receipt| receipt.blind.as_ref());
    let state = Workflow::read(app, first)?;
    let mut candidates = Vec::new();
    let mut approval_unmet = false;
    let mut pending_repairs = false;
    let mut serious = false;
    let mut serious_deferred = false;
    let first_findings = first_run
        .coverage
        .iter()
        .flat_map(|coverage| &coverage.findings);
    for finding in first_findings {
        let decision = state.decisions.get(&finding.id);
        let repair = state.repairs.get(&finding.id);
        if repair.is_some_and(|repair| repair.completed && repair.action.status == Status::Fixed) {
            continue;
        }
        let mut ledger_id = None;
        match decision.map(|decision| &decision.action) {
            Some(DecisionAction::Reject) => continue,
            Some(DecisionAction::Accept {
                id,
                registered: true,
                ..
            }) => {
                ledger_id = Some(id);
                pending_repairs |= !repair.is_some_and(|repair| repair.completed);
                serious |= finding.severity != Severity::Minor;
                serious_deferred |= finding.severity != Severity::Minor
                    && repair.is_some_and(|repair| {
                        repair.completed && repair.action.status == Status::Deferred
                    });
            }
            _ => approval_unmet = true,
        }
        candidates.push(json!({"finding": finding, "ledger_id": ledger_id, "repair": repair}));
    }
    let recheck_findings = checked_run
        .and_then(|run| run.coverage.as_ref())
        .map(|cover| &cover.findings);
    let serious_recheck = recheck_findings.is_some_and(|items| {
        items
            .iter()
            .any(|finding| finding.severity != Severity::Minor)
    });
    let recheck_complete = match checked {
        Some(_) if run.round == 2 => fresh_complete,
        Some(check) => complete_coverage(check).is_ok() && require_current(app, check).is_ok(),
        None => false,
    };
    let off_section = checked_run
        .and_then(|run| run.recheck.as_ref())
        .map(|link| &link.affected.outside_section);
    let stage = if let Some(stop) = &state.stop {
        serde_json::to_value(stop.reason)?
    } else if checked_run.is_some_and(|run| run.stage == Stage::Pending)
        || first_run.stage == Stage::Pending
    {
        json!("review_pending")
    } else if let Some(checked) = checked {
        if complete_coverage(checked).is_err() {
            json!("recheck_incomplete")
        } else if !recheck_complete {
            json!("changed_premise_after_recheck")
        } else if serious_recheck || serious_deferred {
            json!("serious_remainder")
        } else if pending_repairs {
            json!("repair_confirmation_unmet")
        } else if recheck_findings.is_some_and(|items| !items.is_empty())
            || off_section.is_some_and(|items| !items.is_empty())
        {
            json!("recheck_remainder")
        } else {
            json!("completed")
        }
    } else if complete_coverage(first).is_err() {
        json!("initial_review_incomplete")
    } else if approval_unmet {
        if fresh_complete {
            json!("approval_target_unmet")
        } else {
            json!("initial_review_stale")
        }
    } else if pending_repairs {
        json!("awaiting_recheck")
    } else if serious {
        json!("serious_remainder")
    } else if !fresh_complete {
        json!("initial_review_stale")
    } else {
        json!("completed")
    };
    let unchecked = checked_run
        .unwrap_or(first_run)
        .coverage
        .as_ref()
        .map(|coverage| {
            coverage
                .targets
                .iter()
                .filter(|target| !target.unchecked.is_empty())
                .map(|target| json!({"card": target.card, "ranges": target.unchecked}))
                .collect::<Vec<_>>()
        });
    let end_reason = match stage.as_str() {
        Some("review_pending" | "awaiting_recheck" | "repair_confirmation_unmet") => Value::Null,
        _ => stage.clone(),
    };
    Ok(json!({
        "initial_session": first.review.session, "recheck_session": checked.map(|receipt| &receipt.review.session),
        "provider_calls": usize::from(first_run.sent.is_some()) + checked_run.map_or(0, |run| usize::from(run.sent.is_some())),
        "stage": stage, "end_reason": end_reason, "decisions": state.decisions, "previous_decisions": state.previous_decisions, "repairs": state.repairs,
        "stop": state.stop, "remaining": {"candidates": candidates, "recheck_findings": recheck_findings,
            "outside_section": off_section, "unchecked": unchecked,
            "unverified_cards": if checked.is_some() && !recheck_complete {
                checked.map(|receipt| &receipt.scope.cards)
            } else if checked.is_none() && !fresh_complete { Some(&first.scope.cards) } else { None },
        },
    }))
}
