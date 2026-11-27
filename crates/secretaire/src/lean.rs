//! カードが指す Lean の宣言を確かめる。
//!
//! 宣言名の先頭（`LabSample.Hamming.x` なら `LabSample`）で Lake パッケージを選ぶ。
//! パッケージをビルドしてから、`#print axioms` を `lake env lean` で実行し、
//! 宣言ごとに「証明済み（標準の公理だけ）」「sorry あり」「ほかの公理」「見つからない」を判定する。
//!
//! 結果はパッケージごとに入力の指紋（[`fingerprint`]）と一緒にサーバーの状態へ残し、
//! 指紋が同じあいだは Lake も Lean も起動しない。サーバー全体で一度に一パッケージずつ確かめる。

mod fingerprint;
mod process;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) use workspace::State;

/// 標準の公理。これだけに依存していれば「証明済み」とみなす。
const STANDARD: &[&str] = &["propext", "Classical.choice", "Quot.sound"];
/// 判定、指紋、宣言の読み方を変えたら増やす。保存した結果はすべて作り直しになる。
const CHECKER_VERSION: u32 = 1;
const CACHE_FILE: &str = "lean-check.json";
const COMPLETE: &str = "fuzukue audit complete";
const CHANGED: &str = "error: 検査のあいだに入力が変わった。もう一度確かめる";

static ONE_PACKAGE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) type Statuses = BTreeMap<String, String>;
/// このサーバーで結果を保存できなかったパッケージ。開き直すたびには繰り返さない。
pub(crate) type Failures = BTreeMap<PathBuf, (String, Statuses)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Run {
    /// 保存した結果と、このサーバーで起きた失敗を使い回す。
    Open,
    /// 手動の検査。失敗はやり直す。`full` なら保存した結果も捨てる。
    Manual { full: bool },
}

pub(crate) struct Check<'a> {
    pub project: &'a Path,
    /// 結果を保存するディレクトリ。
    pub state: &'a Path,
    pub reading: bool,
    pub run: Run,
    /// Lake の起動コマンド。
    pub lake: &'a [OsString],
    pub failures: &'a Mutex<Failures>,
    /// 偽になったら、動いているプロセスを止めて戻る。
    pub admitted: &'a (dyn Fn() -> bool + Send + Sync),
    pub progress: &'a mut (dyn FnMut(&Statuses) + Send),
}

