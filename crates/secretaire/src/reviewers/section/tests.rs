use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use axum::{
    body::to_bytes,
    extract::{Query, State},
    Json,
};
use fuzukue_proto::ServerEvent;
use serde_json::{json, Value};

use super::*;
use crate::test_support::TestDir;

fn fixture() -> TestDir {
    let dir = TestDir::new();
    dir.write("lecture/deck.toml", "kind = 'notes'\n");
    dir.write(
        "lecture/cards/z/card.md",
        "---\nid: z\nrequires: [needed]\n---\nOriginal notes.\n",
    );
    dir.write(
        "lecture/cards/z/statement.tex",
        r"\ref{lem:proof-dep} \input{nested/proof-input}",
    );
    dir.write("lecture/cards/z/proof.tex", "Original proof.\n");
    dir.write("lecture/cards/z/nested/proof-input.tex", r"\input{detail}");
    dir.write("lecture/cards/z/nested/detail.tex", "Nested evidence.");
    dir.write("lecture/cards/a/card.md", "---\nid: a\n---\nSecond.\n");
    dir.write(
        "lecture/cards/needed/card.md",
        "---\nid: needed\n---\nPrerequisite.\n",
    );
    dir.write(
        "lecture/cards/proof-dep/card.md",
        "---\nid: proof-dep\n---\n",
    );
    dir.write(
        "lecture/cards/proof-dep/statement.tex",
        r"\label{lem:proof-dep} A TeX prerequisite.",
    );
    dir.write(
        "lecture/cards/unrelated/card.md",
        "---\nid: unrelated\n---\nUnrelated.\n",
    );
    dir
}

fn ids() -> Vec<String> {
    vec!["z".into(), "a".into()]
}

#[test]
fn math_captures_ordered_targets_and_transitive_dependencies() -> Result<()> {
    let live = fixture();
    let work = TestDir::new();
    let first_dir = work.path().join("first");
    let first = prepare(
        live.path(),
        &first_dir,
        "lecture",
        &[Kind::Math],
        Some("s1".into()),
        ids(),
    )?;
    let (review, hash) = first.math.unwrap();
    let prompt = review.prompt("lecture", &[])?;
    assert!(prompt.find("\"id\": \"z\"").unwrap() < prompt.find("\"id\": \"a\"").unwrap());
    assert!(prompt.contains("lecture/cards/proof-dep/statement.tex"));
    assert!(prompt.contains("lecture/cards/needed/card.md"));
    assert!(first_dir
        .join("lecture/cards/z/nested/detail.tex")
        .is_file());
    assert!(!first_dir.join("lecture/cards/unrelated/card.md").exists());

    live.write(
        "lecture/cards/needed/card.md",
        "---\nid: needed\n---\nChanged prerequisite.\n",
    );
    let second = prepare(
        live.path(),
        &work.path().join("second"),
        "lecture",
        &[Kind::Math],
        Some("s1".into()),
        ids(),
    )?;
    assert_eq!(first.scope.reviewed_version, second.scope.reviewed_version);
    assert_ne!(hash, second.math.unwrap().1);
    assert!(
        fs::read_to_string(first_dir.join("lecture/cards/needed/card.md"))?
            .contains("Prerequisite.")
    );

    live.write(
        "lecture/cards/z/nested/detail.tex",
        "Changed nested evidence.",
    );
    let third = prepare(
        live.path(),
        &work.path().join("third"),
        "lecture",
        &[Kind::Math],
        Some("s1".into()),
        ids(),
    )?;
    assert_ne!(first.scope.reviewed_version, third.scope.reviewed_version);
    Ok(())
}

async fn request(app: &Arc<Workspace>, kind: &str, cards: Vec<String>) -> Result<Value> {
    call(
        app,
        "request_review",
        json!({"deck":"lecture", "kind":kind, "section":"s1", "cards":cards}),
    )
    .await
}

