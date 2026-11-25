//! カードが指す Lean の宣言を確かめる。
//!
//! 宣言名の先頭（`LabSample.Hamming.x` なら `LabSample`）で Lake パッケージを選ぶ。
//! パッケージをビルドしてから、`#print axioms` を `lake env lean` で実行し、
//! 宣言ごとに「証明済み（標準の公理だけ）」「sorry あり」「ほかの公理」「見つからない」を判定する。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// 標準の公理。これだけに依存していれば「証明済み」とみなす。
const STANDARD: &[&str] = &["propext", "Classical.choice", "Quot.sound"];

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
        if let Ok(g) = fuzukue_cards::Graph::open(&root) {
            for c in g.cards.values().filter(|c| !c.external) {
                decls.extend(c.lean.iter().cloned());
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
pub(crate) async fn check(project: &Path) -> Result<BTreeMap<String, String>> {
    check_project(project, false).await
}

pub async fn check_reading(project: &Path) -> Result<BTreeMap<String, String>> {
    check_project(project, true).await
}

async fn check_project(project: &Path, reading: bool) -> Result<BTreeMap<String, String>> {
    let config = LeanConfig::read(project)?;
    let decls = referenced_decls(project);
    let mut by_pkg: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &decls {
        let pkg = d.split('.').next().unwrap_or("").to_string();
        by_pkg.entry(pkg).or_default().push(d.clone());
    }
    let mut packages = BTreeMap::<PathBuf, (BTreeSet<String>, BTreeSet<String>)>::new();
    let mut out = BTreeMap::new();
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
                out.insert(d, format!("error: {} のパッケージがない", path.display()));
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
            let Ok(source) = std::fs::read_to_string(&path) else { continue };
            let group = packages.entry(dir.to_owned()).or_default();
            group.0.insert(module);
            for decl in fuzukue_view::lean::parse(&source).declarations().filter(|d| d.public && lean_name(&d.name)) {
                out.remove(&decl.name);
                group.1.insert(decl.name.clone());
            }
        }
    }
    for (dir, (imports, decls)) in packages {
        let label = dir.display();
        let decls: Vec<_> = decls.into_iter().filter(|d| lean_name(d)).collect();
        if decls.is_empty() {
            continue;
        }
        let build = Command::new("lake").arg("build").current_dir(&dir).output().await.context("lake を起動できない")?;
        if !build.status.success() {
            let msg = String::from_utf8_lossy(&build.stdout).lines().filter(|l| l.contains("error")).take(3).collect::<Vec<_>>().join(" / ");
            for d in decls {
                out.insert(d, format!("error: {label} のビルドに失敗（{msg}）"));
            }
            continue;
        }
        let mut src = String::from("import Lean\n");
        for module in imports.into_iter().filter(|name| lean_name(name)) {
            src.push_str(&format!("import {module}\n"));
        }
        for d in &decls {
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
        let mut audit = Command::new("lake")
            .args(["env", "lean", "--stdin"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = audit.stdin.take().expect("piped stdin");
        let (write, run) = tokio::join!(
            async move {
                stdin.write_all(src.as_bytes()).await?;
                stdin.shutdown().await
            },
            audit.wait_with_output(),
        );
        write?;
        let run = run?;
        let text = String::from_utf8_lossy(&run.stdout).into_owned() + &String::from_utf8_lossy(&run.stderr);
        out.extend(parse_axioms(&text, &decls));
    }
    Ok(out)
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
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn reads_package_paths_and_imports_with_convention_as_fallback() {
        let project = TestDir::new();
        let conventional = (PathBuf::from("lean/Legacy"), vec!["Legacy".into()]);
        assert_eq!(LeanConfig::read(project.path()).unwrap().resolve("Legacy"), conventional);
        project.write("fuzukue.toml", "title = 'Existing project'\n");
        assert_eq!(LeanConfig::read(project.path()).unwrap().resolve("Legacy"), conventional);

        project.write("fuzukue.toml", r#"
[lean.packages]
Toy = "lean"
Other = { path = "proofs", imports = ["Proofs.Core", "Proofs.Extra"] }
Defaults = { path = "formal" }
"#);
        let config = LeanConfig::read(project.path()).unwrap();
        assert_eq!(config.resolve("Toy"), (PathBuf::from("lean"), vec!["Toy".into()]));
        assert_eq!(config.resolve("Other"), (PathBuf::from("proofs"), vec!["Proofs.Core".into(), "Proofs.Extra".into()]));
        assert_eq!(config.resolve("Defaults"), (PathBuf::from("formal"), vec!["Defaults".into()]));
        assert_eq!(config.resolve("Legacy"), conventional);
    }

    #[tokio::test]
    #[ignore = "requires Lake and the pilot's Lean toolchain"]
    async fn checks_pilot_proofs_with_default_and_configured_imports() {
        let project = TestDir::pilot();
        let expected = BTreeMap::from([
            ("Toy.postfixIfFalse".to_string(), "sorry".to_string()),
            ("Toy.postfixIfTrue".to_string(), "proved".to_string()),
        ]);
        assert_eq!(check(project.path()).await.unwrap(), expected);

        let project = TestDir::pilot();
        project.write("fuzukue.toml", r#"
[lean.packages.Toy]
path = "lean"
imports = ["Init", "PilotProofs"]
"#);
        project.write("lean/lakefile.toml", "name = 'Toy'\ndefaultTargets = ['PilotProofs']\n[[lean_lib]]\nname = 'PilotProofs'\n");
        std::fs::rename(project.path().join("lean/Toy.lean"), project.path().join("lean/PilotProofs.lean")).unwrap();
        assert_eq!(check(project.path()).await.unwrap(), expected);
    }

    #[test]
    fn reads_axiom_lines() {
        let out = "'P.a' depends on axioms: [propext, Quot.sound]\n'P.b' depends on axioms: [propext, sorryAx]\n\
                   'P.c' does not depend on any axioms\n'P.d' depends on axioms: [propext, myAxiom]\nx.lean:5:14: error: Unknown constant `P.e`\n\
                   fuzukue definition: P.b\nfuzukue definition: P.c";
        let decls: Vec<String> = ["P.a", "P.b", "P.c", "P.d", "P.e"].iter().map(|s| s.to_string()).collect();
        let s = parse_axioms(out, &decls);
        assert_eq!(s["P.a"], "proved");
        assert_eq!(s["P.b"], "sorry");
        assert_eq!(s["P.c"], "defined");
        assert_eq!(s["P.d"], "axioms: propext, myAxiom");
        assert_eq!(s["P.e"], "missing");
    }

    #[tokio::test]
    #[ignore = "requires the installed Lean and Lake toolchain"]
    async fn audit_distinguishes_definitions_from_theorems() {
        let project = TestDir::new();
        project.write("deck.toml", "title = 'Synthetic deck'\n");
        project.write("cards/example.md", "---\nid: example\nkind: definition\nlean: [Synthetic.Item, Synthetic.value, Synthetic.other, Synthetic.result, Synthetic.missing]\n---\n");
        project.write("lean/Synthetic/lakefile.toml", "name = 'Synthetic'\ndefaultTargets = ['Synthetic']\n[[lean_lib]]\nname = 'Synthetic'\n");
        project.write("lean/Synthetic/Synthetic.lean", r#"
namespace Synthetic

/-- A structure with one sample field. -/
structure Item where
  number : Nat

/-- A sample natural number. -/
def value : Nat := 7

/-- A sample value with an unproved body. -/
def other : Nat := sorry

/-- The sample value is seven. -/
theorem result : value = 7 := rfl

end Synthetic
"#);
        let statuses = check(project.path()).await.unwrap();
        assert_eq!(statuses["Synthetic.Item"], "defined");
        assert_eq!(statuses["Synthetic.value"], "defined");
        assert_eq!(statuses["Synthetic.other"], "sorry");
        assert_eq!(statuses["Synthetic.result"], "proved");
        assert_eq!(statuses["Synthetic.missing"], "missing");
    }
}
