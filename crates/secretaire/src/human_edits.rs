//! 人の編集を指示として扱い、影響を受ける所の整合を取る案を出す（cards/design/human-edits.md）。
//!
//! 利用者の保存がカードに初めて触れた時に、保存前のカードのファイルを控える。編集をやめた時
//! （保存で閉じる編集画面、ソースエディタのタブを閉じた時、ビルドを押した時）に、控えと今を
//! 比べて整合の査読役に回す。結果はコメントの下書きになり、人の編集そのものは書き換えない。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fuzukue_cards::{Card, Graph};
use fuzukue_proto::{CommentView, ServerEvent, SymbolKind};
use serde::Serialize;

use crate::auto_review::CardFiles;
use crate::cancellation::Cancelled;
use crate::graph::{card_symbols, WorkspaceGraph};
use crate::reviewers::{Baseline, Job, Kind, Review};
use crate::{comments, usage, Workspace};

/// 閉じてから確認を始めるまでの待ち。続けて別のカードを閉じたら一つにまとめる。
pub(crate) const SETTLE: Duration = Duration::from_secs(5);
/// 確認に渡す候補の上限。
const CANDIDATE_LIMIT: usize = 40;
/// 一つのファイルの差分として渡す行の上限（消えた行と増えた行の合計）。
const DIFF_LINE_LIMIT: usize = 120;
/// 覚えておく断った提案の数。
const REJECTED_LIMIT: usize = 30;
/// コメントの下書きに付ける印。消されたら断った提案として覚える。
pub(crate) const MARK: &str = "[人の編集]";

pub(crate) type DeckBaseline = BTreeMap<String, CardBaseline>;

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct CardBaseline {
    files: CardFiles,
    names: Vec<(SymbolKind, String)>,
}

/// デッキのルート → カード ID → 人が編集する前のファイルと名前。
#[derive(Default)]
pub(crate) struct Pending(std::sync::Mutex<BTreeMap<PathBuf, DeckBaseline>>);

pub(crate) struct BeforeSave<'a> {
    pending: &'a Pending,
    root: PathBuf,
    card: String,
    baseline: CardBaseline,
}

impl BeforeSave<'_> {
    pub(crate) fn record(self) {
        self.pending.0.lock().unwrap().entry(self.root).or_default()
            .entry(self.card).or_insert(self.baseline);
    }
}

fn card_snapshot(project: &Path, graph: &Graph, card: &Card) -> Result<CardFiles> {
    let mut files = CardFiles::new();
    for path in std::iter::once(&card.path).chain(&card.tex) {
        let absolute = graph.root.join(path);
        match absolute.canonicalize() {
            Ok(full) if full.starts_with(project) => {
                let bytes = std::fs::read(full).with_context(|| format!("{} を読めない", absolute.display()))?;
                files.insert(path.clone(), bytes);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("{} を読めない", absolute.display())),
        }
    }
    Ok(files)
}

fn owns(graph: &Graph, card: &Card, file: &Path) -> bool {
    std::iter::once(&card.path)
        .chain(&card.tex)
        .any(|path| same_file(&graph.root.join(path), file))
}

fn same_file(path: &Path, canonical: &Path) -> bool {
    path == canonical || path.canonicalize().is_ok_and(|path| path == canonical)
}

impl Pending {
    /// 控えがまだなければ、カードの今のファイルを控える。
    fn capture(&self, project: &Path, graph: &Graph, card: &Card) -> Result<Option<BeforeSave<'_>>> {
        if self
            .0
            .lock()
            .unwrap()
            .get(&graph.root)
            .is_some_and(|deck| deck.contains_key(&card.id))
        {
            return Ok(None);
        }
        let files = card_snapshot(project, graph, card)?;
        let names = card_symbols(&files);
        Ok(Some(BeforeSave {
            pending: self,
            root: graph.root.clone(),
            card: card.id.clone(),
            baseline: CardBaseline { files, names },
        }))
    }

    fn take(&self, root: &Path) -> Option<DeckBaseline> {
        self.0.lock().unwrap().remove(root)
    }

    /// 閉じたファイルを所有するデッキの控えだけを取り出す。
    fn take_file(&self, file: &Path) -> Option<(PathBuf, DeckBaseline)> {
        let mut pending = self.0.lock().unwrap();
        let root = pending.iter().rev().find_map(|(root, cards)| {
            cards.values().any(|card| card.files.keys().any(|path| same_file(&root.join(path), file)))
                .then(|| root.clone())
        })?;
        pending.remove_entry(&root)
    }
}

