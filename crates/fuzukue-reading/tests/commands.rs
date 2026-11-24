use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/reading-companion"
);

fn fixture(name: &str) -> Result<TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("reading book % ")
        .tempdir()?;
    let source = Path::new(FIXTURES).join(name);
    for entry in walkdir::WalkDir::new(&source).min_depth(1) {
        let entry = entry?;
        let target = directory.path().join(entry.path().strip_prefix(&source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(directory)
}

fn run(root: &Path, args: &[&str], success: bool) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_reading-companion"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn mutate(path: &Path, update: impl FnOnce(&mut Value)) -> Result<()> {
    let mut json = read_json(path)?;
    update(&mut json);
    fs::write(path, serde_json::to_vec_pretty(&json)?)?;
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git").current_dir(root).args(args).status()?;
    anyhow::ensure!(status.success(), "git {args:?} exited {status}");
    Ok(())
}

fn has(command: &str) -> bool {
    Command::new(command)
        .arg("-v")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn companion_stays_linked_to_source_and_respects_private_paths() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "build/companion.html";
    git(project, &["init", "-q"])?;
    run(project, &["check_outline", spec], true);
    run(project, &["build_companion", spec, "--out", page], true);
    let html = fs::read_to_string(project.join(page))?;
    let (_, payload) = html.split_once("const DATA = ").unwrap();
    let (payload, _) = payload.split_once(";\n").unwrap();
    let data: Value = serde_json::from_str(payload)?;
    assert_eq!(data["figures"][0]["note_links"][0]["text"], "§1");
    assert_eq!(data["figures"][0]["note_links"][0]["page"], 1);
    assert_eq!(
        data["features"][0]["codes"]["author_code"]["text"],
        "average(a, b) = (a + b) / 2"
    );
    assert_eq!(
        data["features"][0]["results"]["author"]["default"]["points"][2][0],
        2
    );
    let check = |name, ok| {
        run(
            project,
            &[
                "check_companion",
                spec,
                name,
                "--workspace",
                ".",
                "--no-dom",
            ],
            ok,
        )
    };
    check(page, true);
    let mut edited = data;
    edited["features"][0]["codes"]["author_code"]["text"] = "average(a, b) = 0".into();
    fs::write(
        project.join(page),
        html.replacen(payload, &serde_json::to_string(&edited)?, 1),
    )?;
    assert!(stdout(&check(page, false)).contains("differs from the current source"));
    fs::write(project.join(page), html)?;
    fs::copy(project.join(page), project.join("public.html"))?;
    assert!(stdout(&check("public.html", false)).contains("not ignored"));
    fs::write(
        project.join("companion/average.jl"),
        "# Fictional\naverage(a, b) = (b + a) / 2\n",
    )?;
    assert!(stdout(&check(page, false)).contains("differs from the current source"));
    mutate(&project.join(spec), |spec| {
        spec["background"]["question"] = Value::Null
    })?;
    let fail = run(project, &["check_outline", spec], false);
    assert!(stdout(&fail).contains("background: missing question"));
    mutate(&project.join(spec), |spec| {
        spec["background"]["question"] = "What is halfway between the two inputs?".into();
        spec["difficulties"] = serde_json::json!([
            {"id":"midpoint", "title":"Midpoint", "text":"Choosing either input biases the result."}
        ]);
        spec["overview"][0]["why"] = "Both inputs should contribute equally.".into();
        spec["overview"][0]["answers"] = serde_json::json!(["unknown"]);
        spec["features"] = Value::Null;
        spec["methods"] = Value::Null;
    })?;
    let fail = stdout(&run(project, &["check_outline", spec], false));
    assert!(fail.contains("answers 'unknown', which is not a difficulty id"));
    assert!(fail.contains("difficulty midpoint: no step answers it"));
    assert!(!fail.contains("method author: missing"));
    mutate(&project.join(spec), |spec| {
        spec["overview"][0]["answers"] = serde_json::json!(["midpoint"]);
    })?;
    let checked = stdout(&run(project, &["check_outline", spec], true));
    assert!(checked.contains("0 problem(s) in the storyline layer"));
    assert!(!checked.contains("(0 elsewhere"));
    Ok(())
}

#[test]
fn companion_check_rejects_links_after_the_note_changes() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "companion/index.html";
    let build = || run(project, &["build_companion", spec, "--out", page], true);
    let check = |ok| run(project, &["check_companion", spec, page, "--no-dom"], ok);

    build();
    check(true);
    fs::write(
        project.join("note/main.aux"),
        r"\newlabel{sec:mean}{{2}{3}{Averages}{section.2}{}}",
    )?;
    assert!(stdout(&check(false)).contains("PDF links differ"));
    build();
    check(true);

    fs::copy(
        project.join("note/main.pdf"),
        project.join("note/rebuilt.pdf"),
    )?;
    mutate(&project.join(spec), |spec| {
        spec["note"]["pdf"] = "../note/rebuilt.pdf".into();
    })?;
    assert!(stdout(&check(false)).contains("PDF links differ"));
    build();
    check(true);

    let previous = fs::read(project.join(page))?;
    fs::write(project.join("note/main.aux"), "\\relax\n")?;
    let result = run(project, &["build_companion", spec, "--out", page], false);
    assert!(stdout(&result).contains("note label 'sec:mean' not in the note .aux"));
    assert_eq!(fs::read(project.join(page))?, previous);
    Ok(())
}

