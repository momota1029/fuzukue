//! Connect the persisted section review receipts to the committed course section.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use fuzukue_cards::course::{
    Course, Review, ReviewRequest, ReviewStatus, Section, Snapshot, Status,
};
use fuzukue_cards::workspace_paths;
use fuzukue_proto::ServerEvent;

use crate::reviewers::{
    section::{self, Dispatch, Receipt, Scope},
    Kind,
};
use crate::Workspace;

pub(crate) struct Captured {
    pub(crate) key: String,
    book: String,
    math: String,
    output: String,
}

fn committed(section: &Section, cards: &[String]) -> bool {
    matches!(section.status, Status::Written | Status::Done) && section.cards == cards
}

impl Captured {
    pub(crate) fn read(
        lecture: &Path,
        key: Option<&str>,
        cards: &[String],
    ) -> Result<Option<Self>> {
        let Some(course) = Course::read(lecture)? else {
            return Ok(None);
        };
        let section = match key {
            Some(key) => {
                let section = course
                    .sections
                    .iter()
                    .find(|section| section.key == key)
                    .with_context(|| format!("節 `{key}` は course.toml にない"))?;
                ensure!(
                    committed(section, cards),
                    "節 `{key}` に保存したカードを同じ順序で指定する"
                );
                section
            }
            None => {
                let mut matched = course
                    .sections
                    .iter()
                    .filter(|section| committed(section, cards));
                let Some(section) = matched.next() else {
                    return Ok(None);
                };
                ensure!(
                    matched.next().is_none(),
                    "対象の講義の節を section で指定する"
                );
                section
            }
        };
        let (math, output) = course.review_versions(&section.key, &Snapshot::read(lecture)?)?;
        Ok(Some(Self {
            key: section.key.clone(),
            book: course.book,
            math,
            output,
        }))
    }

    /// Record the whole dispatch before either job can run, using versions from the snapshot.
    pub(crate) fn record(
        &self,
        app: &Workspace,
        deck: &str,
        lecture: &Path,
        scope: &Scope,
        runs: &[Dispatch],
    ) -> Result<()> {
        let mut course =
            Course::read(lecture)?.context("査読の準備中に course.toml がなくなった")?;
        ensure!(
            course.book == self.book,
            "査読の準備中に講義の原本が変わった"
        );
        let section = course
            .sections
            .iter_mut()
            .find(|section| section.key == self.key && committed(section, &scope.cards))
            .context("査読の準備中に講義の節が変わった")?;
        let requested_at = crate::usage::now().to_string();
        for run in runs {
            let version = match run.kind {
                Kind::Math => &self.math,
                Kind::Output => &self.output,
                _ => anyhow::bail!("講義の査読に未対応: {}", run.kind.name()),
            };
            section.reviews.insert(
                run.kind.name().into(),
                Review {
                    version: version.clone(),
                    requested_at: Some(requested_at.clone()),
                    status: ReviewStatus::Pending,
                    request: Some(ReviewRequest {
                        session: run.session.clone(),
                        reviewed_version: scope.reviewed_version.clone(),
                        input_hash: run.input_hash.clone(),
                    }),
                },
            );
        }
        save(app, deck, lecture, &mut course)?;
        Ok(())
    }
}

fn save(app: &Workspace, deck: &str, lecture: &Path, course: &mut Course) -> Result<bool> {
    let changed = course.save(lecture)?;
    if changed {
        let path = Path::new(deck)
            .join(fuzukue_cards::course::FILE)
            .to_string_lossy()
            .replace('\\', "/");
        app.hub.publish_snapshot(ServerEvent::FileChanged { path });
    }
    Ok(changed)
}

fn matches(
    receipt: &Receipt,
    deck: &str,
    key: &str,
    cards: &[String],
    kind: &str,
    request: &ReviewRequest,
) -> bool {
    receipt.blind.is_none()
        && receipt.deck == deck
        && receipt.scope.section.as_deref() == Some(key)
        && receipt.scope.cards == cards
        && receipt.scope.reviewed_version == request.reviewed_version
        && receipt.review.kind.name() == kind
        && receipt.review.session == request.session
        && receipt.review.input_hash == request.input_hash
}

fn status(receipt: &Receipt) -> ReviewStatus {
    if receipt.findings.is_empty() {
        ReviewStatus::Clean
    } else {
        ReviewStatus::Findings
    }
}

/// Replay results under the course write lock. A receipt can only finish its own pending job.
pub(crate) fn reconcile(app: &Workspace, deck: &str, lecture: &Path) -> Result<bool> {
    let Some(mut course) = Course::read(lecture)? else {
        return Ok(false);
    };
    let mut changed = false;
    for section in &mut course.sections {
        for (kind, review) in &mut section.reviews {
            if review.status != ReviewStatus::Pending {
                continue;
            }
            let Some(request) = &review.request else {
                continue;
            };
            let Some(receipt) = Receipt::read(&app.state_dir, &request.session)? else {
                continue;
            };
            ensure!(
                matches(&receipt, deck, &section.key, &section.cards, kind, request),
                "査読結果 `{}` が講義の依頼と一致しない",
                request.session
            );
            review.status = status(&receipt);
            changed = true;
        }
    }
    if changed {
        save(app, deck, lecture, &mut course)
    } else {
        Ok(false)
    }
}

/// Check the shared section version and the workspace files the completed reviewer saw.
pub(super) fn freshness<'a>(
    app: &'a Workspace,
    deck: &'a str,
    project: &'a Path,
    lecture: &'a Path,
) -> impl FnMut(&Section, &str, &Review) -> bool + 'a {
    let mut versions = BTreeMap::new();
    let mut files = BTreeMap::new();
    move |section, kind, review| {
        let Some(request) = &review.request else {
            return false;
        };
        let version = versions
            .entry(section.key.clone())
            .or_insert_with(|| section::current_version(project, lecture, &section.cards).ok());
        if version.as_ref() != Some(&request.reviewed_version) {
            return false;
        }
        let Ok(Some(receipt)) = Receipt::read(&app.state_dir, &request.session) else {
            return false;
        };
        if review.status != status(&receipt)
            || !matches(&receipt, deck, &section.key, &section.cards, kind, request)
        {
            return false;
        }
        let Some(capture) = receipt.capture else {
            return false;
        };
        let captured_files = match kind {
            "math" => &capture["files"],
            "output" => &capture["inputs"]["workspace_files"],
            _ => return false,
        };
        captured_files.as_object().is_some_and(|captured| {
            captured.iter().all(|(file, digest)| {
                let live = files.entry(PathBuf::from(file)).or_insert_with_key(|file| {
                    let path = workspace_paths::resolve(project, project, file).ok()?;
                    section::file_hash(&path).ok()
                });
                live.as_deref()
                    .is_some_and(|hash| Some(hash) == digest.as_str())
            })
        })
    }
}
