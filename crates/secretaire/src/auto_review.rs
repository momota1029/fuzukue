//! 相談役のターン前後でカードを比較し、変更をまとめてバックグラウンドで査読する。

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fuzukue_cards::Graph;
use fuzukue_proto::{CommentView, ServerEvent};
use serde::Deserialize;

use crate::cancellation::Cancelled;
use crate::reviewers::{Baseline, EditedPassage, Job, Kind, Review};
use crate::{comments, usage, Workspace};

pub(crate) fn enabled() -> bool {
    !crate::canary() && fuzukue_proto::compat::env("FUZUKUE_AUTO_REVIEW").as_deref() != Ok("0")
}

#[derive(Debug, PartialEq, Eq)]
struct Config {
    auto: bool,
    reviewers: Vec<Kind>,
    lints: bool,
}

#[derive(Default, Deserialize)]
struct Overrides {
    auto: Option<bool>,
    reviewers: Option<Vec<Kind>>,
    lints: Option<bool>,
}

impl Overrides {
    fn read(path: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            review: Overrides,
        }
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<File>(&text)
                .map(|file| file.review)
                .with_context(|| format!("{} の査読設定を読めない", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{} を読めない", path.display())),
        }
    }
}

/// デッキの `[review]` 設定で、この種類の自動査読が有効か。
pub(crate) fn wants(project: &Path, root: &Path, kind: Kind) -> Result<bool> {
    let config = Config::read(&Overrides::read(&fuzukue_proto::compat::workspace_config(project))?, root)?;
    Ok(config.auto && config.reviewers.contains(&kind))
}

impl Config {
    fn read(project: &Overrides, deck: &Path) -> Result<Self> {
        let config = ["deck.toml", "cards.toml", "harness.toml"]
            .into_iter()
            .map(|name| deck.join(name))
            .find(|path| path.is_file());
        let deck = config.as_deref().map(Overrides::read).transpose()?.unwrap_or_default();
        Ok(Self {
            auto: deck.auto.or(project.auto).unwrap_or(false),
            reviewers: deck
                .reviewers
                .or_else(|| project.reviewers.clone())
                .unwrap_or_else(|| vec![Kind::Style, Kind::Lean, Kind::Math, Kind::Consistency]),
            lints: deck.lints.or(project.lints).unwrap_or(true),
        })
    }
}

pub(crate) type CardFiles = BTreeMap<PathBuf, Vec<u8>>;
pub(crate) type DeckFiles = BTreeMap<String, CardFiles>;

pub(crate) fn card_files(graph: &Graph) -> Result<DeckFiles> {
    card_files_matching(graph, |_| true)
}

fn automatic_files(project: &Path, graph: &Graph) -> Result<DeckFiles> {
    let paths = graph.cards.values().flat_map(|card| {
        std::iter::once(&card.path).chain(&card.tex).map(|path| graph.root.join(path))
    });
    let eligible = crate::auto_paths::eligible(project, paths)?;
    card_files_matching(graph, |path| eligible.contains(&graph.root.join(path)))
}

fn card_files_matching(graph: &Graph, include: impl Fn(&Path) -> bool) -> Result<DeckFiles> {
    graph
        .cards
        .values()
        .filter(|card| include(&card.path))
        .map(|card| {
            let files = std::iter::once(&card.path)
                .chain(&card.tex)
                .filter(|path| include(path))
                .map(|path| {
                    let absolute = graph.root.join(path);
                    let bytes = std::fs::read(&absolute)
                        .with_context(|| format!("{} を読めない", absolute.display()))?;
                    Ok((path.clone(), bytes))
                })
                .collect::<Result<_>>()?;
            Ok((card.id.clone(), files))
        })
        .collect()
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct Snapshot {
    decks: BTreeMap<PathBuf, Option<DeckFiles>>,
    available: bool,
}

// Mark both sides of the same unchanged subsequence with a linear-space LCS split.
pub(crate) fn mark_changed(before: &[&str], after: &[&str], removed: &mut [bool], added: &mut [bool]) {
    let prefix = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let before = &before[prefix..];
    let after = &after[prefix..];
    let suffix = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let before = &before[..before.len() - suffix];
    let after = &after[..after.len() - suffix];
    let removed = &mut removed[prefix..prefix + before.len()];
    let added = &mut added[prefix..prefix + after.len()];
    if after.is_empty() {
        removed.fill(true);
        return;
    }
    if before.is_empty() {
        added.fill(true);
        return;
    }
    if let [line] = before {
        added.fill(true);
        if let Some(i) = after.iter().position(|other| other == line) {
            added[i] = false;
        } else {
            removed[0] = true;
        }
        return;
    }

    fn lengths<'a>(
        before: impl Iterator<Item = &'a &'a str>,
        after: impl ExactSizeIterator<Item = &'a &'a str> + Clone,
    ) -> Vec<usize> {
        let mut lengths = vec![0; after.len() + 1];
        for line in before {
            let mut diagonal = 0;
            for (i, other) in after.clone().enumerate() {
                let above = lengths[i + 1];
                lengths[i + 1] = if line == other {
                    diagonal + 1
                } else {
                    above.max(lengths[i])
                };
                diagonal = above;
            }
        }
        lengths
    }

    let (left, right) = before.split_at(before.len() / 2);
    let (split, common) = {
        let forward = lengths(left.iter(), after.iter());
        let backward = lengths(right.iter().rev(), after.iter().rev());
        forward
            .into_iter()
            .zip(backward.into_iter().rev())
            .enumerate()
            .map(|(i, (a, b))| (i, a + b))
            .max_by_key(|&(_, common)| common)
            .unwrap()
    };
    if common == 0 {
        removed.fill(true);
        added.fill(true);
        return;
    }
    let (removed_left, removed_right) = removed.split_at_mut(left.len());
    let (added_left, added_right) = added.split_at_mut(split);
    mark_changed(left, &after[..split], removed_left, added_left);
    mark_changed(right, &after[split..], removed_right, added_right);
}