#[test]
fn checks_pending_figures_and_offline_resources() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "build/companion.html";
    mutate(&project.join(spec), |spec| {
        spec["figures"][0]["pending"] = Value::Bool(true)
    })?;
    run(project, &["build_companion", spec, "--out", page], true);
    let failed = run(project, &["check_companion", spec, page, "--no-dom"], false);
    assert!(stdout(&failed).contains("1 figure unit(s) not built yet"));
    run(
        project,
        &["check_companion", spec, page, "--no-dom", "--allow-pending"],
        true,
    );
    let oversized = run(
        project,
        &[
            "check_companion",
            spec,
            page,
            "--no-dom",
            "--allow-pending",
            "--max-kb",
            "1",
        ],
        false,
    );
    assert!(stdout(&oversized).contains("KB over the 1 KB limit"));
    let html = fs::read_to_string(project.join(page))?;
    fs::write(
        project.join(page),
        format!("{html}<img src='https://example.test/p.png'>"),
    )?;
    let failed = run(
        project,
        &["check_companion", spec, page, "--no-dom", "--allow-pending"],
        false,
    );
    assert!(stdout(&failed).contains("external resource"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn companion_checks_rendered_anchors_when_browser_output_is_available() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "build/companion.html";
    let template = "template.html";
    fs::write(project.join(template), "<main>No marker</main>")?;
    let failure = run(
        project,
        &[
            "build_companion",
            spec,
            "--out",
            page,
            "--template",
            template,
        ],
        false,
    );
    assert!(String::from_utf8_lossy(&failure.stderr).contains("template has no data marker"));
    assert!(!project.join(page).exists());

    fs::write(
        project.join(template),
        "<main></main><script>\nconst DATA = /*__COMPANION_DATA__*/null;\n</script>",
    )?;
    run(
        project,
        &[
            "build_companion",
            spec,
            "--out",
            page,
            "--template",
            template,
        ],
        true,
    );
    let browser = project.join("test-browser");
    fs::write(
        &browser,
        r##"#!/bin/sh
cat <<'EOF'
<main><div id="same"></div><div id="same"></div>
<a href="#missing">broken</a><script>const template = '<div id="missing">';</script></main>
EOF
"##,
    )?;
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o700))?;
    let out = Command::new(env!("CARGO_BIN_EXE_reading-companion"))
        .current_dir(project)
        .env("CHROME", &browser)
        .args(["check_companion", spec, page])
        .output()?;
    let diagnostics = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{diagnostics}");
    assert!(diagnostics.contains("id \"same\" appears 2 times"));
    assert!(diagnostics.contains("link #missing points to no element"));
    Ok(())
}