impl Workspace {
    /// 利用者がカードを保存する直前に呼ぶ（論文のカード編集）。
    pub(crate) fn before_human_card_save(&self, graph: &Graph, card: &Card, path: &Path) -> Option<BeforeSave<'_>> {
        if !crate::auto_review::enabled() || !crate::auto_paths::path_eligible(&self.project_dir, path) {
            return None;
        }
        self.human_edits.capture(&self.project_dir, graph, card).unwrap_or_else(|e| {
            tracing::warn!("人の編集の控え {}#{}: {e:#}", graph.root.display(), card.id);
            None
        })
    }

    /// 利用者がファイルを保存する直前に呼ぶ（作業台の編集、ソースエディタ）。カードのファイルだけを控える。
    pub(crate) fn before_human_file_save(&self, file: &Path, text: &str) -> Option<BeforeSave<'_>> {
        if !crate::auto_review::enabled() {
            return None;
        }
        for root in file.ancestors().skip(1).take_while(|dir| dir.starts_with(&self.project_dir)) {
            if crate::workspace::resolve_deck(&self.project_dir, root).is_err() {
                continue;
            }
            match Graph::open_local(root) {
                Ok(graph) => {
                    if let Some(card) = graph.cards.values().find(|card| owns(&graph, card, file)) {
                        if std::fs::read(file).is_ok_and(|before| before == text.as_bytes()) {
                            return None;
                        }
                        return self.before_human_card_save(&graph, card, file);
                    }
                }
                Err(e) => tracing::warn!("人の編集の控え {}: {e:#}", file.display()),
            }
        }
        None
    }

    pub(crate) fn human_file_closed(self: &Arc<Self>, file: &Path) {
        if let Some((root, baseline)) = self.human_edits.take_file(file) {
            self.start_consistency(root, baseline, SETTLE);
        }
    }

    pub(crate) fn human_deck_closed(self: &Arc<Self>, root: &Path, settle: Duration) {
        if let Some(baseline) = self.human_edits.take(root) {
            self.start_consistency(root.to_owned(), baseline, settle);
        } else if settle.is_zero() {
            if let Ok((_, deck)) = crate::workspace::resolve_deck(&self.project_dir, root) {
                self.expedite_review(&deck, Kind::Consistency);
            }
        }
    }

    pub(crate) fn start_consistency(self: &Arc<Self>, root: PathBuf, baseline: DeckBaseline, settle: Duration) {
        if !crate::auto_review::enabled() {
            return;
        }
        match crate::auto_review::wants(&self.project_dir, &root, Kind::Consistency) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!("整合の確認の設定 {}: {e:#}", root.display());
                return;
            }
        }
        let Ok((root, deck)) = crate::workspace::resolve_deck(&self.project_dir, &root) else { return };
        let Ok(work) = self.admit_work() else {
            self.drain.defer(&self.state_dir, &deck, crate::mail::session_key(),
                crate::self_update::drain::Work::HumanReview { root, baseline });
            return;
        };
        let settings = self.review_settings();
        let Some(job) = self.queue_review(&deck, Kind::Consistency, Some(Baseline::Human(baseline)), &settings) else {
            return;
        };
        let app = self.clone();
        tokio::spawn(async move {
            let _work = work;
            tokio::select! {
                biased;
                _ = job.cancel.cancelled() => {}
                _ = job.settle_now.notified() => {}
                _ = tokio::time::sleep(settle) => {}
            }
            let _run = job.gate.lock().await;
            let mut drafts = Vec::new();
            let result = async {
                let Ok(_admission) = app.admit_work() else {
                    if let Some(Baseline::Human(baseline)) = &job.baseline {
                        app.drain.defer(&app.state_dir, &deck, crate::mail::session_key(),
                            crate::self_update::drain::Work::HumanReview { root: root.clone(), baseline: baseline.clone() });
                    }
                    return Err(Cancelled.into());
                };
                job.cancel.check()?;
                app.check_consistency(&root, &deck, &job, &mut drafts).await
            }
            .await;
            let cancelled = result.as_ref().is_err_and(|error| error.is::<Cancelled>());
            app.finish_review(&deck, Kind::Consistency, &job, result);
            drop(_run);
            if !cancelled && !drafts.is_empty() && settings.apply_in_side_thread {
                if let Err(e) = crate::apply_comment_drafts(&app, &deck, String::new(), Some(drafts)).await {
                    tracing::warn!("整合の提案の反映 {deck}: {e:#}");
                }
            }
        });
    }

    async fn check_consistency(
        self: &Arc<Self>,
        root: &Path,
        deck: &str,
        job: &Arc<Job>,
        drafts: &mut Vec<CommentView>,
    ) -> Result<(usize, Option<String>)> {
        let (project, dir, data) = (self.project_dir.clone(), root.to_owned(), job.clone());
        let edits = tokio::task::spawn_blocking(move || {
            let Some(Baseline::Human(baseline)) = &data.baseline else {
                unreachable!("consistency review requires a human edit baseline");
            };
            HumanEdits::read(&project, &dir, baseline)
        }).await??;
        let Some(edits) = edits else {
            return Ok((0, None));
        };
        self.graph.invalidate();
        let candidates = self.candidates(deck, &edits.cards).await;
        let mut allowed: BTreeSet<String> = edits.cards.iter().map(|card| card.id.clone()).collect();
        allowed.extend(candidates.iter().map(|candidate| candidate.card.clone()));
        let rejected = read_rejected(root);
        let body = edits.prompt(&candidates, &rejected, &allowed)?;
        let rules = crate::rules::resolve(&self.project_dir)?;
        let review = Review::consistency(&self.project_dir, body, allowed, &rules)?;

        self.mark_review_running(deck, Kind::Consistency, job)?;
        let slug = root.file_name().and_then(|name| name.to_str()).unwrap_or("project");
        let session = format!("review-consistency-{slug}");
        let answer = self
            .run_reviewer(&review, &self.project_dir, deck, deck, &session, (&session, None), job)
            .await?;
        let mut items: Vec<_> = review
            .findings(&answer)?
            .into_iter()
            .map(|finding| CommentView {
                card: finding.card,
                quote: None,
                proof: None,
                source: None,
                text: format!("{MARK} {} {}", Kind::Consistency.label(), finding.text),
                created: usage::now(),
            })
            .collect();

        let _edit = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => return Err(Cancelled.into()),
            lock = self.comment_edit.lock() => lock,
        };
        self.with_current_review(deck, Kind::Consistency, job, || {
            if !items.is_empty() {
                let (current, _) = crate::workspace::resolve_deck(&self.project_dir, Path::new(deck))?;
                anyhow::ensure!(current == root, "整合を確かめるデッキが変わった");
                // 確認中に消えたカードへの提案は置かない。
                let graph = Graph::open_local(root)?;
                items.retain(|item| graph.cards.contains_key(&item.card));
                let mut saved = comments::read(root)?;
                saved.extend(items.iter().cloned());
                comments::save(root, &saved)?;
                self.hub.publish_snapshot(ServerEvent::Comments { deck: deck.into(), items: saved });
            }
            let result = (items.len(), items.first().map(|item| item.card.clone()));
            *drafts = std::mem::take(&mut items);
            job.mark_committed();
            Ok(result)
        })
    }

    /// 消えた名前の使用箇所を先に集めてから、編集したカードと今の名前の参照を集める。
    async fn candidates(self: &Arc<Self>, deck: &str, cards: &[EditedCard]) -> Vec<Candidate> {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        let mut lookups = Vec::new();
        for card in cards {
            lookups.extend(card.removed.iter().map(|name| (None, name.kind, &name.old)));
        }
        for card in cards {
            lookups.extend(card.symbols.iter().map(|(kind, name)| (Some(&card.id), *kind, name)));
        }
        for (card, kind, name) in lookups {
            // Several occurrences of an old name on one line count as one candidate.
            let limit = if card.is_none() { usize::MAX } else { CANDIDATE_LIMIT };
            let references = WorkspaceGraph::card_references(
                self.clone(), deck.into(), card.cloned(), (kind, name.clone()), limit,
            ).await;
            for reference in references {
                let snippet = reference.location.snippet.clone();
                for location in std::iter::once(reference.location).chain(reference.alternates) {
                    let Some(target) = location.card else { continue };
                    if !seen.insert((target.clone(), location.file.clone(), location.line)) {
                        continue;
                    }
                    found.push(Candidate {
                        card: target,
                        file: location.file,
                        line: location.line,
                        uses: name.clone(),
                        note: card.is_none().then(|| harness_format!(
                            "uses the old name `{}` (renamed/removed in this edit)",
                            "旧名 `{}` を使用している（今回の編集で改名・削除）",
                            name,
                        )),
                        snippet: snippet.clone(),
                    });
                    if found.len() == CANDIDATE_LIMIT {
                        return found;
                    }
                }
            }
        }
        found
    }
}