fn edited_paragraphs(before: &str, after: &[&str]) -> Vec<RangeInclusive<usize>> {
    let before: Vec<_> = before.lines().collect();
    let mut changed = vec![false; after.len()];
    mark_changed(&before, after, &mut vec![false; before.len()], &mut changed);

    static PARAGRAPH_START: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(?:(?:[-+*]|[0-9]+[.)]|\([a-zA-Z0-9]+\))\s+|\\(?:item|par)\b)")
            .unwrap()
    });
    let standalone = |line: &str| {
        line.starts_with(['>', '＞'])
            || line.starts_with("```")
            || line.starts_with("~~~")
            || line
                .split_once(' ')
                .is_some_and(|(marks, _)| !marks.is_empty() && marks.bytes().all(|c| c == b'#'))
    };
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut finish = |start, end| {
        if changed[start..end].contains(&true) {
            ranges.push(start + 1..=end);
        }
    };
    for (i, line) in after.iter().map(|line| line.trim()).enumerate() {
        if line.is_empty() || standalone(line) {
            finish(start, i);
            start = i + 1;
            if !line.is_empty() {
                finish(i, i + 1);
            }
        } else if PARAGRAPH_START.is_match(line) {
            finish(start, i);
            start = i;
        }
    }
    finish(start, after.len());
    ranges
}

#[derive(Default)]
struct Edits {
    cards: BTreeSet<String>,
    body_lines: BTreeMap<String, Vec<RangeInclusive<usize>>>,
    passages: Vec<EditedPassage>,
}

fn file_text(bytes: Option<&Vec<u8>>) -> &str {
    bytes
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or_default()
}

impl Snapshot {
    pub(crate) fn take(project: &Path) -> Self {
        let roots = match crate::auto_paths::eligible(project, crate::auto_paths::deck_roots(project)) {
            Ok(roots) => roots,
            Err(error) => {
                tracing::warn!("automatic review: {error:#}");
                return Self { decks: BTreeMap::new(), available: false };
            }
        };
        Self {
            available: true,
            decks: roots.into_iter()
                .map(|root| {
                    let files = Graph::open_local(&root).and_then(|graph| automatic_files(project, &graph));
                    if let Err(e) = &files {
                        tracing::warn!(
                            "自動査読の変更前スナップショット {}: {e:#}",
                            root.display()
                        );
                    }
                    (root, files.ok())
                })
                .collect(),
        }
    }

    #[cfg(test)]
    fn edits(&self, root: &Path, deck: &Path, graph: &Graph) -> Result<Edits> {
        let after = card_files(graph)?;
        Ok(Self::edited_cards(
            &self.changed_baseline(root, &after),
            deck,
            graph,
            &after,
        ))
    }

    // A missing entry is a new deck. A failed snapshot must not review the whole deck.
    fn changed_baseline(&self, root: &Path, after: &DeckFiles) -> DeckFiles {
        let empty = DeckFiles::new();
        let before = match self.decks.get(root) {
            Some(Some(files)) => files,
            Some(None) => return DeckFiles::new(),
            None => &empty,
        };
        before
            .keys()
            .chain(after.keys())
            .filter(|&id| before.get(id) != after.get(id))
            .map(|id| (id.clone(), before.get(id).cloned().unwrap_or_default()))
            .collect()
    }

