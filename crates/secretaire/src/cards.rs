//! プロジェクトのカードを読み、変更を見張ってスナップショットを配る。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result};
use fuzukue_cards::{Card, DeckConfig, Severity};
use fuzukue_proto::{CardDiagnostic, CardEditPart, CardEditSource, CardSaveOutcome, CardView, DocItem, DocSection, MathMacro, ServerEvent, TranslationRevision};
use fuzukue_proto::card_tex::{self, Access};
use sha2::{Digest, Sha256};
use notify::{RecursiveMode, Watcher};

use crate::{build_jobs, Workspace};

pub(crate) mod browse;
mod translation;
#[cfg(test)]
mod r81_tests;
#[cfg(test)]
mod translation_tests;

/// 縦タブ（研究）を見つけ直す。
pub fn workspace(project: &Path, deck: &str) -> ServerEvent {
    let ServerEvent::Artifacts { pdfs } = artifacts(project) else { unreachable!() };
    let tabs = crate::workspace::scan(project, &pdfs);
    ServerEvent::Workspace { tabs, deck: deck.to_owned() }
}

pub fn deck_snapshot(project: &Path, deck: &str) -> (ServerEvent, ServerEvent, ServerEvent) {
    let root = project.join(deck);
    (ServerEvent::DeckCardsChanged { deck: deck.into() }, document_for_deck(&root, None), workspace(project, deck))
}

pub(crate) fn snapshot_for_deck(project: &Path, deck: &Path) -> ServerEvent {
    snapshot_for_deck_with(project, deck, legacy_diagnostics)
}

pub(super) fn snapshot_for_deck_with(
    project: &Path,
    deck: &Path,
    project_diagnostics: impl FnOnce(&Path, &[fuzukue_cards::Diagnostic]) -> Vec<CardDiagnostic>,
) -> ServerEvent {
    let prefix = deck.strip_prefix(project).unwrap_or(Path::new(""));
    match crate::rules::open_graph(project, deck) {
        Ok(g) => {
            let tex = tex_access(project, deck, &g);
            let mut sources = translation::Sources::new(project, &g);
            let cards = g.cards.values().map(|c| {
                let mut card = card_summary(project, prefix, &g, c, &mut sources);
                let content = card_content(deck, c, tex);
                card.body = content.body;
                card.edit_body = content.edit_body;
                card.edit_proof = content.edit_proof;
                card
            }).collect();
            let diagnostics = project_diagnostics(prefix, &g.diagnostics);
            ServerEvent::Cards { deck: prefix.to_string_lossy().into_owned(), cards, diagnostics }
        }
        Err(e) => ServerEvent::Cards {
            deck: prefix.to_string_lossy().into_owned(),
            cards: Vec::new(),
            diagnostics: vec![CardDiagnostic { error: true, path: ".".into(), message: format!("{e:#}"), style: None }],
        },
    }
}


fn legacy_diagnostics(prefix: &Path, diagnostics: &[fuzukue_cards::Diagnostic]) -> Vec<CardDiagnostic> {
    let mut per_path = BTreeMap::new();
    let total = diagnostics.len();
    let mut diagnostics: Vec<_> = diagnostics.iter().filter(|d| {
        let count = per_path.entry(&d.path).or_insert(0);
        *count += 1;
        *count <= 16
    }).take(256).map(|d| diagnostic_view(prefix, d)).collect();
    if diagnostics.len() < total {
        diagnostics.push(CardDiagnostic {
            error: false,
            path: ".".into(),
            message: format!("Showing {} of {total} deck diagnostics. Update the client and server to view more.", diagnostics.len()),
            style: None,
        });
    }
    diagnostics
}

fn card_summary(
    project: &Path,
    prefix: &Path,
    g: &fuzukue_cards::Graph,
    c: &Card,
    sources: &mut translation::Sources<'_>,
) -> CardView {
    CardView {
        id: c.id.clone(),
        kind: c.kind.clone(),
        title: c.title.clone(),
        status: c.status.clone(),
        path: prefix.join(&c.path).to_string_lossy().into(),
        links: c.links.iter().map(|l| (l.rel.clone(), l.to.clone())).collect(),
        backlinks: g
            .backlinks
            .get(&c.id)
            .into_iter()
            .flatten()
            .map(|(from, rel)| (rel.clone(), from.clone()))
            .collect(),
        issues: c.issues.clone(),
        files: c.files.clone(),
        body: String::new(),
        edit_body: None,
        edit_proof: None,
        external: c.external,
        source: c.source.clone(),
        translation: sources.check(c),
        research_origin: g.research_origins.get(&c.id).and_then(|(deck, card)| {
            Some(fuzukue_proto::ResearchOrigin {
                deck: deck.strip_prefix(project).ok()?.to_string_lossy().replace('\\', "/"),
                card: card.clone(),
            })
        }),
        tex: c.tex.iter().map(|t| prefix.join(t).to_string_lossy().replace('\\', "/")).collect(),
        lean: c.lean.clone(),
    }
}

