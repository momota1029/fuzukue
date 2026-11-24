use std::collections::{BTreeMap, BTreeSet};

use super::*;

pub(super) struct Plan {
    sources: BTreeMap<PathBuf, String>,
}

#[derive(Serialize)]
pub(super) struct Evidence {
    pub(super) inputs: Inputs,
    pdf: String,
    artifacts: BTreeMap<PathBuf, String>,
    diagnostics: serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct Inputs {
    tex_root: Option<String>,
    engine: Option<&'static str>,
    workspace_files: BTreeMap<PathBuf, String>,
    toolchain_files: BTreeMap<PathBuf, String>,
}

impl Plan {
    pub(super) fn new(
        project: &Path,
        deck: &Path,
        cfg: &DeckConfig,
        graph: &Graph,
        targets: &[&Card],
    ) -> Result<Self> {
        // The renderer reads these sources itself, before latexmk starts. The
        // recorder below separately accounts for the inputs consumed by TeX.
        let mut paths: Vec<_> = manifest(deck).into_iter().collect();
        if let Some(root) = cfg
            .meta
            .build
            .as_ref()
            .and_then(|build| build.tex_root.as_ref())
        {
            paths.push(deck.join(root));
            paths.extend(targets.iter().flat_map(|card| card_paths(deck, card)));
        } else {
            paths.push(deck.join(&cfg.structure));
            for (_, root) in cfg.external_roots(project, deck) {
                paths.extend(manifest(&root?));
            }
            paths.extend(graph.cards.values().flat_map(|card| card_paths(deck, card)));
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
                paths.push(deck.join(name));
            }
            if let Some(template) = cfg
                .meta
                .template
                .as_deref()
                .filter(|name| name.ends_with(".tex"))
            {
                paths.push(project.join(template));
            }
        }
        let mut sources = BTreeMap::new();
        for path in paths {
            let name = relative(project, &path)?;
            if path.is_file() && !sources.contains_key(&name) {
                sources.insert(name, file_hash(&path)?);
            }
        }
        Ok(Self { sources })
    }

    pub(super) async fn render(
        self,
        live: &Path,
        copied: &Path,
        destination: &Path,
        deck: &str,
    ) -> Result<Evidence> {
        let recorded = crate::build::build_recorded(copied, deck, &Cancellation::default()).await?;
        ensure!(
            recorded.built.ok,
            "出力査読のためのビルドに失敗した\n{}",
            recorded.built.log
        );
        let live = live.to_owned();
        let copied = copied.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || {
            let evidence = self.collect(&live, &copied, recorded)?;
            for (path, expected) in evidence
                .inputs
                .workspace_files
                .iter()
                .chain(&evidence.artifacts)
            {
                let file = destination.join(path);
                fs::create_dir_all(
                    file.parent()
                        .context("出力査読用ファイルの親ディレクトリがない")?,
                )?;
                let source = workspace_paths::resolve(&copied, &copied, path)?;
                fs::copy(source, &file)?;
                ensure!(
                    &file_hash(&file)? == expected,
                    "出力査読用の入力がビルド中に変わった: {}",
                    path.display()
                );
            }
            Ok(evidence)
        })
        .await?
    }

    pub(super) fn collect(
        self,
        live: &Path,
        copied: &Path,
        recorded: crate::build::RecordedBuild,
    ) -> Result<Evidence> {
        let mut inputs = Inputs {
            tex_root: recorded.built.tex_root,
            engine: recorded.built.engine,
            workspace_files: self.sources,
            toolchain_files: BTreeMap::new(),
        };
        let mut artifacts = BTreeSet::new();
        ensure!(!recorded.runs.is_empty(), "TeX の入力記録がない");
        for run in recorded.runs {
            let paths = recorded_files(&run)?;
            for path in paths.inputs.difference(&paths.generated) {
                if let Ok(rel) = path.strip_prefix(copied) {
                    let digest = file_hash(path)?;
                    if let Some(original) = inputs
                        .workspace_files
                        .insert(rel.to_owned(), digest.clone())
                    {
                        ensure!(
                            original == digest,
                            "ビルド中に入力ファイルが変わった: {}",
                            rel.display()
                        );
                    }
                } else {
                    ensure!(
                        !path.starts_with(live),
                        "出力が元のワークスペースを参照した: {}",
                        path.display()
                    );
                    if !inputs.toolchain_files.contains_key(path) {
                        inputs
                            .toolchain_files
                            .insert(path.clone(), file_hash(path)?);
                    }
                }
            }
            // Include index registrations, auxiliary references, generated TeX and
            // source maps alongside each PDF, where produced by this build.
            for path in paths
                .generated
                .iter()
                .chain(paths.inputs.iter().filter(|path| {
                    path.starts_with(run.log.parent().unwrap_or(copied))
                        && path.extension().is_some_and(|ext| ext == "tex")
                }))
            {
                if let Ok(rel) = path.strip_prefix(copied) {
                    artifacts.insert(rel.to_owned());
                }
            }
            for extension in ["fls", "fdb_latexmk", "log", "synctex.gz", "sources.json"] {
                let path = run.log.with_extension(extension);
                if path.is_file() {
                    artifacts.insert(relative(copied, &path)?);
                }
            }
            let map = crate::build::harness::map_path(&run.log);
            if map.is_file() {
                artifacts.insert(relative(copied, &map)?);
            }
            for name in [".latexmkrc", "latexmkrc"] {
                let path = run.cwd.join(name);
                if path.is_file() {
                    inputs
                        .workspace_files
                        .insert(relative(copied, &path)?, file_hash(&path)?);
                }
            }
        }
        if let Some(log) = recorded.built.log_path {
            artifacts.insert(PathBuf::from(log));
        }
        let pdf = recorded.built.pdf;
        ensure!(copied.join(&pdf).is_file(), "出力査読の PDF がない: {pdf}");
        artifacts.insert(PathBuf::from(&pdf));
        let artifacts = artifacts
            .into_iter()
            .map(|path| {
                let source = workspace_paths::resolve(copied, copied, &path)?;
                let digest = file_hash(&source)?;
                Ok((path, digest))
            })
            .collect::<Result<_>>()?;
        Ok(Evidence {
            inputs,
            pdf,
            artifacts,
            diagnostics: recorded.diagnostics,
        })
    }
}

