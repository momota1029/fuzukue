//! Local writing and workflow preferences supplied as advisory prompt text.
use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use fuzukue_cards::DeckConfig;

use crate::mcp::workspace_read::open_no_symlinks;

const MAX_FILE: usize = 8 * 1024;
const MAX_LAYER: usize = 24 * 1024;
const MAX_FILES: usize = 32;
const MAX_MANIFEST: usize = 256 * 1024;

pub(crate) fn desk_guidance() -> &'static str {
    harness_text!(
        "\n\nUse the personal preferences supplied with the current turn. Apply paper writing preferences only when that turn supplies them and the task concerns exactly the named paper. Use the paper attachment to select its deck when the user chooses a paper. Keep private preference text out of replies, delegation requests, and deliverables. Keep personal preference files out of Git staging. Save a new habit only when the user explicitly asks to register it.",
        "\n\n個人の好みは現在のターンで渡されたものを使う。論文の執筆メモは、現在のターンにメモがあり、依頼の対象がその論文一つに絞れる場合に適用する。利用者が論文を選んだら、添付先で論文デッキを指定する。非公開の好みの本文は返答・委譲依頼・成果物に転載しない。個人用の設定ファイルは Git のステージに含めない。新しい習慣を保存するのは、利用者が登録を明示的に頼んだときだけ。",
    )
}

/// Format a fresh block or explicitly supersede every block retained in Codex history.
pub(crate) fn codex_turn_guidance(text: &str, reuse: bool) -> String {
    if reuse {
        return harness_text!(
            "\n\n## Current-turn personal preference guidance\nThe most recent full personal preference block still applies to this turn. Ignore any blocks older than that full block. Any paper memo applies only to the paper named in that full block.",
            "\n\n## 現在のターンの個人の好み\n直近の省略されていない個人の好みのブロックをこのターンにも適用する。それより前のブロックは無効とする。執筆メモはそのブロックに明記された論文にのみ適用する。",
        ).into();
    }
    let guidance = if text.is_empty() {
        harness_text!(
            "\n\n## Current-turn personal preference guidance\nThis block supersedes all earlier injected personal preference blocks. No personal preferences or paper writing memos apply to this turn.",
            "\n\n## 現在のターンの個人の好み\nこのブロックは以前に注入された個人の好みのブロックすべてに優先する。このターンには個人の好みも論文の執筆メモも適用しない。",
        )
    } else {
        harness_text!(
            "\n\n## Current-turn personal preference guidance\nThis block supersedes all earlier injected personal preference blocks. Apply only the preferences below to this turn. Each paper memo applies only to the paper named in its heading.",
            "\n\n## 現在のターンの個人の好み\nこのブロックは以前に注入された個人の好みのブロックすべてに優先する。このターンには以下の好みだけを適用する。執筆メモはその見出しに明記された論文にのみ適用する。",
        )
    };
    format!("{guidance}{text}")
}

pub(crate) async fn load_selection(
    selection: Option<Selection>, app: &crate::Workspace, session: &str,
) -> Result<String> {
    match selection.filter(|_| trusted_session(app, session)) {
        Some(selection) => selection.load(app, session).await,
        None => Ok(String::new()),
    }
}

/// Preferences follow the originating user turn independently of execution authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeskSelection {
    user: Option<Selection>,
    active: Option<Selection>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    active_digest: bool,
}