pub(crate) struct Outcome {
    pub statuses: Statuses,
    pub interrupted: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ProjectConfig {
    lean: LeanConfig,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LeanConfig {
    packages: BTreeMap<String, PackageConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PackageConfig {
    Path(PathBuf),
    Detailed { path: PathBuf, imports: Option<Vec<String>> },
}

impl LeanConfig {
    fn read(project: &Path) -> Result<Self> {
        let path = fuzukue_proto::compat::workspace_config(project);
        let config = match std::fs::read_to_string(&path) {
            Ok(config) => config,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("{} を読めない", path.display())),
        };
        let config: ProjectConfig = toml::from_str(&config)
            .with_context(|| format!("{} を読めない", path.display()))?;
        Ok(config.lean)
    }

    fn resolve(&self, pkg: &str) -> (PathBuf, Vec<String>) {
        let (path, imports) = match self.packages.get(pkg) {
            Some(PackageConfig::Path(path)) => (path.clone(), None),
            Some(PackageConfig::Detailed { path, imports }) => (path.clone(), imports.clone()),
            None => (Path::new("lean").join(pkg), None),
        };
        (path, imports.unwrap_or_else(|| vec![pkg.to_string()]))
    }
}

/// プロジェクトのすべてのデッキのカードから、Lean の宣言名を集める。
pub fn referenced_decls(project: &Path) -> BTreeSet<String> {
    let mut decls = BTreeSet::new();
    let mut roots = fuzukue_cards::find_decks(project);
    if roots.is_empty() {
        roots.push(project.to_path_buf());
    }
    for root in roots {
        if let Ok(deck) = fuzukue_cards::Graph::open_local_in_workspace(&root, project) {
            for card in deck.cards.into_values() {
                decls.extend(card.lean);
            }
        }
    }
    decls
}

/// `#print axioms` の出力を、宣言 → 状態 に読む。
pub fn parse_axioms(output: &str, decls: &[String]) -> BTreeMap<String, String> {
    let mut status: BTreeMap<String, String> = decls.iter().map(|d| (d.clone(), "missing".to_string())).collect();
    let definitions: BTreeSet<_> = output.lines()
        .filter_map(|line| line.strip_prefix("fuzukue definition: ")).collect();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix('\'') else { continue };
        let Some((name, tail)) = rest.split_once("' ") else { continue };
        if !status.contains_key(name) {
            continue;
        }
        let checked = if definitions.contains(name) { "defined" } else { "proved" };
        let state = if tail.contains("does not depend on any axioms") {
            checked.to_string()
        } else if let Some(list) = tail.split_once('[').and_then(|(_, r)| r.split_once(']')).map(|(l, _)| l) {
            let axioms: Vec<&str> = list.split(',').map(str::trim).filter(|a| !a.is_empty()).collect();
            if axioms.contains(&"sorryAx") {
                "sorry".to_string()
            } else if axioms.iter().all(|a| STANDARD.contains(a)) {
                checked.to_string()
            } else {
                format!("axioms: {}", axioms.join(", "))
            }
        } else {
            continue;
        };
        status.insert(name.to_string(), state);
    }
    status
}

/// すべての宣言を確かめる。パッケージが見つからない・ビルドできないものは "error: …"。
#[cfg(test)]
pub(crate) async fn check_all(project: &Path) -> Result<Statuses> {
    let state = tempfile::tempdir()?;
    let outcome = check(Check {
        project,
        state: state.path(),
        reading: false,
        run: Run::Manual { full: true },
        lake: &["lake".into()],
        failures: &Default::default(),
        admitted: &|| true,
        progress: &mut |_| {},
    })
    .await?;
    Ok(outcome.statuses)
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    version: u32,
    files: fingerprint::Files,
    packages: BTreeMap<PathBuf, Saved>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    fingerprint: String,
    statuses: Statuses,
}

impl Store {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|store| store.version == CHECKER_VERSION)
            .unwrap_or_else(|| Self { version: CHECKER_VERSION, ..Default::default() })
    }

    fn save(&self, path: &Path) {
        let result = (|| -> Result<()> {
            let dir = path.parent().context("no state directory")?;
            std::fs::create_dir_all(dir)?;
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(std::io::BufWriter::new(&mut file), self)?;
            crate::durable::persist(file, path)?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "could not save Lean check results");
            // An older file must not outlive the results it was meant to replace.
            let _ = std::fs::remove_file(path);
        }
    }
}

struct Package {
    dir: PathBuf,
    decls: Vec<String>,
    /// `lake env lean --stdin` に渡すソース。
    audit: String,
}

struct Plan {
    packages: Vec<Package>,
    errors: Statuses,
}

