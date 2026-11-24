//! Source-level checks shared by Lean status, reviewer prompts and branch review.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use fuzukue_proto::{BranchDiffFile, LeanBranchReview, LeanClaimChange, LeanDocFinding};
use fuzukue_view::lean::{self, Declaration, Page};
use serde::Serialize;

use crate::git::Repo;

pub(crate) fn sources(project: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
                if !matches!(
                    entry.file_name().to_str(),
                    Some("target" | "node_modules" | "build")
                ) {
                    walk(&entry.path(), out);
                }
            } else if kind.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "lean")
                && entry.file_name() != "lakefile.lean"
            {
                out.push(entry.path());
            }
        }
    }
    let mut files = Vec::new();
    walk(project, &mut files);
    files.sort();
    files
}

pub(crate) fn is_lean_workspace(root: &Path) -> bool {
    [root.to_path_buf(), root.join("lean")]
        .into_iter()
        .any(|dir| dir.join("lakefile.toml").is_file() || dir.join("lakefile.lean").is_file())
        || !sources(root).is_empty()
}

#[derive(Debug, Serialize)]
pub(crate) struct DocCheck {
    pub findings: Vec<LeanDocFinding>,
    pub incomplete: Vec<String>,
}

fn finding(path: &str, decl: &Declaration, code: &'static str) -> LeanDocFinding {
    LeanDocFinding {
        path: path.into(),
        declaration: decl.name.clone(),
        line: decl.position.line + 1,
        card: decl.card.clone(),
        code: code.into(),
    }
}

pub(crate) async fn doc_check(project: &Path) -> Result<DocCheck> {
    let repo = Repo::open(project).await.ok();
    let mut base = None;
    let mut renames = BTreeMap::new();
    if let Some(repo) = &repo {
        for reference in ["refs/heads/main", "refs/remotes/origin/HEAD", "HEAD"] {
            if let Ok(output) = repo.git(&["merge-base", reference, "HEAD"]).await {
                base = Some(output.trim().to_owned());
                break;
            }
        }
        if let Some(base) = &base {
            if let Ok(names) = repo
                .git(&["diff", "--find-renames", "--name-status", "-z", base, "--"])
                .await
            {
                let mut fields = names.split_terminator('\0');
                while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
                    if status.starts_with(['R', 'C']) {
                        if let Some(new) = fields.next() {
                            renames.insert(new.to_owned(), path.to_owned());
                        }
                    }
                }
            }
        }
    }
    let mut result = DocCheck {
        findings: Vec::new(),
        incomplete: Vec::new(),
    };
    for path in sources(project) {
        let label = path.strip_prefix(project)?.to_string_lossy();
        let Ok(source) = std::fs::read_to_string(&path) else {
            result.incomplete.push(label.into_owned());
            continue;
        };
        let page = lean::parse(&source);
        if page.incomplete {
            result.incomplete.push(label.to_string());
        }
        result.findings.extend(
            page.declarations()
                .filter(|d| d.needs_doc() && d.doc.is_empty())
                .map(|d| finding(&label, d, "missing_docstring")),
        );
        if let (Some(repo), Some(base)) = (&repo, &base) {
            let old_path = renames
                .get(label.as_ref())
                .map_or(label.as_ref(), String::as_str);
            if let Ok(before) = read_revision(repo, base, old_path).await {
                let prior = lean::parse(&before);
                let old = indexed(&prior);
                result.findings.extend(
                    page.declarations()
                        .filter(|d| old.get(d.name.as_str()).is_some_and(|b| stale_doc(b, d)))
                        .map(|d| finding(&label, d, "unchanged_docstring")),
                );
            }
        }
    }
    Ok(result)
}

fn indexed(page: &Page) -> BTreeMap<&str, &Declaration> {
    page.declarations().map(|d| (d.name.as_str(), d)).collect()
}

fn stale_doc(old: &Declaration, new: &Declaration) -> bool {
    new.kind == "theorem" && !new.same_statement(old) && old.doc == new.doc
}

async fn read_revision(repo: &Repo, revision: &str, path: &str) -> Result<String> {
    repo.git(&["show", "--no-textconv", &format!("{revision}:{path}")])
        .await
}

pub(crate) async fn branch_claims(
    repo: &Repo,
    base: &str,
    tip: &str,
    files: &[BranchDiffFile],
) -> Result<Option<LeanBranchReview>> {
    let mut review = None;
    for file in files {
        let old_path = file.old_path.as_deref().unwrap_or(&file.path);
        let old_lean = old_path.ends_with(".lean") && !file.status.starts_with(['A', 'C']);
        let new_lean = file.path.ends_with(".lean") && !file.status.starts_with('D');
        if !old_lean && !new_lean {
            continue;
        }
        let review = review.get_or_insert_with(LeanBranchReview::default);
        let before = if old_lean {
            read_revision(repo, base, old_path).await
        } else {
            Ok(String::new())
        };
        let after = if new_lean {
            read_revision(repo, tip, &file.path).await
        } else {
            Ok(String::new())
        };
        let (Ok(before), Ok(after)) = (before, after) else {
            review.incomplete.push(file.path.clone());
            continue;
        };
        compare_file(
            review,
            &file.path,
            &lean::parse(&before),
            &lean::parse(&after),
        );
    }
    Ok(review)
}