#[test]
fn companion_uses_the_original_parameter_grid_keys() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "build/companion.html";
    let point: Value = serde_json::from_str(r#"{"z":"平均","a":[0.00001,1e16,1e-305]}"#)?;
    let grid_value = r#"{"z": "\u5e73\u5747", "a": [1e-05, 1e+16, 1e-305]}"#;
    let key = format!("shape={grid_value}");
    mutate(&project.join(spec), |spec| {
        let feature = &mut spec["features"][0];
        feature["terms"] = serde_json::json!([{"term": "shape", "meaning": "synthetic inputs"}]);
        feature["author_code"]["params"] = serde_json::json!([{
            "name":"shape", "data":true, "effect":"changes the input", "default":point,
            "grid":["plain", point]
        }]);
        feature["presets"] = serde_json::json!([
            {"label":"Synthetic shape", "values":{"shape":point}, "expect":"a midpoint"}
        ]);
    })?;
    let results = project.join("companion/results.json");
    mutate(&results, |results| {
        let original = results["author"]["default"].clone();
        results["author"] = serde_json::json!({&key:original, "shape=plain":original});
    })?;
    run(project, &["build_companion", spec, "--out", page], true);
    let html = fs::read_to_string(project.join(page))?;
    let (_, payload) = html.split_once("const DATA = ").unwrap();
    let (payload, _) = payload.split_once(";\n").unwrap();
    let data: Value = serde_json::from_str(payload)?;
    let feature = &data["features"][0];
    let param = &feature["codes"]["author_code"]["params"][0];
    assert_eq!(param["grid_str"][1], grid_value);
    assert_eq!(param["default_str"], grid_value);
    assert!(feature["results"]["author"][&key]["points"].is_array());
    if has("node") {
        let (_, source) = html.split_once("const gridIndex = ").unwrap();
        let (grid_index, _) = source.split_once("\nconst circ = ").unwrap();
        let output = Command::new("node")
            .arg("-e")
            .arg(format!(
                "const gridIndex = {grid_index}\nconst data = {payload};\n\
                 const f = data.features[0];\n\
                 const assert = require('node:assert/strict');\n\
                 assert.equal(gridIndex(f.codes.author_code.params[0], f.presets[0].values.shape), 1);\n\
                 assert.equal(gridIndex({{grid: [0, 1e-13]}}, 1e-13), 1);"
            ))
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    mutate(&results, |results| {
        results["author"].as_object_mut().unwrap().remove(&key);
    })?;
    let failed = run(project, &["check_companion", spec, page, "--no-dom"], false);
    assert!(stdout(&failed).contains(&format!("author grid point '{key}' missing in results")));
    Ok(())
}

#[test]
fn companion_uses_the_last_dataset_record_in_list_and_provenance() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    let spec = "companion/companion.json";
    let page = "build/companion.html";
    let other = serde_json::json!({
        "id": "other", "name": "Other data", "source": "Other note",
        "identity": "[2, 4]", "level": "L2"
    });
    let last = serde_json::json!({
        "id": "toy", "name": "Recorded values", "source": "Revised note",
        "identity": "[5, 7]", "level": "L3", "revision": 2
    });
    mutate(&project.join(spec), |spec| {
        spec["datasets"][0]["obsolete"] = "discarded".into();
        let datasets = spec["datasets"].as_array_mut().unwrap();
        datasets.extend([other.clone(), last.clone()]);
    })?;
    run(project, &["build_companion", spec, "--out", page], true);
    let html = fs::read_to_string(project.join(page))?;
    let (_, payload) = html.split_once("const DATA = ").unwrap();
    let data = serde_json::Deserializer::from_str(payload)
        .into_iter::<Value>()
        .next()
        .unwrap()?;
    assert_eq!(data["datasets"], serde_json::json!([last, other]));
    assert_eq!(data["features"][0]["dataset_info"], last);

    mutate(&project.join(spec), |spec| {
        spec["datasets"][0]["name"] = Value::Null;
    })?;
    run(project, &["build_companion", spec, "--out", page], true);
    Ok(())
}

#[test]
fn section_builds_annotated_code_and_tex_and_checks_exact_line_coverage() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    run(project, &["check_section_record", "section.json"], true);
    let mut args = vec![
        "build_section_page",
        "section.json",
        "--out",
        "build/sections",
    ];
    let crop = has("pdftotext") && has("pdftoppm");
    if crop {
        args.extend(["--original", "note/main.pdf"]);
    }
    run(project, &args, true);
    let html = fs::read_to_string(project.join("build/sections/index.html"))?;
    if crop {
        assert!(html.contains("data-same=\"ot-1\""));
        assert!(html.contains("data:image/jpeg;base64,"));
        assert!(html.contains("p.1-2 (original"));
    }
    assert!(html.contains("The arithmetic mean</div>") || html.contains("The arithmetic mean<br>"));
    assert!(html.contains("Reuse the average for equal inputs."));
    assert!(html.contains("the same input twice."));
    assert!(
        fs::read_to_string(project.join("build/sections/codecards/c1.tex"))?
            .contains("language=Julia,firstline=2")
    );
    assert!(
        fs::read_to_string(project.join("build/sections/lang_defs.tex"))?
            .contains(r"\lstdefinelanguage{Julia}")
    );
    assert!(project.join("build/sections/panels/p2.tex").is_file());
    mutate(&project.join("section.json"), |record| {
        record["sections"][0]["steps"][0]["lines"]
            .as_array_mut()
            .unwrap()
            .pop();
    })?;
    let failed = run(project, &["check_section_record", "section.json"], false);
    assert!(stdout(&failed).contains("notes must cover lines 2-3 exactly once"));
    Ok(())
}

