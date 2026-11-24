use std::collections::{BTreeMap, BTreeSet, VecDeque};

use fuzukue_cards::FOLLOW_RELS;
use sha2::{Digest, Sha256};

use super::*;

#[derive(Default)]
struct CardFiles {
    files: Vec<PathBuf>,
    labels: BTreeSet<String>,
    references: BTreeSet<String>,
    missing: BTreeSet<String>,
}

pub(super) struct Sources<'a> {
    project: &'a Path,
    deck: &'a Path,
    by_card: BTreeMap<String, CardFiles>,
    bytes: BTreeMap<PathBuf, Vec<u8>>,
}

#[derive(Serialize)]
pub(super) struct Evidence {
    files: BTreeMap<PathBuf, String>,
    unresolved: BTreeSet<String>,
}

impl<'a> Sources<'a> {
    pub(super) fn new(project: &'a Path, deck: &'a Path) -> Self {
        Self {
            project,
            deck,
            by_card: BTreeMap::new(),
            bytes: BTreeMap::new(),
        }
    }

    fn lookup(&self, from: &Path, name: &Path, tex: bool) -> Option<PathBuf> {
        let build = self.deck.join("build");
        for base in [from, self.deck, build.as_path(), self.project] {
            for path in [
                base.join(name),
                base.join(format!("{}.tex", name.display())),
            ]
            .into_iter()
            .take(if tex { 2 } else { 1 })
            {
                if let Ok(found) = relative(self.project, &path) {
                    if self.project.join(&found).is_file() {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    fn collect(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<CardFiles> {
        let mut files = CardFiles::default();
        let mut seen = BTreeSet::new();
        let mut pending = VecDeque::new();
        for path in paths {
            pending.push_back(relative(self.project, &path)?);
        }
        while let Some(path) = pending.pop_front() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let bytes = match self.bytes.entry(path.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                    fs::read(self.project.join(&path))
                        .with_context(|| format!("{} を査読用に取得できない", path.display()))?,
                ),
            };
            if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("md" | "tex" | "sty" | "cls")
            ) {
                let source = std::str::from_utf8(bytes)
                    .with_context(|| format!("{} を UTF-8 として読めない", path.display()))?;
                let symbols = crate::graph::review_symbols(&path, source);
                files.labels.extend(symbols.labels);
                files.references.extend(symbols.references);
                let parent = self.project.join(path.parent().unwrap_or(Path::new("")));
                for name in symbols.includes {
                    match self.lookup(&parent, Path::new(&name), true) {
                        Some(found) => pending.push_back(found),
                        None => {
                            files.missing.insert(name);
                        }
                    }
                }
            }
            files.files.push(path);
        }
        Ok(files)
    }

    fn card(&mut self, card: &Card) -> Result<()> {
        if self.by_card.contains_key(&card.id) {
            return Ok(());
        }
        let mut tex: Vec<_> = card.tex.iter().map(|path| self.deck.join(path)).collect();
        tex.sort();
        let paths = std::iter::once(self.deck.join(&card.path)).chain(tex);
        let mut files = self.collect(paths)?;
        files.labels.extend(card.label.clone());
        files.labels.insert(format!("card:{}", card.id));
        self.by_card.insert(card.id.clone(), files);
        Ok(())
    }

    // The deck-relative path and exact bytes of each target and its literal
    // inputs enter in card order. Context outside the targets has its own hash.
    pub(super) fn version(&mut self, targets: &[&Card]) -> Result<String> {
        fn frame(hash: &mut Sha256, tag: &[u8], bytes: &[u8]) {
            hash.update((tag.len() as u64).to_le_bytes());
            hash.update(tag);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let deck = self.deck.strip_prefix(self.project)?;
        let mut hash = Sha256::new();
        frame(&mut hash, b"kind", b"content");
        for card in targets {
            self.card(card)?;
            frame(&mut hash, b"card", card.id.as_bytes());
            let files = &self.by_card[&card.id];
            for path in &files.files {
                let path_in_deck = path.strip_prefix(deck).unwrap_or(path).to_string_lossy();
                frame(&mut hash, b"file", path_in_deck.as_bytes());
                frame(&mut hash, b"bytes", &self.bytes[path]);
            }
            for input in &files.missing {
                frame(&mut hash, b"file", format!("missing:{input}").as_bytes());
                frame(&mut hash, b"bytes", b"");
            }
        }
        Ok(format!("section-v1:{:x}", hash.finalize()))
    }

    pub(super) fn capture(
        mut self,
        cfg: &DeckConfig,
        graph: &Graph,
        targets: &[&Card],
        destination: &Path,
    ) -> Result<Evidence> {
        let mut owners = BTreeMap::<String, Vec<&Card>>::new();
        for card in graph.cards.values() {
            self.card(card)?;
            for label in &self.by_card[&card.id].labels {
                owners.entry(label.clone()).or_default().push(card);
            }
        }

        let mut context_files: Vec<_> = manifest(self.deck).into_iter().collect();
        for name in [
            cfg.meta
                .preamble
                .as_deref()
                .or(Some(Path::new("preamble.tex"))),
            cfg.macros_file.as_deref().or(Some(Path::new("macros.tex"))),
            cfg.meta.bibliography.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let path = self.deck.join(name);
            if path.is_file() {
                context_files.push(path);
            }
        }
        let context = self.collect(context_files)?;
        let mut evidence = Evidence {
            files: BTreeMap::new(),
            unresolved: BTreeSet::new(),
        };
        let mut selected: BTreeSet<_> = context.files.into_iter().collect();
        evidence.unresolved.extend(
            context
                .missing
                .into_iter()
                .map(|name| format!("TeX input: {name}")),
        );
        let mut queue: VecDeque<&Card> = targets.iter().copied().collect();
        let mut visited = BTreeSet::new();
        while let Some(card) = queue.pop_front() {
            if !visited.insert(&card.id) {
                continue;
            }
            let files = &self.by_card[&card.id];
            selected.extend(files.files.iter().cloned());
            evidence.unresolved.extend(
                files
                    .missing
                    .iter()
                    .map(|name| format!("{} TeX input: {name}", card.id)),
            );
            for label in &files.references {
                match owners.get(label) {
                    Some(cards) => queue.extend(cards.iter().copied()),
                    None => {
                        evidence
                            .unresolved
                            .insert(format!("{} TeX reference: {label}", card.id));
                    }
                }
            }
            for link in &card.links {
                if !FOLLOW_RELS.contains(&link.rel.as_str()) && link.rel != "mentions" {
                    continue;
                }
                match graph.cards.get(&link.to) {
                    Some(card) => queue.push_back(card),
                    None => {
                        evidence
                            .unresolved
                            .insert(format!("{} card dependency: {}", card.id, link.to));
                    }
                }
            }
            let from = self.deck.join(&card.path);
            for name in &card.files {
                match self.lookup(from.parent().unwrap_or(self.deck), Path::new(name), false) {
                    Some(path) => {
                        if !self.bytes.contains_key(&path) {
                            self.bytes
                                .insert(path.clone(), fs::read(self.project.join(&path))?);
                        }
                        selected.insert(path);
                    }
                    None => {
                        evidence
                            .unresolved
                            .insert(format!("{} file: {name}", card.id));
                    }
                }
            }
        }

        for path in selected {
            let bytes = &self.bytes[&path];
            let file = destination.join(&path);
            fs::create_dir_all(
                file.parent()
                    .context("査読用ファイルの親ディレクトリがない")?,
            )?;
            fs::write(file, bytes)?;
            evidence
                .files
                .insert(path, format!("{:x}", Sha256::digest(bytes)));
        }
        Ok(evidence)
    }
}
