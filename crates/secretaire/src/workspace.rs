//! プロジェクトのデッキを見つけ、縦タブとスレッドの文脈に使う。
//!
//! - `research/<id>/plan.md`（frontmatter に title / summary / status / next）が研究 1 つ。
//! - 研究のディレクトリの Markdown が参照する `papers/<name>` や `writing/<name>` が、その研究の原稿。
//!   原稿の `harness*` デッキと PDF（`output/<name>*.pdf` またはデッキの `build/`）も集める。
//! - 原稿を共有する研究どうしを「関連」としてつなぐ（絡み合い）。
//! - `research/` がなければ、プロジェクト全体を 1 つのタブにする。

use anyhow::Context as _;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fuzukue_cards::DeckConfig;
use fuzukue_proto::{PaperView, RelatedTab, TabView};
use regex::Regex;

fn frontmatter(text: &str) -> serde_yaml::Value {
    fuzukue_cards::split_frontmatter(text)
        .and_then(|(f, _)| serde_yaml::from_str(f).ok())
        .unwrap_or(serde_yaml::Value::Null)
}

fn rel(project: &Path, p: &Path) -> String {
    p.strip_prefix(project).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

fn papers_for(project: &Path, paths: &BTreeMap<PathBuf, usize>, pdfs: &[String]) -> Vec<PaperView> {
    let mut papers: Vec<PaperView> = paths
        .iter()
        .filter_map(|(path, &refs)| {
            let dir = project.join(path);
            let name = path.file_name()?.to_string_lossy().into_owned();
            if !dir.is_dir() {
                return None;
            }
            let mut decks: Vec<String> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("harness")
                    && (e.path().join("harness.toml").is_file() || e.path().join("cards").is_dir()))
                .map(|e| rel(project, &e.path()))
                .collect();
            decks.sort();
            let prefix = format!("output/{name}");
            let pdfs = pdfs.iter().filter(|p| p.starts_with(&prefix)
                || decks.iter().any(|d| Path::new(p).starts_with(Path::new(d).join("build"))))
                .cloned().collect();
            Some(PaperView { name, refs, decks, pdfs })
        })
        .collect();
    // 参照の多い順（その研究の中心の原稿が上に来る）
    papers.sort_by(|a, b| b.refs.cmp(&a.refs).then(a.name.cmp(&b.name)));
    papers
}