fn card_content(deck: &Path, card: &Card, tex: Access) -> fuzukue_proto::CardContent {
    fuzukue_proto::CardContent {
        body: card.body.clone(),
        edit_body: edit_source(deck, card, CardEditPart::Body, tex).ok(),
        edit_proof: edit_source(deck, card, CardEditPart::Proof, tex).ok(),
    }
}

pub(super) fn diagnostic_view(prefix: &Path, d: &fuzukue_cards::Diagnostic) -> CardDiagnostic {
    CardDiagnostic {
        error: d.severity == Severity::Error,
        path: prefix.join(&d.path).to_string_lossy().into(),
        message: d.display_message(),
        style: d.code.filter(|code| code.starts_with("style/")).map(|code| fuzukue_proto::CardStyleDiagnostic {
            code: code.into(),
            textlint_rule: d.textlint_rule.clone(),
            detail: d.message.clone(),
            note: d.style_note.clone(),
        }),
    }
}

const IGNORED_DIRS: &[&str] = &[".git", "target", ".lake", "node_modules", ".temp", ".corpus"];

pub(crate) fn is_ignored(project: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(project).unwrap_or(path);
    rel.components().any(|c| c.as_os_str().to_str().is_some_and(|s| IGNORED_DIRS.contains(&s) || s.starts_with(".fuzukue-research-") || s.starts_with(".fuzukue-book-import-")))
}

fn is_card_change(project: &Path, path: &Path) -> bool {
    !is_ignored(project, path)
        && !path.starts_with(project.join("threads"))
        && (matches!(path.extension().and_then(|e| e.to_str()), Some("md" | "yml" | "yaml"))
            || (path.extension().is_some_and(|ext| ext == "toml")
                && path.parent().and_then(Path::file_name).is_some_and(|name| name == "style" || name == "lint"))
            || matches!(path.file_name().and_then(|n| n.to_str()), Some(".textlintrc" | ".textlintrc.json" | "cards.toml" | "harness.toml" | "structure.yaml" | "deck.toml" | "fuzukue.toml" | "labo.toml" | "fuzukue.lock" | "labo.lock" | "style.toml")))
}

fn is_source_change(project: &Path, path: &Path) -> bool {
    !crate::auto_paths::excluded(project, path)
        && build_jobs::card_source(path)
        && path.strip_prefix(project).is_ok_and(|relative| {
            !relative.components().any(|component| component.as_os_str() == "build")
        })
}

/// デッキの並び（structure.yaml）と、プリアンブルおよび設定のマクロ。
pub(crate) fn document_for_deck(deck: &Path, built_pdf: Option<&Path>) -> ServerEvent {
    let cfg = DeckConfig::read(deck).ok();
    let (title, sections) = cfg.as_ref().and_then(|cfg| read_document_structure(deck, cfg).ok()).unwrap_or_default();
    let references = cfg.as_ref().and_then(|cfg| crate::paper_refs::read(deck, cfg, built_pdf));
    let title = deck_display_title(deck, cfg.as_ref(), title.as_deref());
    let math_macros: Vec<_> = cfg
        .as_ref()
        .into_iter()
        .flat_map(|cfg| math_macros(deck, cfg))
        .collect();
    // Older clients can expand only macros without arguments.
    let macros = math_macros.iter().filter(|m| m.arguments == 0)
        .map(|m| (m.name.clone(), m.body.clone())).collect();
    ServerEvent::Document { title, sections, macros, math_macros, references }
}

pub(crate) fn math_macros(deck: &Path, cfg: &DeckConfig) -> impl Iterator<Item = MathMacro> {
    // Match the harness's choice of explicit preamble or macro file.
    let file = cfg.meta.preamble.as_deref().or_else(|| {
        cfg.harness
            .then(|| cfg.macros_file.as_deref().unwrap_or(Path::new("macros.tex")))
    });
    let mut macros = file
        .and_then(Path::to_str)
        .and_then(|path| crate::safe_join(deck, path).ok())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map_or_else(BTreeMap::new, |tex| preamble_macros(&tex));
    // Explicit renderer definitions can supply an equivalent of a package-dependent command.
    macros.extend(
        cfg.meta.macros.iter().map(|(name, body)| (name.clone(), (body.clone(), 0))),
    );
    macros.extend(cfg.meta.macros_with_args.clone());
    macros.into_iter().map(|(name, (body, arguments))| MathMacro { name, body, arguments })
}