#[derive(Debug, Serialize)]
struct Candidate {
    card: String,
    /// プロジェクト相対のパス。
    file: String,
    line: usize,
    /// 編集したカードの何を使っているか。
    uses: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    snippet: String,
}

struct EditedCard {
    id: String,
    /// 参照を探す名前。
    symbols: Vec<(SymbolKind, String)>,
    removed: Vec<RemovedName>,
    /// プロジェクト相対のパスと、そのファイルの差分。
    diffs: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct RemovedName {
    kind: SymbolKind,
    old: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    new: Option<String>,
}

fn removed_names(before: &[(SymbolKind, String)], after: &[(SymbolKind, String)]) -> Vec<RemovedName> {
    let removed: Vec<_> = before.iter().filter(|name| !after.contains(name)).collect();
    let added: Vec<_> = after.iter().filter(|name| !before.contains(name)).collect();
    removed.iter().map(|(kind, old)| {
        let mut same_kind = added.iter().filter(|(other, _)| other == kind);
        let new = same_kind.next().filter(|_| {
            same_kind.next().is_none() && removed.iter().filter(|(other, _)| other == kind).count() == 1
        }).map(|(_, name)| name.clone());
        RemovedName { kind: *kind, old: old.clone(), new }
    }).collect()
}

struct HumanEdits {
    cards: Vec<EditedCard>,
}

impl HumanEdits {
    /// 控えと今を比べる。変わったカードがなければ `None`。
    fn read(project: &Path, root: &Path, baseline: &DeckBaseline) -> Result<Option<Self>> {
        let graph = Graph::open_local(root)?;
        let mut cards = Vec::new();
        for (id, snapshot) in baseline {
            let before = &snapshot.files;
            let Some(card) = graph.cards.get(id).or_else(|| {
                graph.cards.values().find(|card| before.contains_key(&card.path))
            }) else { continue };
            let after = card_snapshot(project, &graph, card)?;
            if &after == before {
                continue;
            }
            let mut diffs = Vec::new();
            let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
            let eligible = crate::auto_paths::eligible(project, paths.iter().map(|path| root.join(path)))?;
            for path in paths.into_iter().filter(|path| eligible.contains(&root.join(path))) {
                let old = before.get(path).map(|bytes| String::from_utf8_lossy(bytes)).unwrap_or_default();
                let new = after.get(path).map(|bytes| String::from_utf8_lossy(bytes)).unwrap_or_default();
                if let Some(diff) = line_diff(&old, &new) {
                    let rel = root.join(path);
                    let rel = rel.strip_prefix(project).unwrap_or(&rel);
                    diffs.push((rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"), diff));
                }
            }
            if diffs.is_empty() {
                continue;
            }
            let names = card_symbols(&after);
            let removed = removed_names(&snapshot.names, &names);
            let mut symbols = vec![(SymbolKind::Card, card.id.clone())];
            symbols.extend(names);
            cards.push(EditedCard { id: card.id.clone(), symbols, removed, diffs });
        }
        Ok((!cards.is_empty()).then_some(Self { cards }))
    }

    fn prompt(&self, candidates: &[Candidate], rejected: &[String], allowed: &BTreeSet<String>) -> Result<String> {
        let mut body = String::from(harness_text!(
            "Human edits (project-relative paths; `-` is a removed line with its old line number, `+` an added line with its new line number):\n",
            "人の編集（パスはプロジェクト相対。`-` は消えた行と元の行番号、`+` は増えた行と今の行番号）:\n",
        ));
        for card in &self.cards {
            body.push_str(&harness_format!("\n### Card `{}`\n", "\n### カード `{}`\n", card.id));
            if !card.removed.is_empty() {
                body.push_str(harness_text!(
                    "Renamed or removed names (`new` is present for a possible rename):\n",
                    "改名・削除された名前（改名とみなせる場合は `new` に新しい名前を示す）:\n",
                ));
                body.push_str(&serde_json::to_string_pretty(&card.removed)?);
                body.push('\n');
            }
            for (path, diff) in &card.diffs {
                body.push_str(&format!("{path}\n```diff\n{diff}```\n"));
            }
        }
        body.push_str(harness_text!(
            "\nCandidate locations that use current or old names from the edited cards (found through links and the symbol index):\n",
            "\n編集したカードの今の名前や旧名を使っている候補（リンクと記号の索引から集めた）:\n",
        ));
        body.push_str(&serde_json::to_string_pretty(candidates)?);
        body.push('\n');
        if !rejected.is_empty() {
            body.push_str(harness_text!(
                "\nProposals the person rejected before. Do not repeat the same intent:\n",
                "\n前に利用者が断った提案。同じ趣旨の提案を繰り返さない:\n",
            ));
            body.push_str(&serde_json::to_string_pretty(rejected)?);
            body.push('\n');
        }
        body.push_str(&harness_format!(
            "\nCards you may report on: {}\nRead the source at the paths above. Do not edit files or comments, write through external tools, or delegate changes.\nReturn one JSON object only, without a preamble or code fence.\n",
            "\n指摘してよいカード: {}\n上のパスから原文を読む。ファイルやコメントを編集せず、外部ツールでの書き込みや変更作業の委任も行わない。\n最終回答は JSON オブジェクト一つだけにする。前置きやコードフェンスを付けない。\n",
            serde_json::to_string(allowed)?,
        ));
        Ok(body)
    }
}

/// 消えた行と増えた行を、行番号付きで並べる。変わっていなければ `None`。
fn line_diff(before: &str, after: &str) -> Option<String> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut added = vec![false; new.len()];
    let mut removed = vec![false; old.len()];
    crate::auto_review::mark_changed(&old, &new, &mut removed, &mut added);
    if !added.contains(&true) && !removed.contains(&true) {
        return None;
    }
    // 消えた行と増えた行を、元の位置の順に混ぜて並べる。
    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && removed[i] {
            lines.push(format!("-{:>4}: {}", i + 1, old[i]));
            i += 1;
        } else if j < new.len() && added[j] {
            lines.push(format!("+{:>4}: {}", j + 1, new[j]));
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    let total = lines.len();
    let mut out = String::new();
    for line in lines.iter().take(DIFF_LINE_LIMIT) {
        out.push_str(line);
        out.push('\n');
    }
    if total > DIFF_LINE_LIMIT {
        out.push_str(&harness_format!(
            "… {} more changed lines omitted\n",
            "… 残り {} 行の変更は省いた\n",
            total - DIFF_LINE_LIMIT,
        ));
    }
    Some(out)
}

fn rejected_path(root: &Path) -> PathBuf {
    root.join(".fuzukue").join("rejected-proposals.json")
}

fn read_rejected(root: &Path) -> Vec<String> {
    std::fs::read(fuzukue_proto::compat::workspace_data(root, "rejected-proposals.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 利用者が消した整合の提案を、次の確認で繰り返さないように覚える。
pub(crate) fn remember_rejected(root: &Path, removed: &CommentView) -> Result<()> {
    let prefix = format!("{MARK} {} ", Kind::Consistency.label());
    if !removed.text.starts_with(&prefix) {
        return Ok(());
    }
    let mut rejected = read_rejected(root);
    let text = format!("{}: {}", removed.card, removed.text.trim_start_matches(MARK).trim());
    rejected.retain(|old| *old != text);
    rejected.push(text);
    let excess = rejected.len().saturating_sub(REJECTED_LIMIT);
    rejected.drain(..excess);
    let path = rejected_path(root);
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), &rejected)?;
    tmp.persist(&path).with_context(|| format!("{} に書けない", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_records_the_first_success_and_takes_only_the_owning_deck() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let root = dir.path();
        let nested = root.join("nested");
        let card = "cards/a.md";
        let before = "---\nid: a\ndefines: [Before]\n---\nBefore\n";
        let after = "---\nid: a\ndefines: [After]\n---\nAfter\n";
        for deck in [root, nested.as_path()] {
            std::fs::create_dir_all(deck.join("cards"))?;
            std::fs::write(deck.join(card), before)?;
        }
        let outer = Graph::open_local(root)?;
        let inner = Graph::open_local(&nested)?;
        let pending = Pending::default();

        let capture = |graph: &Graph| pending.capture(&outer.root, graph, &graph.cards["a"]);
        drop(capture(&outer)?);
        assert!(pending.take(&outer.root).is_none());
        capture(&outer)?.unwrap().record();
        capture(&inner)?.unwrap().record();
        std::fs::write(root.join(card), after)?;
        assert!(capture(&outer)?.is_none());
        assert!(pending.take_file(&nested.join("not-a-card.md")).is_none());
        let outer_files = pending.take(&outer.root).unwrap();
        assert_eq!(outer_files["a"].files[Path::new(card)], before.as_bytes());
        assert_eq!(outer_files["a"].names, [(SymbolKind::Term, "Before".into())]);
        let (owner, inner_files) = pending.take_file(&inner.root.join(card)).unwrap();
        assert_eq!(owner, inner.root);
        assert_eq!(inner_files["a"].files[Path::new(card)], before.as_bytes());
        assert!(pending.take(&inner.root).is_none());

        capture(&outer)?.unwrap().record();
        assert_eq!(pending.take(&outer.root).unwrap()["a"].files[Path::new(card)], after.as_bytes());
        Ok(())
    }

    #[tokio::test]
    async fn removed_names_pair_unambiguous_renames_and_find_old_uses() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let root = dir.path();
        let card_dir = root.join("cards/a");
        std::fs::create_dir_all(&card_dir)?;
        std::fs::write(card_dir.join("card.md"), "---\nid: a\ndefines: [Old, Steady]\nlean: [N.old]\n---\nBody\n")?;
        std::fs::write(card_dir.join("statement.tex"), r"\label{one}\label{two}\newcommand{\old}{x}")?;
        dir.write("cards/b/card.md", "---\nid: b\nlean: [N.old, N.other]\n---\nAn old usage.\n");
        dir.write("cards/b/statement.tex", "\\ref{one}\n\\ref{two}\n\\old\n");
        let graph = Graph::open_local(root)?;
        let pending = Pending::default();
        pending.capture(&graph.root, &graph, &graph.cards["a"])?.unwrap().record();
        std::fs::write(card_dir.join("card.md"), "---\nid: a\ndefines: [Steady, New]\nlean: [M.old]\n---\nBody\n")?;
        std::fs::write(card_dir.join("statement.tex"), r"\label{three}\newcommand{\new}{x}")?;

        let baseline = pending.take(&graph.root).unwrap();
        let edits = HumanEdits::read(&graph.root, &graph.root, &baseline)?.unwrap();
        let change = |kind, old: &str, new: Option<&str>| RemovedName {
            kind, old: old.into(), new: new.map(str::to_owned),
        };
        assert_eq!(edits.cards[0].removed, [
            change(SymbolKind::Term, "Old", Some("New")),
            change(SymbolKind::Label, "one", None),
            change(SymbolKind::Label, "two", None),
            change(SymbolKind::Macro, r"\old", Some(r"\new")),
            change(SymbolKind::Lean, "N.old", Some("M.old")),
        ]);
        let app = Arc::new(Workspace::new(
            root.to_owned(), String::new(), root.join(".fuzukue"), Arc::new(crate::Hub::new()),
        ));
        let candidates = app.candidates("", &edits.cards).await;
        let old_uses: BTreeSet<_> = candidates.iter().filter(|candidate| candidate.note.is_some())
            .map(|candidate| (candidate.card.as_str(), candidate.uses.as_str())).collect();
        assert_eq!(old_uses, BTreeSet::from([
            ("b", "Old"), ("b", "one"), ("b", "two"), ("b", r"\old"), ("b", "N.old"),
        ]));
        Ok(())
    }

    #[test]
    fn line_diff_lists_removed_and_added_lines_in_order() {
        let before = "a\nold term\nb\nc\n";
        let after = "a\nnew term\nb\nc\nd\n";
        assert_eq!(
            line_diff(before, after).unwrap(),
            "-   2: old term\n+   2: new term\n+   5: d\n",
        );
        assert_eq!(line_diff("a\nb\n", "b\na\n").unwrap(), "+   1: b\n-   2: b\n");
        assert_eq!(line_diff("same\n", "same\n"), None);
    }

    #[test]
    fn rejected_proposals_are_remembered_once_and_capped() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let root = dir.path();
        let proposal = |text: &str| CommentView {
            card: "thm-a".into(),
            quote: None,
            proof: None,
            source: None,
            text: text.into(),
            created: 0,
        };
        remember_rejected(root, &proposal("[文体査読] not a human edit"))?;
        assert!(read_rejected(root).is_empty());
        remember_rejected(root, &proposal("[人の編集] [整合] rename X"))?;
        remember_rejected(root, &proposal("[人の編集] [整合] rename X"))?;
        assert_eq!(read_rejected(root), vec!["thm-a: [整合] rename X".to_string()]);
        for i in 0..40 {
            remember_rejected(root, &proposal(&format!("{MARK} [整合] {i}")))?;
        }
        let rejected = read_rejected(root);
        assert_eq!(rejected.len(), REJECTED_LIMIT);
        assert_eq!(rejected.last().unwrap(), "thm-a: [整合] 39");
        Ok(())
    }
}