/// deck.toml の形（新しい形）。研究のデッキ 1 つが 1 タブ。どの研究にも属さない本・原稿もタブにする。
fn scan_decks(project: &Path, root: &Path) -> Vec<TabView> {
    use std::path::PathBuf;
    let norm = |p: PathBuf| -> String {
        // ".." を畳んでプロジェクトからの相対にする
        let mut parts: Vec<String> = Vec::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into()),
                _ => {}
            }
        }
        parts.join("/")
    };
    let decks: Vec<(String, fuzukue_cards::DeckConfig)> = fuzukue_cards::find_decks(root)
        .into_iter()
        .filter_map(|d| fuzukue_cards::DeckConfig::read(&d).ok().map(|c| (rel(project, &d), c)))
        .filter(|(_, cfg)| cfg.meta.kind != crate::memos::DECK)
        .collect();
    let meta_of = |path: &str| decks.iter().find(|(p, _)| p == path).map(|(_, c)| c.meta.clone());
    let paper_view = |path: &str| {
        let cfg = decks.iter().find(|(p, _)| p == path).map(|(_, cfg)| cfg);
        let name = deck_title(&project.join(path), cfg);
        let pdfs = cfg.into_iter().flat_map(|cfg| &cfg.meta.outputs)
            .map(|output| norm(PathBuf::from(path).join(output)))
            .filter(|pdf| project.join(pdf).is_file()).collect();
        PaperView { name, refs: 0, decks: vec![path.to_string()], pdfs }
    };
    let mut tabs = Vec::new();
    let mut claimed = BTreeSet::new();
    // 研究ごとに「触れているデッキ」の集合（papers と external の先）
    let mut touches: Vec<BTreeSet<String>> = Vec::new();
    for (path, cfg) in decks.iter().filter(|(_, c)| c.meta.kind == "research") {
        let papers: Vec<String> = cfg.meta.papers.iter().map(|p| norm(PathBuf::from(path).join(p))).collect();
        claimed.extend(papers.iter().cloned());
        let mut touched: BTreeSet<String> = papers.iter().cloned().collect();
        let deck = project.join(path);
        touched.extend(cfg.external_roots(project, &deck).filter_map(|(_, root)| {
            Some(norm(root.ok()?.strip_prefix(project).ok()?.to_path_buf()))
        }));
        touches.push(touched);
        let m = &cfg.meta;
        tabs.push(TabView {
            id: path.rsplit('/').next().unwrap_or(path).to_string(),
            title: deck_title(&project.join(path), Some(cfg)),
            summary: m.summary.clone(),
            status: m.status.clone(),
            next: m.next.clone(),
            plan: None,
            deck: Some(path.clone()),
            kind: "research".into(),
            has_exploration: project.join(path).join(crate::exploration::TREE_PATH).is_file(),
            papers: papers.iter().map(|p| paper_view(p)).collect(),
            notes: Vec::new(),
            lean: Vec::new(),
            related: Vec::new(),
        });
    }
    // 絡み合い: 研究どうしが触れているデッキ（原稿の共有、別の研究のカードの参照）
    let research_paths: Vec<String> = tabs.iter().filter_map(|t| t.deck.clone()).collect();
    let ids: Vec<String> = tabs.iter().map(|t| t.id.clone()).collect();
    for i in 0..tabs.len() {
        for j in 0..tabs.len() {
            if i == j {
                continue;
            }
            let mut shared: BTreeSet<String> = touches[i].intersection(&touches[j]).cloned().collect();
            if touches[i].contains(&research_paths[j]) || touches[j].contains(&research_paths[i]) {
                shared.insert(research_paths[j].clone());
            }
            if !shared.is_empty() {
                let shared = shared.iter().map(|p| meta_of(p).filter(|m| !m.title.is_empty()).map(|m| m.title).unwrap_or(p.clone())).collect();
                tabs[i].related.push(RelatedTab { id: ids[j].clone(), shared });
            }
        }
    }
    // 既存タブの順序を保ち、研究に含まれていた原稿を最後に加える。
    let other_decks = decks.iter().filter(|(path, c)| {
        is_paper(&project.join(path), c) || matches!(c.meta.kind.as_str(), "book" | "slides" | "document")
    });
    let (claimed_decks, independent): (Vec<_>, Vec<_>) =
        other_decks.partition(|(path, _)| claimed.contains(path));
    for (path, cfg) in independent.into_iter().chain(claimed_decks) {
        let paper = paper_view(path);
        let related = ids.iter().zip(&touches)
            .filter(|(_, paths)| paths.contains(path))
            .map(|(id, _)| RelatedTab { id: id.clone(), shared: vec![paper.name.clone()] })
            .collect();
        let m = &cfg.meta;
        tabs.push(TabView {
            id: path.rsplit('/').next().unwrap_or(path).to_string(),
            title: paper.name.clone(),
            summary: m.summary.clone(),
            status: m.status.clone(),
            next: m.next.clone(),
            plan: None,
            deck: Some(path.clone()),
            kind: if is_paper(&project.join(path), cfg) { "paper".into() } else { m.kind.clone() },
            has_exploration: false,
            papers: vec![paper],
            notes: Vec::new(),
            lean: Vec::new(),
            related,
        });
    }
    tabs
}