impl DeskSelection {
    pub(crate) fn is_empty(&self) -> bool {
        self.user.is_none() && self.active.is_none()
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) enum Inheritance {
    #[default]
    Desk,
    Captured(Option<Selection>),
}

pub(crate) fn for_turn(
    app: &crate::Workspace, session: &str, kind: crate::front_desk::TurnKind,
    automatic_resume: bool, task: &str, inheritance: &Inheritance,
) -> Result<Option<Selection>> {
    use crate::front_desk::TurnKind;

    let trusted = !matches!(kind, TurnKind::ExternalInput) && trusted_session(app, session);
    let new_user = trusted && kind.user_conversation() && !automatic_resume;
    let check_attachment = automatic_resume || !matches!(kind, TurnKind::WorkerDigest);
    let paper = (trusted && check_attachment)
        .then(|| selected_paper(app, session, task, std::iter::empty::<&str>())).flatten();
    let mut records = app.session_records.lock().unwrap();
    let previous = records.get(session).map(|record| record.desk_preferences.clone()).unwrap_or_default();
    let mut desk = previous.clone();
    if !trusted {
        desk = DeskSelection::default();
    } else if new_user {
        desk.user = Some(Selection { paper });
        desk.active = desk.user.clone();
        desk.active_digest = false;
    } else {
        if !matches!(kind, TurnKind::Nudge) {
            desk.active_digest = matches!(kind, TurnKind::WorkerDigest);
        }
        desk.active = match inheritance {
            Inheritance::Captured(selection) => selection.clone(),
            Inheritance::Desk if automatic_resume || matches!(kind, TurnKind::Nudge) => desk.active.clone(),
            Inheritance::Desk => desk.user.clone(),
        };
        let inherits_digest = matches!(kind, TurnKind::Nudge) && desk.active_digest;
        if let Some(selection) = desk.active.as_mut().filter(|_| check_attachment && (automatic_resume || !inherits_digest)) {
            selection.keep_matching_paper(paper.as_deref());
        }
        // Older pause records may have captured a user turn before its origin was saved.
        if automatic_resume && kind.user_conversation() && desk.user.is_none() {
            desk.user = desk.active.clone();
        }
    }
    if desk != previous {
        records.entry(session.to_owned()).or_default().desk_preferences = desk.clone();
        if let Err(error) = crate::state::save_sessions_checked(&app.state_dir, &records) {
            records.get_mut(session).unwrap().desk_preferences = previous;
            return Err(error);
        }
    }
    Ok(desk.active)
}

pub(crate) fn current(app: &crate::Workspace, session: &str) -> Option<Selection> {
    if !trusted_session(app, session) {
        return None;
    }
    app.session_records.lock().unwrap().get(session)?.desk_preferences.active.clone()
}

pub(crate) fn trusted_session(app: &crate::Workspace, session: &str) -> bool {
    crate::mcp::exec::trusted_session(app, session) && personal_session(app, session)
}

fn personal_session(app: &crate::Workspace, session: &str) -> bool {
    !session.starts_with("thread-issue-")
        && !app.hub.read_threads(|store| {
            store.github_state(session).is_some_and(|state| !state.links.is_empty() || state.intake_repo.is_some())
        })
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct Selection {
    paper: Option<String>,
}

impl Selection {
    pub(crate) fn common<'a>(mut selections: impl Iterator<Item = &'a Option<Self>>) -> Option<Self> {
        let mut common = selections.next()?.clone()?;
        for selection in selections {
            if common.paper != selection.as_ref()?.paper {
                common.paper = None;
            }
        }
        Some(common)
    }

    pub(crate) fn capture(
        app: &crate::Workspace, session: &str, user_turn: bool, task: &str,
        related: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Option<Self> {
        (user_turn && trusted_session(app, session)).then(|| Self {
            paper: selected_paper(app, session, task, related),
        })
    }

    pub(crate) fn for_thread(
        mut self, app: &crate::Workspace, session: &str, task: &str,
        related: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        if self.paper.is_some() {
            self.keep_matching_paper(selected_paper(app, session, task, related).as_deref());
        }
        self
    }

    fn keep_matching_paper(&mut self, paper: Option<&str>) {
        if self.paper.as_deref() != paper {
            self.paper = None;
        }
    }

    pub(crate) fn for_task(
        mut self, app: &crate::Workspace, task: &str, related: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        if self.paper.as_deref().is_some_and(|paper| {
            other_paper_reference(&app.project_dir, paper, task)
                || related.into_iter().any(|text| other_paper_reference(&app.project_dir, paper, text.as_ref()))
        }) {
            self.paper = None;
        }
        self
    }

    pub(crate) async fn load(self, app: &crate::Workspace, session: &str) -> Result<String> {
        if personal_session(app, session) {
            for_session(app, session, self.paper).await
        } else {
            Ok(String::new())
        }
    }
}

fn selected_paper(
    app: &crate::Workspace, session: &str, task: &str, related: impl IntoIterator<Item = impl AsRef<str>>,
) -> Option<String> {
    let attached = app.hub.thread_attachments(session);
    let mut papers = attached.iter().filter(|deck| {
        let root = app.project_dir.join(deck);
        // Treat an unreadable attachment as a candidate so a single malformed
        // manifest is reported by the loader, and two candidates stay ambiguous.
        read_deck(&root).map_or(true, |cfg| crate::workspace::is_paper(&root, &cfg))
    });
    let paper = papers.next()?;
    if papers.next().is_some()
        || other_paper_reference(&app.project_dir, paper, task)
        || related.into_iter().any(|text| other_paper_reference(&app.project_dir, paper, text.as_ref()))
    {
        return None;
    }
    Some(paper.clone())
}

fn other_paper_reference(project: &Path, paper: &str, text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if ["other paper", "another paper", "both papers", "other manuscript", "both manuscripts",
        "arxiv.org/", "doi.org/", "openreview.net/", "arxiv:", "doi:", "別の論文", "両方の論文"]
        .iter().any(|phrase| lower.contains(phrase))
    {
        return true;
    }
    let selected = project.join(paper);
    // Legacy writing harnesses live beside the manuscript in its paper directory.
    let manuscript = selected.parent().filter(|parent| {
        Path::new(paper).file_name().is_some_and(|name| name.to_string_lossy().starts_with("harness"))
            && parent.strip_prefix(project).is_ok_and(|relative| {
                relative.components().count() > 1 && (relative.starts_with("papers") || relative.starts_with("writing"))
            })
    });
    text.split(|c: char| !c.is_alphanumeric() && !matches!(c, '/' | '.' | '-' | '_' | ':'))
        .map(|token| token.trim_end_matches('.'))
        .filter(|token| token.contains('/') || token.to_ascii_lowercase().ends_with(".pdf"))
        .any(|token| {
            let (qualifier, local) = token.split_once(':').unwrap_or(("", token));
            let path = Path::new(local);
            let path = path.strip_prefix(project).or_else(|_| path.strip_prefix(".")).unwrap_or(path);
            let path = project.join(path);
            if qualifier.is_empty() && paper != "."
                && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
                && (path.starts_with(&selected) || manuscript.is_some_and(|dir| path.starts_with(dir)))
            {
                return false;
            }
            if token.to_ascii_lowercase().ends_with(".pdf") {
                return true;
            }
            if path.strip_prefix(project).is_ok_and(|relative| {
                relative.starts_with("papers") || relative.starts_with("writing")
            }) {
                return true;
            }
            path.ancestors().take_while(|path| *path != project && path.starts_with(project))
                .any(|root| read_deck(root).is_ok_and(|cfg| crate::workspace::is_paper(root, &cfg)))
        })
}

/// The worker file tools protect rule directories and existing memo folders
/// linked by a paper manifest. The separate add_memo tool creates fresh IDs.
pub(crate) fn protected_write(path: &Path) -> bool {
    if path.ancestors().any(|part| part.ends_with(".fuzukue/rules")) {
        return true;
    }
    path.ancestors().any(|folder| {
        let Some(memos) = folder.parent().filter(|parent| parent.ends_with(crate::memos::DECK)) else {
            return false;
        };
        let (Some(project), Some(id)) = (memos.parent(), folder.file_name()) else {
            return false;
        };
        writing_memo_ids(project).any(|linked| id == linked.as_str())
    })
}

fn writing_memo_ids(project: &Path) -> impl Iterator<Item = String> {
    fuzukue_cards::find_decks(project).into_iter().filter_map(|root| {
        let cfg = read_deck(&root).ok()?;
        crate::workspace::is_paper(&root, &cfg).then_some(cfg.meta.writing_memo).flatten()
    })
}

pub(crate) fn protected_workspace_dirs(project: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let rules = project.join(".fuzukue/rules");
    let rules = rules.canonicalize().is_ok_and(|path| path == rules && path.is_dir()).then_some(rules);
    rules.into_iter().chain(writing_memo_ids(project).filter_map(|id| crate::memos::memo_folder(project, &id).ok()))
}

async fn for_session(
    app: &crate::Workspace,
    session: &str,
    paper: Option<String>,
) -> Result<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute())
        .map(|path| path.canonicalize().unwrap_or(path));
    let loaded = load(home.as_deref(), &app.project_dir, paper.as_deref()).await?;
    let warnings: Vec<_> = {
        let mut seen = app.preference_warnings.lock().unwrap();
        let seen = seen.entry(session.to_owned()).or_default();
        loaded.warnings.iter().filter(|warning| seen.insert((*warning).clone())).cloned().collect()
    };
    for warning in &warnings {
        tracing::warn!(workspace = %app.project_dir.display(), "{warning}");
    }
    if !warnings.is_empty() && app.hub.has_thread(session) {
        crate::publish_prompt(
            &app.hub,
            session,
            fuzukue_proto::Actor::System,
            &warnings.join("\n\n"),
        );
    }
    Ok(loaded.text)
}

