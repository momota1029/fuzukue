use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fuzukue_cards::{Card, Graph};
use fuzukue_proto::{TranslationCheck, TranslationRevision, TranslationStatus};
use sha2::{Digest, Sha256};

pub(super) struct Sources<'a> {
    project: &'a Path,
    local: &'a Graph,
    paths: HashMap<String, Option<String>>,
    decks: HashMap<String, Option<Graph>>,
    hashes: HashMap<String, Option<Source>>,
}

#[derive(Clone)]
pub(super) struct Source {
    deck: String,
    path: PathBuf,
    id: String,
    pub(super) fingerprint: String,
}

impl Source {
    pub(super) fn is_current(&self, project: &Path) -> bool {
        read_fingerprint(project, &self.deck, &self.path, &self.id)
            .is_some_and(|fingerprint| fingerprint == self.fingerprint)
    }
}

fn read_fingerprint(project: &Path, deck: &str, path: &Path, id: &str) -> Option<String> {
    let root = crate::safe_join(project, deck).ok()?;
    let path = crate::safe_join(project, root.join(path).to_str()?).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let card = fuzukue_cards::parse_card(&root, &path, &text).ok()?;
    (card.id == id)
        .then(|| fuzukue_cards::source_fingerprint(&text).ok())
        .flatten()
}

impl<'a> Sources<'a> {
    pub(super) fn new(project: &'a Path, local: &'a Graph) -> Self {
        Self {
            project,
            local,
            paths: HashMap::new(),
            decks: HashMap::new(),
            hashes: HashMap::new(),
        }
    }

    pub(super) fn get(&mut self, source_id: &str) -> Option<Source> {
        self.hashes
            .entry(source_id.to_owned())
            .or_insert_with(|| {
                let (deck, id) = source_id.split_once('#')?;
                let deck = self
                    .paths
                    .entry(deck.to_owned())
                    .or_insert_with(|| crate::workspace::deck_path(self.project, deck).ok())
                    .as_ref()?;
                let root = self.project.join(deck);
                let graph = if root.canonicalize().ok().as_ref() == Some(&self.local.root) {
                    self.local
                } else {
                    self.decks
                        .entry(deck.clone())
                        .or_insert_with(|| Graph::open_local_in_workspace(&root, self.project).ok())
                        .as_ref()?
                };
                let card = graph.cards.get(id).filter(|card| !card.external)?;
                let path = card.path.clone();
                let fingerprint = read_fingerprint(self.project, deck, &path, id)?;
                Some(Source {
                    deck: deck.clone(),
                    path,
                    id: id.to_owned(),
                    fingerprint,
                })
            })
            .clone()
    }

    pub(super) fn check(&mut self, card: &Card) -> Option<TranslationCheck> {
        let source_id = card.source_id.as_deref().filter(|_| !card.external)?;
        let Some(source) = self.get(source_id) else {
            return Some(TranslationCheck {
                status: TranslationStatus::SourceUnavailable,
                revision: None,
            });
        };
        let status = match card.source_fingerprint.as_deref() {
            None | Some("" | "PENDING") => TranslationStatus::Unverified,
            Some(saved) if saved == source.fingerprint => TranslationStatus::Current,
            Some(_) => TranslationStatus::Stale,
        };
        let revision = (status != TranslationStatus::Current)
            .then(|| {
                let path = card_path(&self.local.root, card).ok()?;
                let text = std::fs::read_to_string(&path).ok()?;
                let base_hash = version(self.project, &path, &text).ok()?;
                Some(TranslationRevision {
                    base_hash,
                    source_fingerprint: source.fingerprint,
                })
            })
            .flatten();
        Some(TranslationCheck { status, revision })
    }
}

pub(super) fn card_path(deck: &Path, card: &Card) -> Result<PathBuf> {
    anyhow::ensure!(!card.external, "外部デッキのカードはここで編集できない");
    crate::safe_join(deck, card.path.to_str().context("ソースのパスを読めない")?)
}

pub(super) fn version(project: &Path, path: &Path, text: &str) -> Result<String> {
    let markdown = super::source_version(path, text);
    if !path.ends_with("card.md") {
        return Ok(markdown);
    }
    // A folder card's translation can also change through its TeX parts.
    let mut tex = BTreeMap::new();
    let parent = path.parent().context("カードのディレクトリがない")?;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "tex") {
            let entry_path = entry.path();
            let part = crate::safe_join(
                project,
                entry_path.to_str().context("ソースのパスを読めない")?,
            )?;
            tex.insert(
                entry.file_name(),
                super::source_version(&part, &std::fs::read_to_string(&part)?),
            );
        }
    }
    let mut hash = Sha256::new();
    hash.update(markdown);
    for (name, version) in tex {
        hash.update(name.as_encoded_bytes());
        hash.update([0]);
        hash.update(version);
    }
    Ok(format!("{:x}", hash.finalize()))
}