pub fn scan(project: &Path, pdfs: &[String]) -> Vec<TabView> {
    // fuzukue.toml がある研究室ではすべてのデッキを探す。互換モードでは
    // research/ にあるデッキだけを見る（examples/ などのサンプルを拾わない）。
    if fuzukue_proto::compat::workspace_config(project).is_file() {
        let decks = scan_decks(project, project);
        if !decks.is_empty() {
            return unique_tab_ids(decks);
        }
    }
    let paper_re = Regex::new(r"(?:papers|writing)/[A-Za-z0-9][A-Za-z0-9_-]*").unwrap();
    let lean_re = Regex::new(r"lean/([A-Za-z0-9][A-Za-z0-9_-]*)").unwrap();
    let research = project.join("research");
    let mut tabs = scan_decks(project, &research);
    let modern: BTreeSet<_> = tabs.iter().map(|t| t.id.clone()).collect();
    let mut dirs: Vec<_> = std::fs::read_dir(&research)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("plan.md").is_file() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('_')))
        .collect();
    dirs.sort();
    for dir in dirs {
        let id = dir.file_name().unwrap().to_string_lossy().to_string();
        if modern.contains(&id) {
            continue;
        }
        let plan = std::fs::read_to_string(dir.join("plan.md")).unwrap_or_default();
        let fm = frontmatter(&plan);
        let mut paper_paths: BTreeMap<PathBuf, usize> = BTreeMap::new();
        let mut lean: BTreeSet<String> = BTreeSet::new();
        let mut notes = Vec::new();
        for entry in walkdir::WalkDir::new(&dir).into_iter()
            .filter_entry(|entry| entry.depth() == 0 || (
                !crate::cards::is_ignored(project, entry.path())
                    && !entry.file_name().to_str().is_some_and(|name| matches!(name, "build" | "_build" | "dist"))
            ))
            .filter_map(Result::ok)
        {
            let p = entry.path();
            if !entry.file_type().is_file() || p.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = std::fs::read_to_string(p).unwrap_or_default();
            for path in paper_re.find_iter(&text) {
                *paper_paths.entry(PathBuf::from(path.as_str())).or_default() += 1;
            }
            for c in lean_re.captures_iter(&text) {
                if project.join("lean").join(&c[1]).is_dir() {
                    lean.insert(c[1].to_string());
                }
            }
            if p.file_name().is_some_and(|n| n != "plan.md") {
                notes.push(rel(project, p));
            }
        }
        notes.sort();
        let s = |k: &str| fm[k].as_str().unwrap_or("").trim().to_string();
        tabs.push(TabView {
            id: id.clone(),
            title: if s("title").is_empty() { id.clone() } else { s("title") },
            summary: s("summary"),
            status: s("status"),
            next: s("next"),
            plan: Some(rel(project, &dir.join("plan.md"))),
            deck: Some(rel(project, &dir)),
            kind: "research".into(),
            has_exploration: dir.join(crate::exploration::TREE_PATH).is_file(),
            papers: papers_for(project, &paper_paths, pdfs),
            notes,
            lean: lean.into_iter().collect(),
            related: Vec::new(),
        });
    }
    if tabs.is_empty() {
        // 研究の区分がないプロジェクト（fuzukue 自身、本のデッキ）は 1 タブ。
        let name = project.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let writing = std::fs::read_dir(project.join("writing")).into_iter().flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_type().ok()?.is_dir()
                .then(|| (Path::new("writing").join(entry.file_name()), 0)))
            .collect();
        let mut papers = papers_for(project, &writing, pdfs);
        papers.retain(|paper| !paper.decks.is_empty());
        if papers.is_empty() {
            // A standalone paper can have its own config without a fuzukue.toml workspace.
            let decks: Vec<_> = fuzukue_cards::find_decks(project).into_iter()
                .filter(|deck| deck != &project.join(crate::memos::DECK)).collect();
            let standalone = if project.join("harness.toml").is_file() {
                Some(project)
            } else if let [deck] = decks.as_slice() {
                DeckConfig::read(deck).is_ok_and(|cfg| matches!(cfg.meta.kind.as_str(), "paper" | "book")).then_some(deck.as_path())
            } else {
                None
            };
            if let Some(deck) = standalone {
                let path = rel(project, deck);
                let prefix = Path::new(&path).join("build");
                let pdfs = pdfs.iter()
                    .filter(|pdf| path.is_empty() || Path::new(pdf).starts_with(&prefix))
                    .cloned().collect();
                papers.push(PaperView { name: name.clone(), refs: 0, decks: vec![path], pdfs });
            }
        }
        if papers.is_empty() || (project.join("cards").is_dir() && !papers.iter().any(|p| p.decks.iter().any(String::is_empty))) {
            let decks = if project.join("cards").is_dir() { vec![String::new()] } else { Vec::new() };
            papers.insert(0, PaperView { name: name.clone(), refs: 0, decks, pdfs: pdfs.to_vec() });
        }
        let title = if let [paper] = papers.as_slice() {
            let mut titles = paper.decks.iter().map(|deck| paper_title(&project.join(deck)));
            titles.next().flatten().filter(|title| titles.all(|other| other.as_ref() == Some(title)))
        } else {
            None
        };
        tabs.push(TabView {
            id: "project".into(),
            title: title.unwrap_or(name),
            summary: String::new(),
            status: String::new(),
            next: String::new(),
            plan: None,
            deck: None,
            kind: String::new(),
            has_exploration: false,
            papers,
            notes: Vec::new(),
            lean: Vec::new(),
            related: Vec::new(),
        });
    }
    // 絡み合い: 原稿を共有するタブどうし
    let paper_sets: Vec<BTreeSet<String>> = tabs.iter().map(|t| t.papers.iter().map(|p| p.name.clone()).collect()).collect();
    let ids: Vec<String> = tabs.iter().map(|t| t.id.clone()).collect();
    for (i, tab) in tabs.iter_mut().enumerate() {
        for (j, other) in paper_sets.iter().enumerate() {
            if i == j {
                continue;
            }
            let shared: Vec<String> = paper_sets[i].intersection(other).cloned().collect();
            if !shared.is_empty() && !tab.related.iter().any(|other| &other.id == &ids[j]) {
                tab.related.push(RelatedTab { id: ids[j].clone(), shared });
            }
        }
    }
    if let [tab] = tabs.as_mut_slice() {
        if tab.id == "project" {
            if let [paper] = tab.papers.as_slice() {
                if let [path] = paper.decks.as_slice() {
                    let attachment = if path.is_empty() { "." } else { path };
                    if deck_path(project, attachment).is_ok() {
                        tab.deck = Some(path.clone());
                        if let Ok(cfg) = DeckConfig::read(&project.join(path)) {
                            let root = project.join(path);
                            tab.title = deck_title(&root, Some(&cfg));
                            tab.kind = if is_paper(&root, &cfg) { "paper".into() } else { cfg.meta.kind };
                            tab.summary = cfg.meta.summary;
                            tab.next = cfg.meta.next;
                        }
                    }
                }
            }
        }
    }
    // 互換モードでも、原稿や本を独立した添付先として開ける。
    let mut displayed: BTreeSet<_> = tabs.iter().filter_map(|tab| tab.deck.clone()).collect();
    let paper_paths: BTreeSet<_> = tabs.iter()
        .flat_map(|tab| tab.papers.iter().flat_map(|paper| paper.decks.iter().cloned()))
        .filter(|path| !path.is_empty() && !displayed.contains(path))
        .collect();
    for path in paper_paths {
        tabs.extend(scan_decks(project, &project.join(&path)).into_iter()
            .filter(|tab| tab.deck.as_ref() == Some(&path) && displayed.insert(path.clone())));
    }
    for directory in ["papers", "books", "writing"] {
        tabs.extend(scan_decks(project, &project.join(directory)).into_iter()
            .filter(|tab| tab.deck.as_ref().is_some_and(|path| displayed.insert(path.clone()))));
    }
    unique_tab_ids(tabs)
}