fn compare_file(review: &mut LeanBranchReview, path: &str, before: &Page, after: &Page) {
    if before.incomplete || after.incomplete {
        review.incomplete.push(path.into());
    }
    let mut old = indexed(before);
    for new in after.declarations() {
        let prior = old.remove(new.name.as_str());
        if let Some(prior) = prior.filter(|prior| new.same_statement(prior)) {
            if prior.proof != new.proof {
                if new.kind == "theorem" {
                    review.proof_only += 1;
                } else {
                    review.changes.push(change(path, Some(prior), Some(new)));
                }
            }
        } else {
            review.changes.push(change(path, prior, Some(new)));
        }
    }
    for prior in old.into_values() {
        review.changes.push(change(path, Some(prior), None));
    }
}

fn change(
    path: &str,
    before: Option<&Declaration>,
    after: Option<&Declaration>,
) -> LeanClaimChange {
    let decl = after.or(before).expect("one revision has the declaration");
    let claim = |d: &Declaration| {
        let mut claim = d.claim();
        if d.kind != "theorem" && !d.proof.is_empty() {
            claim.push_str("\n");
            claim.push_str(&d.proof);
        }
        claim
    };
    LeanClaimChange {
        name: decl.name.clone(),
        path: path.into(),
        before: before.map(claim),
        after: after.map(claim),
        before_doc: before.map_or_else(String::new, |d| d.doc.clone()),
        after_doc: after.map_or_else(String::new, |d| d.doc.clone()),
        card: after
            .and_then(|d| d.card.clone())
            .or_else(|| before.and_then(|d| d.card.clone())),
        card_deck: None,
        stale_doc: before.zip(after).is_some_and(|(b, a)| stale_doc(b, a)),
    }
}

fn decks(project: &Path) -> Vec<PathBuf> {
    let mut roots = fuzukue_cards::find_decks(project);
    if roots.is_empty() {
        roots.push(project.to_owned());
    }
    roots
}

pub(crate) fn reading_cards(
    project: &Path,
    preferred_deck: &str,
    declarations: &[(String, Option<String>)],
) -> Vec<fuzukue_proto::LeanReadingCard> {
    let mut matches = BTreeMap::new();
    for root in decks(project) {
        let Ok(relative) = root.strip_prefix(project) else {
            continue;
        };
        let deck = relative.to_string_lossy();
        let Ok(graph) = fuzukue_cards::Graph::open(&root) else {
            continue;
        };
        let linked: BTreeMap<_, _> = graph
            .cards
            .values()
            .filter(|card| !card.external)
            .flat_map(|card| card.lean.iter().map(move |name| (name.as_str(), card)))
            .collect();
        for (name, id) in declarations {
            let forward = linked.get(name.as_str()).copied();
            let reverse = id
                .as_ref()
                .and_then(|id| graph.cards.get(id))
                .filter(|card| !card.external);
            let Some(card) = reverse.or(forward) else {
                continue;
            };
            let score = 2 * u8::from(card.lean.contains(name))
                + 2 * u8::from(reverse.is_some())
                + u8::from(deck == preferred_deck);
            if matches.get(name).is_some_and(|(prior, _)| *prior >= score) {
                continue;
            }
            let statement = fuzukue_view::card_section_ranges(&card.body)
                .into_iter()
                .filter(|(label, _)| matches!(*label, "" | "Statement" | "主張"))
                .map(|(_, range)| &card.body[range])
                .collect::<Vec<_>>()
                .join("\n\n");
            matches.insert(
                name.clone(),
                (
                    score,
                    fuzukue_proto::LeanReadingCard {
                        declaration: name.clone(),
                        deck: deck.to_string(),
                        id: card.id.clone(),
                        title: card.title.clone(),
                        statement,
                    },
                ),
            );
        }
    }
    matches.into_values().map(|(_, link)| link).collect()
}

pub(crate) fn card_links(project: &Path) -> BTreeMap<String, String> {
    decks(project)
        .into_iter()
        .filter_map(|deck| fuzukue_cards::Graph::open(&deck).ok())
        .flat_map(|graph| graph.cards.into_values())
        .filter(|card| !card.external)
        .flat_map(|card| {
            card.lean
                .into_iter()
                .map(move |name| (name, card.id.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_claims_and_proofs_independently() {
        let a = lean::parse(
            "/-- Same prose. -/\ntheorem a : True := trivial\ntheorem b : True := trivial\n",
        );
        let b = lean::parse(
            "/-- Same prose. -/\ntheorem a : 1 = 1 := rfl\ntheorem b : True := by trivial\n",
        );
        let mut review = LeanBranchReview::default();
        compare_file(&mut review, "Test.lean", &a, &b);
        assert_eq!(review.proof_only, 1);
        assert_eq!(review.changes.len(), 1);
        assert!(review.changes[0].stale_doc);
        assert!(review.unconfirmed());
    }
}