fn plan(project: &Path, reading: bool, files: &mut fingerprint::Files) -> Result<Plan> {
    let config = LeanConfig::read(project)?;
    let decls = referenced_decls(project);
    let mut by_pkg: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &decls {
        let pkg = d.split('.').next().unwrap_or("").to_string();
        by_pkg.entry(pkg).or_default().push(d.clone());
    }
    let mut packages = BTreeMap::<PathBuf, (BTreeSet<String>, BTreeSet<String>)>::new();
    let mut errors = BTreeMap::new();
    for (pkg, decls) in by_pkg {
        let (path, imports) = config.resolve(&pkg);
        if let Some(dir) = path.to_str().filter(|_| path.is_relative())
            .and_then(|p| crate::safe_join(project, p).ok()).filter(|dir| lake_package(dir))
        {
            let group = packages.entry(dir).or_default();
            group.0.extend(imports);
            group.1.extend(decls);
        } else {
            for d in decls {
                errors.insert(d, format!("error: {} のパッケージがない", path.display()));
            }
        }
    }
    if reading {
        for path in crate::lean_reading::sources(project) {
            let Some(dir) = path.ancestors().skip(1).take_while(|dir| dir.starts_with(project))
                .find(|dir| lake_package(dir)) else { continue };
            let module = path.strip_prefix(dir)?.with_extension("").iter()
                .map(|part| part.to_string_lossy()).collect::<Vec<_>>().join(".");
            if !lean_name(&module) {
                continue;
            }
            let Some(declared) = files.declarations(&path) else { continue };
            let group = packages.entry(dir.to_owned()).or_default();
            group.0.insert(module);
            for decl in declared.into_iter().filter(|name| lean_name(name)) {
                errors.remove(&decl);
                group.1.insert(decl);
            }
        }
    }
    let packages = packages
        .into_iter()
        .filter_map(|(dir, (imports, decls))| {
            let decls: Vec<_> = decls.into_iter().filter(|d| lean_name(d)).collect();
            (!decls.is_empty()).then(|| {
                let audit = audit_source(imports.iter().filter(|name| lean_name(name)), &decls);
                Package { dir, decls, audit }
            })
        })
        .collect();
    Ok(Plan { packages, errors })
}

fn audit_source<'a>(imports: impl Iterator<Item = &'a String>, decls: &[String]) -> String {
    let mut src = String::from("import Lean\n");
    for module in imports {
        src.push_str(&format!("import {module}\n"));
    }
    for d in decls {
        src.push_str(&format!("#print axioms {d}\n"));
    }
    // Read declaration kinds from the elaborated environment. A definition
    // with an unchecked axiom dependency still keeps its warning status.
    src.push_str("run_cmd do\n  let env ← Lean.getEnv\n  for decl in [");
    src.push_str(&decls.iter().map(|name| format!("\"{name}\"")).collect::<Vec<_>>().join(", "));
    src.push_str(concat!(
        "] do\n    match env.find? decl.toName with\n",
        "    | some (.defnInfo _) | some (.inductInfo _) | some (.ctorInfo _)\n",
        "    | some (.recInfo _) | some (.opaqueInfo _) => Lean.logInfo (\"fuzukue definition: \" ++ decl)\n",
        "    | _ => pure ()\n",
    ));
    // Reaching this line shows that the imports loaded and the audit ran to its end.
    src.push_str(&format!("  Lean.logInfo \"{COMPLETE}\"\n"));
    src
}

struct Checker {
    store: Store,
    plan: Plan,
    lake: Vec<OsString>,
}

impl Checker {
    /// 同じ時点の指紋を読む。読めないパッケージは `None`。
    fn prints(&mut self, which: &[usize]) -> Vec<Option<fingerprint::Print>> {
        let mut snapshot = fingerprint::Snapshot::default();
        which
            .iter()
            .map(|&index| {
                let package = &self.plan.packages[index];
                let audit = format!("{CHECKER_VERSION} {STANDARD:?}\n{}", package.audit);
                snapshot
                    .print(&mut self.store.files, &package.dir, &audit, &self.lake)
                    .inspect_err(|error| {
                        tracing::warn!(package = %package.dir.display(), %error, "could not fingerprint a Lean package");
                    })
                    .ok()
            })
            .collect()
    }
}

/// 指紋の読み取りと保存は、非同期の実行スレッドの外で行う。
async fn blocking<T: Send + 'static>(
    mut checker: Checker,
    work: impl FnOnce(&mut Checker) -> T + Send + 'static,
) -> Result<(Checker, T)> {
    Ok(tokio::task::spawn_blocking(move || {
        let result = work(&mut checker);
        (checker, result)
    })
    .await?)
}