// 既存 ID は最初のタブに残し、同名のデッキにも固有の ID を割り当てる。
fn unique_tab_ids(mut tabs: Vec<TabView>) -> Vec<TabView> {
    let mut seen = BTreeSet::new();
    for tab in &mut tabs {
        if !seen.insert(tab.id.clone()) {
            if let Some(deck) = &tab.deck {
                tab.id = deck.clone();
            }
        }
    }
    tabs
}

pub fn deck_path(project: &Path, path: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        path == "." || (!path.is_empty() && !path.contains(['\\', ':'])
            && path.split('/').all(|part| !matches!(part, "" | "." | ".."))),
        "デッキのパスはプロジェクトからの相対パスで指定する: {path}"
    );
    let (_, key) = resolve_deck(project, Path::new(path))?;
    Ok(if key.is_empty() { ".".into() } else { key })
}

pub(crate) fn resolve_deck(project: &Path, path: &Path) -> anyhow::Result<(PathBuf, String)> {
    let project = project.canonicalize()?;
    let deck = project.join(path).canonicalize()
        .with_context(|| format!("デッキが見つからない: {}", path.display()))?;
    let relative = deck.strip_prefix(&project).with_context(|| format!("path escapes project: {}", path.display()))?;
    let key = relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let configured = ["deck.toml", "cards.toml", "harness.toml"]
        .iter()
        .any(|name| deck.join(name).is_file());
    let legacy_research = key.starts_with("research/") && deck.join("plan.md").is_file();
    anyhow::ensure!(deck.is_dir() && (configured || legacy_research), "デッキが見つからない: {}", path.display());
    Ok((deck, key))
}