fn preamble_macros(tex: &str) -> BTreeMap<String, (String, u8)> {
    static COMMENTS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\\(?:[A-Za-z]+|[^\r\n])|%[^\r\n]*").unwrap());
    static COMMAND: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"\\(?:newcommand|renewcommand)\*?\s*(?:\{\s*\\([A-Za-z]+)\s*\}|\\([A-Za-z]+))\s*(?:\[([0-9])\]\s*)?\{|\\(?:[A-Za-z]+|[^\r\n])",
        ).unwrap()
    });
    let tex = COMMENTS.replace_all(tex, |captures: &regex::Captures<'_>| {
        let token = captures.get(0).unwrap().as_str();
        if token.starts_with('%') {
            String::new()
        } else {
            token.to_owned()
        }
    });
    let mut macros = BTreeMap::new();
    let mut rest = tex.as_ref();
    while let Some(command) = COMMAND.captures(rest) {
        rest = &rest[command.get(0).unwrap().end()..];
        let Some(name) = command.get(1).or_else(|| command.get(2)) else {
            continue;
        };
        let arguments = command.get(3).map_or(0, |arg| arg.as_str().as_bytes()[0] - b'0');
        let mut depth = 1;
        let mut chars = rest.char_indices();
        while let Some((i, ch)) = chars.next() {
            match ch {
                '\\' => {
                    chars.next();
                }
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                macros.insert(name.as_str().to_owned(), (rest[..i].to_owned(), arguments));
                rest = &rest[i + 1..];
                break;
            }
        }
    }
    macros
}