async fn closed(admitted: &(dyn Fn() -> bool + Send + Sync)) {
    while admitted() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn compose(errors: &Statuses, results: &BTreeMap<usize, Statuses>) -> Statuses {
    let mut out = errors.clone();
    for statuses in results.values() {
        out.extend(statuses.clone());
    }
    out
}

/// 入力が変わった結果の「証明済み」「定義あり」を取り下げる。
fn withdraw(statuses: &mut Statuses) {
    for state in statuses.values_mut() {
        if matches!(state.as_str(), "proved" | "defined") {
            *state = CHANGED.into();
        }
    }
}

pub(crate) async fn check(check: Check<'_>) -> Result<Outcome> {
    let cache = check.state.join(CACHE_FILE);
    let (project, reading, lake) = (check.project.to_owned(), check.reading, check.lake.to_vec());
    let path = cache.clone();
    let full = check.run == Run::Manual { full: true };
    let (mut checker, prints) = tokio::task::spawn_blocking(move || {
        let mut store = Store::load(&path);
        let plan = plan(&project, reading, &mut store.files)?;
        let mut checker = Checker { store, plan, lake };
        let all: Vec<_> = (0..checker.plan.packages.len()).collect();
        let prints = checker.prints(&all);
        let dirs: BTreeSet<_> = checker.plan.packages.iter().map(|package| &package.dir).collect();
        // A forced check discards the saved results first, so that an
        // interrupted run resumes with the packages it has not reached.
        checker.store.packages.retain(|dir, _| !full && dirs.contains(dir));
        checker.store.save(&path);
        anyhow::Ok((checker, prints))
    })
    .await??;

    let mut results = BTreeMap::new();
    // The fingerprint that each shown result belongs to.
    let mut shown = BTreeMap::new();
    let mut queue = Vec::new();
    for (index, print) in prints.into_iter().enumerate() {
        let dir = &checker.plan.packages[index].dir;
        let reused = print.as_ref().and_then(|print| {
            if let Some(saved) = checker.store.packages.get(dir).filter(|saved| saved.fingerprint == print.digest) {
                return Some(saved.statuses.clone());
            }
            if check.run != Run::Open {
                return None;
            }
            let failures = check.failures.lock().unwrap();
            failures.get(dir).filter(|(digest, _)| *digest == print.digest).map(|(_, statuses)| statuses.clone())
        });
        match (reused, print) {
            (Some(statuses), Some(print)) => {
                results.insert(index, statuses);
                shown.insert(index, print.digest);
            }
            _ => queue.push(index),
        }
    }
    (check.progress)(&compose(&checker.plan.errors, &results));

    let mut interrupted = false;
    let checked = !queue.is_empty();
    for index in queue {
        let _slot = tokio::select! {
            biased;
            _ = closed(check.admitted) => { interrupted = true; break }
            slot = ONE_PACKAGE.lock() => slot,
        };
        let (next, before) = blocking(checker, move |checker| checker.prints(&[index]).pop().flatten()).await?;
        checker = next;
        let package = &checker.plan.packages[index];
        let (mut statuses, mut keep) = tokio::select! {
            biased;
            // Dropping the audit stops its whole process tree.
            _ = closed(check.admitted) => { interrupted = true; break }
            result = audit(check.lake, package) => result,
        };
        let path = cache.clone();
        let (next, (after, held)) = blocking(checker, move |checker| {
            let after = checker.prints(&[index]).pop().flatten();
            let held = before.as_ref().zip(after.as_ref()).map(|(before, after)| before.held(after));
            (after, held)
        })
        .await?;
        checker = next;
        if held == Some(false) {
            withdraw(&mut statuses);
        }
        // Without two matching fingerprints the result belongs to no known input.
        let digest = after.filter(|_| held == Some(true)).map(|print| print.digest);
        keep &= digest.is_some();
        let dir = checker.plan.packages[index].dir.clone();
        checker.store.packages.remove(&dir);
        check.failures.lock().unwrap().remove(&dir);
        if let Some(digest) = digest {
            if keep {
                let saved = Saved { fingerprint: digest.clone(), statuses: statuses.clone() };
                checker.store.packages.insert(dir, saved);
            } else {
                check.failures.lock().unwrap().insert(dir, (digest.clone(), statuses.clone()));
            }
            shown.insert(index, digest);
        }
        checker = blocking(checker, move |checker| checker.store.save(&path)).await?.0;
        results.insert(index, statuses);
        (check.progress)(&compose(&checker.plan.errors, &results));
    }

    // A long sweep can outlive the inputs of the results it showed first.
    if checked {
        let indices: Vec<_> = shown.keys().copied().collect();
        let (next, prints) = blocking(checker, move |checker| {
            let prints = checker.prints(&indices);
            indices.into_iter().zip(prints).collect::<Vec<_>>()
        })
        .await?;
        checker = next;
        for (index, print) in prints {
            if print.is_none_or(|print| shown.get(&index) != Some(&print.digest)) {
                if let Some(statuses) = results.get_mut(&index) {
                    withdraw(statuses);
                }
            }
        }
    }
    let statuses = compose(&checker.plan.errors, &results);
    if !interrupted {
        checker.store.files.prune();
    }
    blocking(checker, move |checker| checker.store.save(&cache)).await?;
    Ok(Outcome { statuses, interrupted })
}

/// 一つのパッケージを確かめる。二つめの値は、結果を保存してよいか。
async fn audit(lake: &[OsString], package: &Package) -> (Statuses, bool) {
    let label = package.dir.display();
    let failed = |message: String| -> Statuses {
        package.decls.iter().map(|d| (d.clone(), message.clone())).collect()
    };
    let build = match process::run(lake, &["build"], &package.dir, None).await {
        Ok(build) => build,
        Err(error) => return (failed(format!("error: lake を起動できない: {error}")), false),
    };
    if !build.status.success() {
        let msg = build.stdout.lines().chain(build.stderr.lines())
            .filter(|l| l.contains("error")).take(3).collect::<Vec<_>>().join(" / ");
        return (failed(format!("error: {label} のビルドに失敗（{msg}）")), source_error(&build));
    }
    let run = match process::run(lake, &["env", "lean", "--stdin"], &package.dir, Some(&package.audit)).await {
        Ok(run) => run,
        Err(error) => return (failed(format!("error: lake を起動できない: {error}")), false),
    };
    let text = format!("{}\n{}", run.stdout, run.stderr);
    let mut statuses = parse_axioms(&text, &package.decls);
    let exited = matches!(run.status.code(), Some(0 | 1)) && !run.truncated;
    if exited && text.contains(COMPLETE) {
        return (statuses, true);
    }
    // Without the end marker, definitions are indistinguishable from theorems.
    let msg = text.lines().find(|l| l.contains("error") || l.contains("unknown module"))
        .unwrap_or_default();
    for state in statuses.values_mut() {
        if matches!(state.as_str(), "proved" | "defined" | "missing") {
            *state = format!("error: {label} の検査を終えられなかった（{msg}）");
        }
    }
    // A module that a successful build did not produce stays missing until the inputs change.
    let missing_module = text.contains("unknown module prefix")
        || text.contains("object file") && text.contains("does not exist");
    (statuses, exited && run.status.code() == Some(1) && missing_module)
}

/// ソースの誤りによるビルドの失敗。ほかの失敗は一時的かもしれないので保存しない。
fn source_error(build: &process::Output) -> bool {
    let transient = [
        "exited with code 134", "exited with code 135", "exited with code 137", "exited with code 139",
        "exited with code 143", "out of memory", "Cannot allocate memory", "No space left", "Killed",
    ];
    let lines = || build.stdout.lines().chain(build.stderr.lines());
    build.status.code() == Some(1)
        && !build.truncated
        && !lines().any(|line| transient.iter().any(|word| line.contains(word)))
        && lines().any(|line| {
            line.contains("error")
                && line.match_indices(".lean:").any(|(at, found)| {
                    line[at + found.len()..].starts_with(|c: char| c.is_ascii_digit())
                })
        })
}

fn lake_package(dir: &Path) -> bool {
    dir.join("lakefile.toml").is_file() || dir.join("lakefile.lean").is_file()
}

fn lean_name(name: &str) -> bool {
    name.split('.').all(|part| {
        let mut chars = part.chars();
        chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
            && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '\'' | '₁' | '₂' | '₃'))
    })
}

#[cfg(test)]
mod tests;