#[derive(Default)]
struct Loaded {
    text: String,
    warnings: Vec<String>,
}

async fn load(home: Option<&Path>, project: &Path, paper: Option<&str>) -> Result<Loaded> {
    let home = home.filter(|home| *home != project);
    let (home_git, project_git) = tokio::join!(async {
        match home {
            Some(home) => rule_tracking(home).await,
            None => Ok(None),
        }
    }, rule_tracking(project));
    let (home, project, paper) = (home.map(Path::to_path_buf), project.to_owned(), paper.map(str::to_owned));
    Ok(tokio::task::spawn_blocking(move || load_files(
        home.as_deref().map(|root| (root, home_git)), (&project, project_git), paper.as_deref(),
    )).await?)
}

type Tracking = Result<Option<BTreeSet<PathBuf>>>;

fn load_files(home: Option<(&Path, Tracking)>, project: (&Path, Tracking), paper: Option<&str>) -> Loaded {
    let mut loaded = Loaded::default();
    let home_text = home.map(|(root, tracking)| read_layer(root, tracking, &mut loaded.warnings))
        .unwrap_or_default();
    let (project, tracking) = project;
    let project_text = read_layer(project, tracking, &mut loaded.warnings);
    let paper_text = paper.and_then(|deck| match read_paper(project, deck) {
        Ok(text) => text,
        Err(error) => {
            loaded.warnings.push(harness_format!(
                "Could not load writing preferences for paper {deck}: {error:#}",
                "論文 {deck} の執筆メモを読み込めない: {error:#}",
            ));
            None
        }
    });
    if home_text.is_empty() && project_text.is_empty() && paper_text.is_none() {
        return loaded;
    }
    loaded.text.push_str(harness_text!(
        "\n\n## Personal preferences\nApply these when consistent with shared policy and the current user request. Workspace preferences take precedence over home preferences. Paper writing preferences take precedence over general preferences only for that paper. Approval, access, and publishing requirements still apply. Keep preference text out of replies, further delegation requests, and deliverables. Keep personal preference files out of Git staging.",
        "\n\n## 個人の好み\n共有ポリシーと現在の依頼に沿う範囲で適用する。ワークスペースの好みをホームの好みより優先する。論文の執筆メモは対象の論文でのみ一般の好みより優先する。承認・アクセス・公開に関する要件は引き続き適用する。好みの本文は返答・再委譲・成果物に転載しない。個人用の設定ファイルは Git のステージに含めない。",
    ));
    for (heading, body) in [
        (harness_text!("Home preferences", "ホームの好み"), home_text),
        (harness_text!("Workspace preferences", "ワークスペースの好み"), project_text),
    ] {
        if !body.is_empty() {
            loaded.text.push_str(&format!("\n\n### {heading}\n{body}"));
        }
    }
    if let (Some(deck), Some(body)) = (paper, paper_text) {
        loaded.text.push_str(&harness_format!(
            "\n\n### Writing preferences for paper `{deck}`\n{body}",
            "\n\n### 論文 `{deck}` の執筆メモ\n{body}",
        ));
    }
    loaded.text
        .push_str(harness_text!("\n\nEnd of personal preferences.", "\n\n個人の好みはここまで。"));
    loaded
}