pub fn thread_context(project: &Path, attached: &[String]) -> String {
    if attached.is_empty() {
        return String::new();
    }
    let tabs = scan(project, &[]);
    attached.iter().map(|path| {
        let Ok(path) = deck_path(project, path) else {
            return attachment_context(path, path);
        };
        let deck = if path == "." { "" } else { &path };
        let root = project.join(&path);
        let cfg = DeckConfig::read(&root).ok();
        let paper = cfg.as_ref().filter(|cfg| is_paper(&root, cfg));
        let mut context = if let Some(tab) = tabs.iter().find(|tab| tab.deck.as_deref() == Some(deck)) {
            context_for(tab, &path)
        } else if let Some(cfg) = &cfg {
            // 互換表示でタブに出ないデッキにも、個々の添付先の文脈を渡す。
            let title = deck_title(&root, Some(cfg));
            let mut context = attachment_context(&title, &path);
            append_summary_and_next(&mut context, &cfg.meta.summary, &cfg.meta.next);
            context
        } else {
            attachment_context(&path, &path)
        };
        if let Some(cfg) = paper {
            let structure = Path::new(&path).join(&cfg.structure);
            let structure = structure.display();
            context.push_str(&harness_format!(
                " Before answering about this manuscript, read its title and card order in `{structure}`. Find relevant cards with graph tools or search_library and read them with card_read. Cite card IDs and ground the answer in the manuscript rather than general knowledge.",
                " この原稿について答える前に、`{structure}` で題とカードの並びを読む。グラフの道具や search_library で関係するカードを探し、card_read で読む。カード ID を示し、一般論ではなく原稿を根拠に答える。",
            ));
        }
        context
    }).collect::<Vec<_>>().join("\n\n")
}

pub(crate) fn structure_title(deck: &Path, cfg: &DeckConfig) -> Option<String> {
    crate::build::read_structure(deck, cfg).ok()
        .and_then(|structure| structure["title"].as_str().map(str::trim).filter(|title| !title.is_empty()).map(str::to_owned))
}

pub(crate) fn is_paper(deck: &Path, cfg: &DeckConfig) -> bool {
    cfg.harness || cfg.meta.kind == "paper"
        || (cfg.meta.kind.is_empty() && deck.join("harness.toml").is_file())
}

fn paper_title(deck: &Path) -> Option<String> {
    let cfg = DeckConfig::read(deck).ok()?;
    is_paper(deck, &cfg).then(|| deck_title(deck, Some(&cfg)))
}

fn deck_title(deck: &Path, cfg: Option<&DeckConfig>) -> String {
    let document_title = cfg.filter(|cfg| is_paper(deck, cfg))
        .and_then(|cfg| structure_title(deck, cfg));
    crate::cards::deck_display_title(deck, cfg, document_title.as_deref())
}

/// スレッドに付いたデッキの文脈。
fn attachment_context(title: &str, deck: &str) -> String {
    harness_format!("This thread is attached to deck “{title}” (`{deck}`).", "このスレッドはデッキ「{title}」（`{deck}`）に付いている。")
}