#[test]
fn section_language_override_applies_only_to_its_own_code_version() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    fs::write(project.join("avg.m"), "avg = (a + b) / 2;\n")?;
    fs::write(project.join("avg.R"), "mean(c(a,b)) # hidden_call(a,b)\n")?;
    mutate(&project.join("section.json"), |record| {
        record["sections"][0]["steps"][0] = serde_json::json!({
            "title":"Compute the average", "what":"Apply the formula", "math":"$m=(a+b)/2$",
            "file":"avg.m", "language":"octave", "first":1, "last":1,
            "lines":[[1,1,"Divide the sum of the inputs by two."]],
            "also":[{"file":"avg.R", "first":1, "last":1, "kind":"port",
                "verified":"Compared with Octave on two inputs.",
                "lines":[[1,1,"Use the built-in arithmetic mean."]],
                "apis":[{"name":"r:mean","note":"Mean of the input vector."},
                    {"name":"r:hidden_call","note":"This text appears in a comment only."}]}]
        });
    })?;
    let checked = run(project, &["check_section_record", "section.json"], true);
    assert!(stdout(&checked).contains("0 errors, 0 warnings"));
    let built = run(
        project,
        &[
            "build_section_page",
            "section.json",
            "--out",
            "build/sections",
        ],
        true,
    );
    let html = fs::read_to_string(project.join("build/sections/index.html"))?;
    assert!(html.contains("<b>Octave</b>"));
    assert!(html.contains("<b>R</b> · AI port"));
    assert!(html.contains("Compared with Octave on two inputs."));
    assert!(html.contains("Mean of the input vector."));
    assert!(!html.contains("This text appears in a comment only."));
    assert!(String::from_utf8_lossy(&built.stderr).contains("matches no call"));
    let tex = fs::read_to_string(project.join("build/sections/codecards/c1.tex"))?;
    assert!(tex.contains("language=Octave,firstline=1"));
    assert!(tex.contains("language=R,firstline=1"));
    Ok(())
}

#[test]
fn last_section_keeps_all_original_pages_in_its_recorded_range() -> Result<()> {
    if !has("pdftotext") || !has("pdftoppm") {
        return Ok(());
    }
    let dir = fixture("synthetic")?;
    let project = dir.path();
    mutate(&project.join("section.json"), |record| {
        record["sections"].as_array_mut().unwrap().truncate(1);
        record["edges"] = serde_json::json!([]);
    })?;
    run(
        project,
        &[
            "build_section_page",
            "section.json",
            "--out",
            "build/sections",
            "--original",
            "note/main.pdf",
        ],
        true,
    );
    let html = fs::read_to_string(project.join("build/sections/index.html"))?;
    assert!(html.contains("p.1-2 (original"));
    assert!(html.contains("to the end of p.2"));
    assert!(html.contains("p.2</span>"));
    Ok(())
}