async fn call(app: &Arc<Workspace>, tool: &str, arguments: Value) -> Result<Value> {
    let response = crate::mcp::handler(
        State(app.clone()),
        Query(crate::mcp::McpQuery {
            session: "test".into(),
            backend: None,
            workspace: None,
            job: None,
        }),
        axum::http::HeaderMap::new(),
        Json(
            json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params": {
                "name": tool,
                "arguments": arguments
            }}),
        ),
    )
    .await;
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
    Ok(body["result"].clone())
}

fn recorder(live: &TestDir, card: &str) -> Result<()> {
    live.write(
        "lecture/deck.toml",
        "kind = 'notes'\n[build]\ntex_root = 'lecture.tex'\nengine = 'pdflatex'\n",
    );
    live.write(
        "lecture/lecture.tex",
        &format!("\\input{{cards/{card}/proof.tex}}\n"),
    );
    live.write("lecture/earlier.tex", "Earlier section.");
    live.write("lecture/.latexmkrc", "$pdflatex = './engine %O %S';\n");
    let script = r#"#!/usr/bin/python3
from pathlib import Path
import sys
out = Path(next(arg.split('=', 1)[1] for arg in sys.argv if arg.startswith('-output-directory=')))
assert '-recorder' in sys.argv
root = Path.cwd()
proof = root / 'cards/RENDERED_CARD/proof.tex'
(out / 'lecture.log').write_text('This is pdfTeX\nOutput written on ' + str(out / 'lecture.pdf') + ' (1 page, 60 bytes).\n')
(out / 'lecture.pdf').write_text('%PDF-1.7\n' + proof.read_text())
(out / 'lecture.aux').write_text('\\relax\n')
(out / 'lecture.idx').write_text('index entries\n')
paths = [root / 'lecture.tex', root / 'earlier.tex', proof, out / 'lecture.aux']
outputs = [out / p for p in ('lecture.pdf', 'lecture.log', 'lecture.aux', 'lecture.idx')]
(out / 'lecture.fls').write_text('PWD ' + str(root) + '\n' + ''.join(
    command + ' ' + str(path) + '\n'
    for command, group in [('INPUT', paths), ('OUTPUT', outputs)] for path in group))
"#;
    live.write("lecture/engine", &script.replace("RENDERED_CARD", card));
    fs::set_permissions(
        live.path().join("lecture/engine"),
        fs::Permissions::from_mode(0o755),
    )?;
    Ok(())
}