fn context_for(tab: &TabView, deck: &str) -> String {
    let mut s = attachment_context(&tab.title, deck);
    if let Some(plan) = &tab.plan {
        s.push_str(&harness_format!(" The plan and status are in `{plan}`. Read it before working and update it at checkpoints.", "計画と現状は `{plan}` にある。作業の前に読み、区切りで更新する。"));
    } else if tab.kind == "research" {
        s.push_str(
            harness_text!(" deck.toml lists card directories in dirs. Use cards for plans (kind: plan), questions, ideas, and claims. Read the plan card before working. At checkpoints, update it and status / next in deck.toml.", "カードの置き場は deck.toml の dirs。計画は kind: plan のカード、問い・着想・主張もカードで持つ。\
             作業の前に計画のカードを読み、区切りで deck.toml の status / next と計画のカードを更新する。")
        );
    }
    append_summary_and_next(&mut s, &tab.summary, &tab.next);
    let papers: Vec<_> = tab.papers.iter().filter_map(|paper| {
        let paths: Vec<_> = paper.decks.iter().filter(|path| path.as_str() != deck && !path.is_empty()).map(String::as_str).collect();
        if !paths.is_empty() {
            Some(harness_format!("{} ({})", "{}（{}）", paper.name, paths.join(harness_text!(", ", "、"))))
        } else if paper.decks.is_empty() {
            Some(paper.name.clone())
        } else {
            None
        }
    }).collect();
    if !papers.is_empty() {
        s.push_str(&harness_format!(" Related manuscripts: {}.", "関連する原稿: {}。", papers.join(harness_text!(", ", "、"))));
    }
    if !tab.related.is_empty() {
        let rel: Vec<String> = tab.related.iter().map(|r| harness_format!("{} (shared: {})", "{}（共有: {}）", r.id, r.shared.join(", "))).collect();
        s.push_str(&harness_format!(" Related research: {}. Consider the impact on these projects when changing a shared manuscript.", "絡み合う研究: {}。共有の原稿を変えるときは、そちらへの影響も考える。", rel.join(harness_text!(", ", "、"))));
    }
    s
}

fn append_summary_and_next(s: &mut String, summary: &str, next: &str) {
    if !summary.is_empty() {
        s.push_str(&harness_format!(" Summary: {summary}.", "概要: {summary}。"));
    }
    if !next.is_empty() {
        s.push_str(&harness_format!(" Next in the plan: {next}.", "計画の「次」: {next}。"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn paper_context_requires_reading_structure_and_relevant_cards() {
        for modern_tabs in [false, true] {
            let project = TestDir::new();
            if modern_tabs {
                project.write("fuzukue.toml", "");
            } else {
                project.write("papers/other/deck.toml", "kind = 'paper'");
            }
            project.write("papers/draft/deck.toml", "kind = 'paper'\ntitle = 'Short title'\nsummary = 'Draft summary'");
            project.write("papers/draft/structure.yaml", "title: Manuscript title\nsections: []");
            let context = thread_context(project.path(), &["papers/draft".into()]);
            for expected in [
                "Manuscript title", "Draft summary", "papers/draft/structure.yaml",
                "card_read", "search_library", harness_text!("title and card order", "題とカードの並び"),
                harness_text!("Cite card IDs", "カード ID を示し"),
                harness_text!("rather than general knowledge", "原稿を根拠に答える"),
            ] {
                assert!(context.contains(expected), "missing {expected}: {context}");
            }
        }
    }

    #[test]
    fn legacy_paper_context_uses_the_configured_structure_path() {
        let project = TestDir::new();
        project.write("writing/draft/harness/harness.toml", "structure = 'outline.yml'");
        project.write("writing/draft/harness/outline.yml", "title: Legacy manuscript\nsections: []");
        let context = thread_context(project.path(), &["writing/draft/harness".into()]);
        assert!(context.contains("Legacy manuscript"));
        assert!(context.contains("writing/draft/harness/outline.yml"));
        assert!(context.contains("card_read"));
    }

    #[test]
    fn legacy_research_discovers_papers_harnesses_and_their_pdfs() {
        let project = TestDir::new();
        project.write(
            "research/topic/plan.md",
            "See `papers/draft/harness`, `papers/draft/harness-en`, and `writing/earlier/harness`.",
        );
        for deck in [
            "papers/draft/harness",
            "papers/draft/harness-en",
            "writing/earlier/harness",
        ] {
            project.write(&format!("{deck}/harness.toml"), "");
        }
        let pdf = "papers/draft/harness-en/build/cards.pdf".to_owned();
        let tabs = scan(project.path(), std::slice::from_ref(&pdf));
        let research = tabs.iter().find(|tab| tab.id == "topic").unwrap();
        let [draft, earlier] = research.papers.as_slice() else {
            panic!("expected both referenced manuscripts");
        };
        assert_eq!(draft.refs, 2);
        assert_eq!(draft.decks, ["papers/draft/harness", "papers/draft/harness-en"]);
        assert_eq!(draft.pdfs, [pdf]);
        assert_eq!(earlier.decks, ["writing/earlier/harness"]);
    }
}