#[test]
fn archive_pair_snapshots_local_files_and_rejects_a_remote() -> Result<()> {
    let dir = fixture("synthetic")?;
    let project = dir.path();
    git(project, &["init", "-q"])?;
    git(project, &["init", "-q", "writing/.corpus"])?;
    run(
        project,
        &[
            "build_companion",
            "companion/companion.json",
            "--out",
            "build/companion.html",
        ],
        true,
    );
    let related_code = project.join("note/related/code");
    fs::create_dir_all(related_code.parent().unwrap())?;
    fs::write(&related_code, "ordinary source file")?;
    let modified = UNIX_EPOCH + Duration::new(946_684_800, 123_456_700);
    for file in ["note/main.pdf", "build/companion.html", "note/related/code"] {
        fs::File::options()
            .write(true)
            .open(project.join(file))?
            .set_modified(modified)?;
    }
    run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        true,
    );
    let pair = read_json(&project.join("pair.json"))?;
    let archive = project.join(pair["corpus"]["dir"].as_str().unwrap());
    let manifest_name = archive.join("MANIFEST.json");
    let manifest = read_json(&manifest_name)?;
    assert_eq!(manifest["slug"], "mean");
    assert!(manifest["files"].get("source-note/main.aux").is_none());
    assert_eq!(
        fs::read(archive.join("source-companion/average.jl"))?,
        fs::read(project.join("companion/average.jl"))?
    );
    assert!(archive.join("source-note/main.tex").is_file());
    for (source, target) in [
        ("note/main.pdf", "note.pdf"),
        ("build/companion.html", "companion.html"),
        ("note/related/code", "source-note/related/code"),
    ] {
        let source = project.join(source);
        let archived = archive.join(target);
        let bytes = fs::read(&source)?;
        assert_eq!(fs::read(&archived)?, bytes);
        assert_eq!(
            manifest["files"][target],
            format!("{:x}", Sha256::digest(bytes))
        );
        assert_eq!(
            fs::metadata(archived)?.modified()?,
            fs::metadata(source)?.modified()?
        );
    }
    fs::remove_file(&related_code)?;
    fs::create_dir(&related_code)?;
    fs::write(related_code.join("clone.txt"), "repository clone")?;
    run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        true,
    );
    let manifest = read_json(&manifest_name)?;
    assert!(manifest["files"].get("source-note/related/code").is_none());
    assert!(manifest["files"]
        .get("source-note/related/code/clone.txt")
        .is_none());
    assert!(!archive.join("source-note/related/code").exists());
    fs::create_dir_all(project.join("results/work"))?;
    fs::write(project.join("results/kept.json"), "{}")?;
    fs::write(project.join("results/work/dropped.txt"), "scratch data")?;
    mutate(&project.join("pair.json"), |pair| {
        pair["note"]["source_dir"] = "results".into();
    })?;
    run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        true,
    );
    let manifest = read_json(&manifest_name)?;
    assert!(manifest["files"]["source-note/kept.json"].is_string());
    assert!(manifest["files"]
        .get("source-note/work/dropped.txt")
        .is_none());
    assert!(!archive.join("source-note/main.tex").exists());
    assert!(manifest["files"].get("source-note/main.tex").is_none());
    assert_eq!(
        read_json(&project.join("pair.json"))?["corpus"]["dir"],
        pair["corpus"]["dir"]
    );
    mutate(&project.join("pair.json"), |pair| {
        pair["note"]["pdf"] = "https://example.test/note.pdf".into();
    })?;
    let remote_pair = fs::read(project.join("pair.json"))?;
    run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        false,
    );
    assert_eq!(fs::read(project.join("pair.json"))?, remote_pair);
    assert_eq!(read_json(&manifest_name)?, manifest);
    mutate(&project.join("pair.json"), |pair| {
        pair["note"]["pdf"] = "note/main.pdf".into();
    })?;
    let local_pair = fs::read(project.join("pair.json"))?;
    git(
        &project.join("writing/.corpus"),
        &["remote", "add", "origin", "https://example.test/corpus"],
    )?;
    let refused = run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        false,
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("corpus has a remote"));
    assert_eq!(fs::read(project.join("pair.json"))?, local_pair);
    assert_eq!(read_json(&manifest_name)?, manifest);
    Ok(())
}

#[cfg(unix)]
#[test]
fn archive_pair_copies_in_tree_file_symlinks_as_files() -> Result<()> {
    use std::os::unix::fs::symlink;

    let dir = fixture("synthetic")?;
    let project = dir.path();
    git(project, &["init", "-q", "writing/.corpus"])?;
    fs::create_dir(project.join("build"))?;
    fs::write(project.join("build/companion.html"), "<!doctype html>")?;
    let note = project.join("note");
    let source = note.join("main.tex");
    fs::File::options()
        .write(true)
        .open(&source)?
        .set_modified(UNIX_EPOCH + Duration::new(946_684_800, 123_456_700))?;
    symlink("main.tex", note.join("alias.tex"))?;
    symlink("../companion/average.jl", note.join("outside.jl"))?;
    symlink(".", note.join("directory"))?;
    symlink("missing.tex", note.join("dangling.tex"))?;
    run(
        project,
        &["archive_pair", "pair.json", "--workspace", "."],
        true,
    );
    let pair = read_json(&project.join("pair.json"))?;
    let archive = project.join(pair["corpus"]["dir"].as_str().unwrap());
    let manifest = read_json(&archive.join("MANIFEST.json"))?;
    let alias = archive.join("source-note/alias.tex");
    assert!(fs::symlink_metadata(&alias)?.is_file());
    assert_eq!(fs::read(&alias)?, fs::read(&source)?);
    assert_eq!(
        fs::metadata(alias)?.modified()?,
        fs::metadata(source)?.modified()?
    );
    assert_eq!(
        manifest["files"]["source-note/alias.tex"],
        manifest["files"]["source-note/main.tex"]
    );
    for path in [
        "source-note/outside.jl",
        "source-note/directory",
        "source-note/dangling.tex",
    ] {
        assert!(archive.join(path).symlink_metadata().is_err());
        assert!(manifest["files"].get(path).is_none());
    }
    Ok(())
}