fn latexmk_available() -> bool {
    std::process::Command::new("latexmk")
        .arg("-version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn paired_request_reviews_only_captured_files_and_records_the_shared_version() -> Result<()> {
    if !latexmk_available() {
        eprintln!("skipping recorder fixture; missing latexmk");
        return Ok(());
    }
    let live = fixture();
    recorder(&live, "z")?;
    let state = TestDir::new();
    let reviewer = state.path().join("reviewer");
    let release = serde_json::to_string(&state.path().join("release"))?;
    state.write("reviewer", &format!(r#"#!/usr/bin/python3
import json, pathlib, sys, time
prompt = json.loads(sys.stdin.readline())["message"]["content"][0]["text"]
release = pathlib.Path({release})
for _ in range(500):
    if release.exists():
        break
    time.sleep(.02)
else:
    raise AssertionError('review not released')
p = pathlib.Path('lecture/cards/z/proof.tex')
assert p.read_text() == 'Original proof.\n'
if 'lecture/build/lecture.pdf' in prompt:
    assert 'Original proof.' in pathlib.Path('lecture/build/lecture.pdf').read_text()
    assert pathlib.Path('lecture/earlier.tex').read_text() == 'Earlier section.'
    assert 'Original notes.' in pathlib.Path('lecture/cards/z/card.md').read_text()
    assert 'Second.' in pathlib.Path('lecture/cards/a/card.md').read_text()
    assert pathlib.Path('lecture/build/lecture.idx').exists()
    findings = []
else:
    assert not pathlib.Path('lecture/cards/unrelated/card.md').exists()
    assert 'A TeX prerequisite.' in pathlib.Path('lecture/cards/proof-dep/statement.tex').read_text()
    findings = [{{"card":"z", "text":"Scoped finding"}}, {{"card":"needed", "text":"Out of scope"}}]
print(json.dumps({{"type":"result", "result":json.dumps({{"findings":findings}})}}))
"#));
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(
        live.path().canonicalize()?,
        String::new(),
        state.path().to_owned(),
        Arc::new(crate::hub::Hub::new()),
    );
    app.claude_program = Some(reviewer);
    app.review_settings.lock().unwrap().math_model = "claude-sonnet-5-5".into();
    let app = Arc::new(app);

    let (_, mut events) = app.hub.subscribe(0);
    let invalid = request(&app, "both", vec!["z".into(), "missing".into()]).await?;
    assert_eq!(invalid["isError"], true, "{invalid}");
    assert!(app.reviews.jobs.lock().unwrap().is_empty());
    let dispatched = request(&app, "both", ids()).await?;
    assert_eq!(dispatched["isError"], false, "{dispatched}");
    let dispatched = &dispatched["structuredContent"];
    assert_eq!(dispatched["cards"], json!(["z", "a"]));
    let runs = dispatched["reviews"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0]["input_hash"], runs[1]["input_hash"]);
    assert_eq!(runs[0]["kind"], "math");
    assert_eq!(runs[1]["kind"], "output");

    live.write("lecture/cards/z/proof.tex", "Edited after dispatch.\n");
    live.write("lecture/earlier.tex", "Edited after dispatch.\n");
    let busy = request(&app, "both", ids()).await?;
    assert_eq!(busy["isError"], true, "{busy}");
    assert_eq!(app.reviews.jobs.lock().unwrap().len(), 2);
    state.write("release", "");

    let mut done = BTreeSet::new();
    while done.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await??;
        if let ServerEvent::ReviewStatus {
            kind,
            state,
            count,
            message,
            ..
        } = event.event
        {
            match state.as_str() {
                "running" => {}
                "done" => {
                    assert_eq!(count, Some(if kind == "math" { 1 } else { 0 }));
                    done.insert(kind);
                }
                _ => panic!("{kind} review {state}: {message}"),
            }
        }
    }
    assert!(!live.path().join("lecture/build").exists());
    for run in runs {
        let session = run["session"].as_str().unwrap();
        let path = state
            .path()
            .join("section-reviews")
            .join(format!("{session}.json"));
        let receipt: Value = serde_json::from_slice(&fs::read(path)?)?;
        assert_eq!(receipt["scope"]["section"], "s1");
        assert_eq!(
            receipt["scope"]["reviewed_version"],
            dispatched["reviewed_version"]
        );
        assert_eq!(receipt["review"]["input_hash"], run["input_hash"]);
    }
    let drafts = comments::read(&live.path().join("lecture"))?;
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].card, "z");
    Ok(())
}