pub(crate) fn deck_display_title(deck: &Path, cfg: Option<&DeckConfig>, structure_title: Option<&str>) -> String {
    if let Some(title) = structure_title.map(str::trim).filter(|title| !title.is_empty()) {
        return title.to_owned();
    }
    if let Some(cfg) = cfg {
        // A deck.toml can supply the card settings while a legacy harness still names the paper.
        if !cfg.harness {
            if let Some(title) = std::fs::read_to_string(deck.join("harness.toml")).ok()
                .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
                .and_then(|harness| harness.get("title")?.as_str().map(str::trim)
                    .filter(|title| !title.is_empty()).map(str::to_owned))
            {
                return title;
            }
        }
        let title = cfg.meta.title.trim();
        if !title.is_empty() {
            return title.to_owned();
        }
    }
    deck.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn read_structure(deck: &Path) -> anyhow::Result<(Option<String>, Vec<DocSection>)> {
    read_document_structure(deck, &DeckConfig::read(deck)?)
}

fn read_document_structure(deck: &Path, cfg: &DeckConfig) -> anyhow::Result<(Option<String>, Vec<DocSection>)> {
    let v = crate::build::read_structure(deck, cfg)?;
    let title = v["title"].as_str().map(str::to_owned);
    let mut sections: Vec<_> = v["sections"]
        .as_sequence()
        .into_iter()
        .flatten()
        .map(|sec| DocSection {
            title: sec["title"].as_str().unwrap_or("").to_string(),
            items: document_items(&sec["items"], cfg.harness),
        })
        .collect();
    if cfg.harness {
        let items = document_items(&v["appendix"], true);
        if !items.is_empty() {
            sections.push(DocSection { title: cfg.meta.appendix_title.clone().unwrap_or_else(|| "Deferred proofs".into()), items });
        }
    }
    Ok((title, sections))
}

fn document_items(items: &serde_yaml::Value, harness: bool) -> Vec<DocItem> {
    items.as_sequence().into_iter().flatten().filter_map(|item| {
        if let Some(title) = item["subsection"].as_str().or_else(|| item["heading"]["title"].as_str()) {
            return Some(DocItem::Subsection { title: title.to_owned() });
        }
        let id = if harness { crate::build::harness::item_card(item) } else { item.as_str() }?;
        Some(DocItem::Card { id: id.to_owned() })
    }).collect()
}

fn tex_access(project: &Path, deck: &Path, graph: &fuzukue_cards::Graph) -> Access {
    let Ok(cfg) = DeckConfig::read_in_workspace(deck, project) else {
        return Access::Unavailable;
    };
    if cfg.meta.imported_latex {
        // There is no recorded switch to card ownership for imported decks yet.
        Access::ImportedManuscript
    } else if cfg.meta.build.as_ref().is_some_and(|build| build.tex_root.is_some()) {
        Access::ManuscriptBuild
    } else if matches!(cfg.meta.kind.as_str(), "paper" | "book")
        && deck.join(&cfg.structure).is_file()
        && !crate::build::uses_harness(&cfg, graph)
    {
        Access::Editable
    } else {
        Access::Unavailable
    }
}

fn edit_path(deck: &Path, card: &Card, part: CardEditPart) -> Result<(PathBuf, bool)> {
    anyhow::ensure!(!card.external, "外部デッキのカードはここで編集できない");
    let name = match part {
        CardEditPart::Body | CardEditPart::TexBody => "statement.tex",
        CardEditPart::Proof | CardEditPart::TexProof => "proof.tex",
    };
    let tex = card.tex.iter().find(|path| path.ends_with(name));
    let (relative, frontmatter) = match (tex, part) {
        (Some(path), _) => (path, false),
        (None, CardEditPart::Body) => (&card.path, true),
        _ => anyhow::bail!("証明のソースが見つからない"),
    };
    let path = crate::safe_join(deck, relative.to_str().context("ソースのパスを読めない")?)?;
    Ok((path, frontmatter))
}

fn source_parts(text: &str, frontmatter: bool) -> Result<(&str, &str)> {
    if !frontmatter {
        return Ok(("", text));
    }
    let (_, body) = fuzukue_cards::split_frontmatter(text).context("カードの front matter が見つからない")?;
    Ok((&text[..text.len() - body.len()], body))
}

fn source_version(path: &Path, text: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(text.as_bytes());
    format!("{:x}", hash.finalize())
}

fn edit_source(deck: &Path, card: &Card, part: CardEditPart, tex: Access) -> Result<CardEditSource> {
    let (path, frontmatter) = edit_path(deck, card, part)?;
    let mut source = read_edit_source(&path, frontmatter, &std::fs::read_to_string(&path)?)?;
    if !frontmatter && card.path.ends_with("card.md") {
        source.tex = tex;
    }
    Ok(source)
}

fn read_edit_source(path: &Path, frontmatter: bool, text: &str) -> Result<CardEditSource> {
    Ok(CardEditSource {
        body: source_parts(text, frontmatter)?.1.to_owned(),
        base_hash: source_version(path, text),
        tex: Access::Unavailable,
    })
}

pub(crate) static CARD_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn save_card(
    project: &Path,
    deck: &str,
    id: &str,
    part: CardEditPart,
    body: &str,
    base_hash: &str,
    human: Option<&Arc<Workspace>>,
) -> Result<CardSaveOutcome> {
    save_card_with_build(project, deck, id, part, body, base_hash, human).map(|(outcome, _)| outcome)
}

pub(crate) fn save_card_with_build(
    project: &Path,
    deck: &str,
    id: &str,
    part: CardEditPart,
    body: &str,
    base_hash: &str,
    human: Option<&Arc<Workspace>>,
) -> Result<(CardSaveOutcome, Option<SavedCardTex>)> {
    save_card_edit(project, deck, id, CardEdit::Body { part, body, base_hash }, human)
}

pub(crate) struct SavedCardTex {
    pub(crate) deck: String,
    path: PathBuf,
    hash: String,
}

impl SavedCardTex {
    pub(crate) fn is_current(&self) -> bool {
        std::fs::read_to_string(&self.path)
            .is_ok_and(|text| source_version(&self.path, &text) == self.hash)
    }
}

pub(crate) fn record_source_fingerprint(
    project: &Path,
    deck: &str,
    id: &str,
    revision: &TranslationRevision,
    human: Option<&Arc<Workspace>>,
) -> Result<CardSaveOutcome> {
    save_card_edit(project, deck, id, CardEdit::Translation(revision), human).map(|(outcome, _)| outcome)
}

enum CardEdit<'a> {
    Body { part: CardEditPart, body: &'a str, base_hash: &'a str },
    Translation(&'a TranslationRevision),
}

fn save_card_edit(
    project: &Path,
    deck: &str,
    id: &str,
    edit: CardEdit<'_>,
    human: Option<&Arc<Workspace>>,
) -> Result<(CardSaveOutcome, Option<SavedCardTex>)> {
    let _write = CARD_WRITE_LOCK.lock().unwrap();
    let tex_edit = matches!(edit, CardEdit::Body { part, .. } if part.is_tex());
    let recording_fingerprint = matches!(edit, CardEdit::Translation(_));
    let deck = if deck.is_empty() { "." } else { deck };
    let deck = crate::workspace::deck_path(project, deck)?;
    let root = crate::safe_join(project, &deck)?;
    let graph = crate::rules::open_graph(project, &root)?;
    let card = match graph.cards.get(id) {
        Some(card) => card,
        None if recording_fingerprint => return Ok((CardSaveOutcome::TranslationConflict, None)),
        None => anyhow::bail!("カードが見つからない"),
    };
    if tex_edit {
        let access = if card.external || !card.path.ends_with("card.md") {
            Access::Unavailable
        } else {
            tex_access(project, &root, &graph)
        };
        if access != Access::Editable {
            return Ok((CardSaveOutcome::TexUnavailable { access }, None));
        }
    }
    let (path, frontmatter, base_hash) = match edit {
        CardEdit::Body { part, base_hash, .. } => {
            if base_hash.is_empty() {
                return Ok((CardSaveOutcome::Error {
                    message: "Load the card source before saving.".into(),
                }, None));
            }
            let (path, frontmatter) = edit_path(&root, card, part)?;
            (path, frontmatter, base_hash)
        }
        CardEdit::Translation(revision) => (translation::card_path(&root, card)?, true, revision.base_hash.as_str()),
    };
    let read = || -> Result<_> { Ok((std::fs::metadata(&path)?, std::fs::read_to_string(&path)?)) };
    let (info, current) = match read() {
        Err(_) if recording_fingerprint => return Ok((CardSaveOutcome::TranslationConflict, None)),
        result => result?,
    };
    let matches_version = |text: &str| match edit {
        CardEdit::Body { .. } => source_version(&path, text) == base_hash,
        CardEdit::Translation(_) => translation::version(project, &path, text).is_ok_and(|hash| hash == base_hash),
    };
    let conflict = |text: &str| -> Result<_> {
        let outcome = if recording_fingerprint {
            CardSaveOutcome::TranslationConflict
        } else {
            CardSaveOutcome::Conflict { latest: read_edit_source(&path, frontmatter, text)? }
        };
        Ok((outcome, None))
    };
    if !matches_version(&current) {
        return conflict(&current);
    }
    let (text, source) = match edit {
        CardEdit::Body { part, body, .. } => {
            if part.is_tex() {
                if let Err(issue) = card_tex::validate(body) {
                    return Ok((CardSaveOutcome::TexInvalid { issue }, None));
                }
            }
            (replace_body(&current, frontmatter, body)?, None)
        }
        CardEdit::Translation(revision) => {
            let latest = fuzukue_cards::parse_card(&root, &path, &current)?;
            let mut sources = translation::Sources::new(project, &graph);
            let Some(source) = latest.source_id.as_deref().and_then(|id| sources.get(id))
                .filter(|source| source.fingerprint == revision.source_fingerprint) else {
                return Ok((CardSaveOutcome::TranslationConflict, None));
            };
            let text = fuzukue_cards::with_source_fingerprint(&current, &source.fingerprint)?;
            (text, Some(source))
        }
    };
    if text != current {
        let tmp = replacement_file(&path, info.permissions(), &text)?;
        let baseline = human.and_then(|app| app.before_human_card_save(&graph, card, &path));
        let mut rejected = None;
        // Check inside the history operation, after its baseline has been read.
        let replace = || -> Result<()> {
            let latest = match std::fs::read_to_string(&path) {
                Err(error) if recording_fingerprint => {
                    rejected = Some((CardSaveOutcome::TranslationConflict, None));
                    return Err(error.into());
                }
                result => result?,
            };
            if latest != current
                || source.as_ref().is_some_and(|source| !matches_version(&latest) || !source.is_current(project))
            {
                rejected = Some(conflict(&latest)?);
                anyhow::bail!("card source changed before replacement");
            }
            tmp.persist(&path)?;
            Ok(())
        };
        let result = match human {
            Some(app) => app.history.write("human".into(), &path, replace),
            None => replace(),
        };
        if let Some(rejected) = rejected {
            return Ok(rejected);
        }
        result?;
        if let Some(baseline) = baseline {
            baseline.record();
        }
        drop(_write);
        if let Some(app) = human {
            crate::extensions::card_saved(app, &path);
        }
    }
    let build = if tex_edit {
        Some(SavedCardTex { deck, hash: source_version(&path, &text), path })
    } else {
        if let Some(app) = human {
            app.schedule_folder_paper_build(deck, &path);
        }
        None
    };
    Ok((CardSaveOutcome::Saved, build))
}

fn replace_body(current: &str, frontmatter: bool, body: &str) -> Result<String> {
    let (front, _) = source_parts(current, frontmatter)?;
    let separator = if !front.is_empty() && !front.ends_with('\n') && !body.is_empty() {
        if front.starts_with("---\r\n") { "\r\n" } else { "\n" }
    } else {
        ""
    };
    Ok(format!("{front}{separator}{body}"))
}

pub fn save_file(app: &Workspace, project: &Path, path: &Path, draft: String) -> anyhow::Result<()> {
    let _write = CARD_WRITE_LOCK.lock().unwrap();
    let info = std::fs::metadata(path)?;
    anyhow::ensure!(info.is_file(), "{} はファイルではない", path.display());
    let harness = is_harness_card(project, path)?;
    let folder_card = is_card_source(project, path);
    let draft_body = fuzukue_cards::split_frontmatter(&draft).map_or(draft.as_str(), |(_, body)| body);
    if draft_body.trim().is_empty() {
        let inline_card = path.extension().is_some_and(|ext| ext == "md")
            && std::fs::read_to_string(path).is_ok_and(|current| {
                fuzukue_cards::parse_card(project, path, &current).is_ok()
            });
        anyhow::ensure!(!(harness || folder_card || inline_card), "Load the card source and use a versioned save to clear it.");
    }
    let text = if harness {
        let current = std::fs::read_to_string(path)?;
        if fuzukue_cards::split_frontmatter(&current).is_some() {
            fuzukue_cards::merge_card_edit(&current, &draft)?
        } else {
            draft
        }
    } else {
        draft
    };
    let tmp = replacement_file(path, info.permissions(), &text)?;
    let baseline = app.before_human_file_save(path, &text);
    app.history.write("human".into(), path, || tmp.persist(path))?;
    if let Some(baseline) = baseline {
        baseline.record();
    }
    drop(_write);
    if harness || folder_card {
        crate::extensions::card_saved(app, path);
    }
    Ok(())
}

pub(crate) fn is_card_source(project: &Path, path: &Path) -> bool {
    matches!(path.file_name().and_then(|name| name.to_str()), Some("card.md" | "statement.tex" | "proof.tex"))
        && path.starts_with(project)
        && path.parent().is_some_and(|parent| parent.join("card.md").is_file())
}

pub(crate) fn replacement_file(path: &Path, permissions: std::fs::Permissions, text: &str) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let parent = path.parent().context("保存先ディレクトリがない")?;
    let mut tmp = tempfile::Builder::new().prefix(".fuzukue-write-").tempfile_in(parent)?;
    tmp.as_file().set_permissions(permissions)?;
    tmp.write_all(text.as_bytes())?;
    Ok(tmp)
}

