//! Capture a single ordered section before the math and output jobs enter the review queue.

pub(crate) mod blind;
mod math;
mod output;

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use fuzukue_cards::{workspace_paths, Card, DeckConfig};

use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Scope {
    pub(crate) section: Option<String>,
    pub(crate) cards: Vec<String>,
    pub(crate) reviewed_version: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Dispatch {
    pub(crate) kind: Kind,
    pub(crate) session: String,
    pub(crate) input_hash: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub(crate) deck: String,
    pub(crate) scope: Scope,
    pub(crate) review: Dispatch,
    pub(crate) findings: Vec<Finding>,
    pub(crate) capture: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blind: Option<blind::Run>,
}

impl Receipt {
    pub(crate) fn read(state: &Path, session: &str) -> Result<Option<Self>> {
        ensure!(
            session.starts_with("review-") && !session.contains(['/', '\\', ':']),
            "査読セッション名が不正: {session}"
        );
        let path = receipt_path(state, session);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("{} を読めない", path.display())),
        };
        serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("{} を解析できない", path.display()))
            .map(Some)
    }
}

#[derive(Serialize)]
pub(crate) struct Requested {
    #[serde(flatten)]
    scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    reviews: Vec<Dispatch>,
}

struct Prepared {
    scope: Scope,
    course: Option<crate::course::reviews::Captured>,
    math: Option<(Review, String)>,
    output: Option<(Review, output::Plan)>,
}

fn card_paths<'a>(deck: &'a Path, card: &'a Card) -> impl Iterator<Item = PathBuf> + 'a {
    let notes = deck.join(&card.path);
    std::iter::once(notes).chain(card.tex.iter().map(|path| deck.join(path)))
}

fn manifest(root: &Path) -> Option<PathBuf> {
    ["deck.toml", "cards.toml", "harness.toml"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
}

pub(crate) fn file_hash(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    std::io::copy(&mut BufReader::new(fs::File::open(path)?), &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn input_hash(value: &impl Serialize) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}

fn relative(project: &Path, path: &Path) -> Result<PathBuf> {
    let path = workspace_paths::resolve(project, project, path)?;
    Ok(path.strip_prefix(project)?.to_owned())
}

fn targets<'a>(graph: &'a Graph, cards: &[String]) -> Result<Vec<&'a Card>> {
    let mut seen = BTreeSet::new();
    ensure!(!cards.is_empty(), "節の査読対象のカードがない");
    cards
        .iter()
        .map(|id| {
            ensure!(seen.insert(id), "節のカード ID が重複している: {id}");
            graph
                .cards
                .get(id)
                .filter(|card| !card.external)
                .with_context(|| format!("節のカード `{id}` がデッキ内にない"))
        })
        .collect()
}

pub(crate) fn current_version(project: &Path, root: &Path, cards: &[String]) -> Result<String> {
    let cfg = DeckConfig::read_in_workspace(root, project)?;
    let graph = Graph::load_deck(root, &cfg, project)?;
    math::Sources::new(project, root).version(&targets(&graph, cards)?)
}

fn prepare(
    copied: &Path,
    math_dir: &Path,
    deck: &str,
    kinds: &[Kind],
    section: Option<String>,
    cards: Vec<String>,
) -> Result<Prepared> {
    let root = copied.join(deck);
    let cfg = DeckConfig::read_in_workspace(&root, copied)?;
    for (_, external) in cfg.external_roots(copied, &root) {
        relative(copied, &external?)?;
    }
    let graph = Graph::load_deck(&root, &cfg, copied)?;
    let targets = targets(&graph, &cards)?;
    let course = crate::course::reviews::Captured::read(&root, section.as_deref(), &cards)?;

    let rules = crate::rules::resolve(copied)?;
    let mut sources = math::Sources::new(copied, &root);
    let reviewed_version = sources.version(&targets)?;
    let scope = Scope {
        section: course
            .as_ref()
            .map(|capture| capture.key.clone())
            .or(section),
        cards,
        reviewed_version,
    };
    let math = if kinds.contains(&Kind::Math) {
        let evidence = sources.capture(&cfg, &graph, &targets, math_dir)?;
        let mut review = Review::scoped(
            copied,
            Path::new(deck),
            targets.iter().copied(),
            Kind::Math,
            &rules,
        )?
        .context("数学査読の対象カードがない")?;
        let hash = input_hash(&(&scope, &evidence, &review.instructions))?;
        review.captured = Some(serde_json::to_value(evidence)?);
        Some((review, hash))
    } else {
        None
    };
    let output = if kinds.contains(&Kind::Output) {
        let review = Review::scoped(
            copied,
            Path::new(deck),
            targets.iter().copied(),
            Kind::Output,
            &rules,
        )?
        .context("出力査読の対象カードがない")?;
        let plan = output::Plan::new(copied, &root, &cfg, &graph, &targets)?;
        Some((review, plan))
    } else {
        None
    };
    Ok(Prepared {
        scope,
        course,
        math,
        output,
    })
}