fn read_layer(root: &Path, tracking: Tracking, warnings: &mut Vec<String>) -> String {
    let dir = root.join(".fuzukue/rules");
    let tracked = match tracking {
        Ok(tracked) => tracked,
        Err(error) => {
            warnings.push(read_warning(&dir, &error));
            return String::new();
        }
    };
    let list = || -> Result<Vec<PathBuf>> {
        let directory = match open_no_symlinks(&dir) {
            Ok(directory) => directory,
            Err(error) if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        ensure!(directory.metadata()?.is_dir(), "not a directory");
        let mut paths = Vec::new();
        for entry in rustix::fs::Dir::read_from(&directory)? {
            let entry = entry?;
            let path = dir.join(std::ffi::OsStr::from_bytes(entry.file_name().to_bytes()));
            if path.extension().is_some_and(|ext| ext == "md") {
                ensure!(paths.len() < MAX_FILES, "limit of {MAX_FILES} Markdown files exceeded");
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    };
    let mut text = String::new();
    match list() {
        Ok(paths) => {
            if !paths.is_empty() && tracked.as_ref().is_some_and(|tracked| {
                tracked.is_empty() && !crate::git::ignored(root, &[Path::new(".fuzukue/")])
                    .contains(Path::new(".fuzukue/"))
            }) {
                let dir = dir.display();
                warnings.push(harness_format!(
                    "Git has not confirmed that {dir} is ignored. Exclude personal files locally before staging.",
                    "{dir} が Git の除外対象か確認できない。ステージする前にローカルの除外設定に追加する。",
                ));
            }
            for path in paths {
                if tracked.as_ref().is_some_and(|tracked| tracked.contains(&path)) {
                    let path = path.display();
                    warnings.push(harness_format!(
                        "Git tracks {path}. Skipping this personal preference file.",
                        "Git が {path} を追跡しているため、この個人用の好みは読み込まない。",
                    ));
                    continue;
                }
                let result = read_text(&path, MAX_FILE).and_then(|body| {
                    let body = body.trim();
                    if !body.is_empty() {
                        ensure!(text.len() + 2 + body.len() <= MAX_LAYER, "layer limit of {MAX_LAYER} bytes exceeded");
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(body);
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    warnings.push(read_warning(&path, &error));
                }
            }
        }
        Err(error) => warnings.push(read_warning(&dir, &error)),
    }
    text
}

fn read_warning(path: &Path, error: &anyhow::Error) -> String {
    let path = path.display();
    harness_format!(
        "Could not load personal preferences from {path}: {error:#}",
        "個人の好みを {path} から読み込めない: {error:#}",
    )
}

fn read_text(path: &Path, max: usize) -> Result<String> {
    let file = open_no_symlinks(path).with_context(|| format!("opening {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "{} is not a regular file", path.display());
    let mut text = String::new();
    file.take(max as u64 + 1).read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(text.len() <= max, "{} exceeds {max} bytes", path.display());
    ensure!(!text.contains('\0'), "{} contains NUL", path.display());
    Ok(text)
}

fn read_deck(root: &Path) -> Result<DeckConfig> {
    let mut manifest = root.to_path_buf();
    DeckConfig::read_with(root, |name| {
        manifest = root.join(name);
        match read_text(&manifest, MAX_MANIFEST) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }).map_err(|error| {
        if error.is::<toml::de::Error>() {
            anyhow::anyhow!("{}: invalid deck TOML", manifest.display())
        } else {
            error
        }
    })
}

fn read_paper(project: &Path, deck: &str) -> Result<Option<String>> {
    let key = crate::workspace::deck_path(project, deck)?;
    ensure!(key == deck, "paper deck must use its canonical workspace path");
    let root = project.join(key);
    let cfg = read_deck(&root)?;
    if !crate::workspace::is_paper(&root, &cfg) {
        return Ok(None);
    }
    let Some(id) = cfg.meta.writing_memo else { return Ok(None) };
    let memo = crate::memos::memo_folder(project, &id)
        .map_err(|_| anyhow::anyhow!("{}: invalid or missing memo path", project.join(crate::memos::DECK).display()))?;
    let path = memo.join("card.md");
    let text = read_text(&path, MAX_FILE)?;
    let (front, body) = fuzukue_cards::split_frontmatter(&text)
        .with_context(|| format!("missing memo frontmatter in {}", path.display()))?;
    #[derive(serde::Deserialize)]
    struct Memo {
        id: String,
        kind: String,
    }
    let front: Memo = serde_yaml::from_str(front)
        .map_err(|_| anyhow::anyhow!("{}: invalid memo YAML", path.display()))?;
    ensure!(front.kind == "memo" && front.id == id, "invalid memo identity in {}", path.display());
    let body = body.trim();
    Ok((!body.is_empty()).then(|| body.to_owned()))
}

async fn rule_tracking(root: &Path) -> Tracking {
    let relative = ".fuzukue/rules";
    let dir = root.join(relative);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        _ => {}
    }
    if !root.ancestors().any(|path| fs::symlink_metadata(path.join(".git")).is_ok()) {
        return Ok(None);
    }
    const LIMIT: usize = 256 * 1024;
    let output = crate::git::run(root, &["ls-files", "-z", "--cached", "--", relative], LIMIT).await;
    let output = output.ok().filter(|output| output.status.success() && output.stdout.len() < LIMIT)
        .context("could not determine Git tracking")?;
    Ok(Some(output.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty())
        .map(|path| root.join(std::ffi::OsStr::from_bytes(path)))
        .collect()))
}

#[cfg(test)]
mod tests;