pub(crate) fn is_harness_card(project: &Path, path: &Path) -> anyhow::Result<bool> {
    if path.extension().is_none_or(|ext| ext != "md") || path.ends_with("card.md") {
        return Ok(false);
    }
    for dir in path.ancestors().skip(1).take_while(|dir| dir.starts_with(project)) {
        if dir.join("harness.toml").is_file() {
            let cfg = DeckConfig::read(dir)?;
            return Ok(cfg.harness && cfg.dirs.iter().any(|cards| path.starts_with(dir.join(cards))));
        }
        if dir.join("deck.toml").is_file() || dir.join("cards.toml").is_file() {
            break;
        }
    }
    Ok(false)
}

fn is_pdf(project: &Path, path: &Path) -> bool {
    !is_ignored(project, path) && path.extension().and_then(|e| e.to_str()) == Some("pdf")
}

/// プロジェクト内の PDF（相対パス、名前順）。
pub fn artifacts(project: &Path) -> ServerEvent {
    let mut pdfs: Vec<String> = walkdir::WalkDir::new(project)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_str().is_some_and(|s| IGNORED_DIRS.contains(&s) || s.starts_with(".fuzukue-research-") || s.starts_with(".fuzukue-book-import-")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().and_then(|x| x.to_str()) == Some("pdf"))
        .filter_map(|e| e.path().strip_prefix(project).ok().map(|p| p.to_string_lossy().replace('\\', "/")))
        .collect();
    pdfs.sort();
    ServerEvent::Artifacts { pdfs }
}