impl Workspace {
    pub(crate) async fn request_section_review(
        self: &Arc<Self>,
        deck: String,
        kind: &str,
        section: Option<String>,
        cards: Vec<String>,
        origin: &str,
    ) -> Result<Requested> {
        let work = Arc::new(self.admit_work()?);
        let kinds = match kind {
            "math" => vec![Kind::Math],
            "output" => vec![Kind::Output],
            "both" => vec![Kind::Math, Kind::Output],
            _ => bail!("節の査読の kind は math、output、both のいずれかで指定する"),
        };
        ensure!(
            section.as_ref().is_none_or(|key| !key.trim().is_empty()),
            "section が空"
        );
        ensure!(
            Path::new(&deck).is_relative(),
            "deck はプロジェクト相対のパスで指定する"
        );
        let caller = Some(fuzukue_proto::AgentCaller::FrontDesk);
        for kind in &kinds {
            self.check_review_policy(*kind, caller).await?;
        }
        let project = self.project_dir.canonicalize()?;
        let (root, deck) = resolve_deck(&project, Path::new(&deck))?;
        let slug = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        self.check_section_review_slots(&self.reviews.jobs.lock().unwrap(), &deck, &kinds)?;

        let temp = Arc::new(
            tempfile::Builder::new()
                .prefix("fuzukue-section-review-")
                .tempdir()?,
        );
        let copied = temp.path().canonicalize()?.join("workspace");
        let math_dir = copied.with_file_name("math");
        let guard = self.file_save.clone().lock_owned().await;
        let Prepared {
            scope,
            course,
            math,
            output,
        } = tokio::task::spawn_blocking({
            let (project, copied, math_dir, deck, kinds) = (
                project.clone(),
                copied.clone(),
                math_dir.clone(),
                deck.clone(),
                kinds.clone(),
            );
            let temp = temp.clone();
            let work = work.clone();
            move || {
                let (_temp, _work) = (temp, work);
                crate::build::batch::copy_workspace(&project, &copied, &Cancellation::default())?;
                drop(guard);
                prepare(&copied, &math_dir, &deck, &kinds, section, cards)
            }
        })
        .await??;
        let mut prepared = Vec::new();
        if let Some((review, hash)) = math {
            prepared.push((review, hash, math_dir));
        }
        if let Some((mut review, plan)) = output {
            let output_dir = copied.with_file_name("output");
            let evidence = plan.render(&project, &copied, &output_dir, &deck).await?;
            let hash = input_hash(&(&scope, &evidence.inputs, &review.instructions))?;
            review.captured = Some(serde_json::to_value(evidence)?);
            prepared.push((review, hash, output_dir));
        }

        // Reserve the whole request under one lock. A competing request gets a busy
        // error, with no math-only or output-only fragment of the pair left queued.
        let settings = self.review_settings();
        let reviews: Vec<_> = prepared
            .iter()
            .map(|(review, hash, _)| Dispatch {
                kind: review.kind,
                session: format!(
                    "review-{}-{slug}-{:016x}",
                    review.kind.name(),
                    rand::random::<u64>()
                ),
                input_hash: hash.clone(),
            })
            .collect();
        let jobs = {
            let _save = if course.is_some() {
                Some(self.file_save.lock().await)
            } else {
                None
            };
            let mut jobs = self.reviews.jobs.lock().unwrap();
            self.check_section_review_slots(&jobs, &deck, &kinds)?;
            if let Some(capture) = &course {
                capture.record(self, &deck, &root, &scope, &reviews)?;
            }
            kinds
                .iter()
                .map(|&kind| {
                    let job = self.new_review_job(kind, None, &settings, Arc::default(), true);
                    jobs.insert((deck.clone(), kind), job.clone());
                    job
                })
                .collect::<Vec<_>>()
        };
        for (((review, _, captured_dir), dispatch), job) in
            prepared.into_iter().zip(reviews.iter().cloned()).zip(jobs)
        {
            let kind = review.kind;
            let course_review = course.is_some();
            let (app, deck, root, scope, origin) = (
                self.clone(),
                deck.clone(),
                root.clone(),
                scope.clone(),
                origin.to_owned(),
            );
            let (temp, work) = (temp.clone(), work.clone());
            tokio::spawn(async move {
                let (_temp, _work) = (temp, work);
                let result = async {
                    app.mark_review_running(&deck, kind, &job)?;
                    let answer = app
                        .run_reviewer(
                            &review,
                            &captured_dir,
                            &deck,
                            &deck,
                            &dispatch.session,
                            (&origin, caller),
                            &job,
                        )
                        .await?;
                    let findings = review.findings(&answer)?;
                    let result = {
                        let _edit = app.comment_edit.lock().await;
                        app.with_current_review(&deck, kind, &job, || {
                            let (current, _) = resolve_deck(&app.project_dir, Path::new(&deck))?;
                            ensure!(current == root, "査読中にデッキの場所が変わった");
                            persist_result(
                                &app.state_dir,
                                &deck,
                                &scope,
                                &dispatch,
                                &review,
                                &findings,
                            )?;
                            append_scope_drafts(&app, &deck, &root, kind, &findings)
                        })?
                    };
                    if course_review {
                        let _save = app.file_save.lock().await;
                        crate::course::reviews::reconcile(&app, &deck, &root)?;
                    }
                    Ok(result)
                }
                .await;
                app.finish_review(&deck, kind, &job, result);
            });
        }
        let session = (reviews.len() == 1).then(|| reviews[0].session.clone());
        Ok(Requested {
            scope,
            session,
            reviews,
        })
    }