    fn edited_cards(baseline: &DeckFiles, deck: &Path, graph: &Graph, after: &DeckFiles) -> Edits {
        let mut edits = Edits::default();
        for (id, previous) in baseline {
            let Some(files) = after.get(id) else { continue };
            if previous == files {
                continue;
            }
            edits.cards.insert(id.clone());
            let card = &graph.cards[id];
            let mut assembled_offset = 0;
            let mut ranges = Vec::new();
            for path in card.tex.iter().chain(std::iter::once(&card.path)) {
                let text = file_text(files.get(path));
                let old_path = |old: &PathBuf| {
                    if path == &card.path {
                        old.extension().is_some_and(|ext| ext == "md")
                    } else {
                        old.file_name() == path.file_name()
                    }
                };
                let prior = file_text(
                    previous
                        .iter()
                        .find(|(old, _)| old_path(old))
                        .map(|(_, bytes)| bytes),
                );
                let (prior, file_offset, body_offset, body_line_count) = if path == &card.path {
                    let body = fuzukue_cards::split_frontmatter(text).map_or("", |(_, body)| body);
                    let prior = fuzukue_cards::split_frontmatter(prior).map_or("", |(_, body)| body);
                    let header_lines = text[..text.len() - body.len()].lines().count();
                    (prior, header_lines, assembled_offset, body.lines().count())
                } else {
                    // Graph::attach_tex prefixes a heading and appends two line breaks.
                    let offset = assembled_offset + 1;
                    assembled_offset += text.trim_end().bytes().filter(|&b| b == b'\n').count() + 3;
                    (prior, 0, offset, text.trim_end().lines().count())
                };
                let file_lines: Vec<_> = text.lines().collect();
                let body_lines = &file_lines[file_offset..];
                let changed = if previous.is_empty() && !body_lines.is_empty() {
                    vec![1..=body_lines.len()]
                } else {
                    edited_paragraphs(prior, body_lines)
                };
                for range in changed {
                    if *range.start() <= body_line_count {
                        let end = (*range.end()).min(body_line_count);
                        ranges.push(range.start() + body_offset..=end + body_offset);
                    }
                    edits.passages.push(EditedPassage::new(
                        id,
                        deck.join(path),
                        body_lines,
                        range,
                        file_offset,
                    ));
                }
            }
            edits.body_lines.insert(id.clone(), ranges);
        }
        edits
    }

    fn changes(self, project: &Path) -> Result<Vec<(PathBuf, DeckFiles)>> {
        let defaults = Overrides::read(&fuzukue_proto::compat::workspace_config(project))?;
        if !self.available {
            return Ok(Vec::new());
        }
        let after = Self::take(project);
        let mut changed = Vec::new();
        for (root, files) in after.decks {
            if let Some(files) = files {
                let baseline = self.changed_baseline(&root, &files);
                if baseline.is_empty() {
                    continue;
                }
                match Config::read(&defaults, &root) {
                    Ok(config) if config.auto => changed.push((root, baseline)),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("自動査読 {}: {e:#}", root.display()),
                }
            }
        }
        Ok(changed)
    }

    #[cfg(test)]
    fn prepare(self, project: &Path, rule_project: &Path) -> Result<Vec<PreparedDeck>> {
        self.changes(project)?
            .into_iter()
            .map(|(root, baseline)| prepare_deck(project, rule_project, &root, &baseline, None))
            .collect()
    }
}

struct PreparedDeck {
    items: Vec<CommentView>,
    reviewers: Vec<(Kind, Review)>,
}