#[derive(Default)]
struct Pending {
    cards: bool,
    macros: bool,
    findings: std::collections::BTreeSet<PathBuf>,
    /// 中身が変わった PDF
    pdfs: std::collections::BTreeSet<PathBuf>,
    /// PDF が増えた・消えた
    pdf_set: bool,
}

fn graph_change(project: &Path, path: &Path) -> bool {
    !is_ignored(project, path)
        && (crate::references::is_change(project, path)
            || path.extension().and_then(|ext| ext.to_str()).is_none_or(|ext| {
                matches!(ext, "md" | "tex" | "lean" | "bib" | "yaml" | "yml" | "toml")
            })
            || !path.exists() // A removed or renamed directory may have a suffix.
            || path.is_dir())
}

async fn refresh_graph(app: &Arc<Workspace>) {
    let app = app.clone();
    let _ = tokio::task::spawn_blocking(move || app.graph.refresh(&app.project_dir, &app.state_dir)).await;
}

/// 変更を見張り、落ち着いたら（300ms）スナップショットを配り直す。
/// カードが変われば Cards、PDF が書き換われば FileChanged、増減すれば Artifacts。
pub fn watch(
    app: Arc<Workspace>,
    references_changed: Arc<tokio::sync::Notify>,
) -> notify::Result<()> {
    let project = app.project_dir.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(notify::EventKind, Vec<PathBuf>)>();
    let root = project.clone();
    let (dir_tx, mut dir_rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let new_dir_events = tx.clone();
    let (source_tx, mut sources) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
    let new_dir_sources = source_tx.clone();
    let graph_app = app.clone();
    let git_changes = Arc::new(tokio::sync::Notify::new());
    let git_changed = git_changes.clone();
    let history_changes = Arc::new(tokio::sync::Notify::new());
    let history_changed = history_changes.clone();
    let state = app.state_dir.clone();
    let git_dir = crate::git::metadata_dir(&project);
    let watched_git_dir = git_dir.clone();
    let build_changes = Arc::new(tokio::sync::Notify::new());
    let build_changed = build_changes.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(mut ev) = res {
            // 読み取り（Access）は無視する。スナップショット自身の読み込みで再発火しないように。
            let relevant = matches!(
                ev.kind,
                notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
            );
            if relevant && ev.paths.iter().any(|p| (p.starts_with(&root) && !is_ignored(&root, p))
                || watched_git_dir.as_ref().and_then(|dir| p.strip_prefix(dir).ok())
                    .is_some_and(|path| matches!(
                        path.to_str(), Some("HEAD" | "index" | "FETCH_HEAD" | "config")
                    )))
            {
                git_changed.notify_one();
            }
            if relevant && ev.paths.iter().any(|path| path.strip_prefix(&root).is_ok_and(|relative| {
                !relative.starts_with("threads") && !path.starts_with(&state) && !is_ignored(&root, path)
            }))
            {
                history_changed.notify_one();
            }
            ev.paths.retain(|p| p.starts_with(&root));
            let references_changed_here =
                relevant && ev.paths.iter().any(|p| crate::references::is_change(&root, p));
            if references_changed_here {
                references_changed.notify_one();
            }
            // 新しくできたディレクトリや、移動してきた資料フォルダも見張る。
            let created = matches!(
                ev.kind,
                notify::EventKind::Create(notify::event::CreateKind::Folder | notify::event::CreateKind::Any)
            );
            let moved = matches!(ev.kind, notify::EventKind::Modify(notify::event::ModifyKind::Name(_)));
            if created || moved {
                for p in &ev.paths {
                    if p.is_dir() && !p.starts_with(&state) && !is_ignored(&root, p) {
                        let _ = dir_tx.send(p.clone());
                    }
                }
            }
            if relevant {
                if matches!(ev.kind, notify::EventKind::Remove(_) | notify::EventKind::Modify(notify::event::ModifyKind::Name(_)))
                    || ev.paths.iter().any(|p| p.file_name().is_some_and(|name| name == ".gitignore"))
                {
                    build_changed.notify_one();
                }
                let changed_sources: Vec<_> = ev.paths.iter()
                    .filter(|p| is_source_change(&root, p))
                    .cloned()
                    .collect();
                if !changed_sources.is_empty() {
                    let _ = source_tx.send(changed_sources);
                }
                let graph_paths: Vec<_> = ev.paths.iter().filter(|path| {
                    !path.starts_with(&graph_app.state_dir) && graph_change(&root, path)
                }).cloned().collect();
                if !graph_paths.is_empty() {
                    graph_app.graph.invalidate_paths(&graph_paths);
                }
                if !graph_paths.is_empty() || ev.paths.iter().any(|p| is_card_change(&root, p) || is_pdf(&root, p) || is_source_change(&root, p)) {
                    let _ = tx.send((ev.kind, ev.paths));
                }
            }
        }
    })?;
    // 再帰の見張りは .lake や target まで登録してしまい、大きなリポジトリでは起動が止まる。
    // 除外するディレクトリを避けてたどり、1 つずつ登録する。
    for entry in walkdir::WalkDir::new(&project)
        .into_iter()
        .filter_entry(|e| e.path() == project || (!e.path().starts_with(&app.state_dir) && !is_ignored(&project, e.path())))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir())
    {
        if let Err(e) = watcher.watch(entry.path(), RecursiveMode::NonRecursive) {
            tracing::warn!("watch {}: {e}", entry.path().display());
        }
    }
    if let Some(git_dir) = git_dir {
        let _ = watcher.watch(&git_dir, RecursiveMode::NonRecursive);
    }
    let history = app.history.clone();
    tokio::spawn(async move {
        loop {
            let history = history.clone();
            if let Ok(Err(error)) = tokio::task::spawn_blocking(move || history.observe()).await {
                tracing::warn!("project history watcher: {error:#}");
            }
            history_changes.notified().await;
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
    });
    let git_app = app.clone();
    tokio::spawn(async move {
        loop {
            git_changes.notified().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            git_app.git_changed();
        }
    });
    let watcher = Arc::new(std::sync::Mutex::new(watcher));
    let adder = watcher.clone();
    let watch_root = project.clone();
    let graph_app = app.clone();
    tokio::spawn(async move {
        while let Some(dir) = dir_rx.recv().await {
            let mut watcher = adder.lock().unwrap();
            let mut paths = Vec::new();
            let mut graph_paths = Vec::new();
            for entry in walkdir::WalkDir::new(dir)
                .into_iter()
                .filter_entry(|e| !is_ignored(&watch_root, e.path()))
                .filter_map(Result::ok)
            {
                if entry.file_type().is_dir() {
                    let _ = watcher.watch(entry.path(), RecursiveMode::NonRecursive);
                } else if is_source_change(&watch_root, entry.path()) {
                    paths.push(entry.into_path());
                } else if graph_change(&watch_root, entry.path()) {
                    graph_paths.push(entry.into_path());
                }
            }
            if !paths.is_empty() {
                let _ = new_dir_sources.send(paths.clone());
                graph_paths.extend(paths);
            }
            if !graph_paths.is_empty() {
                graph_app.graph.invalidate_paths(&graph_paths);
                let _ = new_dir_events.send((notify::EventKind::Create(notify::event::CreateKind::File), graph_paths));
            }
        }
    });
    let build_app = app.clone();
    tokio::spawn(async move {
        loop {
            let first = tokio::select! {
                paths = sources.recv() => {
                    let Some(paths) = paths else { break };
                    paths
                }
                _ = build_changes.notified() => {
                    let app = build_app.clone();
                    let _ = tokio::task::spawn_blocking(move || app.clear_stale_build_failure()).await;
                    continue;
                }
            };
            let mut paths: std::collections::HashSet<_> = first.into_iter().collect();
            while let Ok(more) = sources.try_recv() {
                paths.extend(more);
            }
            let project = build_app.project_dir.clone();
            if let Ok(decks) = tokio::task::spawn_blocking(move || build_jobs::changed_decks(&project, paths)).await {
                for deck in decks {
                    build_app.schedule_build(deck);
                }
            }
        }
    });
    tokio::spawn(async move {
        let _watcher = watcher; // タスクが生きている間は見張り続ける
        refresh_graph(&app).await;
        while let Some(first) = rx.recv().await {
            let mut pending = Pending::default();
            let mut note = |(kind, paths): (notify::EventKind, Vec<PathBuf>)| {
                for p in paths {
                    if is_card_change(&project, &p) || is_source_change(&project, &p) {
                        pending.cards = true;
                        pending.macros |= p.extension().is_some_and(|ext| ext == "tex")
                            || matches!(p.file_name().and_then(|name| name.to_str()),
                                Some("deck.toml" | "harness.toml" | "cards.toml"));
                    } else if is_pdf(&project, &p) {
                        let set_changed = matches!(kind, notify::EventKind::Create(_) | notify::EventKind::Remove(_))
                            || matches!(kind, notify::EventKind::Modify(notify::event::ModifyKind::Name(_)));
                        pending.pdf_set |= set_changed;
                        pending.pdfs.insert(p);
                    } else if !is_ignored(&project, &p)
                        && p.file_name().is_some_and(|name| name == crate::review_findings::FILE)
                    {
                        pending.findings.insert(p);
                    }
                }
            };
            note(first);
            // latexmk などは何度も書くので、静かになるまで待つ。
            tokio::time::sleep(Duration::from_millis(300)).await;
            while let Ok(ev) = rx.try_recv() {
                note(ev);
            }
            if app.graph.is_dirty() {
                refresh_graph(&app).await;
            }
            let hub = &app.hub;
            if pending.cards {
                app.deck_view.clear();
                let p = project.clone();
                let deck = app.current_deck();
                if let Ok((c, d, w)) = tokio::task::spawn_blocking(move || deck_snapshot(&p, &deck)).await {
                    hub.publish_snapshot(w);
                    hub.publish_snapshot(c);
                    hub.publish_snapshot(d);
                }
            }
            if pending.macros {
                let hub = hub.clone();
                let _ = tokio::task::spawn_blocking(move || hub.refresh_threads()).await;
            }
            if pending.pdf_set {
                let p = project.clone();
                if let Ok(ev) = tokio::task::spawn_blocking(move || artifacts(&p)).await {
                    hub.publish_snapshot(ev);
                }
            }
            for p in pending.pdfs {
                if p.exists() {
                    let rel = p.strip_prefix(&project).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    hub.publish_snapshot(ServerEvent::FileChanged { path: rel });
                }
            }
            for p in pending.findings {
                if let Ok(path) = p.strip_prefix(&project) {
                    hub.publish_snapshot(ServerEvent::FileChanged { path: path.to_string_lossy().replace('\\', "/") });
                }
            }
        }
    });
    Ok(())
}