    fn check_section_review_slots(
        &self,
        jobs: &BTreeMap<(String, Kind), Arc<Job>>,
        deck: &str,
        kinds: &[Kind],
    ) -> Result<()> {
        ensure!(
            !self.reviews.stopping.load(Ordering::Relaxed),
            "査読キューは停止中"
        );
        for kind in kinds {
            ensure!(
                !jobs.contains_key(&(deck.into(), *kind)),
                "{deck} の {} 査読は実行中",
                kind.name()
            );
        }
        Ok(())
    }
}

fn persist_result(
    state: &Path,
    deck: &str,
    scope: &Scope,
    run: &Dispatch,
    review: &Review,
    findings: &[Finding],
) -> Result<()> {
    persist_receipt(state, &Receipt {
        deck: deck.into(), scope: scope.clone(), review: run.clone(), findings: findings.to_vec(),
        capture: review.captured.clone(), blind: None,
    }, false)
}

fn receipt_path(state: &Path, session: &str) -> PathBuf {
    state.join("section-reviews").join(format!("{session}.json"))
}

fn persist_receipt(state: &Path, receipt: &Receipt, replace: bool) -> Result<()> {
    let path = receipt_path(state, &receipt.review.session);
    persist_json(&path, receipt, replace)
}

fn persist_json(path: &Path, value: &impl Serialize, replace: bool) -> Result<()> {
    let dir = path.parent().expect("receipt directory");
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.as_file().sync_all()?;
    if replace {
        file.persist(path)?;
    } else {
        file.persist_noclobber(path)?;
    }
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn append_scope_drafts(
    app: &Workspace,
    deck: &str,
    root: &Path,
    kind: Kind,
    findings: &[Finding],
) -> Result<(usize, Option<String>)> {
    if findings.is_empty() {
        return Ok((0, None));
    }
    let graph = Graph::open_in_project(root, &[], Some(&app.project_dir))?;
    let mut items = comments::read(root)?;
    let old_len = items.len();
    let created = usage::now();
    items.extend(
        findings
            .iter()
            .filter(|finding| graph.cards.contains_key(&finding.card))
            .map(|finding| CommentView {
                card: finding.card.clone(),
                quote: None,
                proof: None,
                source: None,
                text: format!("{} {}", kind.label(), finding.text),
                created,
            }),
    );
    let count = items.len() - old_len;
    let card = (count > 0).then(|| items[old_len].card.clone());
    if count > 0 {
        comments::save(root, &items)?;
        app.hub.publish_snapshot(ServerEvent::Comments {
            deck: deck.into(),
            items,
        });
    }
    Ok((count, card))
}

#[cfg(test)]
mod tests;