#[tokio::test]
async fn course_recovers_two_clean_section_reviews_in_a_fresh_process() -> Result<()> {
    use fuzukue_cards::course::{Course, ReviewStatus, Status};

    const RESTART_PROJECT: &str = "FUZUKUE_TEST_SECTION_RECEIPT_RESTART_PROJECT";
    const RESTART_STATE: &str = "FUZUKUE_TEST_SECTION_RECEIPT_RESTART_STATE";
    let done = json!({"deck":"lecture", "sections":[{"key":"s1", "status":"done"}]});
    let workspace = |project: &Path, state: &Path| {
        Workspace::new(
            project.to_owned(),
            String::new(),
            state.to_owned(),
            Arc::new(crate::hub::Hub::new()),
        )
    };
    if let Some(project) = std::env::var_os(RESTART_PROJECT) {
        let state = std::env::var_os(RESTART_STATE).context("missing restart state directory")?;
        let app = Arc::new(workspace(Path::new(&project), Path::new(&state)));
        let result = call(&app, "course_plan", done).await?;
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(result["structuredContent"]["sections"][0]["status"], "done");
        return Ok(());
    }

    if !latexmk_available() {
        eprintln!("skipping recorder fixture; missing latexmk");
        return Ok(());
    }
    let live = TestDir::new();
    let state = TestDir::new();
    let lecture = live.path().join("lecture");
    let card = "lec-s1";
    let proof = "lecture/cards/lec-s1/proof.tex";
    recorder(&live, card)?;
    live.write("book/deck.toml", "kind = 'book'\n");
    live.write(
        "book/structure.yaml",
        "title: Book\nsections:\n- key: s1\n  title: Section\n  items: [source]\n",
    );
    live.write(
        "book/cards/source/card.md",
        "---\nid: source\nkind: expl\ntitle: Source\n---\n",
    );

    let release = serde_json::to_string(&state.path().join("release"))?;
    state.write(
        "reviewer",
        &format!(
            r#"#!/usr/bin/python3
import json, pathlib, sys, time
sys.stdin.readline()
release = pathlib.Path({release})
for _ in range(500):
    if release.exists():
        break
    time.sleep(.02)
else:
    raise AssertionError('review not released')
print(json.dumps({{"type":"result", "result":"{{\"findings\":[]}}"}}))
"#
        ),
    );
    let mut app = workspace(live.path(), state.path());
    let reviewer = state.path().join("reviewer");
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o755))?;
    app.claude_program = Some(reviewer);
    app.review_settings.lock().unwrap().math_model = "claude-sonnet-5-5".into();
    let app = Arc::new(app);
    for (tool, args) in [
        (
            "course_plan",
            json!({"deck":"lecture", "book":"book", "sections":[{"key":"s1","status":"adopted"}]}),
        ),
        ("course_next", json!({"deck":"lecture"})),
    ] {
        let result = call(&app, tool, args).await?;
        assert_eq!(result["isError"], false, "{result}");
    }
    live.write(
        "lecture/cards/lec-s1/card.md",
        "---\nid: lec-s1\nkind: expl\ntitle: Lecture\nfrom: book#s1\n---\n",
    );
    live.write(proof, "Reviewed proof.\n");
    let committed = call(
        &app,
        "course_commit",
        json!({"deck":"lecture", "section":"s1", "cards":[card]}),
    )
    .await?;
    assert_eq!(committed["isError"], false, "{committed}");

    let wrong = call(
        &app,
        "request_review",
        json!({"deck":"lecture", "section":"other", "kind":"both", "cards":[card]}),
    )
    .await?;
    assert_eq!(wrong["isError"], true, "{wrong}");
    assert!(app.reviews.jobs.lock().unwrap().is_empty());
    let (_, mut events) = app.hub.subscribe(0);
    let result = call(
        &app,
        "request_review",
        json!({"deck":"lecture", "kind":"both", "cards":[card]}),
    )
    .await?;
    assert_eq!(result["isError"], false, "{result}");
    let dispatched = &result["structuredContent"];
    assert_eq!(dispatched["section"], "s1");
    let runs = dispatched["reviews"].as_array().context("missing jobs")?;
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0]["input_hash"], runs[1]["input_hash"]);
    let course_path = Course::path(&lecture);
    let pending = fs::read(&course_path)?;
    assert!(Course::read(&lecture)?.unwrap().sections[0]
        .reviews
        .values()
        .all(|review| review.status == ReviewStatus::Pending));
    assert_eq!(
        call(&app, "course_plan", done.clone()).await?["isError"],
        true
    );
    state.write("release", "");
    let mut finished = BTreeSet::new();
    while finished.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await??;
        if let ServerEvent::ReviewStatus {
            kind,
            state,
            count,
            message,
            ..
        } = event.event
        {
            match state.as_str() {
                "running" => {}
                "done" => {
                    assert_eq!(count, Some(0), "{kind}");
                    finished.insert(kind);
                }
                _ => panic!("{kind} review {state}: {message}"),
            }
        }
    }
    let applied = Course::read(&lecture)?.unwrap();
    assert_eq!(applied.sections[0].status, Status::Written);
    assert!(applied.sections[0]
        .reviews
        .values()
        .all(|review| review.status == ReviewStatus::Clean));
    assert!(comments::read(&lecture)?.is_empty());
    drop(app);

    // Recreate the interruption after the receipts reached disk and before the course write.
    fs::write(&course_path, &pending)?;
    let restarted = std::process::Command::new(std::env::current_exe()?)
        .env(RESTART_PROJECT, live.path())
        .env(RESTART_STATE, state.path())
        .args(["--exact", "reviewers::section::tests::course_recovers_two_clean_section_reviews_in_a_fresh_process", "--nocapture"])
        .output()?;
    assert!(
        restarted.status.success()
            && !String::from_utf8_lossy(&restarted.stdout).contains("running 0 tests"),
        "{}\n{}",
        String::from_utf8_lossy(&restarted.stdout),
        String::from_utf8_lossy(&restarted.stderr)
    );
    let recovered = Course::read(&lecture)?.unwrap();
    let section = &recovered.sections[0];
    assert_eq!(section.key, "s1");
    assert_eq!(section.status, Status::Done);
    assert_eq!(section.reviews, applied.sections[0].reviews);
    for run in runs {
        let kind = run["kind"].as_str().unwrap();
        let request = section.reviews[kind].request.as_ref().unwrap();
        assert_eq!(request.session, run["session"]);
        assert_eq!(request.reviewed_version, dispatched["reviewed_version"]);
        assert_eq!(request.input_hash, run["input_hash"]);
    }

    let reopened = Arc::new(workspace(live.path(), state.path()));
    let saved = fs::read(&course_path)?;
    let replay = call(&reopened, "course_plan", done.clone()).await?;
    assert_eq!(replay["structuredContent"]["changed"], false, "{replay}");
    assert_eq!(fs::read(&course_path)?, saved);
    live.write("lecture/earlier.tex", "Edited output dependency.\n");
    let inspect = json!({"deck":"lecture"});
    let report = call(&reopened, "course_plan", inspect.clone()).await?;
    assert_eq!(
        report["structuredContent"]["sections"][0]["stale_reviews"],
        json!(["output"]),
        "{report}"
    );
    live.write(proof, "Changed proof.\n");
    let report = call(&reopened, "course_plan", inspect).await?;
    assert_eq!(
        report["structuredContent"]["sections"][0]["stale_reviews"],
        json!(["math", "output"]),
        "{report}"
    );
    assert_eq!(call(&reopened, "course_plan", done).await?["isError"], true);
    Ok(())
}