fn prepare_deck(
    project: &Path,
    rule_project: &Path,
    root: &Path,
    baseline: &DeckFiles,
    only: Option<Kind>,
) -> Result<PreparedDeck> {
    let mut deck = PreparedDeck {
        items: Vec::new(),
        reviewers: Vec::new(),
    };
    if !crate::auto_paths::deck_eligible(project, root) {
        return Ok(deck);
    }
    let config = Config::read(&Overrides::read(&fuzukue_proto::compat::workspace_config(project))?, root)?;
    if !config.auto {
        return Ok(deck);
    }
    let rules = crate::rules::resolve(rule_project)?;
    let graph = Graph::open_local(root)?;
    let rel = root.strip_prefix(project)?;
    let after = automatic_files(project, &graph)?;
    let style = only.is_none_or(|kind| kind == Kind::Style);
    let Edits {
        cards: ids,
        body_lines,
        passages,
    } = if style {
        Snapshot::edited_cards(baseline, rel, &graph, &after)
    } else {
        Edits {
            cards: baseline
                .iter()
                .filter(|(id, old)| after.get(*id).is_some_and(|new| new != *old))
                .map(|(id, _)| id.clone())
                .collect(),
            ..Edits::default()
        }
    };
    if ids.is_empty() {
        return Ok(deck);
    }
    if style && config.lints {
        match Graph::lint_edited_cards_with_rules(root, &ids, &body_lines, &rules.layers(project), Some(project)) {
            Ok(diagnostics) => {
                let by_path: BTreeMap<_, _> =
                    ids.iter().map(|id| (&graph.cards[id].path, id)).collect();
                for diag in diagnostics {
                    if diag.code == Some("style/textlint-status") {
                        tracing::warn!("{}", diag.display_message());
                    } else if let (Some(card), Some(code)) = (by_path.get(&diag.path), diag.code) {
                        let text = if code.starts_with("style/") {
                            diag.display_message()
                        } else {
                            format!("[{code}] {}", diag.display_message())
                        };
                        deck.items.push(finding(card, text));
                    }
                }
            }
            Err(e) => tracing::warn!("自動査読の lint {}: {e:#}", root.display()),
        }
    }
    for kind in [Kind::Lean, Kind::Math] {
        if only.is_none_or(|only| only == kind) && config.reviewers.contains(&kind) {
            let targets = ids.iter().filter_map(|id| graph.cards.get(id));
            if let Some(review) = Review::scoped(project, rel, targets, kind, &rules)? {
                deck.reviewers.push((kind, review));
            }
        }
    }
    if style && config.reviewers.contains(&Kind::Style) {
        if let Some(review) = Review::edited(project, passages, &rules)? {
            deck.reviewers.push((Kind::Style, review));
        }
    }
    Ok(deck)
}

fn finding(card: &str, text: String) -> CommentView {
    CommentView {
        card: card.into(),
        quote: None,
        proof: None,
        source: None,
        text,
        created: usage::now(),
    }
}