#[derive(Default)]
struct RecordedFiles {
    inputs: BTreeSet<PathBuf>,
    generated: BTreeSet<PathBuf>,
}

fn recorded_files(run: &crate::build::RecordedRun) -> Result<RecordedFiles> {
    fn existing(cwd: &Path, path: &str) -> Option<PathBuf> {
        let path = cwd.join(path.trim_matches('"')).canonicalize().ok()?;
        path.is_file().then_some(path)
    }
    let fls = run.log.with_extension("fls");
    let mut files = RecordedFiles::default();
    let mut cwd = run.cwd.clone();
    for line in String::from_utf8_lossy(
        &fs::read(&fls).with_context(|| format!("TeX の入力記録 {} を読めない", fls.display()))?,
    )
    .lines()
    {
        if let Some(path) = line.strip_prefix("PWD ") {
            cwd = run.cwd.join(path);
        } else if let Some(path) = line
            .strip_prefix("INPUT ")
            .and_then(|path| existing(&cwd, path))
        {
            files.inputs.insert(path);
        } else if let Some(path) = line
            .strip_prefix("OUTPUT ")
            .and_then(|path| existing(&cwd, path))
        {
            files.generated.insert(path);
        }
    }
    ensure!(
        !files.inputs.is_empty(),
        "TeX の入力記録が空: {}",
        fls.display()
    );
    // latexmk's dependency database also lists paths read by BibTeX, index tools
    // and other rules. Those reads are outside the TeX engine's .fls.
    let database = run.log.with_extension("fdb_latexmk");
    if database.is_file() {
        let mut generated = false;
        for line in String::from_utf8_lossy(&fs::read(database)?)
            .lines()
            .map(str::trim)
        {
            match line {
                "(generated)" => generated = true,
                _ if line.starts_with('[') || line.starts_with('(') => generated = false,
                _ => {
                    let path = line
                        .strip_prefix('"')
                        .and_then(|line| line.split_once('"'))
                        .and_then(|(path, _)| existing(&run.cwd, path));
                    if let Some(path) = path {
                        if generated {
                            files.generated.insert(path);
                        } else {
                            files.inputs.insert(path);
                        }
                    }
                }
            }
        }
    }
    Ok(files)
}