#[tokio::test]
async fn paired_request_renders_frozen_inputs_and_records_two_clean_receipts() -> Result<()> {
    for (tool, args) in [
        ("latexmk", &["-v"][..]),
        ("pdflatex", &["-v"][..]),
        ("pdftotext", &["-v"][..]),
        (
            "kpsewhich",
            &["-engine=pdftex", "-format=fmt", "pdflatex.fmt"][..],
        ),
    ] {
        match std::process::Command::new(tool).args(args).output() {
            Ok(output) if output.status.success() => {}
            Ok(_) => {
                eprintln!("skipping real section render; {tool} found no usable installation");
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping real section render; missing {tool}");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }

    let live = fixture();
    let state = TestDir::new();
    let proof = "lecture/cards/z/proof.tex";
    let preamble = "lecture/preamble.tex";
    let asset = "lecture/assets/figure.pdf";
    let pdf = "lecture/build/lecture.pdf";
    live.write(
        "lecture/deck.toml",
        "kind = 'notes'\n[build]\ntex_root = 'lecture.tex'\nengine = 'pdflatex'\n",
    );
    live.write(
        "lecture/cards/z/card.md",
        "---\nid: z\nfiles: [assets/figure.pdf]\n---\nSection notes.\n",
    );
    live.write(
        "lecture/lecture.tex",
        r"\documentclass{article}
\usepackage{graphicx}
\input{preamble.tex}
\begin{document}
\input{cards/z/proof.tex}
\par\sharedmarker
\par\includegraphics[width=.75\linewidth]{assets/figure.pdf}
\end{document}
",
    );
    live.write(proof, "FrozenSection\n");
    live.write(
        preamble,
        "\\newcommand{\\sharedmarker}{FrozenConfiguration}\n",
    );
    for marker in ["FrozenAsset", "LiveAsset"] {
        state.write(
            &format!("{marker}.tex"),
            &format!(
                "\\documentclass{{article}}\n\\pagestyle{{empty}}\n\\begin{{document}}{marker}\\end{{document}}\n"
            ),
        );
        let render = std::process::Command::new("pdflatex")
            .args(["-halt-on-error", "-interaction=nonstopmode"])
            .arg(format!("{marker}.tex"))
            .current_dir(state.path())
            .output()?;
        ensure!(
            render.status.success(),
            "{}",
            String::from_utf8_lossy(&render.stdout)
        );
    }
    fs::create_dir_all(live.path().join("lecture/assets"))?;
    fs::copy(
        state.path().join("FrozenAsset.pdf"),
        live.path().join(asset),
    )?;
    let originals = [proof, preamble, asset]
        .map(|path| Ok((path, file_hash(&live.path().join(path))?)))
        .into_iter()
        .collect::<Result<BTreeMap<_, _>>>()?;

    // Block immediately before the real engine runs. The marker carries its captured cwd.
    let state_path = serde_json::to_string(state.path())?;
    live.write("lecture/.latexmkrc", "$pdflatex = './engine %O %S';\n");
    live.write(
        "lecture/engine",
        &format!(
            r#"#!/usr/bin/python3
import os, sys, time
from pathlib import Path
state = Path({state_path})
release = state / 'release-engine'
if not release.exists():
    marker = state / 'engine-ready.tmp'
    marker.write_text(str(Path.cwd()))
    marker.rename(state / 'engine-ready')
    deadline = time.monotonic() + 20
    while not release.exists():
        assert time.monotonic() < deadline, 'engine was not released'
        time.sleep(.02)
os.execvp('pdflatex', ['pdflatex', *sys.argv[1:]])
"#,
        ),
    );
    fs::set_permissions(
        live.path().join("lecture/engine"),
        fs::Permissions::from_mode(0o755),
    )?;

    // The reviewer fixtures inspect the staged bytes and return deterministic empty results.
    let reviewer = state.path().join("reviewer");
    let originals_json = serde_json::to_string(&serde_json::to_string(&originals)?)?;
    state.write(
        "reviewer",
        &format!(
            r#"#!/usr/bin/python3
import hashlib, json, shutil, sys
from pathlib import Path
prompt = json.loads(sys.stdin.readline())["message"]["content"][0]["text"]
state = Path({state_path})
for path, digest in json.loads({originals_json}).items():
    assert hashlib.sha256(Path(path).read_bytes()).hexdigest() == digest, path
if {pdf:?} in prompt:
    shutil.copyfile({pdf:?}, state / 'rendered.pdf')
print(json.dumps({{"type":"result", "result":"{{\"findings\":[]}}"}}))
"#,
        ),
    );
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o755))?;

    let mut app = Workspace::new(
        live.path().to_owned(),
        String::new(),
        state.path().to_owned(),
        Arc::new(crate::hub::Hub::new()),
    );
    app.claude_program = Some(reviewer);
    app.review_settings.lock().unwrap().math_model = "claude-sonnet-5-5".into();
    let app = Arc::new(app);
    let (_, mut events) = app.hub.subscribe(0);
    let deck = live.path().join("lecture");
    assert!(comments::read(&deck)?.is_empty());

    let dispatch_task = tokio::spawn({
        let app = app.clone();
        async move { request(&app, "both", ids()).await }
    });
    let ready = state.path().join("engine-ready");
    crate::test_support::wait_for_path(&ready).await?;
    let captured_deck = PathBuf::from(fs::read_to_string(&ready)?);
    assert_ne!(captured_deck, deck);
    assert!(app.reviews.jobs.lock().unwrap().is_empty());
    for (path, digest) in &originals {
        assert_eq!(
            file_hash(&captured_deck.parent().unwrap().join(path))?,
            *digest
        );
    }
    live.write(proof, "LiveSection\n");
    live.write(
        preamble,
        "\\newcommand{\\sharedmarker}{LiveConfiguration}\n",
    );
    fs::copy(state.path().join("LiveAsset.pdf"), live.path().join(asset))?;
    for (path, digest) in &originals {
        assert_ne!(file_hash(&live.path().join(path))?, *digest, "{path}");
    }
    state.write("release-engine", "");

    let response = tokio::time::timeout(Duration::from_secs(20), dispatch_task).await???;
    assert_eq!(response["isError"], false, "{response}");
    let dispatched = &response["structuredContent"];
    assert_eq!(dispatched["section"], "s1");
    assert_eq!(dispatched["cards"], json!(["z", "a"]));
    let runs = dispatched["reviews"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0]["input_hash"], runs[1]["input_hash"]);

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut done = BTreeSet::new();
        while done.len() < 2 {
            let event = events.recv().await.unwrap();
            if let ServerEvent::ReviewStatus {
                kind,
                state,
                count,
                message,
                ..
            } = event.event
            {
                match state.as_str() {
                    "running" => {}
                    "done" => {
                        assert_eq!(count, Some(0), "{kind}");
                        assert!(comments::read(&deck).unwrap().is_empty());
                        assert!(!comments::file(&deck).exists());
                        done.insert(kind);
                    }
                    _ => panic!("{kind} review {state}: {message}"),
                }
            }
        }
        assert_eq!(done, BTreeSet::from(["math".into(), "output".into()]));
    })
    .await?;

    let rendered = state.path().join("rendered.pdf");
    let extracted = std::process::Command::new("pdftotext")
        .arg(&rendered)
        .arg("-")
        .output()?;
    assert!(
        extracted.status.success(),
        "{}",
        String::from_utf8_lossy(&extracted.stderr)
    );
    let content = String::from_utf8(extracted.stdout)?;
    for source in ["Section", "Configuration", "Asset"] {
        assert!(content.contains(&format!("Frozen{source}")), "{content}");
        assert!(!content.contains(&format!("Live{source}")), "{content}");
    }

    for (run, kind) in runs.iter().zip(["math", "output"]) {
        assert_eq!(run["kind"], kind);
        let session = run["session"]
            .as_str()
            .context("missing dispatch session")?;
        let path = state
            .path()
            .join("section-reviews")
            .join(format!("{session}.json"));
        let receipt: Value = serde_json::from_slice(&fs::read(path)?)?;
        assert_eq!(receipt["deck"], "lecture");
        for key in ["section", "cards", "reviewed_version"] {
            assert_eq!(receipt["scope"][key], dispatched[key]);
        }
        assert_eq!(&receipt["review"], run);
        assert_eq!(receipt["findings"], json!([]));
        let capture = &receipt["capture"];
        let files = if kind == "math" {
            &capture["files"]
        } else {
            assert_eq!(capture["inputs"]["engine"], "pdflatex");
            assert_eq!(capture["pdf"], pdf);
            assert_eq!(capture["artifacts"][pdf], file_hash(&rendered)?);
            &capture["inputs"]["workspace_files"]
        };
        for (path, digest) in &originals {
            assert_eq!(&files[path], digest, "{kind}: {path}");
        }
        eprintln!(
            "{kind}: {session}, reviewed_version={}, input_hash={}",
            dispatched["reviewed_version"], run["input_hash"]
        );
    }
    eprintln!(
        "Captured SHA-256: {originals:?}\nExtracted PDF text: {}",
        content.trim()
    );
    assert!(!deck.join("build").exists());
    Ok(())
}