#[test]
fn proof_refs_and_hidden_formula_diagnostics_match_source() -> Result<()> {
    let dir = fixture("proof-book-example")?;
    let project = dir.path();
    let refs = run(project, &["proof_book", "refs", ".", "--block", "P"], true);
    assert_eq!(
        stdout(&refs),
        "P.1.2:\n    P.1.1 = \\frac1{n(n-1)}=\\frac1{n-1}-\\frac1n\n"
    );
    let path = project.join("blocks/P.txt");
    let block = fs::read_to_string(&path)?.replace("(P.1.1)", "(P.1.3)");
    fs::write(path, block)?;
    let bad = run(project, &["proof_book", "build", "."], false);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("reference to hidden (fine) line P.1.3"));
    assert!(!project.join("out").exists());
    Ok(())
}

#[test]
fn proof_check_reports_invalid_review_tex_and_real_html_anchors() -> Result<()> {
    let dir = fixture("proof-book-example")?;
    let project = dir.path();
    let out = project.join("out");
    fs::create_dir_all(out.join(".tex/.temp"))?;
    fs::write(out.join(".tex/.temp/main.log"), "Undefined reference\n")?;
    fs::copy(
        Path::new(FIXTURES).join("synthetic/note/main.pdf"),
        out.join("example.pdf"),
    )?;
    fs::write(
        out.join("example.html"),
        r##"<main><div data-id="missing"></div><a href="#missing">broken</a>
<div id='here'></div><a href="#here">valid</a>
<div id="one"></div><div data-id="one"></div></main>"##,
    )?;
    mutate(&project.join("review.json"), |review| {
        review["findings"][0]["where"] = serde_json::json!(["P.99"]);
    })?;
    let diagnostics = stdout(&run(project, &["proof_book", "check", "."], false));
    assert!(diagnostics.contains("unknown step P.99"));
    assert!(diagnostics.contains("Undefined reference"));
    assert!(diagnostics.contains("link #missing points to no element"));
    assert!(!diagnostics.contains("link #here points to no element"));
    assert!(!diagnostics.contains("id \"one\" appears"));
    Ok(())
}

#[test]
#[ignore = "requires LuaLaTeX with luatex-ja and latexmk"]
fn proof_book_builds_checks_and_links_every_formula() -> Result<()> {
    let dir = fixture("proof-book-example")?;
    let root = dir.path();
    let out = run(root, &["proof_book", "build", "."], true);
    assert!(stdout(&out).contains("equations numbered: 2 | ids mapped: 18 | blocks: 2"));
    run(root, &["proof_book", "check", "."], true);
    let pages = read_json(&root.join("out/pages.json"))?;
    assert_eq!(
        pages,
        read_json(&PathBuf::from(FIXTURES).join("proof-book-example/expected-pages.json"))?
    );
    let html = fs::read_to_string(root.join("out/example.html"))?;
    assert!(html.contains(r#"class="eqno">(P.1.1)"#));
    assert!(html.contains(r#"class="fine" id="P.1.3""#));
    assert!(html.contains(r#"href="example.pdf#page=4""#));
    if has("pdfinfo") {
        let pdf = Command::new("pdfinfo")
            .args(["-url", "out/example.pdf"])
            .current_dir(root)
            .output()?;
        assert!(pdf.status.success());
        let links = stdout(&pdf);
        assert!(links
            .lines()
            .any(|line| line.contains("reading%20book%20%25%20")
                && line.ends_with("example.html#P.1.1")));
    }
    Ok(())
}
