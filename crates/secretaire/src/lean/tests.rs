use super::*;
use crate::test_support::TestDir;
use std::sync::atomic::{AtomicBool, Ordering};

/// 合成した Lake。呼び出しを `log` に書き、制御ファイルがあれば先に実行する。
/// `sorry` を含むソースのパッケージは sorry ありと答える。
const FAKE_LAKE: &str = r##"ctl=$(dirname "$0")
name=$(basename "$PWD")
echo "$name $1" >> "$ctl/log"
if [ "$1" = build ]; then
  if [ -f "$ctl/$name.build" ]; then . "$ctl/$name.build"; fi
  exit 0
fi
source=$(cat)
if [ -f "$ctl/$name.audit" ]; then . "$ctl/$name.audit"; fi
printf '%s\n' "$source" | sed -n 's/^#print axioms //p' | while read -r decl; do
  if grep -q sorry ./*.lean; then
    echo "'$decl' depends on axioms: [sorryAx]"
  else
    echo "'$decl' does not depend on any axioms"
  fi
done
echo 'fuzukue audit complete'
"##;

struct Fixture {
    project: TestDir,
    state: TestDir,
    tools: TestDir,
    failures: Mutex<Failures>,
}

impl Fixture {
    fn new() -> Self {
        let project = TestDir::new();
        project.write("deck.toml", "title = 'Synthetic deck'\n");
        project.write("cards/example.md", "---\nid: example\nkind: definition\nlean: [A.x, B.y]\n---\n");
        for name in ["A", "B"] {
            project.write(&format!("lean/{name}/lakefile.toml"), &format!("name = '{name}'\n"));
            project.write(&format!("lean/{name}/{name}.lean"), "theorem sample : True := trivial\n");
        }
        let tools = TestDir::new();
        tools.write("lake", FAKE_LAKE);
        Self { project, state: TestDir::new(), tools, failures: Default::default() }
    }

    fn lake(&self) -> Vec<OsString> {
        vec!["/bin/sh".into(), self.tools.path().join("lake").into()]
    }

    async fn run_while(&self, run: Run, admitted: &(dyn Fn() -> bool + Send + Sync)) -> Outcome {
        check(Check {
            project: self.project.path(),
            state: self.state.path(),
            reading: false,
            run,
            lake: &self.lake(),
            failures: &self.failures,
            admitted,
            progress: &mut |_| {},
        })
        .await
        .unwrap()
    }

    async fn run(&self, run: Run) -> Statuses {
        self.run_while(run, &|| true).await.statuses
    }

    /// 前に読んでから起動したパッケージ（ビルドの順）。
    fn built(&self) -> Vec<String> {
        let path = self.tools.path().join("log");
        let log = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(path);
        log.lines().filter_map(|line| line.strip_suffix(" build").map(str::to_owned)).collect()
    }

    fn control(&self, name: &str, script: Option<&str>) {
        match script {
            Some(script) => self.tools.write(name, script),
            None => std::fs::remove_file(self.tools.path().join(name)).unwrap(),
        }
    }
}

fn states(statuses: &Statuses) -> [&str; 2] {
    [&statuses["A.x"], &statuses["B.y"]]
}

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
    assert_eq!(check_all(project.path()).await.unwrap(), expected);

    let project = TestDir::pilot();
    project.write("fuzukue.toml", r#"
[lean.packages.Toy]
path = "lean"
imports = ["Init", "PilotProofs"]
"#);
    project.write("lean/lakefile.toml", "name = 'Toy'\ndefaultTargets = ['PilotProofs']\n[[lean_lib]]\nname = 'PilotProofs'\n");
    std::fs::rename(project.path().join("lean/Toy.lean"), project.path().join("lean/PilotProofs.lean")).unwrap();
    assert_eq!(check_all(project.path()).await.unwrap(), expected);
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
    let statuses = check_all(project.path()).await.unwrap();
    assert_eq!(statuses["Synthetic.Item"], "defined");
    assert_eq!(statuses["Synthetic.value"], "defined");
    assert_eq!(statuses["Synthetic.other"], "sorry");
    assert_eq!(statuses["Synthetic.result"], "proved");
    assert_eq!(statuses["Synthetic.missing"], "missing");

    // The real toolchain's output and files must leave a reusable result.
    let state = TestDir::new();
    let saved = check(Check {
        project: project.path(),
        state: state.path(),
        reading: false,
        run: Run::Open,
        lake: &["lake".into()],
        failures: &Default::default(),
        admitted: &|| true,
        progress: &mut |_| {},
    })
    .await
    .unwrap();
    assert_eq!(saved.statuses, statuses);
    assert_eq!(Store::load(&state.path().join(CACHE_FILE)).packages.len(), 1);
}

#[tokio::test]
async fn unchanged_packages_reuse_saved_results_without_starting_lake() {
    let fixture = Fixture::new();
    assert_eq!(states(&fixture.run(Run::Open).await), ["proved", "proved"]);
    assert_eq!(fixture.built(), ["A", "B"]);

    for run in [Run::Open, Run::Manual { full: false }] {
        // The saved file serves a new server process as well.
        fixture.failures.lock().unwrap().clear();
        assert_eq!(states(&fixture.run(run).await), ["proved", "proved"]);
        assert_eq!(fixture.built(), [""; 0]);
    }
    // A new timestamp with the same content is not a change.
    fixture.project.write("lean/A/A.lean", "theorem sample : True := trivial\n");
    fixture.run(Run::Open).await;
    assert_eq!(fixture.built(), [""; 0]);

    fixture.project.write("lean/A/A.lean", "theorem sample : True := by sorry\n");
    assert_eq!(states(&fixture.run(Run::Open).await), ["sorry", "proved"]);
    assert_eq!(fixture.built(), ["A"]);

    fixture.run(Run::Manual { full: true }).await;
    assert_eq!(fixture.built(), ["A", "B"]);
}

#[tokio::test]
async fn every_kind_of_input_invalidates_only_its_package() {
    let fixture = Fixture::new();
    let project = &fixture.project;
    project.write("shared/Shared.lean", "def one := 1\n");
    project.write("lean/A/lakefile.toml", "name = 'A'\n[[require]]\nname = 'shared'\npath = '../../shared'\n");
    project.write("far/Far.lean", "def two := 2\n");
    project.write("data/table.txt", "1\n");
    fixture.run(Run::Open).await;
    assert_eq!(fixture.built(), ["A", "B"]);

    let changes: &[(&str, &str, &[&str])] = &[
        ("shared/Shared.lean", "def one := 2\n", &["A"]),
        ("shared/lakefile.toml", "name = 'shared'\n", &["A"]),
        ("lean/B/lakefile.toml", "name = 'B'\nversion = '2'\n", &["B"]),
        (
            "lean/B/lake-manifest.json",
            r#"{"packages": [{"type": "path", "name": "far", "dir": "../../far"}]}"#,
            &["B"],
        ),
        ("far/Far.lean", "def two := 3\n", &["B"]),
        ("lean/B/lean-toolchain", "synthetic/lean:v1\n", &["B"]),
        ("lean/lean-toolchain", "synthetic/lean:v2\n", &["A"]),
        ("lean/A/A.lean", "def table := include_str \"../../data/table.txt\"\n", &["A"]),
        ("data/table.txt", "2\n", &["A"]),
        ("lean/B/Extra/More.lean", "def more := 0\n", &["B"]),
        ("lean/B/.lake/packages/dep/Dep.lean", "def dep := 0\n", &[]),
        ("README.md", "unrelated\n", &[]),
        ("cards/example.md", "---\nid: example\nkind: definition\nlean: [A.x, B.y, B.z]\n---\n", &["B"]),
        ("fuzukue.toml", "[lean.packages.A]\npath = 'lean/A'\nimports = ['A', 'A.Other']\n", &["A"]),
    ];
    for (path, content, expected) in changes {
        project.write(path, content);
        fixture.run(Run::Open).await;
        assert_eq!(fixture.built(), *expected, "after changing {path}");
    }
    std::fs::remove_file(project.path().join("lean/B/Extra/More.lean")).unwrap();
    fixture.run(Run::Open).await;
    assert_eq!(fixture.built(), ["B"]);
}

#[tokio::test]
async fn an_unreadable_input_never_reuses_a_result() {
    let fixture = Fixture::new();
    fixture.run(Run::Open).await;
    fixture.built();
    fixture.project.write("lean/A/lake-manifest.json", "not JSON");
    for _ in 0..2 {
        assert_eq!(states(&fixture.run(Run::Open).await), ["proved", "proved"]);
        assert_eq!(fixture.built(), ["A"]);
    }
}

#[tokio::test]
async fn transient_failures_are_retried_and_source_errors_are_saved() {
    let fixture = Fixture::new();
    fixture.control("A.build", Some("kill -9 $$\n"));
    fixture.control("B.build", Some("echo 'error: ./B.lean:1:2: unsolved goals'; exit 1\n"));
    let first = fixture.run(Run::Open).await;
    assert!(first.values().all(|state| state.starts_with("error: ")), "{first:?}");
    assert_eq!(fixture.built(), ["A", "B"]);

    // Reopening the same server does not repeat the failure.
    assert_eq!(fixture.run(Run::Open).await, first);
    assert_eq!(fixture.built(), [""; 0]);
    // A manual check and a new server process do, but not the saved source error.
    fixture.run(Run::Manual { full: false }).await;
    assert_eq!(fixture.built(), ["A"]);
    fixture.failures.lock().unwrap().clear();
    fixture.run(Run::Open).await;
    assert_eq!(fixture.built(), ["A"]);

    fixture.control("A.build", Some("echo 'error: external command git failed'; exit 1\n"));
    for _ in 0..2 {
        fixture.run(Run::Manual { full: false }).await;
        assert_eq!(fixture.built(), ["A"]);
    }
    fixture.control("A.build", Some("echo 'error: A.lean:1:1: out of memory'; exit 1\n"));
    for _ in 0..2 {
        fixture.run(Run::Manual { full: false }).await;
        assert_eq!(fixture.built(), ["A"]);
    }

    // An audit that stops early proves nothing and is not saved.
    fixture.control("A.build", None);
    fixture.control("A.audit", Some("echo \"'A.x' does not depend on any axioms\"; exit 1\n"));
    for _ in 0..2 {
        let statuses = fixture.run(Run::Manual { full: false }).await;
        assert!(statuses["A.x"].starts_with("error: "), "{statuses:?}");
        assert_eq!(fixture.built(), ["A"]);
    }
    fixture.control("A.audit", None);
    assert_eq!(fixture.run(Run::Manual { full: false }).await["A.x"], "proved");
    fixture.built();
    fixture.run(Run::Manual { full: false }).await;
    assert_eq!(fixture.built(), [""; 0]);
}

#[tokio::test]
async fn a_source_changed_during_the_check_is_not_reported_or_saved_as_proved() {
    let fixture = Fixture::new();
    fixture.control("A.audit", Some("echo '-- edited' >> A.lean\n"));
    assert_eq!(states(&fixture.run(Run::Open).await), [CHANGED, "proved"]);
    fixture.built();
    fixture.control("A.audit", None);
    assert_eq!(states(&fixture.run(Run::Open).await), ["proved", "proved"]);
    assert_eq!(fixture.built(), ["A"]);

    // A result shown from the cache is withdrawn if its source changes later in the sweep.
    fixture.control("B.audit", Some("echo '-- edited' >> ../A/A.lean\n"));
    fixture.project.write("lean/B/B.lean", "def changed := 0\n");
    assert_eq!(states(&fixture.run(Run::Open).await), [CHANGED, "proved"]);
}

#[tokio::test]
async fn creating_the_manifest_during_the_first_build_still_saves_the_result() {
    let fixture = Fixture::new();
    fixture.control("A.build", Some("echo '{\"packages\": []}' > lake-manifest.json\n"));
    assert_eq!(states(&fixture.run(Run::Open).await), ["proved", "proved"]);
    fixture.built();
    fixture.run(Run::Open).await;
    assert_eq!(fixture.built(), [""; 0]);
}

/// 子は終了している（回収前のゾンビも含む）。
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .is_ok_and(|stat| stat.rsplit_once(") ").is_some_and(|(_, tail)| !tail.starts_with(['Z', 'X'])))
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn interruption_stops_the_process_tree_and_keeps_finished_packages() {
    let fixture = Fixture::new();
    fixture.run(Run::Open).await;
    fixture.built();
    let child = fixture.tools.path().join("child");
    fixture.control(
        "B.audit",
        Some("sleep 300 & echo $! > \"$ctl/pid\"; mv \"$ctl/pid\" \"$ctl/child\"; wait\n"),
    );
    let admitted = AtomicBool::new(true);
    let open = || admitted.load(Ordering::SeqCst);
    // The forced check redoes A, then stops inside B.
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(fixture.run_while(Run::Manual { full: true }, &open), async {
            wait_until("the audit of B", || child.is_file()).await;
            admitted.store(false, Ordering::SeqCst);
        })
    })
    .await
    .unwrap();
    assert!(outcome.interrupted);
    assert_eq!(outcome.statuses["A.x"], "proved");
    assert!(!outcome.statuses.contains_key("B.y"));
    let pid = std::fs::read_to_string(&child).unwrap();
    wait_until("the descendant to stop", || !running(&pid)).await;
    assert_eq!(fixture.built(), ["A", "B"]);

    // The next run resumes with B, whose older result the forced check discarded.
    fixture.control("B.audit", None);
    assert_eq!(states(&fixture.run(Run::Open).await), ["proved", "proved"]);
    assert_eq!(fixture.built(), ["B"]);
}