impl Workspace {
    pub(crate) async fn review_snapshot(&self, project: &Path, session: &str) -> Option<Snapshot> {
        if !enabled() || self.comment_applications.lock().await.contains_key(session) {
            return None;
        }
        let project = project.to_path_buf();
        match tokio::task::spawn_blocking(move || Snapshot::take(&project)).await {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                tracing::warn!("自動査読のスナップショット: {e}");
                None
            }
        }
    }

    pub(crate) fn schedule_turn_reviews<'a>(
        self: &'a Arc<Self>,
        project: &'a Path,
        session: &'a str,
        before: Snapshot,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        self.schedule_reviews_for(project, session, before, None)
    }

    pub(crate) fn schedule_reviews_for<'a>(
        self: &'a Arc<Self>, project: &'a Path, session: &'a str, before: Snapshot, only: Option<Kind>,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if !enabled() {
                return;
            }
            let Ok(work) = self.admit_work() else {
                self.drain.defer(&self.state_dir, session, crate::mail::session_key(),
                    crate::self_update::drain::Work::AutomaticReview { project: project.into(), before, kind: only });
                return;
            };
            let work = Arc::new(work);
            let dir = project.to_owned();
            let changes = match tokio::task::spawn_blocking(move || before.changes(&dir)).await {
                Ok(Ok(changes)) => changes,
                Ok(Err(error)) => {
                    tracing::warn!("自動査読の変更を読めない: {error:#}");
                    return;
                }
                Err(error) => {
                    tracing::warn!("自動査読のスナップショット: {error}");
                    return;
                }
            };
            let settings = self.review_settings();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            for (root, baseline) in changes {
                let Ok((root, deck)) = crate::workspace::resolve_deck(project, &root) else { continue };
                let project = project.canonicalize().unwrap_or_else(|_| project.to_owned());
                let mut tasks = Vec::new();
                for kind in [Kind::Style, Kind::Lean, Kind::Math].into_iter().filter(|kind| only.is_none_or(|only| only == *kind)) {
                    let Some(job) = self.queue_review(
                        &deck, kind, Some(Baseline::Automatic(baseline.clone())), &settings,
                    ) else {
                        continue;
                    };
                    let (app, root, deck, project) = (
                        self.clone(),
                        root.clone(),
                        deck.clone(),
                        project.to_owned(),
                    );
                    let origin = session.to_owned();
                    let work = work.clone();
                    tasks.push(tokio::spawn(async move {
                        let _work = work;
                        let mut drafts = Vec::new();
                        tokio::select! {
                            biased;
                            _ = job.cancel.cancelled() => {}
                            _ = tokio::time::sleep_until(deadline) => {}
                        }
                        let _run = job.gate.lock().await;
                        let result = async {
                            let Ok(_admission) = app.admit_work() else {
                                if let Some(Baseline::Automatic(before)) = &job.baseline {
                                    app.drain.defer(&app.state_dir, &origin, crate::mail::session_key(),
                                        crate::self_update::drain::Work::AutomaticReview {
                                            project: project.clone(), kind: Some(kind),
                                            before: Snapshot { decks: BTreeMap::from([(root.clone(), Some(before.clone()))]), available: true },
                                        });
                                }
                                return Err(Cancelled.into());
                            };
                            job.cancel.check()?;
                            app.review_edited(
                                &project,
                                &root,
                                &deck,
                                &origin,
                                kind,
                                &job,
                                &mut drafts,
                            )
                            .await
                        }
                        .await;
                        let cancelled = result.as_ref().is_err_and(|error| error.is::<Cancelled>());
                        app.finish_review(&deck, kind, &job, result);
                        (drafts, cancelled)
                    }));
                }
                if settings.apply_in_side_thread && root.starts_with(&self.project_dir) {
                    let app = self.clone();
                    let origin = session.to_owned();
                    let work = work.clone();
                    tokio::spawn(async move {
                        let _work = work;
                        let mut drafts = Vec::new();
                        for task in tasks {
                            let Ok((items, false)) = task.await else {
                                return;
                            };
                            drafts.extend(items);
                        }
                        if !drafts.is_empty() {
                            if let Err(e) =
                                crate::apply_comment_drafts(&app, &deck, origin, Some(drafts)).await
                            {
                                tracing::warn!("自動査読のコメント反映 {deck}: {e:#}");
                            }
                        }
                    });
                }
            }
        })
    }

    async fn review_edited(
        &self,
        project: &Path,
        root: &Path,
        deck: &str,
        origin: &str,
        kind: Kind,
        job: &Arc<Job>,
        drafts: &mut Vec<CommentView>,
    ) -> Result<(usize, Option<String>)> {
        let paths = (
            project.to_owned(),
            self.project_dir.clone(),
            root.to_owned(),
        );
        let data = job.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            let Some(Baseline::Automatic(baseline)) = &data.baseline else {
                unreachable!("automatic review requires a baseline");
            };
            prepare_deck(&paths.0, &paths.1, &paths.2, baseline, Some(kind))
        })
        .await??;
        let mut items = if kind == Kind::Style {
            prepared.items
        } else {
            Vec::new()
        };
        let review = prepared
            .reviewers
            .drain(..)
            .find_map(|(role, review)| (role == kind).then_some(review));
        let reviewer_result = if let Some(review) = review {
            self.mark_review_running(deck, kind, job)?;
            let path = root.strip_prefix(project)?.to_string_lossy();
            let session = format!("review-{}-{deck}", kind.name());
            match self
                .run_reviewer(&review, project, deck, &path, &session, (origin, None), job)
                .await
                .and_then(|answer| review.findings(&answer))
            {
                Ok(findings) => {
                    items.extend(findings.into_iter().map(|item| {
                        finding(&item.card, format!("{} {}", kind.label(), item.text))
                    }));
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(())
        };

        let _edit = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => return Err(Cancelled.into()),
            lock = self.comment_edit.lock() => lock,
        };
        let result = self.with_current_review(deck, kind, job, || {
            for item in &mut items {
                item.text.insert_str(0, "[自動査読] ");
            }
            if !items.is_empty() {
                let (current, _) = crate::workspace::resolve_deck(project, Path::new(deck))?;
                anyhow::ensure!(current == root, "査読中にデッキの場所が変わった");
                let mut saved = comments::read(root)?;
                saved.extend(items.iter().cloned());
                comments::save(root, &saved)?;
                self.hub.publish_snapshot(ServerEvent::Comments {
                    deck: deck.into(),
                    items: saved,
                });
            }
            let result = (items.len(), items.first().map(|item| item.card.clone()));
            *drafts = items;
            job.mark_committed();
            Ok(result)
        })?;
        reviewer_result?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn line_diff_keeps_unchanged_paragraphs_between_edits() {
        let before = "First.\n\nKeep one.\nKeep two.\n\nLast.\n";
        let after = [
            "Edited first.",
            "",
            "Keep one.",
            "Keep two.",
            "",
            "Edited last.",
        ];
        assert_eq!(edited_paragraphs(before, &after), [1..=1, 6..=6]);
    }

    #[test]
    fn detects_new_and_changed_cards_including_custom_tex() -> Result<()> {
        let project = TestDir::new();
        project.write("paper/deck.toml", "dirs = ['draft']");
        project.write("paper/draft/main/card.md", "---\nid: main\n---\n");
        project.write("paper/draft/main/statement.tex", "First version.");
        project.write("paper/draft/main/proof.tex", "By definition.");
        project.write("paper/draft/main/extra.tex", "Some context.");
        project.write("paper/draft/other.md", "---\nid: other\n---\n");
        let root = project.path().join("paper");
        let before = Snapshot::take(project.path());

        project.write("paper/draft/main/unrelated.txt", "Not a card.");
        assert!(before
            .edits(&root, Path::new("paper"), &Graph::open_local(&root)?)?
            .cards
            .is_empty());

        project.write("paper/draft/main/extra.tex", "More context.");
        project.write("paper/draft/new/card.md", "---\nid: new\n---\n");
        project.write("another/deck.toml", "");
        project.write("another/cards/single.md", "---\nid: single\n---\n");
        assert_eq!(
            before
                .edits(&root, Path::new("paper"), &Graph::open_local(&root)?)?
                .cards,
            ["main".into(), "new".into()].into()
        );
        let another = project.path().join("another");
        assert_eq!(
            before
                .edits(
                    &another,
                    Path::new("another"),
                    &Graph::open_local(&another)?
                )?
                .cards,
            ["single".into()].into()
        );

        let before = Snapshot::take(project.path());
        std::fs::remove_file(root.join("draft/main/proof.tex"))?;
        project.write("paper/draft/other.md", "---\nid: other\n---\nEdited.\n");
        assert_eq!(
            before
                .edits(&root, Path::new("paper"), &Graph::open_local(&root)?)?
                .cards,
            ["main".into(), "other".into()].into()
        );
        Ok(())
    }

    #[test]
    fn automatic_review_skips_staged_decks_and_ignored_card_edits() -> Result<()> {
        let project = TestDir::new();
        project.write("paper/deck.toml", "dirs = ['cards']\n[review]\nauto = true");
        project.write("paper/.gitignore", "cards/generated/\n");
        project.write(".gitignore", "staged/\n");
        project.write("paper/cards/generated/draft.md", "---\nid: draft\n---\nBefore.\n");
        project.write("paper/cards/main.md", "---\nid: main\n---\nBefore.\n");
        let before = Snapshot::take(project.path());
        for deck in [".compact-target-review-abc", "nested/.stage", "_build", "staged"] {
            project.write(&format!("{deck}/deck.toml"), "[review]\nauto = true");
            project.write(&format!("{deck}/cards/draft.md"), "---\nid: draft\n---\nNew.\n");
        }
        project.write("paper/cards/generated/draft.md", "---\nid: draft\n---\nAfter.\n");
        project.write("paper/cards/main.md", "---\nid: main\n---\nAfter.\n");
        let [(root, baseline)] = before.changes(project.path())?.try_into().unwrap();
        assert_eq!(root, project.path().join("paper"));
        assert_eq!(baseline.into_keys().collect::<Vec<_>>(), ["main"]);
        Ok(())
    }

    #[test]
    fn automatic_review_requires_opt_in() -> Result<()> {
        let project = TestDir::new();
        project.write("deck.toml", "");
        let before = Snapshot::take(project.path());
        project.write("cards/new.md", "---\nid: new\n---\nNew card.\n");
        assert!(before.changes(project.path())?.is_empty());
        assert!(!wants(project.path(), project.path(), Kind::Consistency)?);

        project.write("fuzukue.toml", "[review]\nauto = true");
        let before = Snapshot::take(project.path());
        project.write("cards/new.md", "---\nid: new\n---\nEdited card.\n");
        assert_eq!(before.changes(project.path())?.len(), 1);
        assert!(wants(project.path(), project.path(), Kind::Consistency)?);

        project.write("deck.toml", "[review]\nauto = false");
        assert!(!wants(project.path(), project.path(), Kind::Consistency)?);
        Ok(())
    }

    #[test]
    fn deck_overrides_project_review_settings() -> Result<()> {
        let project = TestDir::new();
        let default = Config {
            auto: false,
            reviewers: vec![Kind::Style, Kind::Lean, Kind::Math, Kind::Consistency],
            lints: true,
        };
        assert_eq!(
            Config::read(&Overrides::default(), project.path())?,
            default
        );
        project.write(
            "fuzukue.toml",
            "[review]\nauto = false\nrounds = 4\nreviewers = ['lean']\nlints = false",
        );
        project.write("deck.toml", "[review]\nauto = true\nreviewers = []");
        let defaults = Overrides::read(&project.path().join("fuzukue.toml"))?;
        assert_eq!(
            Config::read(&defaults, project.path())?,
            Config {
                auto: true,
                reviewers: vec![],
                lints: false
            }
        );
        project.write("deck.toml", "[review]\nreviewers = ['typo']");
        assert!(Config::read(&defaults, project.path()).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn style_findings_are_saved_in_nested_harness_and_legacy_decks() -> Result<()> {
        for config in ["harness.toml", "cards.toml"] {
            let project = TestDir::new();
            let path = "writing/sample-paper/harness";
            let root = project.path().join(path);
            let card = format!("{path}/cards/prose.md");
            // Run the lint stage without a model and exercise the actual comment save.
            project.write("fuzukue.toml", "[review]\nauto = true\nreviewers = []");
            project.write(&format!("{path}/{config}"), "cards_dir = 'cards'");
            project.write(&card, "---\nid: prose\n---\n結果を述べる。\n");
            let before = Snapshot::take(project.path());
            project.write(&card, "---\nid: prose\n---\n重要なのは固定した条件である。\n");
            let [(changed, baseline)] = before.changes(project.path())?.try_into().unwrap();
            assert_eq!(changed, root);
            let app = Workspace::new(
                project.path().into(), String::new(), project.path().join("state"), Arc::new(crate::Hub::new()),
            );
            let (_, deck) = crate::workspace::resolve_deck(project.path(), &root)?;
            let job = app.queue_review(&deck, Kind::Style, Some(Baseline::Automatic(baseline)), &app.review_settings()).unwrap();
            let mut drafts = Vec::new();
            let (count, first) = app.review_edited(project.path(), &root, &deck, "", Kind::Style, &job, &mut drafts).await?;
            assert_eq!(count, 1);
            assert_eq!(first.as_deref(), Some("prose"));
            assert!(comments::read(&root)?[0].text.contains("重要なのは"));
        }
        Ok(())
    }

    #[test]
    fn style_review_uses_edited_paragraph_and_filters_context_findings() -> Result<()> {
        let project = TestDir::new();
        project.write("deck.toml", "[review]\nauto = true\nreviewers = ['style']");
        let original = concat!(
            "---\nid: prose\n---\n",
            "同じ結論ではなく未編集の説明を置く。\n\n",
            "重要なのは固定した条件である。\n",
            "この条件を使う。\n\n",
            "It might possibly hold.\n",
            "最後の段落だ。\n\n",
            "This crucial paragraph is distant.\n",
        );
        project.write("cards/prose.md", original);
        let before = Snapshot::take(project.path());
        project.write(
            "cards/prose.md",
            &original.replace("この条件を使う。", "補題にこの条件を使う。"),
        );
        let decks = before.prepare(project.path(), project.path())?;
        let deck = &decks[0];
        assert_eq!(deck.items.len(), 1);
        assert!(deck.items[0].text.contains("重要なのは"));
        let [(Kind::Style, reviewer)] = deck.reviewers.as_slice() else {
            panic!("expected a style review")
        };
        let prompt = reviewer.prompt(".", &[])?;
        assert!(prompt.contains("\"id\": \"prose\""));
        assert!(prompt.contains("\"path\": \"cards/prose.md\""));
        assert!(prompt.contains("\"start\": 6"));
        assert!(prompt.contains("\"end\": 7"));
        assert!(prompt.contains("補題にこの条件を使う。"));
        assert!(prompt.contains("It might possibly hold."));
        assert!(!prompt.contains("This crucial paragraph is distant."));
        let findings = reviewer.findings(
            r#"{"findings":[
            {"card":"prose","text":"cards/prose.md:L6-L7\nin scope"},
            {"card":"prose","text":"cards/prose.md:L4-L4\ncontext only"},
            {"card":"prose","text":"cards/prose.md:L7-L9\nextends beyond scope"},
            {"card":"prose","text":"cards/prose.md:L7-L6\nreversed"},
            {"card":"prose","text":"cards/other.md:L6-L7\nwrong file"},
            {"card":"prose","text":"missing location"}
        ]}"#,
        )?;
        assert_eq!(findings.len(), 1);
        assert!(findings[0].text.ends_with("in scope"));

        let full = Graph::open(project.path())?;
        assert_eq!(
            full.diagnostics
                .iter()
                .filter(|d| d.code.is_some_and(|c| c.starts_with("style/")))
                .count(),
            4
        );
        Ok(())
    }

    #[test]
    fn deletion_only_skips_style_findings_and_review() -> Result<()> {
        let project = TestDir::new();
        project.write("deck.toml", "[review]\nauto = true\nreviewers = ['style']");
        project.write(
            "cards/prose/card.md",
            "---\nid: prose\n---\nIt is worth noting the result.\n",
        );
        project.write(
            "cards/prose/statement.tex",
            "A crucial result.\n\nIt might possibly hold.\n",
        );
        project.write("cards/prose/proof.tex", "重要なのは証明である。\n");
        let before = Snapshot::take(project.path());
        project.write("cards/prose/statement.tex", "It might possibly hold.\n");
        std::fs::remove_file(project.path().join("cards/prose/proof.tex"))?;
        let decks = before.prepare(project.path(), project.path())?;
        assert_eq!(decks.len(), 1);
        assert!(decks[0].items.is_empty());
        assert!(decks[0].reviewers.is_empty());
        Ok(())
    }

    #[test]
    fn tex_edits_use_file_coordinates_and_map_to_lint_body() -> Result<()> {
        let project = TestDir::new();
        project.write("deck.toml", "[review]\nauto = true\nreviewers = ['style']");
        project.write(
            "cards/card/card.md",
            "---\nid: test\n---\nIt is worth noting this.\n",
        );
        project.write("cards/card/statement.tex", "A crucial result.\n\n\n\n\n");
        project.write(
            "cards/card/proof.tex",
            "% A comment.\n\n重要なのは等式の形である。\nここで式を使う。\n",
        );
        let before = Snapshot::take(project.path());
        project.write(
            "cards/card/proof.tex",
            "% A comment.\n\n重要なのは等式の形である。\nここで新しい式を使う。\n",
        );
        let decks = before.prepare(project.path(), project.path())?;
        let deck = &decks[0];
        assert_eq!(deck.items.len(), 1);
        assert!(deck.items[0].text.contains("重要なのは"));
        let [(Kind::Style, reviewer)] = deck.reviewers.as_slice() else {
            panic!("expected a style review")
        };
        let findings = reviewer.findings(
            r#"{"findings":[
            {"card":"test","text":"cards/card/proof.tex:L3-L4\nin scope"},
            {"card":"test","text":"cards/card/card.md:L4-L4\nwrong file"}
        ]}"#,
        )?;
        assert_eq!(findings.len(), 1);
        assert!(findings[0].text.ends_with("in scope"));
        Ok(())
    }

    #[test]
    fn scopes_lints_and_reviewers_to_changed_cards() -> Result<()> {
        let project = TestDir::new();
        project.write("deck.toml", "[review]\nauto = true");
        project.write(
            "cards/definition.md",
            "---\nid: definition\ndefines: [pivot chain]\n---\n",
        );
        project.write(
            "cards/untouched.md",
            "---\nid: untouched\nlean: [Unused.Lemma]\n---\nIt is worth noting the pivot chain.\n",
        );
        let before = Snapshot::take(project.path());
        project.write(
            "cards/touched.md",
            "---\nid: touched\nlean: [Test.Lemma]\n---\nIt is worth noting the pivot chain.\n\nAnother paragraph.\n",
        );
        project.write("cards/no-lean.md", "---\nid: no-lean\n---\n");
        let mut decks = before.prepare(project.path(), project.path())?;
        assert_eq!(decks.len(), 1);
        let deck = decks.pop().unwrap();
        assert_eq!(deck.reviewers.len(), 3);
        assert!(deck.items.iter().all(|item| item.card == "touched"));
        for code in ["undefined-term/missing-dependency", "style/ai-phrase"] {
            assert!(
                deck.items.iter().any(|item| item.text.contains(code)),
                "{code}"
            );
        }
        for (kind, review) in &deck.reviewers {
            let prompt = review.prompt(".", &[])?;
            assert!(prompt.contains("\"id\": \"touched\""));
            assert!(!prompt.contains("\"id\": \"untouched\""));
            assert_eq!(prompt.contains("\"id\": \"no-lean\""), *kind == Kind::Math);
            assert_eq!(prompt.contains("Test.Lemma"), *kind == Kind::Lean);
            let answer = match kind {
                Kind::Style => {
                    r#"{"findings":[{"card":"untouched","text":"cards/untouched.md:L5-L5\nout of scope"},{"card":"touched","text":"cards/touched.md:L5-L7\nin scope"}]}"#
                }
                Kind::Lean | Kind::Math | Kind::Output | Kind::Consistency => {
                    r#"{"findings":[{"card":"untouched","text":"out of scope"},{"card":"touched","text":"in scope"}]}"#
                }
            };
            let findings = review.findings(answer)?;
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].card, "touched");
        }
        Ok(())
    }
}
